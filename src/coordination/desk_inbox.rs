//! The desk's event queue: what a desk has been told and not yet settled. Chat
//! rows coalesce per thread, everything else is an ordered row; a desk settles
//! rows with ack, noop or defer, and `serve::direct` presents due rows as one
//! `<desk_events>` batch.

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Store, StoreError};

pub const APPROVAL_PRIORITY: i64 = 50;
const SUMMARY_PART_CHARS: usize = 800;
const RECENT_PARTS: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct DeskEvent {
    pub id: String,
    pub agent_id: String,
    pub kind: String,
    pub thread_id: Option<String>,
    pub reply_thread: Option<String>,
    pub source_ref: Option<String>,
    pub priority: i64,
    pub is_mention: bool,
    pub summary: String,
    pub payload: Value,
    pub deadline_at: Option<String>,
    pub wake: bool,
    pub created_at: String,
    pub updated_at: String,
    pub delivered_at: Option<String>,
    pub presented_at: Option<String>,
    pub resolved_at: Option<String>,
    pub resolution: Option<String>,
    pub attempts: i64,
}

#[derive(Debug, Clone, Default)]
pub struct NewDeskEvent {
    pub agent_id: String,
    pub kind: String,
    pub thread_id: Option<String>,
    pub reply_thread: Option<String>,
    pub source_ref: Option<String>,
    pub priority: i64,
    pub is_mention: bool,
    pub summary: String,
    pub payload: Option<Value>,
    pub deadline_at: Option<String>,
    pub wake: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeskBatch {
    pub agent_id: String,
    pub events: Vec<DeskEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskWorkSession {
    pub session_id: String,
    pub task_id: String,
    pub reply_thread: Option<String>,
}

const COLUMNS: &str = "id, agent_id, kind, thread_id, reply_thread, source_ref, priority, is_mention, summary, payload_json, deadline_at, wake, created_at, updated_at, delivered_at, presented_at, resolved_at, resolution, attempts";

const ORDER: &str = "is_mention DESC, priority DESC, (CASE WHEN is_mention = 1 THEN updated_at END) DESC, created_at ASC";

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeskEvent> {
    let payload: String = row.get(9)?;
    Ok(DeskEvent {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        kind: row.get(2)?,
        thread_id: row.get(3)?,
        reply_thread: row.get(4)?,
        source_ref: row.get(5)?,
        priority: row.get(6)?,
        is_mention: row.get::<_, i64>(7)? != 0,
        summary: row.get(8)?,
        payload: serde_json::from_str(&payload).unwrap_or_else(|_| json!({})),
        deadline_at: row.get(10)?,
        wake: row.get::<_, i64>(11)? != 0,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        delivered_at: row.get(14)?,
        presented_at: row.get(15)?,
        resolved_at: row.get(16)?,
        resolution: row.get(17)?,
        attempts: row.get(18)?,
    })
}

pub fn is_chat_kind(kind: &str) -> bool {
    matches!(kind, "dm" | "group")
}

fn is_relay_kind(kind: &str) -> bool {
    matches!(kind, "approval" | "question")
}

/// One UTC second-precision form, so stored timestamps compare as text.
pub fn normalize_ts(ts: &str) -> Result<String, StoreError> {
    chrono::DateTime::parse_from_rfc3339(ts.trim())
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .map_err(|_| StoreError::InvalidInput(format!("`{ts}` is not an RFC 3339 timestamp")))
}

fn clip(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

fn new_id() -> String {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    format!("ev-{}", &raw[..10])
}

fn chat_summary(count: i64, recent: &[String]) -> String {
    let earlier = count - recent.len() as i64;
    let joined = recent.join(" | ");
    if earlier > 0 {
        format!("({earlier} earlier) {joined}")
    } else {
        joined
    }
}

fn parts_of(payload: &Value) -> (i64, Vec<String>) {
    let count = payload.get("count").and_then(Value::as_i64).unwrap_or(1);
    let recent = payload
        .get("recent")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (count, recent)
}

impl Store {
    /// Chat kinds with a thread coalesce into that thread's open row; every
    /// other kind is a new ordered row. A relay (approval or question) with a
    /// `source_ref` is enqueued once per ref.
    pub fn enqueue_desk_event(
        &self,
        event: &NewDeskEvent,
        now: &str,
    ) -> Result<DeskEvent, StoreError> {
        let now = normalize_ts(now)?;
        let deadline = event.deadline_at.as_deref().map(normalize_ts).transpose()?;
        let part = clip(&event.summary, SUMMARY_PART_CHARS);
        if part.is_empty() {
            return Err(StoreError::InvalidInput("summary must not be empty".into()));
        }
        let agent = event.agent_id.as_str();
        if is_relay_kind(&event.kind)
            && let Some(source) = event.source_ref.as_deref()
        {
            let existing = self.with_connection(|conn| {
                conn.query_row(
                    &format!("SELECT {COLUMNS} FROM desk_inbox WHERE agent_id = ?1 AND source_ref = ?2 LIMIT 1"),
                    params![agent, source],
                    row,
                )
                .optional()
            })?;
            if let Some(existing) = existing {
                return Ok(existing);
            }
        }
        if is_chat_kind(&event.kind)
            && let Some(thread) = event.thread_id.as_deref()
            && let Some(open) = self.with_connection(|conn| {
                conn.query_row(
                    &format!("SELECT {COLUMNS} FROM desk_inbox WHERE agent_id = ?1 AND thread_id = ?2 AND kind = ?3 AND resolved_at IS NULL ORDER BY created_at LIMIT 1"),
                    params![agent, thread, event.kind],
                    row,
                )
                .optional()
            })?
        {
            let (count, mut recent) = parts_of(&open.payload);
            if recent.is_empty() {
                recent.push(open.summary.clone());
            }
            recent.push(part);
            let keep = recent.len().saturating_sub(RECENT_PARTS);
            recent.drain(..keep);
            let count = count + 1;
            let mut payload = match open.payload.clone() {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            if let Some(Value::Object(extra)) = &event.payload {
                payload.extend(extra.clone());
            }
            payload.insert("count".into(), json!(count));
            payload.insert("recent".into(), json!(recent));
            let summary = chat_summary(count, &recent);
            let created = open.created_at.clone();
            let reply = event.reply_thread.clone().or(open.reply_thread.clone());
            let deadline = deadline.or(open.deadline_at.clone());
            self.with_connection(|conn| {
                conn.execute(
                    "UPDATE desk_inbox SET summary = ?2, payload_json = ?3, is_mention = MAX(is_mention, ?4),
                         priority = MAX(priority, ?5), updated_at = ?6, created_at = ?7, presented_at = NULL,
                         reply_thread = ?8, deadline_at = ?9, wake = MAX(wake, ?10)
                     WHERE id = ?1",
                    params![
                        open.id,
                        summary,
                        Value::Object(payload).to_string(),
                        event.is_mention as i64,
                        event.priority,
                        now,
                        created,
                        reply,
                        deadline,
                        event.wake as i64
                    ],
                )
            })?;
            return self
                .get_desk_event(agent, &open.id)?
                .ok_or_else(|| StoreError::InvalidInput("coalesced event vanished".into()));
        }
        let mut payload = match &event.payload {
            Some(Value::Object(map)) => map.clone(),
            _ => serde_json::Map::new(),
        };
        if is_chat_kind(&event.kind) {
            payload.insert("count".into(), json!(1));
            payload.insert("recent".into(), json!([part.clone()]));
        }
        let id = new_id();
        self.with_connection(|conn| {
            conn.execute(
                &format!(
                    "INSERT INTO desk_inbox ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, NULL, NULL, NULL, NULL, 0)"
                ),
                params![
                    id,
                    agent,
                    event.kind,
                    event.thread_id,
                    event.reply_thread,
                    event.source_ref,
                    event.priority,
                    event.is_mention as i64,
                    part,
                    Value::Object(payload).to_string(),
                    deadline,
                    event.wake as i64,
                    now
                ],
            )
        })?;
        self.get_desk_event(agent, &id)?
            .ok_or_else(|| StoreError::InvalidInput("inserted event vanished".into()))
    }

    pub fn get_desk_event(
        &self,
        agent_id: &str,
        id: &str,
    ) -> Result<Option<DeskEvent>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM desk_inbox WHERE agent_id = ?1 AND id = ?2"),
                params![agent_id, id],
                row,
            )
            .optional()
        })
    }

