use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Shared;
use crate::ambient_tuning::Profile;
use crate::llm::{HarnessMessage, NativeToolDefinition};
use crate::mission_control::SESSION_ID;
use crate::routing::Route;
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub const DIGEST_SPAN_MS: i64 = 180_000;
pub const DIGEST_IDLE_MS: i64 = 120_000;
pub const DELIVER_IDLE_MS: i64 = 600_000;
pub const CLARIFY_VOICE_SECS: i64 = 120;
pub const CLARIFY_COOLDOWN_SECS: i64 = 30 * 60;
pub const CLARIFY_EXPIRE_SECS: i64 = 4 * 3600;
const MAX_DIGEST_ATTEMPTS: i64 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RoomRow {
    pub device: String,
    pub updated_at: i64,
    pub present: String,
    pub topic: String,
    pub activity: String,
    pub digest: String,
    pub delivered_at: i64,
    pub topic_updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomLine {
    pub id: i64,
    pub device: String,
    pub at_ms: i64,
    pub speaker: String,
    pub text: String,
    pub score: f64,
    pub event_ids: String,
    pub digested: i64,
    pub attempts: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewRoomLine {
    pub at_ms: i64,
    pub speaker: String,
    pub text: String,
    pub score: f64,
    pub event_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomItem {
    pub id: i64,
    pub device: String,
    pub created_at: i64,
    pub kind: String,
    pub text: String,
    pub rank: i64,
    pub remember: bool,
    pub source: String,
    pub state: String,
    pub asked_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateInput {
    pub passive: bool,
    pub alone: bool,
    pub room_busy: bool,
    pub call_active: bool,
    pub mc_busy: bool,
    pub quiet_hours: bool,
    pub user_on_call: bool,
    pub user_voice_recent: bool,
    pub asked_recently: bool,
}

pub fn clarify_allowed(g: GateInput) -> Result<(), &'static str> {
    if !g.passive {
        return Err("ambient mode is active");
    }
    if !g.alone {
        return Err("profile is not alone");
    }
    if g.room_busy {
        return Err("room is busy");
    }
    if g.call_active {
        return Err("call pending or tracked");
    }
    if g.mc_busy {
        return Err("mission control is busy");
    }
    if g.quiet_hours {
        return Err("quiet hours");
    }
    if g.user_on_call {
        return Err("user on call");
    }
    if !g.user_voice_recent {
        return Err("user voice not heard in last 120s");
    }
    if g.asked_recently {
        return Err("clarification asked in last 30m");
    }
    Ok(())
}

pub fn digest_due(first_ms: Option<i64>, newest_ms: Option<i64>, now_ms: i64) -> bool {
    let (Some(first), Some(newest)) = (first_ms, newest_ms) else {
        return false;
    };
    now_ms.saturating_sub(first) >= DIGEST_SPAN_MS
        || now_ms.saturating_sub(newest) >= DIGEST_IDLE_MS
}

pub fn insert_line(c: &Connection, device: &str, line: &NewRoomLine) -> rusqlite::Result<i64> {
    let ids = line.event_ids.join(",");
    c.execute(
        "INSERT INTO ambient_room_lines (device, at_ms, speaker, text, score, event_ids, digested, attempts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)",
        params![device, line.at_ms, line.speaker, line.text, line.score, ids],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn undigested_lines(c: &Connection, device: &str) -> rusqlite::Result<Vec<RoomLine>> {
    let mut stmt = c.prepare(
        "SELECT id, device, at_ms, speaker, text, score, event_ids, digested, attempts
         FROM ambient_room_lines
         WHERE device = ?1 AND digested = 0
         ORDER BY at_ms ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([device], |r| {
            Ok(RoomLine {
                id: r.get(0)?,
                device: r.get(1)?,
                at_ms: r.get(2)?,
                speaker: r.get(3)?,
                text: r.get(4)?,
                score: r.get(5)?,
                event_ids: r.get(6)?,
                digested: r.get(7)?,
                attempts: r.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn mark_digested(c: &Connection, ids: &[i64]) -> rusqlite::Result<()> {
    for id in ids {
        c.execute(
            "UPDATE ambient_room_lines SET digested = 1 WHERE id = ?1",
            [id],
        )?;
    }
    Ok(())
}

pub fn bump_attempts(c: &Connection, ids: &[i64]) -> rusqlite::Result<i64> {
    let mut max_attempts = 0;
    for id in ids {
        c.execute(
            "UPDATE ambient_room_lines SET attempts = attempts + 1 WHERE id = ?1",
            [id],
        )?;
        let a: i64 = c.query_row(
            "SELECT attempts FROM ambient_room_lines WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        max_attempts = max_attempts.max(a);
    }
    Ok(max_attempts)
}

pub fn get_room(c: &Connection, device: &str) -> rusqlite::Result<Option<RoomRow>> {
    c.query_row(
        "SELECT device, updated_at, present, topic, activity, digest, delivered_at, topic_updated_at
         FROM ambient_room WHERE device = ?1",
        [device],
        |r| {
            Ok(RoomRow {
                device: r.get(0)?,
                updated_at: r.get(1)?,
                present: r.get(2)?,
                topic: r.get(3)?,
                activity: r.get(4)?,
                digest: r.get(5)?,
                delivered_at: r.get(6)?,
                topic_updated_at: r.get(7)?,
            })
        },
    )
    .optional()
}

pub fn upsert_room(
    c: &Connection,
    device: &str,
    now_s: i64,
    present: &str,
    topic: &str,
    activity: &str,
    digest: &str,
) -> rusqlite::Result<()> {
    let existing = get_room(c, device)?;
    let delivered_at = existing.as_ref().map(|r| r.delivered_at).unwrap_or(0);
    let topic_changed = existing
        .as_ref()
        .map(|r| r.topic.as_str() != topic)
        .unwrap_or(!topic.is_empty());
    let topic_updated_at = if topic_changed {
        now_s
    } else {
        existing.as_ref().map(|r| r.topic_updated_at).unwrap_or(0)
    };
    c.execute(
        "INSERT INTO ambient_room (device, updated_at, present, topic, activity, digest, delivered_at, topic_updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(device) DO UPDATE SET
             updated_at = excluded.updated_at,
             present = excluded.present,
             topic = excluded.topic,
             activity = excluded.activity,
             digest = excluded.digest,
             topic_updated_at = excluded.topic_updated_at",
        params![device, now_s, present, topic, activity, digest, delivered_at, topic_updated_at],
    )?;
    Ok(())
}

pub struct NewRoomItem<'a> {
    pub kind: &'a str,
    pub text: &'a str,
    pub rank: i64,
    pub remember: bool,
    pub source: &'a str,
}

pub fn insert_item(
    c: &Connection,
    device: &str,
    now_s: i64,
    item: &NewRoomItem<'_>,
) -> rusqlite::Result<i64> {
    c.execute(
        "INSERT INTO ambient_room_items (device, created_at, kind, text, rank, remember, source, state, asked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0)",
        params![device, now_s, item.kind, item.text, item.rank, i64::from(item.remember), item.source],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn set_item_state(
    c: &Connection,
    id: i64,
    state: &str,
    asked_at: Option<i64>,
) -> rusqlite::Result<usize> {
    match asked_at {
        Some(at) => c.execute(
            "UPDATE ambient_room_items SET state = ?2, asked_at = ?3 WHERE id = ?1",
            params![id, state, at],
        ),
        None => c.execute(
            "UPDATE ambient_room_items SET state = ?2 WHERE id = ?1",
            params![id, state],
        ),
    }
}

pub fn items_for_device(
    c: &Connection,
    device: &str,
    kind: Option<&str>,
    state: Option<&str>,
) -> rusqlite::Result<Vec<RoomItem>> {
    let mut stmt = c.prepare(
        "SELECT id, device, created_at, kind, text, rank, remember, source, state, asked_at
         FROM ambient_room_items
         WHERE device = ?1
           AND (?2 IS NULL OR kind = ?2)
           AND (?3 IS NULL OR state = ?3)
         ORDER BY CASE WHEN rank > 0 THEN rank ELSE 999999 END ASC, id ASC",
    )?;
    let rows = stmt
        .query_map(params![device, kind, state], |r| {
            Ok(RoomItem {
                id: r.get(0)?,
                device: r.get(1)?,
                created_at: r.get(2)?,
                kind: r.get(3)?,
                text: r.get(4)?,
                rank: r.get(5)?,
                remember: r.get::<_, i64>(6)? != 0,
                source: r.get(7)?,
                state: r.get(8)?,
                asked_at: r.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn scrub_event_id(c: &Connection, event_id: &str) -> rusqlite::Result<usize> {
    let trimmed = event_id.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let pattern = format!("%{trimmed}%");
    let mut stmt =
        c.prepare("SELECT id, event_ids FROM ambient_room_lines WHERE event_ids LIKE ?1")?;
    let candidates: Vec<(i64, String)> = stmt
        .query_map([pattern], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut deleted = 0;
    for (row_id, csv) in candidates {
        if csv.split(',').any(|part| part.trim() == trimmed) {
            deleted += c.execute("DELETE FROM ambient_room_lines WHERE id = ?1", [row_id])?;
        }
    }
    Ok(deleted)
}

pub fn clear_device(c: &Connection, device: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM ambient_room WHERE device = ?1", [device])?;
    c.execute("DELETE FROM ambient_room_lines WHERE device = ?1", [device])?;
    c.execute(
        "DELETE FROM ambient_room_items WHERE device = ?1 AND state IN ('pending', 'queued')",
        [device],
    )?;
    Ok(())
}

pub fn known_devices(c: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = c.prepare(
        "SELECT device FROM ambient_room
         UNION SELECT device FROM ambient_room_lines
         UNION SELECT device FROM ambient_room_items
         ORDER BY 1",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[async_trait]
pub trait Summariser: Send + Sync {
    async fn summarise(&self, system: &str, prompt: &str) -> Result<Option<String>, String>;
}

pub struct LiveSummariser<'a>(pub &'a Shared);

#[async_trait]
impl Summariser for LiveSummariser<'_> {
    async fn summarise(&self, system: &str, prompt: &str) -> Result<Option<String>, String> {
        let attempts = {
            let config = self.0.config.lock().unwrap();
            let workspace = std::env::current_dir().unwrap_or_default();
            let mut routed: Vec<(String, crate::config::InferenceProfileConfig)> =
                crate::routing::candidates(&config, Route::Memory)
                    .into_iter()
                    .filter_map(|name| {
                        config
                            .setups
                            .as_ref()?
                            .get(&name)
                            .cloned()
                            .map(|c| (name, c))
                    })
                    .collect();
            if routed.is_empty() {
                routed.push((String::new(), config.for_workspace(workspace).model));
            }
            routed
        };

        let messages = vec![
            HarnessMessage::System {
                content: system.to_string(),
            },
            HarnessMessage::User {
                content: prompt.to_string(),
            },
        ];

        let mut tried_non_cli = false;
        let mut last_err = String::new();
        for (name, profile) in attempts {
            let mut model = profile.build_model();
            if model.cli_agent_profile().is_some() {
                continue;
            }
            tried_non_cli = true;
            match model.generate(&messages, &[], false, None).await {
                Ok(out) => {
                    return Ok(Some(out.content_text.unwrap_or_default()));
                }
                Err(e) => {
                    let msg = e.to_string();
                    if !name.is_empty() && crate::routing::is_limit_error(&msg) {
                        crate::routing::mark_limited(&name, crate::routing::reset_from(None));
                    }
                    last_err = msg;
                }
            }
        }
        if !tried_non_cli {
            return Ok(None);
        }
        Err(last_err)
    }
}

const DIGEST_SYSTEM: &str = r#"You maintain a running back-of-the-head model of the room around the user from overheard microphone lines while they were not addressing the assistant.

Reply with strict JSON only (no markdown fences, no commentary):
{"present": ["You", "Speaker 2"], "topic": "...", "activity": "...", "digest": "...",
 "notes": [{"text": "...", "rank": 1, "remember": false, "lines": [1, 2]}],
 "clarify": [{"text": "one short question for the user", "lines": [1]}]}

Rules:
- `present`: list of speaker labels currently in the room.
- `topic`: short phrase for the current subject.
- `activity`: one short phrase such as "cooking and chatting", "quiet", or "meeting".
- `digest`: running two to four sentence summary combining prior room state and new lines.
- `notes`: facts, plans, commitments, names, dates or requests the user would want kept, ranked 1 (most important) upward, at most 5. Set `remember: true` ONLY for durable facts about the user or their commitments. `lines` lists the numeric line ids supporting the note.
- `clarify`: at most 3 short questions for the user, ONLY where a note is ambiguous and the answer would change what gets kept (e.g. "Did you mean Friday the 14th for the dentist?"). NEVER ask about other people's private talk."#;

#[derive(Debug, Clone, Deserialize)]
pub struct DigestNote {
    pub text: String,
    #[serde(default = "default_rank")]
    pub rank: i64,
    #[serde(default)]
    pub remember: bool,
    #[serde(default)]
    pub lines: Vec<i64>,
}

fn default_rank() -> i64 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigestClarify {
    pub text: String,
    #[serde(default)]
    pub lines: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigestReply {
    #[serde(default)]
    pub present: Vec<String>,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub activity: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub notes: Vec<DigestNote>,
    #[serde(default)]
    pub clarify: Vec<DigestClarify>,
}

pub fn parse_digest_json(raw: &str) -> Result<DigestReply, String> {
    let trimmed = raw.trim();
    let stripped = if let Some(rest) = trimmed.strip_prefix("```") {
        let after_lang = match rest.find('\n') {
            Some(idx) => &rest[idx + 1..],
            None => rest,
        };
        after_lang
            .trim_end()
            .strip_suffix("```")
            .unwrap_or(after_lang)
            .trim()
    } else {
        trimmed
    };
    if let Ok(parsed) = serde_json::from_str::<DigestReply>(stripped) {
        return Ok(parsed);
    }
    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}'))
        && start < end
    {
        return serde_json::from_str::<DigestReply>(&stripped[start..=end])
            .map_err(|e| e.to_string());
    }
    Err("invalid digest JSON".into())
}

fn build_digest_prompt(room: Option<&RoomRow>, lines: &[RoomLine]) -> String {
    let mut out = String::new();
    if let Some(r) = room {
        out.push_str(&format!(
            "Current room state:\npresent: {}\ntopic: {}\nactivity: {}\ndigest: {}\n\n",
            r.present, r.topic, r.activity, r.digest
        ));
    } else {
        out.push_str("Current room state: (empty)\n\n");
    }
    out.push_str("Undigested lines (format: id [speaker] text):\n");
    for l in lines {
        let who = if l.speaker.trim().is_empty() {
            "Unknown"
        } else {
            l.speaker.trim()
        };
        out.push_str(&format!("{} [{}] {}\n", l.id, who, l.text.trim()));
    }
    out
}

fn map_line_ids_to_events(line_ids: &[i64], lines: &[RoomLine]) -> String {
    let by_id: HashMap<i64, &str> = lines.iter().map(|l| (l.id, l.event_ids.as_str())).collect();
    let mut seen = HashSet::new();
    let mut events = Vec::new();
    for lid in line_ids {
        if let Some(csv) = by_id.get(lid) {
            for ev in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if seen.insert(ev.to_string()) {
                    events.push(ev.to_string());
                }
            }
        }
    }
    events.join(",")
}

fn running_devices() -> &'static Mutex<HashSet<String>> {
    static RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

struct DeviceRunGuard(String);
impl Drop for DeviceRunGuard {
    fn drop(&mut self) {
        running_devices().lock().unwrap().remove(&self.0);
    }
}

pub async fn digest_with(
    store: &Store,
    device: &str,
    summariser: &dyn Summariser,
    now_s: i64,
) -> Result<bool, String> {
    {
        let mut set = running_devices().lock().unwrap();
        if !set.insert(device.to_string()) {
            return Ok(false);
        }
    }
    let _guard = DeviceRunGuard(device.to_string());

    let (room, lines) = store
        .with_connection(|c| Ok((get_room(c, device)?, undigested_lines(c, device)?)))
        .map_err(|e| e.to_string())?;
    if lines.is_empty() {
        return Ok(false);
    }

    let prompt = build_digest_prompt(room.as_ref(), &lines);
    let ids: Vec<i64> = lines.iter().map(|l| l.id).collect();
    let raw = match summariser.summarise(DIGEST_SYSTEM, &prompt).await {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(false),
        Err(e) => {
            let _ = store.with_connection(|c| {
                let attempts = bump_attempts(c, &ids)?;
                if attempts >= MAX_DIGEST_ATTEMPTS {
                    mark_digested(c, &ids)?;
                }
                Ok(())
            });
            return Err(e);
        }
    };

    let parsed = match parse_digest_json(&raw) {
        Ok(p) => p,
        Err(e) => {
            store
                .with_connection(|c| {
                    let attempts = bump_attempts(c, &ids)?;
                    if attempts >= MAX_DIGEST_ATTEMPTS {
                        mark_digested(c, &ids)?;
                    }
                    Ok(())
                })
                .map_err(|err| err.to_string())?;
            return Err(e);
        }
    };

    store
        .with_connection(|c| {
            let present = parsed
                .present
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(",");
            upsert_room(
                c,
                device,
                now_s,
                &present,
                parsed.topic.trim(),
                parsed.activity.trim(),
                parsed.digest.trim(),
            )?;
            for (idx, note) in parsed.notes.iter().take(5).enumerate() {
                let text = note.text.trim();
                if text.is_empty() {
                    continue;
                }
                let rank = if note.rank > 0 {
                    note.rank
                } else {
                    (idx + 1) as i64
                };
                let source = map_line_ids_to_events(&note.lines, &lines);
                insert_item(
                    c,
                    device,
                    now_s,
                    &NewRoomItem {
                        kind: "note",
                        text,
                        rank,
                        remember: note.remember,
                        source: &source,
                    },
                )?;
            }
            for clarify in parsed.clarify.iter().take(3) {
                let text = clarify.text.trim();
                if text.is_empty() {
                    continue;
                }
                let source = map_line_ids_to_events(&clarify.lines, &lines);
                insert_item(
                    c,
                    device,
                    now_s,
                    &NewRoomItem {
                        kind: "clarify",
                        text,
                        rank: 0,
                        remember: false,
                        source: &source,
                    },
                )?;
            }
            mark_digested(c, &ids)?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;

    Ok(true)
}

pub async fn digest(d: &Shared, device: &str) -> Result<bool, String> {
    let now_s = chrono::Utc::now().timestamp();
    digest_with(&d.store, device, &LiveSummariser(d), now_s).await
}

fn daemon_slot() -> &'static OnceLock<Shared> {
    static DAEMON: OnceLock<Shared> = OnceLock::new();
    &DAEMON
}

pub fn init(d: Shared) {
    let _ = daemon_slot().set(d);
}

pub fn ensure_ticker(d: &Shared) {
    static STARTED: OnceLock<()> = OnceLock::new();
    let _ = daemon_slot().set(d.clone());
    STARTED.get_or_init(|| {
        let d = d.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                tick_all(&d).await;
            }
        });
    });
}

pub async fn tick_all(d: &Shared) {
    let devices = d.store.with_connection(known_devices).unwrap_or_default();
    let now_ms = chrono::Utc::now().timestamp_millis();
    for device in devices {
        let bounds: Option<(Option<i64>, Option<i64>, Option<i64>)> = d
            .store
            .with_connection(|c| {
                let (first_u, newest_u): (Option<i64>, Option<i64>) = c.query_row(
                    "SELECT MIN(at_ms), MAX(at_ms) FROM ambient_room_lines WHERE device = ?1 AND digested = 0",
                    [&device],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let latest_any: Option<i64> = c.query_row(
                    "SELECT MAX(at_ms) FROM ambient_room_lines WHERE device = ?1",
                    [&device],
                    |r| r.get(0),
                )?;
                Ok((first_u, newest_u, latest_any))
            })
            .ok();
        if let Some((first_u, newest_u, latest_any)) = bounds {
            if digest_due(first_u, newest_u, now_ms) {
                let _ = digest(d, &device).await;
            }
            if latest_any.is_some_and(|last| now_ms.saturating_sub(last) >= DELIVER_IDLE_MS) {
                let _ = deliver(d, &device).await;
            }
        }
        let _ = maybe_clarify(d, &device).await;
    }
}

pub fn on_conversation_ended() {
    let Some(d) = daemon_slot().get().cloned() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    handle.spawn(async move {
        let devices = d.store.with_connection(known_devices).unwrap_or_default();
        for device in devices {
            let _ = deliver(&d, &device).await;
        }
    });
}

pub fn prepare_delivery(
    c: &Connection,
    device: &str,
    now_s: i64,
) -> rusqlite::Result<Option<String>> {
    let Some(room) = get_room(c, device)? else {
        return Ok(None);
    };
    let notes = items_for_device(c, device, Some("note"), Some("pending"))?;
    let clarifies = items_for_device(c, device, Some("clarify"), Some("pending"))?;
    let topic_changed = !room.topic.is_empty() && room.topic_updated_at > room.delivered_at;
    if notes.is_empty() && !topic_changed {
        return Ok(None);
    }

    let esc = super::ambient_watch::escape;
    let mut body = format!(
        "<room_digest device=\"{}\" topic=\"{}\" activity=\"{}\" present=\"{}\">\n",
        esc(&room.device),
        esc(&room.topic),
        esc(&room.activity),
        esc(&room.present),
    );
    if !room.digest.trim().is_empty() {
        body.push_str(room.digest.trim());
        body.push('\n');
    }
    if !notes.is_empty() {
        body.push_str("Notes:\n");
        for n in &notes {
            let rem = if n.remember { "[remember] " } else { "" };
            let src = if n.source.is_empty() {
                String::new()
            } else {
                format!(" (events: {})", n.source)
            };
            body.push_str(&format!("- {rem}{}{src}\n", n.text.trim()));
        }
    }
    if !clarifies.is_empty() {
        body.push_str("Queued clarifications:\n");
        for q in &clarifies {
            body.push_str(&format!("- {}\n", q.text.trim()));
        }
    }
    body.push_str("</room_digest>");

    for n in &notes {
        set_item_state(c, n.id, "delivered", None)?;
    }
    for q in &clarifies {
        set_item_state(c, q.id, "queued", None)?;
    }
    c.execute(
        "UPDATE ambient_room SET delivered_at = ?2 WHERE device = ?1",
        params![device, now_s],
    )?;
    Ok(Some(body))
}

pub async fn deliver(d: &Shared, device: &str) -> Result<bool, String> {
    let now_s = chrono::Utc::now().timestamp();
    let msg = d
        .store
        .with_connection(|c| prepare_delivery(c, device, now_s))
        .map_err(|e| e.to_string())?;
    let Some(body) = msg else {
        return Ok(false);
    };
    super::agent_routes::record_agent_message(d, SESSION_ID, "room digest", &body, false).await;
    Ok(true)
}

pub fn user_voice_recent(c: &Connection, device: &str, now_s: i64) -> rusqlite::Result<bool> {
    let cutoff_ms = (now_s - CLARIFY_VOICE_SECS) * 1000;
    let from_room: i64 = c.query_row(
        "SELECT COUNT(*) FROM ambient_room_lines
         WHERE device = ?1 AND at_ms >= ?2 AND speaker = 'You'",
        params![device, cutoff_ms],
        |r| r.get(0),
    )?;
    if from_room > 0 {
        return Ok(true);
    }
    let cutoff_s = now_s - CLARIFY_VOICE_SECS;
    let from_events: i64 = c.query_row(
        "SELECT COUNT(*) FROM ambient_events
         WHERE device = ?1 AND created_at >= ?2 AND status = 'done'
           AND (speaker = 'You' OR COALESCE(user_voice, 0.0) >= 0.5)",
        params![device, cutoff_s],
        |r| r.get(0),
    )?;
    Ok(from_events > 0)
}

pub fn asked_recently(c: &Connection, device: &str, now_s: i64) -> rusqlite::Result<bool> {
    let cutoff_s = now_s - CLARIFY_COOLDOWN_SECS;
    let count: i64 = c.query_row(
        "SELECT COUNT(*) FROM ambient_room_items
         WHERE device = ?1 AND kind = 'clarify' AND state = 'asked' AND asked_at >= ?2",
        params![device, cutoff_s],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

pub fn drop_expired_clarifies(c: &Connection, device: &str, now_s: i64) -> rusqlite::Result<usize> {
    let cutoff_s = now_s - CLARIFY_EXPIRE_SECS;
    c.execute(
        "UPDATE ambient_room_items
         SET state = 'dropped'
         WHERE device = ?1 AND kind = 'clarify' AND state IN ('pending', 'queued') AND created_at < ?2",
        params![device, cutoff_s],
    )
}

pub fn current_gate_input(store: &Store, device: &str, now_s: i64) -> GateInput {
    let passive =
        crate::ambient::state_name() == "passive" && crate::ambient::conversation().is_none();
    let alone = crate::ambient_tuning::current(device).profile == Profile::Alone;
    let room_busy = crate::ambient::room_busy();
    let call_active = super::call_track::has_active_call();
    let mc_busy = super::autonomy::mission_control_busy();
    let autonomy = crate::mission_autonomy::load();
    let quiet_hours = crate::mission_autonomy::in_quiet_hours(&autonomy.settings);
    let user_on_call = crate::mission_autonomy::user_call().is_some();
    let (uv_recent, asked_rec) = store
        .with_connection(|c| {
            Ok((
                user_voice_recent(c, device, now_s)?,
                asked_recently(c, device, now_s)?,
            ))
        })
        .unwrap_or((false, false));
    GateInput {
        passive,
        alone,
        room_busy,
        call_active,
        mc_busy,
        quiet_hours,
        user_on_call,
        user_voice_recent: uv_recent,
        asked_recently: asked_rec,
    }
}

pub async fn maybe_clarify(d: &Shared, device: &str) -> Result<bool, String> {
    let now_s = chrono::Utc::now().timestamp();
    let _ = d
        .store
        .with_connection(|c| drop_expired_clarifies(c, device, now_s));
    let gate = current_gate_input(&d.store, device, now_s);
    if clarify_allowed(gate).is_err() {
        return Ok(false);
    }
    let next_item: Option<RoomItem> = d
        .store
        .with_connection(|c| {
            let mut queued = items_for_device(c, device, Some("clarify"), Some("queued"))?;
            if let Some(item) = queued.drain(..).next() {
                set_item_state(c, item.id, "asked", Some(now_s))?;
                Ok(Some(item))
            } else {
                Ok(None)
            }
        })
        .map_err(|e| e.to_string())?;

    let Some(item) = next_item else {
        return Ok(false);
    };

    let esc = super::ambient_watch::escape;
    let msg = format!(
        "<room_clarify id=\"{}\" device=\"{}\">{}</room_clarify>\nThe user is alone and idle. If this still matters, switch ambient_mode to active and ask it in one short spoken sentence; otherwise ignore it.",
        item.id,
        esc(device),
        esc(item.text.trim()),
    );
    super::autonomy::deliver_to_mission_control(d, msg).await;
    Ok(true)
}

#[derive(Deserialize)]
struct AmbientRoomArgs {
    action: String,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    id: Option<i64>,
}

pub struct AmbientRoom;

#[async_trait]
impl Tool for AmbientRoom {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "ambient_room".into(),
            description: "Your model of the room around the user: who is present, the topic, a running digest, notes waiting to be delivered, and clarification questions queued until the user is alone and idle. get to see it and why a question is or is not being asked; drop removes an item; clear resets a device.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["get", "drop", "clear"]},
                    "device": {"type": "string"},
                    "id": {"type": "integer"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        if ctx.mission_control_root().is_none() {
            return Err(ToolError::msg(
                "Mission Control tools are only available in the Mission Control session.",
            ));
        }
        let args: AmbientRoomArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let path = ctx
            .store_path()
            .unwrap_or_else(crate::store::default_db_path);
        let store = Store::open(path).map_err(|e| ToolError::msg(e.to_string()))?;
        let now_s = chrono::Utc::now().timestamp();

        match args.action.as_str() {
            "get" => {
                let devices = store
                    .with_connection(|c| {
                        if let Some(dev) = args
                            .device
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            Ok(vec![dev.to_string()])
                        } else {
                            known_devices(c)
                        }
                    })
                    .map_err(|e| ToolError::msg(e.to_string()))?;

                let mut out_devices = Vec::new();
                for dev in devices {
                    let gate = current_gate_input(&store, &dev, now_s);
                    let gate_res = clarify_allowed(gate.clone());
                    let (room, pending_notes, pending_clarify, queued_clarify, undigested_count) = store
                        .with_connection(|c| {
                            let room = get_room(c, &dev)?;
                            let pn = items_for_device(c, &dev, Some("note"), Some("pending"))?;
                            let pc = items_for_device(c, &dev, Some("clarify"), Some("pending"))?;
                            let qc = items_for_device(c, &dev, Some("clarify"), Some("queued"))?;
                            let uc: i64 = c.query_row(
                                "SELECT COUNT(*) FROM ambient_room_lines WHERE device = ?1 AND digested = 0",
                                [&dev],
                                |r| r.get(0),
                            )?;
                            Ok((room, pn, pc, qc, uc))
                        })
                        .map_err(|e| ToolError::msg(e.to_string()))?;

                    out_devices.push(json!({
                        "device": dev,
                        "room": room,
                        "undigested_lines": undigested_count,
                        "pending_notes": pending_notes,
                        "pending_clarify": pending_clarify,
                        "queued_clarify": queued_clarify,
                        "gate": {
                            "allowed": gate_res.is_ok(),
                            "blocked_by": gate_res.err(),
                            "conditions": {
                                "passive": gate.passive,
                                "alone": gate.alone,
                                "room_not_busy": !gate.room_busy,
                                "no_call_active": !gate.call_active,
                                "mc_not_busy": !gate.mc_busy,
                                "not_quiet_hours": !gate.quiet_hours,
                                "user_not_on_call": !gate.user_on_call,
                                "user_voice_recent": gate.user_voice_recent,
                                "not_asked_recently": !gate.asked_recently,
                            }
                        }
                    }));
                }
                Ok(ToolResult::success(json!({ "devices": out_devices })))
            }
            "drop" => {
                let id = args
                    .id
                    .ok_or_else(|| ToolError::msg("id is required for drop"))?;
                let changed = store
                    .with_connection(|c| set_item_state(c, id, "dropped", None))
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                Ok(ToolResult::success(json!({ "ok": changed > 0, "id": id })))
            }
            "clear" => {
                let cleared = store
                    .with_connection(|c| {
                        let targets = if let Some(dev) = args
                            .device
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            vec![dev.to_string()]
                        } else {
                            known_devices(c)?
                        };
                        for dev in &targets {
                            clear_device(c, dev)?;
                        }
                        Ok(targets)
                    })
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                Ok(ToolResult::success(
                    json!({ "ok": true, "cleared_devices": cleared }),
                ))
            }
            _ => Err(ToolError::msg("action must be get, drop or clear")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("room.db")).unwrap();
        (dir, store)
    }

    struct StubSummariser {
        replies: Vec<String>,
        idx: AtomicUsize,
    }

    impl StubSummariser {
        fn new(replies: Vec<&str>) -> Self {
            Self {
                replies: replies.into_iter().map(String::from).collect(),
                idx: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Summariser for StubSummariser {
        async fn summarise(&self, _system: &str, _prompt: &str) -> Result<Option<String>, String> {
            let i = self.idx.fetch_add(1, Ordering::SeqCst);
            Ok(Some(
                self.replies
                    .get(i)
                    .or_else(|| self.replies.last())
                    .cloned()
                    .unwrap_or_default(),
            ))
        }
    }

    #[tokio::test]
    async fn summariser_parses_eight_lines_and_handles_three_failures() {
        let (_dir, store) = test_store();
        let mut line_ids = Vec::new();
        store
            .with_connection(|c| {
                for n in 0..8 {
                    let id = insert_line(
                        c,
                        "desk",
                        &NewRoomLine {
                            at_ms: 1_000 + n * 5_000,
                            speaker: if n % 2 == 0 {
                                "You".into()
                            } else {
                                "Speaker 2".into()
                            },
                            text: format!("overheard line {n}"),
                            score: 0.75,
                            event_ids: vec![format!("ev{n}")],
                        },
                    )?;
                    line_ids.push(id);
                }
                Ok(())
            })
            .unwrap();

        let fixed = format!(
            r#"```json
{{
  "present": ["You", "Speaker 2"],
  "topic": "dentist and groceries",
  "activity": "cooking and chatting",
  "digest": "You and Speaker 2 are cooking dinner and planning errands. Dentist appointment came up for next week.",
  "notes": [
    {{"text": "Dentist appointment next Friday", "rank": 1, "remember": true, "lines": [{}, {}]}},
    {{"text": "Pick up olive oil", "rank": 2, "remember": false, "lines": [{}]}}
  ],
  "clarify": [
    {{"text": "Did you mean Friday the 14th for the dentist?", "lines": [{}]}}
  ]
}}
```"#,
            line_ids[0], line_ids[2], line_ids[5], line_ids[2]
        );

        let stub = StubSummariser::new(vec![&fixed]);
        let ran = digest_with(&store, "desk", &stub, 500).await.unwrap();
        assert!(ran);

        let (room, undigested, notes, clarifies) = store
            .with_connection(|c| {
                Ok((
                    get_room(c, "desk")?.unwrap(),
                    undigested_lines(c, "desk")?,
                    items_for_device(c, "desk", Some("note"), Some("pending"))?,
                    items_for_device(c, "desk", Some("clarify"), Some("pending"))?,
                ))
            })
            .unwrap();

        assert!(undigested.is_empty());
        assert_eq!(room.present, "You,Speaker 2");
        assert_eq!(room.topic, "dentist and groceries");
        assert_eq!(room.activity, "cooking and chatting");
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].text, "Dentist appointment next Friday");
        assert!(notes[0].remember);
        assert_eq!(notes[0].source, "ev0,ev2");
        assert_eq!(notes[1].source, "ev5");
        assert_eq!(clarifies.len(), 1);
        assert_eq!(clarifies[0].source, "ev2");

        let body = store
            .with_connection(|c| prepare_delivery(c, "desk", 510))
            .unwrap()
            .unwrap();
        assert!(body.contains("<room_digest device=\"desk\" topic=\"dentist and groceries\""));
        assert!(body.contains("- [remember] Dentist appointment next Friday (events: ev0,ev2)"));
        let queued = store
            .with_connection(|c| items_for_device(c, "desk", Some("clarify"), Some("queued")))
            .unwrap();
        assert_eq!(queued.len(), 1);

        // Malformed JSON: two failures keep lines undigested; third marks them digested with no items.
        store
            .with_connection(|c| {
                insert_line(
                    c,
                    "phone",
                    &NewRoomLine {
                        at_ms: 10_000,
                        speaker: "You".into(),
                        text: "half heard note".into(),
                        score: 0.7,
                        event_ids: vec!["ev9".into()],
                    },
                )?;
                Ok(())
            })
            .unwrap();
        let bad = StubSummariser::new(vec!["not json", "{bad", "still bad"]);
        assert!(digest_with(&store, "phone", &bad, 600).await.is_err());
        assert_eq!(
            store
                .with_connection(|c| undigested_lines(c, "phone"))
                .unwrap()
                .len(),
            1
        );
        assert!(digest_with(&store, "phone", &bad, 610).await.is_err());
        assert_eq!(
            store
                .with_connection(|c| undigested_lines(c, "phone"))
                .unwrap()
                .len(),
            1
        );
        assert!(digest_with(&store, "phone", &bad, 620).await.is_err());
        assert!(
            store
                .with_connection(|c| undigested_lines(c, "phone"))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .with_connection(|c| items_for_device(c, "phone", None, None))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn digest_triggers_on_span_and_gap_and_not_when_empty() {
        assert!(!digest_due(None, None, 500_000));
        assert!(!digest_due(Some(100_000), Some(150_000), 260_000));
        // 180s span from first line fires even when newest line is recent
        assert!(digest_due(Some(100_000), Some(270_000), 280_000));
        // 120s gap from newest line fires even when total span < 180s
        assert!(digest_due(Some(100_000), Some(110_000), 230_000));
    }

    #[test]
    fn clarify_gate_table_driven() {
        let ok = GateInput {
            passive: true,
            alone: true,
            room_busy: false,
            call_active: false,
            mc_busy: false,
            quiet_hours: false,
            user_on_call: false,
            user_voice_recent: true,
            asked_recently: false,
        };
        assert_eq!(clarify_allowed(ok.clone()), Ok(()));

        let cases: Vec<(&str, GateInput)> = vec![
            (
                "mode active",
                GateInput {
                    passive: false,
                    ..ok.clone()
                },
            ),
            (
                "not alone",
                GateInput {
                    alone: false,
                    ..ok.clone()
                },
            ),
            (
                "room busy",
                GateInput {
                    room_busy: true,
                    ..ok.clone()
                },
            ),
            (
                "call held",
                GateInput {
                    call_active: true,
                    ..ok.clone()
                },
            ),
            (
                "mc busy",
                GateInput {
                    mc_busy: true,
                    ..ok.clone()
                },
            ),
            (
                "quiet hours",
                GateInput {
                    quiet_hours: true,
                    ..ok.clone()
                },
            ),
            (
                "user on call",
                GateInput {
                    user_on_call: true,
                    ..ok.clone()
                },
            ),
            (
                "no recent user voice",
                GateInput {
                    user_voice_recent: false,
                    ..ok.clone()
                },
            ),
            (
                "asked in last 30m",
                GateInput {
                    asked_recently: true,
                    ..ok.clone()
                },
            ),
        ];
        for (name, input) in cases {
            assert!(clarify_allowed(input).is_err(), "gate should reject {name}");
        }
    }

    #[tokio::test]
    async fn ambient_room_tool_get_drop_clear() {
        let (dir, store) = test_store();
        let item_id = store
            .with_connection(|c| {
                upsert_room(
                    c,
                    "room-test-dev",
                    100,
                    "You",
                    "budget",
                    "quiet",
                    "Reviewing numbers.",
                )?;
                insert_item(
                    c,
                    "room-test-dev",
                    100,
                    &NewRoomItem {
                        kind: "clarify",
                        text: "Was that Q3 or Q4?",
                        rank: 0,
                        remember: false,
                        source: "e1",
                    },
                )
            })
            .unwrap();
        let ctx = ToolContext::mission_control(dir.path().to_path_buf())
            .unwrap()
            .with_store_path(dir.path().join("room.db"));
        let tool = AmbientRoom;

        let res = tool.execute(&ctx, json!({"action": "get"})).await.unwrap();
        assert_eq!(res.value["data"]["devices"][0]["device"], "room-test-dev");

        tool.execute(&ctx, json!({"action": "drop", "id": item_id}))
            .await
            .unwrap();
        let dropped = store
            .with_connection(|c| {
                items_for_device(c, "room-test-dev", Some("clarify"), Some("dropped"))
            })
            .unwrap();
        assert_eq!(dropped.len(), 1);

        tool.execute(&ctx, json!({"action": "clear", "device": "room-test-dev"}))
            .await
            .unwrap();
        assert!(
            store
                .with_connection(|c| get_room(c, "room-test-dev"))
                .unwrap()
                .is_none()
        );
    }
}
