//! Claude Code OAuth usage probe (`GET /api/oauth/usage`), cached under
//! `~/.snippet/claude-code/usage.json` and refreshed in the background.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::{RateLimitSnapshot, RateLimitWindow};

const API_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const FALLBACK_CLI_VERSION: &str = "2.1.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageCache {
    fetched_at: i64,
    accounts: HashMap<String, Vec<RateLimitSnapshot>>,
}

static REFRESHING: AtomicBool = AtomicBool::new(false);
static LAST_ATTEMPT: AtomicI64 = AtomicI64::new(0);

fn cache_path() -> PathBuf {
    crate::config::snippet_home().join("claude-code/usage.json")
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

/// Every account's probed snapshots, in pool order.
pub fn cached() -> Vec<RateLimitSnapshot> {
    let Some(cache) = load_cache() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in crate::accounts::ids("claude-code") {
        if let Some(snaps) = cache.accounts.get(&id) {
            seen.insert(id.clone());
            out.extend(snaps.iter().cloned());
        }
    }
    let mut rest: Vec<&String> = cache.accounts.keys().filter(|k| !seen.contains(*k)).collect();
    rest.sort();
    for id in rest {
        out.extend(cache.accounts[id].iter().cloned());
    }
    out
}

pub fn refresh_if_stale(max_age_secs: i64) {
    let now = chrono::Utc::now().timestamp();
    if load_cache().is_some_and(|c| now - c.fetched_at < max_age_secs) {
        return;
    }
    // A failing probe writes no cache; without this every /usage call would start another.
    if now - LAST_ATTEMPT.load(Ordering::SeqCst) < max_age_secs {
        return;
    }
    if REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    LAST_ATTEMPT.store(now, Ordering::SeqCst);
    std::thread::spawn(|| {
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                REFRESHING.store(false, Ordering::SeqCst);
            }
        }
        let _done = Done;
        let ids = crate::accounts::ids("claude-code");
        let multi = ids.len() > 1;
        let mut accounts = HashMap::new();
        for id in &ids {
            let Some(token) = account_token(id) else {
                continue;
            };
            let Some(body) = fetch_usage(&token) else {
                continue;
            };
            let label = multi.then(|| crate::accounts::label("claude-code", id));
            let snaps = parse_usage(&body, label.as_deref());
            if snaps.is_empty() {
                continue;
            }
            if let Some(shared) = snaps.first() {
                crate::accounts::record_usage("claude-code", id, shared);
            }
            accounts.insert(id.clone(), snaps);
        }
        if !accounts.is_empty() {
            store_cache(&UsageCache { fetched_at: chrono::Utc::now().timestamp(), accounts });
        }
    });
}

fn account_token(id: &str) -> Option<String> {
    if id == crate::accounts::MAIN {
        return main_token();
    }
    let raw = crate::accounts::read_credential("claude-code", id)?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("token").and_then(Value::as_str).map(str::to_string)
}

fn main_token() -> Option<String> {
    let path = crate::devin::real_home()?.join(".claude").join(".credentials.json");
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    if let Some(expires_at) = oauth.get("expiresAt").and_then(Value::as_i64)
        && expires_at > 0
        && expires_at < chrono::Utc::now().timestamp_millis()
    {
        return None;
    }
    oauth.get("accessToken").and_then(Value::as_str).map(str::to_string)
}

fn cli_version() -> String {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(probe_cli_version).clone()
}

fn probe_cli_version() -> String {
    let probe = |bin: &Path| {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|text| text.split_whitespace().next().map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    if let Some(bin) = std::env::var_os("SNIPPET_CLAUDE_BIN").map(PathBuf::from).filter(|p| p.is_file())
        && let Some(version) = probe(&bin)
    {
        return version;
    }
    probe(Path::new("claude")).unwrap_or_else(|| FALLBACK_CLI_VERSION.to_string())
}

fn fetch_usage(token: &str) -> Option<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?;
    let response = client
        .get(API_URL)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("User-Agent", format!("claude-code/{}", cli_version()))
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().ok()
}