    pub fn list_open_desk_events(
        &self,
        agent_id: &str,
        limit: u32,
    ) -> Result<Vec<DeskEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM desk_inbox WHERE agent_id = ?1 AND resolved_at IS NULL ORDER BY {ORDER} LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![agent_id, limit], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Agents with a batch due now. A waking row with no deadline waits out the
    /// debounce from its first message, or from its latest once it was already
    /// delivered; a row with a deadline waits for it, so a deferred or self-set
    /// event is pushed when it comes due and once per deadline. Non-waking rows
    /// never trigger a batch but ride along with one.
    pub fn due_desk_batches(
        &self,
        now: &str,
        debounce_secs: i64,
    ) -> Result<Vec<DeskBatch>, StoreError> {
        let now = normalize_ts(now)?;
        let cutoff = chrono::DateTime::parse_from_rfc3339(&now)
            .map(|t| {
                (t.with_timezone(&chrono::Utc) - chrono::Duration::seconds(debounce_secs))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            })
            .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        const PENDING: &str = "resolved_at IS NULL
             AND (presented_at IS NULL OR (deadline_at IS NOT NULL AND deadline_at <= ?1 AND presented_at < deadline_at))";
        self.with_connection(|conn| {
            let mut agents = conn.prepare(&format!(
                "SELECT DISTINCT agent_id FROM desk_inbox WHERE {PENDING} AND wake = 1
                 AND ((deadline_at IS NULL AND ((delivered_at IS NULL AND created_at <= ?2)
                                                OR (delivered_at IS NOT NULL AND updated_at <= ?2)))
                      OR (deadline_at IS NOT NULL AND deadline_at <= ?1))
                 ORDER BY agent_id"
            ))?;
            let agents = agents
                .query_map(params![now, cutoff], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut rows = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM desk_inbox WHERE agent_id = ?2 AND {PENDING}
                 AND (deadline_at IS NULL OR deadline_at <= ?1)
                 ORDER BY {ORDER}"
            ))?;
            let mut out = Vec::new();
            for agent in agents {
                let events = rows
                    .query_map(params![now, agent], row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if !events.is_empty() {
                    out.push(DeskBatch {
                        agent_id: agent,
                        events,
                    });
                }
            }
            Ok(out)
        })
    }

