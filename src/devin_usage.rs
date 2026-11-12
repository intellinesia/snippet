//! Devin plan-quota probe (`SeatManagementService/GetUserStatus`), cached under
//! `~/.snippet/devin/usage.json` and refreshed in the background.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::{RateLimitSnapshot, RateLimitWindow};

const ENDPOINT: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const DEFAULT_SERVER: &str = "https://server.codeium.com";
const IDE_VERSION: &str = "1.108.2";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageCache {
    fetched_at: i64,
    accounts: HashMap<String, RateLimitSnapshot>,
}

static REFRESHING: AtomicBool = AtomicBool::new(false);

fn cache_path() -> PathBuf {
    crate::config::snippet_home().join("devin/usage.json")
}

fn load_cache() -> Option<UsageCache> {
    serde_json::from_str(&std::fs::read_to_string(cache_path()).ok()?).ok()
}

fn store_cache(cache: &UsageCache) {
    let path = cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, body);
    }
}

/// Every account's probed snapshot, in pool order.
pub fn cached() -> Vec<RateLimitSnapshot> {
    let Some(cache) = load_cache() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in crate::accounts::ids("devin") {
        if let Some(snap) = cache.accounts.get(&id) {
            seen.insert(id.clone());
            out.push(snap.clone());
        }
    }
    let mut rest: Vec<&String> = cache
        .accounts
        .keys()
        .filter(|k| !seen.contains(*k))
        .collect();
    rest.sort();
    for id in rest {
        out.push(cache.accounts[id].clone());
    }
    out
}

pub fn refresh_if_stale(max_age_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    if load_cache().is_some_and(|c| now - c.fetched_at < max_age_secs) {
        return;
    }
    if REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let ids = crate::accounts::ids("devin");
        let multi = ids.len() > 1;
        let active = crate::accounts::active("devin");
        let mut accounts = HashMap::new();
        for id in &ids {
            let Some((key, server)) = credentials_for(id) else {
                continue;
            };
            let Some(body) = fetch_status(&key, &server) else {
                continue;
            };
            let label = multi.then(|| crate::accounts::label("devin", id));
            let Some(snap) = parse_user_status(&body, label.as_deref()) else {
                continue;
            };
            if active.as_deref() == Some(id.as_str()) {
                crate::accounts::record_usage("devin", id, &snap);
            }
            accounts.insert(id.clone(), snap);
        }
        if !accounts.is_empty() {
            store_cache(&UsageCache {
                fetched_at: chrono::Utc::now().timestamp(),
                accounts,
            });
        }
        REFRESHING.store(false, Ordering::SeqCst);
    });
}

fn slot(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn parse_credentials_toml(raw: &str) -> Option<(String, String)> {
    let table = raw.parse::<toml::Table>().ok()?;
    let key = table
        .get("windsurf_api_key")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|k| !k.is_empty())?
        .to_string();
    let server = table
        .get("api_server_url")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_SERVER)
        .to_string();
    Some((key, server))
}

fn credentials_for(id: &str) -> Option<(String, String)> {
    if id == crate::accounts::MAIN {
        let path = crate::devin::real_data_dir()?.join("credentials.toml");
        return parse_credentials_toml(&std::fs::read_to_string(path).ok()?);
    }
    let home_creds = crate::config::snippet_home()
        .join("devin/accounts")
        .join(slot(id))
        .join("data/devin/credentials.toml");
    if let Ok(raw) = std::fs::read_to_string(&home_creds)
        && let Some(creds) = parse_credentials_toml(&raw)
    {
        return Some(creds);
    }
    let token = crate::devin::account_token(id)?;
    Some((token, DEFAULT_SERVER.to_string()))
}

fn fetch_status(key: &str, server: &str) -> Option<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let url = format!("{}{}", server.trim_end_matches('/'), ENDPOINT);
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .json(&serde_json::json!({
            "metadata": {
                "apiKey": key,
                "ideName": "devin",
                "ideVersion": IDE_VERSION,
                "extensionName": "devin",
                "extensionVersion": IDE_VERSION,
                "locale": "en",
            }
        }))
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().ok()
}

