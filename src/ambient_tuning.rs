use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Map, Value, json};

const CACHE_TTL: Duration = Duration::from_secs(5);
const THRESHOLD_MIN: f64 = 0.3;
const THRESHOLD_MAX: f64 = 0.9;
const NUDGE: f64 = 0.05;
const NOTE_BOOST_SECS: i64 = 30 * 60;
const NOTE_UNTIL_KEY: &str = "note_policy_until";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Alone,
    WithPeople,
    Meeting,
    Demo,
    Driving,
    Custom,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "alone" => Self::Alone,
            "with_people" => Self::WithPeople,
            "meeting" => Self::Meeting,
            "demo" => Self::Demo,
            "driving" => Self::Driving,
            "custom" => Self::Custom,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alone => "alone",
            Self::WithPeople => "with_people",
            Self::Meeting => "meeting",
            Self::Demo => "demo",
            Self::Driving => "driving",
            Self::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Chattiness {
    Quiet,
    Normal,
    Forward,
}

impl Chattiness {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "quiet" => Self::Quiet,
            "normal" => Self::Normal,
            "forward" => Self::Forward,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotePolicy {
    Off,
    Important,
    Everything,
}

impl NotePolicy {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Self::Off,
            "important" => Self::Important,
            "everything" => Self::Everything,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tuning {
    pub profile: Profile,
    pub address_threshold: f64,
    pub pinned_threshold: f64,
    pub live_reply_threshold: f64,
    pub only_my_voice: bool,
    pub chattiness: Chattiness,
    pub hold_ms: u64,
    /// 0 means use `config.ambient.conversation_timeout_secs`.
    pub conversation_ttl_s: u32,
    pub room_quiet_s: i64,
    pub note_policy: NotePolicy,
    pub barge_words: Vec<String>,
    pub expires_at: Option<i64>,
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn derived(address: f64) -> f64 {
    round2(address - 0.1).max(THRESHOLD_MIN)
}

impl Tuning {
    pub fn profile_defaults(profile: Profile) -> Self {
        let (address, only_my_voice, chattiness, hold_ms, ttl, room_quiet_s, note_policy) =
            match profile {
                Profile::Alone | Profile::Custom => (
                    0.6,
                    false,
                    Chattiness::Normal,
                    1600,
                    0,
                    120,
                    NotePolicy::Important,
                ),
                Profile::WithPeople => (
                    0.75,
                    true,
                    Chattiness::Quiet,
                    1600,
                    0,
                    240,
                    NotePolicy::Important,
                ),
                Profile::Meeting => (
                    0.85,
                    true,
                    Chattiness::Quiet,
                    2000,
                    60,
                    600,
                    NotePolicy::Everything,
                ),
                Profile::Demo => (
                    0.5,
                    false,
                    Chattiness::Forward,
                    1200,
                    0,
                    60,
                    NotePolicy::Off,
                ),
                Profile::Driving => (
                    0.55,
                    true,
                    Chattiness::Normal,
                    1600,
                    180,
                    120,
                    NotePolicy::Important,
                ),
            };
        Tuning {
            profile,
            address_threshold: address,
            pinned_threshold: derived(address),
            live_reply_threshold: derived(address),
            only_my_voice,
            chattiness,
            hold_ms,
            conversation_ttl_s: ttl,
            room_quiet_s,
            note_policy,
            barge_words: Vec::new(),
            expires_at: None,
        }
    }

    pub fn alone() -> Self {
        Self::profile_defaults(Profile::Alone)
    }

    pub fn hold(&self) -> Duration {
        Duration::from_millis(self.hold_ms)
    }

    fn apply(&mut self, knobs: &Map<String, Value>, now: i64) {
        if let Some(a) = knobs.get("address_threshold").and_then(Value::as_f64) {
            self.address_threshold = round2(a.clamp(THRESHOLD_MIN, THRESHOLD_MAX));
            self.pinned_threshold = derived(self.address_threshold);
            self.live_reply_threshold = derived(self.address_threshold);
        }
        if let Some(b) = knobs.get("only_my_voice").and_then(Value::as_bool) {
            self.only_my_voice = b;
        }
        if let Some(c) = knobs
            .get("chattiness")
            .and_then(Value::as_str)
            .and_then(Chattiness::parse)
        {
            self.chattiness = c;
        }
        if let Some(h) = knobs.get("hold_ms").and_then(Value::as_u64) {
            self.hold_ms = h.clamp(150, 10_000);
        }
        if let Some(t) = knobs.get("conversation_ttl_s").and_then(Value::as_u64) {
            self.conversation_ttl_s = t.min(3600) as u32;
        }
        if let Some(r) = knobs.get("room_quiet_s").and_then(Value::as_i64) {
            self.room_quiet_s = r.clamp(0, 3600);
        }
        if let Some(n) = knobs
            .get("note_policy")
            .and_then(Value::as_str)
            .and_then(NotePolicy::parse)
        {
            self.note_policy = n;
        }
        if knobs
            .get(NOTE_UNTIL_KEY)
            .and_then(Value::as_i64)
            .is_some_and(|t| t > now)
        {
            self.note_policy = NotePolicy::Everything;
        }
        if let Some(w) = knobs.get("barge_words").and_then(Value::as_array) {
            self.barge_words = w
                .iter()
                .filter_map(Value::as_str)
                .map(barge_key)
                .filter(|k| !k.is_empty())
                .collect();
        }
    }
}

pub fn barge_key(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

type Cache = Mutex<HashMap<String, (Tuning, Instant)>>;

fn cache() -> &'static Cache {
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Row {
    profile: String,
    knobs: Map<String, Value>,
    expires_at: Option<i64>,
}

fn read_row(device: &str) -> Option<Row> {
    let store = crate::ambient::store().ok()?;
    store
        .with_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT profile, knobs_json, expires_at FROM ambient_tuning WHERE device = ?1",
            )?;
            let mut rows = stmt.query_map([device], |r| {
                let knobs: String = r.get(1)?;
                Ok(Row {
                    profile: r.get(0)?,
                    knobs: serde_json::from_str::<Value>(&knobs)
                        .ok()
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default(),
                    expires_at: r.get(2)?,
                })
            })?;
            rows.next().transpose()
        })
        .ok()
        .flatten()
}

fn build(row: &Row, now: i64) -> Tuning {
    let profile = Profile::parse(&row.profile).unwrap_or(Profile::Alone);
    let mut t = Tuning::profile_defaults(profile);
    t.apply(&row.knobs, now);
    t.expires_at = row.expires_at;
    t
}

fn lookup(device: &str) -> Tuning {
    let now = now_s();
    let Some(row) = read_row(device) else {
        return Tuning::alone();
    };
    if row.expires_at.is_some_and(|e| e <= now) {
        delete_row(device);
        return Tuning::alone();
    }
    build(&row, now)
}

fn delete_row(device: &str) {
    if let Ok(store) = crate::ambient::store() {
        store.write(|c| {
            c.execute("DELETE FROM ambient_tuning WHERE device = ?1", [device])?;
            Ok(())
        });
    }
}

fn invalidate(device: &str) {
    cache().lock().unwrap().remove(device);
}

pub fn current(device: &str) -> Tuning {
    let now = now_s();
    if let Some((t, at)) = cache().lock().unwrap().get(device)
        && at.elapsed() < CACHE_TTL
        && t.expires_at.is_none_or(|e| e > now)
    {
        return t.clone();
    }
    let t = lookup(device);
    cache()
        .lock()
        .unwrap()
        .insert(device.to_string(), (t.clone(), Instant::now()));
    t
}

/// Who set the active tuning and why, for the settings screen.
pub fn origin(device: &str) -> Option<(String, String)> {
    let store = crate::ambient::store().ok()?;
    store
        .with_connection(|c| {
            c.query_row(
                "SELECT set_by, reason FROM ambient_tuning WHERE device = ?1",
                [device],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })
        })
        .ok()
        .flatten()
}