    pub fn mark_desk_events_presented(&self, ids: &[String], now: &str) -> Result<(), StoreError> {
        let now = normalize_ts(now)?;
        self.with_connection(|conn| {
            for id in ids {
                conn.execute(
                    "UPDATE desk_inbox SET presented_at = ?2, delivered_at = COALESCE(delivered_at, ?2),
                         attempts = attempts + 1
                     WHERE id = ?1",
                    params![id, now],
                )?;
            }
            Ok(())
        })
    }

    /// On daemon startup, clear `presented_at` on unresolved waking events that
    /// were in-flight when the previous daemon stopped so they re-deliver.
    pub fn requeue_inflight_desk_events(&self) -> Result<usize, StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE desk_inbox SET presented_at = NULL
                 WHERE resolved_at IS NULL AND presented_at IS NOT NULL
                   AND deadline_at IS NULL AND wake = 1",
                [],
            )
        })
    }

    /// Settle the desk's own open events; returns the rows it settled.
    pub fn resolve_desk_events(
        &self,
        agent_id: &str,
        ids: &[String],
        resolution: &str,
        now: &str,
    ) -> Result<Vec<DeskEvent>, StoreError> {
        if !matches!(resolution, "ack" | "noop") {
            return Err(StoreError::InvalidInput(format!(
                "resolution must be ack or noop (got `{resolution}`)"
            )));
        }
        self.settle(agent_id, ids, resolution, now)
    }

    fn settle(
        &self,
        agent_id: &str,
        ids: &[String],
        resolution: &str,
        now: &str,
    ) -> Result<Vec<DeskEvent>, StoreError> {
        let now = normalize_ts(now)?;
        let mut settled = Vec::new();
        for id in ids {
            let changed = self.with_connection(|conn| {
                conn.execute(
                    "UPDATE desk_inbox SET resolved_at = ?3, resolution = ?4, updated_at = ?3
                     WHERE agent_id = ?1 AND id = ?2 AND resolved_at IS NULL",
                    params![agent_id, id, now, resolution],
                )
            })?;
            if changed > 0
                && let Some(event) = self.get_desk_event(agent_id, id)?
            {
                settled.push(event);
            }
        }
        Ok(settled)
    }

    /// Ack the desk's events and finish the fallback any of them stood for,
    /// so the daemon does not post a notice the desk already passed on.
    pub fn ack_desk_events(
        &self,
        agent_id: &str,
        ids: &[String],
        now: &str,
    ) -> Result<Vec<DeskEvent>, StoreError> {
        let settled = self.resolve_desk_events(agent_id, ids, "ack", now)?;
        for event in &settled {
            if is_relay_kind(&event.kind)
                && let Some(key) = event.source_ref.as_deref()
            {
                self.finish_desk_fallback(key, &normalize_ts(now)?)?;
            }
        }
        Ok(settled)
    }

    /// Close the relay events a finished fallback stood for.
    pub fn settle_desk_events_for_source(
        &self,
        agent_id: &str,
        source_ref: &str,
        resolution: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        let now = normalize_ts(now)?;
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE desk_inbox SET resolved_at = ?3, resolution = ?4, updated_at = ?3
                 WHERE agent_id = ?1 AND source_ref = ?2 AND resolved_at IS NULL",
                params![agent_id, source_ref, now, resolution],
            )
            .map(|_| ())
        })
    }

    /// Move an open event's deadline; it is presented again when the deadline
    /// comes, and stays open until the desk settles it.
    pub fn defer_desk_event(
        &self,
        agent_id: &str,
        id: &str,
        deadline_at: &str,
        now: &str,
    ) -> Result<Option<DeskEvent>, StoreError> {
        let now = normalize_ts(now)?;
        let deadline = normalize_ts(deadline_at)?;
        let changed = self.with_connection(|conn| {
            conn.execute(
                "UPDATE desk_inbox SET deadline_at = ?3, presented_at = NULL, updated_at = ?4
                 WHERE agent_id = ?1 AND id = ?2 AND resolved_at IS NULL",
                params![agent_id, id, deadline, now],
            )
        })?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_desk_event(agent_id, id)
    }

    pub fn create_self_desk_event(
        &self,
        agent_id: &str,
        summary: &str,
        deadline_at: Option<&str>,
        thread_id: Option<&str>,
        priority: i64,
        now: &str,
    ) -> Result<DeskEvent, StoreError> {
        self.enqueue_desk_event(
            &NewDeskEvent {
                agent_id: agent_id.to_string(),
                kind: "self".into(),
                thread_id: thread_id.map(str::to_string),
                reply_thread: thread_id.map(str::to_string),
                priority,
                summary: summary.to_string(),
                deadline_at: deadline_at.map(str::to_string),
                wake: true,
                ..Default::default()
            },
            now,
        )
    }

    /// The agent's sessions with a task in progress, and where its requester reads.
    pub fn desk_work_sessions(
        &self,
        agent_id: &str,
        limit: u32,
    ) -> Result<Vec<DeskWorkSession>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) AS sid, ta.task_id, tr.reply_to
                 FROM task_agents ta
                 JOIN tasks t ON t.id = ta.task_id
                 LEFT JOIN task_requesters tr ON tr.task_id = ta.task_id
                 WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL
                   AND COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) IS NOT NULL
                   AND COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) != ''
                   AND t.status IN ('in_progress', 'blocked', 'in_review')
                 ORDER BY t.updated_at DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![agent_id, limit], |r| {
                    Ok(DeskWorkSession {
                        session_id: r.get(0)?,
                        task_id: r.get(1)?,
                        reply_thread: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(agent: &str, kind: &str, thread: &str, text: &str, mention: bool) -> NewDeskEvent {
        NewDeskEvent {
            agent_id: agent.into(),
            kind: kind.into(),
            thread_id: Some(thread.into()),
            reply_thread: Some(thread.into()),
            is_mention: mention,
            summary: text.into(),
            wake: true,
            ..Default::default()
        }
    }

    const T0: &str = "2026-11-26T10:00:00Z";
    const T1: &str = "2026-11-26T10:00:10Z";
    const T2: &str = "2026-11-26T10:00:20Z";

    #[test]
    fn chat_rows_coalesce_per_thread_and_progress_rows_stay_separate() {
        let store = Store::open_in_memory().unwrap();
        let a = store
            .enqueue_desk_event(&chat("a", "dm", "t1", "hi", false), T0)
            .unwrap();
        let b = store
            .enqueue_desk_event(
                &NewDeskEvent {
                    priority: 5,
                    ..chat("a", "dm", "t1", "are you there?", true)
                },
                T1,
            )
            .unwrap();
        assert_eq!(a.id, b.id);
        assert_eq!(b.created_at, T0);
        assert_eq!(b.updated_at, T1);
        assert!(b.is_mention);
        assert_eq!(b.priority, 5);
        assert_eq!(b.summary, "hi | are you there?");
        assert_eq!(b.payload["count"], 2);

        for text in ["p1", "p2"] {
            store
                .enqueue_desk_event(
                    &NewDeskEvent {
                        agent_id: "a".into(),
                        kind: "progress".into(),
                        thread_id: Some("t1".into()),
                        summary: text.into(),
                        wake: true,
                        ..Default::default()
                    },
                    T1,
                )
                .unwrap();
        }
        assert_eq!(store.list_open_desk_events("a", 10).unwrap().len(), 3);

        store
            .mark_desk_events_presented(&[a.id.clone()], T1)
            .unwrap();
        let c = store
            .enqueue_desk_event(&chat("a", "dm", "t1", "one more", false), T2)
            .unwrap();
        assert_eq!(c.id, a.id);
        assert!(c.presented_at.is_none(), "new messages are presented again");
        for i in 0..6 {
            store
                .enqueue_desk_event(&chat("a", "dm", "t1", &format!("m{i}"), false), T2)
                .unwrap();
        }
        let d = store.get_desk_event("a", &a.id).unwrap().unwrap();
        assert_eq!(d.summary, "(5 earlier) m2 | m3 | m4 | m5");
    }

    #[test]
    fn open_events_rank_mentions_by_recency_then_priority_then_age() {
        let store = Store::open_in_memory().unwrap();
        let old_mention = store
            .enqueue_desk_event(&chat("a", "group", "g1", "@a one", true), T0)
            .unwrap();
        let new_mention = store
            .enqueue_desk_event(&chat("a", "group", "g2", "@a two", true), T1)
            .unwrap();
        let urgent = store
            .enqueue_desk_event(
                &NewDeskEvent {
                    priority: APPROVAL_PRIORITY,
                    ..chat("a", "dm", "d1", "approve?", false)
                },
                T2,
            )
            .unwrap();
        let plain = store
            .enqueue_desk_event(&chat("a", "dm", "d2", "fyi", false), T0)
            .unwrap();
        let ids: Vec<String> = store
            .list_open_desk_events("a", 10)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            ids,
            vec![new_mention.id, old_mention.id, urgent.id, plain.id]
        );
    }

    #[test]
    fn bulk_noop_defer_and_self_events() {
        let store = Store::open_in_memory().unwrap();
        let x = store
            .enqueue_desk_event(&chat("a", "dm", "t1", "x", false), T0)
            .unwrap();
        let y = store
            .enqueue_desk_event(&chat("a", "dm", "t2", "y", false), T0)
            .unwrap();
        let other = store
            .enqueue_desk_event(&chat("b", "dm", "t3", "z", false), T0)
            .unwrap();
        let settled = store
            .resolve_desk_events(
                "a",
                &[x.id.clone(), y.id.clone(), other.id.clone()],
                "noop",
                T1,
            )
            .unwrap();
        assert_eq!(settled.len(), 2, "only the desk's own events");
        assert!(
            settled
                .iter()
                .all(|e| e.resolution.as_deref() == Some("noop"))
        );
        assert!(store.list_open_desk_events("a", 10).unwrap().is_empty());
        assert!(
            store
                .resolve_desk_events("a", &[x.id.clone()], "done", T1)
                .is_err()
        );

        let due = store.due_desk_batches("2026-11-26T10:01:00Z", 30).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].agent_id, "b");
        store
            .mark_desk_events_presented(&[other.id.clone()], "2026-11-26T10:01:00Z")
            .unwrap();
        assert!(
            store
                .due_desk_batches("2026-11-26T10:02:00Z", 30)
                .unwrap()
                .is_empty()
        );

        let deferred = store
            .defer_desk_event(
                "b",
                &other.id,
                "2026-11-26T11:00:00+00:00",
                "2026-11-26T10:02:00Z",
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            deferred.deadline_at.as_deref(),
            Some("2026-11-26T11:00:00Z")
        );
        assert!(deferred.presented_at.is_none() && deferred.resolved_at.is_none());
        assert!(
            store
                .due_desk_batches("2026-11-26T10:30:00Z", 30)
                .unwrap()
                .is_empty()
        );
        let at_deadline = store.due_desk_batches("2026-11-26T11:00:00Z", 30).unwrap();
        assert_eq!(at_deadline.len(), 1);
        store
            .mark_desk_events_presented(&[other.id.clone()], "2026-11-26T11:00:05Z")
            .unwrap();
        assert!(
            store
                .due_desk_batches("2026-11-26T11:30:00Z", 30)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .get_desk_event("b", &other.id)
                .unwrap()
                .unwrap()
                .resolved_at
                .is_none()
        );

        let reminder = store
            .create_self_desk_event(
                "a",
                "check CI on #150",
                Some("2026-11-26T12:00:00Z"),
                Some("t1"),
                3,
                T0,
            )
            .unwrap();
        assert_eq!(reminder.kind, "self");
        assert_eq!(reminder.priority, 3);
        assert!(
            store
                .due_desk_batches("2026-11-26T11:59:00Z", 30)
                .unwrap()
                .iter()
                .all(|b| b.agent_id != "a")
        );
        let due = store.due_desk_batches("2026-11-26T12:00:00Z", 30).unwrap();
        assert!(
            due.iter()
                .any(|b| b.agent_id == "a" && b.events[0].id == reminder.id)
        );
    }

    #[test]
    fn quiet_rows_ride_along_with_a_waking_batch_but_never_wake_alone() {
        let store = Store::open_in_memory().unwrap();
        let quiet = store
            .enqueue_desk_event(
                &NewDeskEvent {
                    agent_id: "a".into(),
                    kind: "progress".into(),
                    summary: "step 3 of 5 done".into(),
                    wake: false,
                    ..Default::default()
                },
                T0,
            )
            .unwrap();
        assert!(
            store
                .due_desk_batches("2026-11-26T11:00:00Z", 30)
                .unwrap()
                .is_empty()
        );
        let loud = store
            .enqueue_desk_event(
                &chat("a", "dm", "t1", "ping", false),
                "2026-11-26T11:00:00Z",
            )
            .unwrap();
        let due = store.due_desk_batches("2026-11-26T11:00:30Z", 30).unwrap();
        assert_eq!(due.len(), 1);
        let ids: Vec<&str> = due[0].events.iter().map(|e| e.id.as_str()).collect();
        assert!(
            ids.contains(&loud.id.as_str()) && ids.contains(&quiet.id.as_str()),
            "{ids:?}"
        );
    }

    #[test]
    fn a_reopened_chat_row_keeps_its_age_and_debounces_from_its_latest_message() {
        let store = Store::open_in_memory().unwrap();
        let first = store
            .enqueue_desk_event(&chat("a", "dm", "t1", "x", false), T0)
            .unwrap();
        store
            .mark_desk_events_presented(&[first.id.clone()], "2026-11-26T10:00:30Z")
            .unwrap();
        let again = store
            .enqueue_desk_event(&chat("a", "dm", "t1", "y", false), "2026-11-26T10:05:00Z")
            .unwrap();
        assert_eq!(again.created_at, T0);
        assert!(
            store
                .due_desk_batches("2026-11-26T10:05:10Z", 30)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .due_desk_batches("2026-11-26T10:05:30Z", 30)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn chat_rows_wait_out_the_debounce() {
        let store = Store::open_in_memory().unwrap();
        store
            .enqueue_desk_event(&chat("a", "dm", "t1", "x", false), T0)
            .unwrap();
        assert!(
            store
                .due_desk_batches("2026-11-26T10:00:29Z", 30)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .due_desk_batches("2026-11-26T10:00:30Z", 30)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn ack_finishes_the_fallback_a_relay_stands_for() {
        let store = Store::open_in_memory().unwrap();
        store
            .schedule_desk_fallback(&crate::coordination::desk_fallbacks::DeskFallback {
                key: "ask:s:1".into(),
                desk: "a".into(),
                session_id: "s".into(),
                what: "question".into(),
                event_index: 1,
                body: "b".into(),
                extra: Default::default(),
                created_at: T0.into(),
                due_at: T1.into(),
            })
            .unwrap();
        let relay = NewDeskEvent {
            agent_id: "a".into(),
            kind: "question".into(),
            source_ref: Some("ask:s:1".into()),
            priority: APPROVAL_PRIORITY,
            summary: "s asks".into(),
            wake: true,
            ..Default::default()
        };
        let e = store.enqueue_desk_event(&relay, T0).unwrap();
        assert_eq!(store.enqueue_desk_event(&relay, T1).unwrap().id, e.id);
        store.ack_desk_events("a", &[e.id.clone()], T1).unwrap();
        assert!(
            store
                .due_desk_fallbacks("2999-01-01T00:00:00Z", 10)
                .unwrap()
                .is_empty()
        );
    }
}