fn num(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn unix(value: Option<&Value>) -> i64 {
    num(value).map(|n| n as i64).unwrap_or(0)
}

/// Parse a `GetUserStatus` response. `account_label` is only `Some` when more
/// than one devin account exists. Returns `None` when no plan status is present.
pub fn parse_user_status(body: &Value, account_label: Option<&str>) -> Option<RateLimitSnapshot> {
    let status = body
        .pointer("/userStatus/planStatus")
        .or_else(|| body.get("planStatus"))?;
    let plan_info = status.get("planInfo").unwrap_or(&Value::Null);
    let plan_name = plan_info
        .get("planName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Devin");
    let hide_daily = plan_info
        .get("hideDailyQuota")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let daily_remaining = num(status.get("dailyQuotaRemainingPercent"));
    let weekly_remaining = num(status.get("weeklyQuotaRemainingPercent"));
    let daily_reset = unix(status.get("dailyQuotaResetAtUnix"));
    let weekly_reset = unix(status.get("weeklyQuotaResetAtUnix"));

    let mut primary = None;
    if !hide_daily && let Some(remaining) = daily_remaining {
        primary = Some(RateLimitWindow {
            used_percent: (100.0 - remaining).clamp(0.0, 100.0),
            window_minutes: 1440,
            resets_at: daily_reset,
        });
    }
    // Quirk: with hideDailyQuota and no weekly percent, dailyQuotaRemainingPercent
    // is the weekly USED percent (100 = fully used), not a remaining figure.
    let weekly_used = if let Some(remaining) = weekly_remaining {
        Some(100.0 - remaining)
    } else if hide_daily {
        daily_remaining
    } else {
        None
    };
    let secondary = weekly_used.map(|used| RateLimitWindow {
        used_percent: used.clamp(0.0, 100.0),
        window_minutes: 10080,
        resets_at: weekly_reset,
    });

    let mut label = format!("{plan_name} plan");
    if let Some(micros) = num(status.get("overageBalanceMicros"))
        && micros > 0.0
    {
        label.push_str(&format!(" · ${:.2} extra", micros / 1_000_000.0));
    }
    if let Some(account) = account_label {
        label = format!("{account} · {label}");
    }

    Some(RateLimitSnapshot {
        primary,
        secondary,
        label: Some(label),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hidden_daily_quota_reports_weekly_used_percent_and_extra_balance() {
        let body = json!({
            "userStatus": {
                "planStatus": {
                    "planInfo": {"planName": "Max", "hideDailyQuota": true},
                    "dailyQuotaRemainingPercent": 37,
                    "dailyQuotaResetAtUnix": "1750000000",
                    "weeklyQuotaResetAtUnix": 1750600000,
                    "overageBalanceMicros": "1829590000"
                }
            }
        });
        let snap = parse_user_status(&body, None).expect("snapshot");
        assert!(snap.primary.is_none(), "daily window hidden");
        let weekly = snap.secondary.expect("weekly window");
        assert_eq!(weekly.used_percent, 37.0);
        assert_eq!(weekly.window_minutes, 10080);
        assert_eq!(weekly.resets_at, 1_750_600_000);
        assert_eq!(snap.label.as_deref(), Some("Max plan · $1829.59 extra"));
    }

    #[test]
    #[ignore = "hits the live GetUserStatus endpoint; prints percentages only"]
    fn live_probe() {
        for id in crate::accounts::ids("devin") {
            let Some((key, server)) = credentials_for(&id) else {
                println!("{id}: no credentials");
                continue;
            };
            let Some(body) = fetch_status(&key, &server) else {
                println!("{id}: probe failed");
                continue;
            };
            match parse_user_status(&body, None) {
                Some(snap) => {
                    let pct = |w: &Option<RateLimitWindow>| {
                        w.map(|w| format!("{}%/{}m", w.used_percent, w.window_minutes))
                            .unwrap_or_else(|| "-".into())
                    };
                    println!(
                        "{id} {} primary={} secondary={}",
                        snap.label.as_deref().unwrap_or("-"),
                        pct(&snap.primary),
                        pct(&snap.secondary)
                    );
                }
                None => println!("{id}: no plan status"),
            }
        }
    }

    #[test]
    fn visible_daily_quota_adds_primary_window() {
        let body = json!({
            "userStatus": {
                "planStatus": {
                    "planInfo": {"planName": "Teams", "hideDailyQuota": false},
                    "dailyQuotaRemainingPercent": 80,
                    "weeklyQuotaRemainingPercent": 50,
                    "dailyQuotaResetAtUnix": 1750000000,
                    "weeklyQuotaResetAtUnix": 1750600000,
                    "overageBalanceMicros": "0"
                }
            }
        });
        let snap = parse_user_status(&body, Some("Work")).expect("snapshot");
        let daily = snap.primary.expect("daily window");
        assert_eq!(daily.used_percent, 20.0);
        assert_eq!(daily.window_minutes, 1440);
        let weekly = snap.secondary.expect("weekly window");
        assert_eq!(weekly.used_percent, 50.0);
        assert_eq!(snap.label.as_deref(), Some("Work · Teams plan"));
    }
}