fn pct(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn resets_at(value: &Value) -> i64 {
    let Some(raw) = value.get("resets_at") else {
        return 0;
    };
    if let Some(n) = raw.as_i64() {
        return n;
    }
    let Some(text) = raw.as_str() else {
        return 0;
    };
    if let Ok(n) = text.parse::<i64>() {
        return n;
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn window(obj: &Value, minutes: i64, percent_keys: &[&str]) -> Option<RateLimitWindow> {
    let used = percent_keys.iter().find_map(|k| pct(obj.get(*k)))?;
    Some(RateLimitWindow {
        used_percent: used.clamp(0.0, 100.0),
        window_minutes: minutes,
        resets_at: resets_at(obj),
    })
}

fn scoped_label(account_label: Option<&str>, name: &str) -> String {
    let scoped = format!("{name} weekly");
    match account_label {
        Some(label) if !label.trim().is_empty() => format!("{label} · {scoped}"),
        _ => scoped,
    }
}

/// Parse the `/api/oauth/usage` body into one shared snapshot (5-hour + 7-day)
/// plus one snapshot per model-scoped weekly window. `account_label` is only
/// `Some` when more than one claude-code account exists.
pub fn parse_usage(body: &Value, account_label: Option<&str>) -> Vec<RateLimitSnapshot> {
    let mut out = Vec::new();
    let five_hour = body.get("five_hour").and_then(|o| window(o, 300, &["utilization"]));
    let seven_day = body.get("seven_day").and_then(|o| window(o, 10080, &["utilization"]));
    if five_hour.is_some() || seven_day.is_some() {
        out.push(RateLimitSnapshot {
            primary: five_hour,
            secondary: seven_day,
            label: account_label.map(str::to_string),
        });
    }
    if let Some(limits) = body.get("limits").and_then(Value::as_array) {
        for entry in limits {
            if entry.get("kind").and_then(Value::as_str) != Some("weekly_scoped") {
                continue;
            }
            let name = entry
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str)
                .unwrap_or("Model");
            let Some(weekly) = window(entry, 10080, &["percent", "utilization"]) else {
                continue;
            };
            out.push(RateLimitSnapshot {
                primary: None,
                secondary: Some(weekly),
                label: Some(scoped_label(account_label, name)),
            });
        }
    }
    for (key, name) in [("seven_day_opus", "Opus"), ("seven_day_sonnet", "Sonnet")] {
        let Some(obj) = body.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let Some(weekly) = window(obj, 10080, &["utilization", "percent"]) else {
            continue;
        };
        out.push(RateLimitSnapshot {
            primary: None,
            secondary: Some(weekly),
            label: Some(scoped_label(account_label, name)),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_body_parses_shared_and_scoped_weekly_windows() {
        let body = json!({
            "five_hour": {"utilization": 6.0, "resets_at": "2026-04-08T18:59:59Z"},
            "seven_day": {"utilization": 35.0, "resets_at": "2026-04-15T18:59:59Z"},
            "seven_day_opus": null,
            "seven_day_sonnet": null,
            "limits": [
                {"kind": "session", "group": "session", "percent": 6, "scope": null, "is_active": false, "resets_at": "2026-04-08T18:59:59Z"},
                {"kind": "weekly_all", "group": "weekly", "percent": 35, "scope": null, "resets_at": "2026-04-15T18:59:59Z"},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 61, "is_active": true, "resets_at": "2026-04-15T18:59:59Z",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
            ],
            "extra_usage": {}
        });
        let snaps = parse_usage(&body, None);
        assert_eq!(snaps.len(), 2);

        let shared = &snaps[0];
        assert_eq!(shared.label, None);
        assert_eq!(shared.primary.unwrap().used_percent, 6.0);
        assert_eq!(shared.primary.unwrap().window_minutes, 300);
        assert_eq!(shared.secondary.unwrap().used_percent, 35.0);
        assert_eq!(shared.secondary.unwrap().window_minutes, 10080);
        assert!(shared.secondary.unwrap().resets_at > 0);

        let scoped = &snaps[1];
        assert_eq!(scoped.label.as_deref(), Some("Fable weekly"));
        assert!(scoped.primary.is_none());
        assert_eq!(scoped.secondary.unwrap().used_percent, 61.0);
        assert_eq!(scoped.secondary.unwrap().window_minutes, 10080);
    }

    #[test]
    #[ignore = "hits the live OAuth usage endpoint; prints percentages only"]
    fn live_probe() {
        for id in crate::accounts::ids("claude-code") {
            let Some(token) = account_token(&id) else {
                println!("{id}: no usable token");
                continue;
            };
            let Some(body) = fetch_usage(&token) else {
                println!("{id}: probe failed");
                continue;
            };
            for snap in parse_usage(&body, None) {
                let pct = |w: &Option<RateLimitWindow>| w.map(|w| format!("{}%/{}m", w.used_percent, w.window_minutes)).unwrap_or_else(|| "-".into());
                println!("{id} {} primary={} secondary={}", snap.label.as_deref().unwrap_or("shared"), pct(&snap.primary), pct(&snap.secondary));
            }
        }
    }

    #[test]
    fn account_label_prefixes_shared_and_scoped_snapshots() {
        let body = json!({
            "five_hour": {"utilization": 1.0, "resets_at": "2026-04-08T18:59:59Z"},
            "seven_day": {"utilization": 2.0, "resets_at": "2026-04-15T18:59:59Z"},
            "limits": [
                {"kind": "weekly_scoped", "percent": 10, "scope": {"model": {"display_name": "Fable"}}}
            ]
        });
        let snaps = parse_usage(&body, Some("Work"));
        assert_eq!(snaps[0].label.as_deref(), Some("Work"));
        assert_eq!(snaps[1].label.as_deref(), Some("Work · Fable weekly"));
    }
}