pub fn lease_device() -> String {
    crate::ambient::lease_holder()
        .map(|(d, _)| d)
        .unwrap_or_default()
}

pub fn current_for_lease() -> Tuning {
    current(&lease_device())
}

pub fn clear(device: &str) {
    delete_row(device);
    invalidate(device);
}

fn write_row(
    device: &str,
    profile: Profile,
    knobs: &Map<String, Value>,
    expires_at: Option<i64>,
    set_by: &str,
    reason: &str,
) -> Result<(), String> {
    let json = Value::Object(knobs.clone()).to_string();
    crate::ambient::store()?
        .with_connection(|c| {
            c.execute(
                "INSERT INTO ambient_tuning (device, profile, knobs_json, expires_at, set_by, reason, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(device) DO UPDATE SET profile = ?2, knobs_json = ?3, expires_at = ?4,
                     set_by = ?5, reason = ?6, updated_at = ?7",
                rusqlite::params![device, profile.as_str(), json, expires_at, set_by, reason, now_s()],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    invalidate(device);
    Ok(())
}

fn clean_knobs(knobs: &Value) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    let Some(obj) = knobs.as_object() else {
        return Ok(out);
    };
    for (k, v) in obj {
        if v.is_null() {
            continue;
        }
        let ok = match k.as_str() {
            "address_threshold" => v.is_number(),
            "only_my_voice" => v.is_boolean(),
            "chattiness" => v.as_str().and_then(Chattiness::parse).is_some(),
            "hold_ms" | "conversation_ttl_s" | "room_quiet_s" => v.is_number(),
            "note_policy" => v.as_str().and_then(NotePolicy::parse).is_some(),
            "barge_words" => v.is_array(),
            _ => return Err(format!("unknown knob `{k}`")),
        };
        if !ok {
            return Err(format!("bad value for `{k}`"));
        }
        out.insert(k.clone(), v.clone());
    }
    Ok(out)
}

pub async fn set(
    device: &str,
    profile: Option<Profile>,
    knobs: Value,
    minutes: Option<u64>,
    until: Option<i64>,
    set_by: &str,
    reason: &str,
) -> Result<Tuning, String> {
    let fresh = clean_knobs(&knobs)?;
    let existing = read_row(device);
    let (profile, merged) = match (profile, &existing) {
        (Some(p), _) => (p, fresh),
        (None, Some(row)) => {
            let mut merged = row.knobs.clone();
            merged.extend(fresh);
            (
                Profile::parse(&row.profile).unwrap_or(Profile::Alone),
                merged,
            )
        }
        (None, None) => (
            if fresh.is_empty() {
                Profile::Alone
            } else {
                Profile::Custom
            },
            fresh,
        ),
    };
    let expires_at = match (minutes, until) {
        (Some(m), _) => Some(now_s() + (m.min(60 * 24 * 30) as i64) * 60),
        (None, Some(u)) => Some(u),
        (None, None) => None,
    };
    if expires_at.is_some_and(|e| e <= now_s()) {
        return Err("the expiry is in the past".into());
    }
    write_row(device, profile, &merged, expires_at, set_by, reason)?;
    Ok(current(device))
}

pub fn feedback(
    device: &str,
    event_ids: &[String],
    text: &str,
    was: &str,
) -> Result<Tuning, String> {
    if !matches!(was, "not_for_me" | "for_me" | "should_note") {
        return Err(format!(
            "was must be not_for_me, for_me or should_note, got `{was}`"
        ));
    }
    let now = now_s();
    crate::ambient::store()?
        .with_connection(|c| {
            c.execute(
                "INSERT INTO ambient_feedback (device, event_ids, text, was, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![device, event_ids.join(","), text, was, now],
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    let (profile, mut knobs, expires_at) = match read_row(device) {
        Some(r) if r.expires_at.is_none_or(|e| e > now) => (
            Profile::parse(&r.profile).unwrap_or(Profile::Alone),
            r.knobs,
            r.expires_at,
        ),
        _ => (Profile::Alone, Map::new(), None),
    };
    let now_t = build(
        &Row {
            profile: profile.as_str().into(),
            knobs: knobs.clone(),
            expires_at,
        },
        now,
    );
    match was {
        "not_for_me" | "for_me" => {
            let delta = if was == "not_for_me" { NUDGE } else { -NUDGE };
            let next =
                round2((now_t.address_threshold + delta).clamp(THRESHOLD_MIN, THRESHOLD_MAX));
            knobs.insert("address_threshold".into(), json!(next));
        }
        _ => {
            if now_t.note_policy == NotePolicy::Off {
                knobs.insert(NOTE_UNTIL_KEY.into(), json!(now + NOTE_BOOST_SECS));
            }
        }
    }
    write_row(
        device,
        profile,
        &knobs,
        expires_at,
        "feedback",
        &format!("feedback: {was}"),
    )?;
    Ok(current(device))
}

pub fn recent_feedback(device: &str, limit: usize) -> Vec<Value> {
    let Ok(store) = crate::ambient::store() else {
        return Vec::new();
    };
    store
        .with_connection(|c| {
            let mut stmt = c.prepare("SELECT text, was FROM ambient_feedback WHERE device = ?1 ORDER BY id DESC LIMIT ?2")?;
            let rows = stmt.query_map(rusqlite::params![device, limit as i64], |r| {
                Ok(json!({"text": r.get::<_, String>(0)?, "was": r.get::<_, String>(1)?}))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default()
}

pub fn classifier_json(device: &str) -> Value {
    let t = current(device);
    json!({
        "tuning": {
            "profile": t.profile.as_str(),
            "address_threshold": t.address_threshold,
            "only_my_voice": t.only_my_voice,
        },
        "feedback": recent_feedback(device, 8),
    })
}

pub fn lookup_event_text(ids: &[String]) -> String {
    let Ok(store) = crate::ambient::store() else {
        return String::new();
    };
    let mut texts = Vec::new();
    for id in ids {
        let t = store
            .with_connection(|c| {
                c.query_row("SELECT text FROM ambient_events WHERE id = ?1", [id], |r| {
                    r.get::<_, String>(0)
                })
            })
            .unwrap_or_default();
        if !t.trim().is_empty() {
            texts.push(t.trim().to_string());
        }
    }
    texts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str) -> String {
        format!("tune-test-{name}-{}", std::process::id())
    }

    #[test]
    fn alone_matches_the_old_constants() {
        let t = Tuning::alone();
        assert_eq!(
            (
                t.address_threshold,
                t.pinned_threshold,
                t.live_reply_threshold
            ),
            (0.6, 0.5, 0.5)
        );
        assert_eq!(
            (t.hold_ms, t.room_quiet_s, t.conversation_ttl_s),
            (1600, 120, 0)
        );
        assert!(!t.only_my_voice);
        assert_eq!(current(&dev("none")), t);
    }

    #[test]
    fn origin_reports_who_set_a_timed_profile() {
        let d = dev("origin");
        let t = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(set(
                &d,
                Some(Profile::Meeting),
                Value::Null,
                Some(30),
                None,
                "user",
                "set in the app",
            ))
            .unwrap();
        assert_eq!(t.profile, Profile::Meeting);
        assert!(t.expires_at.is_some());
        assert_eq!(origin(&d), Some(("user".into(), "set in the app".into())));
        clear(&d);
        assert_eq!(origin(&d), None);
        assert_eq!(current(&d).profile, Profile::Alone);
    }

    #[test]
    fn feedback_nudges_and_bounds() {
        let d = dev("fb");
        let ids = vec!["e1".to_string()];
        for _ in 0..3 {
            feedback(&d, &ids, "hello", "not_for_me").unwrap();
        }
        assert_eq!(current(&d).address_threshold, 0.75);
        for _ in 0..10 {
            feedback(&d, &ids, "hello", "not_for_me").unwrap();
        }
        assert_eq!(current(&d).address_threshold, 0.9);
        let t = feedback(&d, &ids, "hello", "for_me").unwrap();
        assert_eq!(t.address_threshold, 0.85);
        assert_eq!(recent_feedback(&d, 8).len(), 8);
        clear(&d);
    }

    #[test]
    fn should_note_lifts_off_policy_temporarily() {
        let d = dev("note");
        let t = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(set(
                &d,
                Some(Profile::Demo),
                json!({}),
                None,
                None,
                "user",
                "demo",
            ))
            .unwrap();
        assert_eq!(t.note_policy, NotePolicy::Off);
        let t = feedback(&d, &["e".into()], "x", "should_note").unwrap();
        assert_eq!(t.note_policy, NotePolicy::Everything);
        assert_eq!(t.profile, Profile::Demo);
        clear(&d);
    }

    #[test]
    fn expired_row_falls_back_to_alone_and_is_deleted() {
        let d = dev("exp");
        crate::ambient::store()
            .unwrap()
            .with_connection(|c| {
                c.execute(
                    "INSERT INTO ambient_tuning (device, profile, knobs_json, expires_at, set_by, reason, updated_at) VALUES (?1, 'meeting', '{}', ?2, 'user', '', 0)",
                    rusqlite::params![d, now_s() - 10],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(current(&d).profile, Profile::Alone);
        assert!(read_row(&d).is_none());
    }

    #[test]
    fn set_profile_then_knob_override() {
        let d = dev("set");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let t = rt
            .block_on(set(
                &d,
                Some(Profile::Meeting),
                json!({"hold_ms": 900}),
                Some(30),
                None,
                "model",
                "meeting",
            ))
            .unwrap();
        assert_eq!(
            (t.profile, t.hold_ms, t.address_threshold),
            (Profile::Meeting, 900, 0.85)
        );
        assert!(t.expires_at.is_some());
        let t = rt
            .block_on(set(
                &d,
                None,
                json!({"barge_words": ["Hush now"]}),
                None,
                None,
                "model",
                "x",
            ))
            .unwrap();
        assert_eq!(t.hold_ms, 900);
        assert_eq!(t.barge_words, vec!["hush now".to_string()]);
        assert!(
            rt.block_on(set(&d, None, json!({"nope": 1}), None, None, "model", "x"))
                .is_err()
        );
        clear(&d);
        assert_eq!(current(&d).profile, Profile::Alone);
    }
}
