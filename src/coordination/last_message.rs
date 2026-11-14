//! Per-agent "last message" preview for the agent list and inbox.
//!
//! One row per agent, written only from the user's own thread with it (direct
//! messages, or Mission Control's session), so listing agents never loads a
//! thread. Desk, task and group turns never count. An empty `text` row is a
//! "looked, found nothing" marker that stops the lazy backfill repeating.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::direct::{LOCAL_HUMAN_ID, direct_thread_id};
use crate::store::{Store, StoreError};

const MAX_CHARS: usize = 200;
const BACKFILL_TAIL: u32 = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LastMessage {
    pub text: String,
    /// RFC 3339.
    pub at: String,
    /// `user` or `agent`.
    pub from: String,
}

/// Plain-text preview: fence lines dropped, whitespace collapsed, capped.
pub fn snippet_text(raw: &str) -> String {
    let kept: Vec<&str> = raw
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
        .collect();
    let collapsed = kept
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= MAX_CHARS {
        return collapsed;
    }
    let mut cut: String = collapsed.chars().take(MAX_CHARS - 1).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

fn millis_of(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis())
}

fn rfc3339_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn is_mission_control(agent_id: &str) -> bool {
    agent_id == crate::mission_control::SESSION_ID
}

impl Store {
    /// Record `text` as the agent's last message if it is at least as new as the
    /// stored one. `at` is RFC 3339 (unparseable falls back to now).
    pub fn note_agent_last_message(
        &self,
        agent_id: &str,
        raw: &str,
        at: &str,
        from: &str,
    ) -> Result<(), StoreError> {
        let text = snippet_text(raw);
        if text.is_empty() {
            return Ok(());
        }
        let at_ms = millis_of(at);
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agent_last_dm (agent_id, text, at_ms, from_side)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent_id) DO UPDATE SET
                    text = excluded.text, at_ms = excluded.at_ms, from_side = excluded.from_side
                 WHERE excluded.at_ms >= agent_last_dm.at_ms",
                params![agent_id, text, at_ms, from],
            )?;
            Ok(())
        })
    }

    /// Stored previews for every agent that has one.
    pub fn agent_last_messages(&self) -> Result<HashMap<String, LastMessage>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT agent_id, text, at_ms, from_side FROM agent_last_dm WHERE text <> ''",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    LastMessage {
                        text: row.get(1)?,
                        at: rfc3339_of(row.get(2)?),
                        from: row.get(3)?,
                    },
                ))
            })?;
            rows.collect::<Result<_, _>>()
        })
    }

    /// The stored preview, computing and storing it once from existing data when
    /// the agent has never been looked at.
    pub fn agent_last_message_or_backfill(
        &self,
        agent_id: &str,
    ) -> Result<Option<LastMessage>, StoreError> {
        let stored: Option<(String, i64, String)> = self.with_connection(|conn| {
            conn.query_row(
                "SELECT text, at_ms, from_side FROM agent_last_dm WHERE agent_id = ?1",
                params![agent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
        })?;
        match stored {
            Some((text, at_ms, from)) if !text.is_empty() => Ok(Some(LastMessage {
                text,
                at: rfc3339_of(at_ms),
                from,
            })),
            Some(_) => Ok(None),
            None => self.backfill_agent_last_dm(agent_id),
        }
    }

    fn backfill_agent_last_dm(&self, agent_id: &str) -> Result<Option<LastMessage>, StoreError> {
        let mut best: Option<(i64, String, &'static str)> = None;

        let thread = direct_thread_id(("agent", agent_id), ("human", LOCAL_HUMAN_ID));
        let dm: Option<(String, String, String)> = self.with_connection(|conn| {
            conn.query_row(
                "SELECT payload_json, actor_kind, created_at FROM board_events
                 WHERE thread_id = ?1 AND event_type = 'direct_message.sent'
                   AND COALESCE(json_extract(payload_json, '$.system'), 0) = 0
                 ORDER BY rowid DESC LIMIT 1",
                params![thread],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
        })?;
        if let Some((payload, kind, created)) = dm {
            let body = serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(str::to_string))
                .unwrap_or_default();
            let text = snippet_text(&super::direct::envelope_text(&body));
            if !text.is_empty() {
                let from = if kind == "human" { "user" } else { "agent" };
                best = Some((millis_of(&created), text, from));
            }
        }

        let sessions: Vec<String> = if is_mission_control(agent_id) {
            vec![agent_id.to_string()]
        } else {
            Vec::new()
        };
        for id in sessions {
            let tail = self.load_recent_conversation(&id, BACKFILL_TAIL)?;
            let last = tail.iter().rev().find_map(|m| match &m.message {
                crate::llm::HarnessMessage::Assistant { content, .. } => {
                    let text = snippet_text(content);
                    (!text.is_empty()).then(|| (millis_of(&m.created_at), text))
                }
                _ => None,
            });
            if let Some((ms, text)) = last
                && best.as_ref().is_none_or(|(b, _, _)| ms > *b)
            {
                best = Some((ms, text, "agent"));
            }
        }

        match best {
            Some((ms, text, from)) => {
                self.with_connection(|conn| {
                    conn.execute(
                        "INSERT OR IGNORE INTO agent_last_dm (agent_id, text, at_ms, from_side)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![agent_id, text, ms, from],
                    )?;
                    Ok(())
                })?;
                Ok(Some(LastMessage {
                    text,
                    at: rfc3339_of(ms),
                    from: from.into(),
                }))
            }
            None => {
                self.with_connection(|conn| {
                    conn.execute(
                        "INSERT OR IGNORE INTO agent_last_dm (agent_id, text, at_ms, from_side)
                         VALUES (?1, '', 0, 'agent')",
                        params![agent_id],
                    )?;
                    Ok(())
                })?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};
    use crate::llm::HarnessMessage;

    fn worker(id: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    #[test]
    fn snippet_strips_fences_collapses_and_caps() {
        let raw = "Done.\n```rust\nfn  main() {}\n```\n\n  next   line";
        assert_eq!(snippet_text(raw), "Done. fn main() {} next line");
        let long = "word ".repeat(100);
        let cut = snippet_text(&long);
        assert!(cut.chars().count() <= MAX_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn a_dm_sets_the_preview_and_a_later_session_turn_replaces_it() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.send_direct_message(
            ("human", LOCAL_HUMAN_ID),
            ("agent", "a"),
            "please look at the build",
            "k1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let got = db.agent_last_messages().unwrap();
        assert_eq!(got["a"].text, "please look at the build");
        assert_eq!(got["a"].from, "user");

        db.note_agent_last_message("a", "Build fixed.", "2026-01-01T00:05:00Z", "agent")
            .unwrap();
        let got = db.agent_last_messages().unwrap();
        assert_eq!(got["a"].text, "Build fixed.");
        assert_eq!(got["a"].from, "agent");

        // An older write never replaces a newer preview.
        db.note_agent_last_message("a", "stale", "2026-01-01T00:01:00Z", "user")
            .unwrap();
        assert_eq!(db.agent_last_messages().unwrap()["a"].text, "Build fixed.");

        db.send_direct_message(
            ("agent", "a"),
            ("human", LOCAL_HUMAN_ID),
            "Anything else?",
            "k2",
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        let got = db.agent_last_messages().unwrap();
        assert_eq!(got["a"].text, "Anything else?");
        assert_eq!(got["a"].from, "agent");
    }

    #[test]
    fn system_notices_are_ignored_by_live_and_backfill_previews() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.send_direct_message(
            ("human", LOCAL_HUMAN_ID),
            ("agent", "a"),
            "check status",
            "k1",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let mut extra = serde_json::Map::new();
        extra.insert("system".into(), serde_json::json!(true));
        extra.insert("kind".into(), serde_json::json!("notice"));
        db.send_direct_message_typed(
            ("agent", "a"),
            ("human", LOCAL_HUMAN_ID),
            "a finished this turn without a reply; its last step was bash.",
            "k2",
            "2026-01-01T00:01:00Z",
            None,
            None,
            None,
            extra,
        )
        .unwrap();
        let got = db.agent_last_messages().unwrap();
        assert_eq!(got["a"].text, "check status");
        assert_eq!(got["a"].from, "user");

        db.with_connection(|c| {
            c.execute("DELETE FROM agent_last_dm", [])?;
            Ok(())
        })
        .unwrap();
        let backfilled = db.agent_last_message_or_backfill("a").unwrap().unwrap();
        assert_eq!(backfilled.text, "check status");
        assert_eq!(backfilled.from, "user");
    }

    #[test]
    fn agent_to_agent_dm_does_not_count() {
        let db = Store::open_in_memory().unwrap();
        db.send_direct_message(
            ("agent", "a"),
            ("agent", "b"),
            "hi",
            "k",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        assert!(db.agent_last_messages().unwrap().is_empty());
    }

    #[test]
    fn mission_control_keeps_its_last_reply() {
        let db = Store::open_in_memory().unwrap();
        let mc = crate::mission_control::SESSION_ID;
        db.note_agent_last_message(mc, "Routed it to Snippet.", "2026-01-01T00:00:00Z", "agent")
            .unwrap();
        assert_eq!(
            db.agent_last_messages().unwrap()[mc].text,
            "Routed it to Snippet."
        );
    }

    #[test]
    fn mission_control_backfills_from_its_own_session() {
        let db = Store::open_in_memory().unwrap();
        let mc = crate::mission_control::SESSION_ID;
        db.save_session_scalar(
            mc,
            "k",
            "/w",
            None,
            "idle",
            "{}",
            "2026-01-02T00:00:00Z",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
        db.append_conversation_messages(
            mc,
            &[HarnessMessage::Assistant {
                content: "Filed the review task.".into(),
                tool_calls: vec![],
            }],
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
        let got = db.agent_last_message_or_backfill(mc).unwrap().unwrap();
        assert_eq!(got.text, "Filed the review task.");
        assert_eq!(got.from, "agent");
    }

    #[test]
    fn backfill_uses_existing_dm_and_session_then_stores_once() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        // Written before the feature existed: drop the row the hook created.
        db.send_direct_message(
            ("human", LOCAL_HUMAN_ID),
            ("agent", "a"),
            "old ask",
            "k",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        db.with_connection(|c| {
            c.execute("DELETE FROM agent_last_dm", [])?;
            Ok(())
        })
        .unwrap();
        let first = db.agent_last_message_or_backfill("a").unwrap().unwrap();
        assert_eq!(first.text, "old ask");
        assert_eq!(db.agent_last_messages().unwrap()["a"].text, "old ask");

        // An agent with nothing gets a marker and no preview.
        assert!(
            db.agent_last_message_or_backfill("nobody")
                .unwrap()
                .is_none()
        );
        assert!(db.agent_last_messages().unwrap().get("nobody").is_none());
    }

    #[test]
    fn backfill_ignores_desk_and_task_sessions() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.save_session_scalar(
            "s1",
            "k",
            "/w",
            None,
            "idle",
            "{}",
            "2026-01-02T00:00:00Z",
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
        db.with_connection(|c| {
            c.execute("UPDATE sessions SET agent_id = 'a' WHERE id = 's1'", [])?;
            Ok(())
        })
        .unwrap();
        db.append_conversation_messages(
            "s1",
            &[HarnessMessage::Assistant {
                content: "From the desk".into(),
                tool_calls: vec![],
            }],
            "2026-01-02T00:00:00Z",
        )
        .unwrap();
        db.send_direct_message(
            ("human", LOCAL_HUMAN_ID),
            ("agent", "a"),
            "older",
            "k",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        db.with_connection(|c| {
            c.execute("DELETE FROM agent_last_dm", [])?;
            Ok(())
        })
        .unwrap();
        let got = db.agent_last_message_or_backfill("a").unwrap().unwrap();
        assert_eq!(got.text, "older");
        assert_eq!(got.from, "user");
    }

    #[test]
    fn schema_ensure_is_idempotent() {
        let db = Store::open_in_memory().unwrap();
        db.note_agent_last_message("a", "x", "2026-01-01T00:00:00Z", "agent")
            .unwrap();
        db.with_connection(|c| {
            crate::coordination::schema::ensure(c)?;
            crate::coordination::schema::ensure(c)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.agent_last_messages().unwrap()["a"].text, "x");
    }
}
