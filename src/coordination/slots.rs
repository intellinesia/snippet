//! A desk's parallelism: how many work sessions it runs at once, the claims it
//! queues for a free slot, and the per-session tool-round counters behind delta
//! progress reports and the stop filter.

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::desk_inbox::{DeskEvent, NewDeskEvent, normalize_ts};
use super::{Store, StoreError};

pub const MAX_PARALLEL: u32 = 6;
pub const SLOT_GRANT_MINUTES: i64 = 10;
pub const SLOT_WAIT_HOURS: i64 = 24;
pub const PROGRESS_EVERY_ROUNDS: u64 = 40;
pub const STOP_NOTIFY_MIN_ROUNDS: u64 = 10;
const SLOT_FREE_PRIORITY: i64 = 20;

pub fn auto_parallel_cap(effective_window: u64) -> u32 {
    ((effective_window / 32_000).min(u32::MAX as u64) as u32).clamp(2, MAX_PARALLEL)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskParallelism {
    pub max_parallel: u32,
    pub source: String,
    pub basis_profile: Option<String>,
    pub basis_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotClaim {
    pub id: String,
    pub agent_id: String,
    pub requested_for: Option<String>,
    pub status: String,
    pub granted_at: Option<String>,
    pub expires_at: Option<String>,
    pub consumed_at: Option<String>,
    pub created_at: String,
}

const CLAIM_COLUMNS: &str =
    "id, agent_id, requested_for, status, granted_at, expires_at, consumed_at, created_at";

fn claim_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SlotClaim> {
    Ok(SlotClaim {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        requested_for: row.get(2)?,
        status: row.get(3)?,
        granted_at: row.get(4)?,
        expires_at: row.get(5)?,
        consumed_at: row.get(6)?,
        created_at: row.get(7)?,
    })
}

fn shift(now: &str, delta: chrono::Duration) -> Result<String, StoreError> {
    chrono::DateTime::parse_from_rfc3339(now)
        .map(|t| {
            (t.with_timezone(&chrono::Utc) + delta)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .map_err(|e| StoreError::InvalidInput(e.to_string()))
}

impl Store {
    /// A manual cap holds as set; an auto cap follows the current window.
    pub fn get_desk_parallelism(
        &self,
        agent_id: &str,
        default_window: u64,
    ) -> Result<DeskParallelism, StoreError> {
        let stored = self.with_connection(|conn| {
            conn.query_row(
                "SELECT max_parallel, source, basis_profile, basis_window FROM agent_desk_settings WHERE agent_id = ?1",
                params![agent_id],
                |r| {
                    Ok(DeskParallelism {
                        max_parallel: r.get::<_, i64>(0)?.clamp(1, MAX_PARALLEL as i64) as u32,
                        source: r.get(1)?,
                        basis_profile: r.get(2)?,
                        basis_window: r.get::<_, Option<i64>>(3)?.map(|w| w.max(0) as u64),
                    })
                },
            )
            .optional()
        })?;
        Ok(match stored {
            Some(manual) if manual.source == "manual" => manual,
            other => DeskParallelism {
                max_parallel: auto_parallel_cap(default_window),
                source: "auto".into(),
                basis_profile: other.and_then(|o| o.basis_profile),
                basis_window: Some(default_window),
            },
        })
    }

    pub fn set_desk_parallelism(
        &self,
        agent_id: &str,
        max_parallel: u32,
        source: &str,
        basis_profile: Option<&str>,
        basis_window: Option<u64>,
        now: &str,
    ) -> Result<DeskParallelism, StoreError> {
        if !(1..=MAX_PARALLEL).contains(&max_parallel) {
            return Err(StoreError::InvalidInput(format!(
                "max_parallel must be 1..={MAX_PARALLEL} (got {max_parallel})"
            )));
        }
        if !matches!(source, "auto" | "manual") {
            return Err(StoreError::InvalidInput(format!(
                "source must be auto or manual (got `{source}`)"
            )));
        }
        let now = normalize_ts(now)?;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agent_desk_settings (agent_id, max_parallel, source, basis_profile, basis_window, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(agent_id) DO UPDATE SET max_parallel = ?2, source = ?3, basis_profile = ?4,
                     basis_window = ?5, updated_at = ?6",
                params![
                    agent_id,
                    max_parallel as i64,
                    source,
                    basis_profile,
                    basis_window.map(|w| w as i64),
                    now
                ],
            )
        })?;
        Ok(DeskParallelism {
            max_parallel,
            source: source.to_string(),
            basis_profile: basis_profile.map(str::to_string),
            basis_window,
        })
    }

    /// Distinct work sessions the agent has live: those carrying a task in
    /// progress, and agent-managed sessions running now. Desks and Lead never count.
    pub fn count_active_slots(&self, agent_id: &str) -> Result<u32, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM (
                     SELECT COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) AS sid
                     FROM task_agents ta JOIN tasks t ON t.id = ta.task_id
                     WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL AND t.status = 'in_progress'
                     UNION
                     SELECT s.id FROM sessions s
                     WHERE s.agent_id = ?1 AND s.status = 'running' AND s.managed_by = 'agent'
                       AND s.kind NOT IN ('inbox', 'lead')
                 ) WHERE sid IS NOT NULL AND sid != '' AND sid NOT LIKE 'inbox-%' AND sid != 'lead'",
                params![agent_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u32)
        })
    }

    /// Granted claims still inside their window: slots held for a start.
    pub fn reserved_slots(&self, agent_id: &str, now: &str) -> Result<u32, StoreError> {
        let now = normalize_ts(now)?;
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM slot_claims WHERE agent_id = ?1 AND status = 'granted' AND expires_at > ?2",
                params![agent_id, now],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u32)
        })
    }

    /// One waiting claim per agent and purpose; asking again returns it.
    pub fn request_slot_claim(
        &self,
        agent_id: &str,
        requested_for: Option<&str>,
        now: &str,
    ) -> Result<SlotClaim, StoreError> {
        let now = normalize_ts(now)?;
        let requested_for = requested_for.map(str::trim).filter(|s| !s.is_empty());
        let existing = self.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {CLAIM_COLUMNS} FROM slot_claims WHERE agent_id = ?1 AND status IN ('waiting', 'granted')
                     AND requested_for IS ?2 ORDER BY created_at LIMIT 1"
                ),
                params![agent_id, requested_for],
                claim_row,
            )
            .optional()
        })?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let raw = uuid::Uuid::new_v4().simple().to_string();
        let id = format!("slot-{}", &raw[..10]);
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO slot_claims (id, agent_id, requested_for, status, created_at) VALUES (?1, ?2, ?3, 'waiting', ?4)",
                params![id, agent_id, requested_for, now],
            )
        })?;
        self.get_slot_claim(&id)?
            .ok_or_else(|| StoreError::InvalidInput("inserted claim vanished".into()))
    }

    pub fn get_slot_claim(&self, id: &str) -> Result<Option<SlotClaim>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!("SELECT {CLAIM_COLUMNS} FROM slot_claims WHERE id = ?1"),
                params![id],
                claim_row,
            )
            .optional()
        })
    }

    /// A grant not used in its window, and a wait older than a day, lapse.
    pub fn expire_stale_slot_claims(&self, now: &str) -> Result<usize, StoreError> {
        let now = normalize_ts(now)?;
        let wait_cutoff = shift(&now, -chrono::Duration::hours(SLOT_WAIT_HOURS))?;
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE slot_claims SET status = 'expired'
                 WHERE (status = 'granted' AND expires_at <= ?1)
                    OR (status = 'waiting' AND created_at <= ?2)",
                params![now, wait_cutoff],
            )
        })
    }

    /// Use the agent's oldest live grant for a start.
    pub fn consume_slot_claim(
        &self,
        agent_id: &str,
        now: &str,
    ) -> Result<Option<SlotClaim>, StoreError> {
        let now = normalize_ts(now)?;
        let claim = self.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {CLAIM_COLUMNS} FROM slot_claims WHERE agent_id = ?1 AND status = 'granted'
                     AND expires_at > ?2 ORDER BY granted_at, created_at LIMIT 1"
                ),
                params![agent_id, now],
                claim_row,
            )
            .optional()
        })?;
        let Some(claim) = claim else {
            return Ok(None);
        };
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE slot_claims SET status = 'consumed', consumed_at = ?2 WHERE id = ?1",
                params![claim.id, now],
            )
        })?;
        self.get_slot_claim(&claim.id)
    }

    pub fn agents_with_waiting_slot_claims(&self) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT agent_id FROM slot_claims WHERE status = 'waiting' ORDER BY agent_id",
            )?;
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Grant the oldest waiting claim when a slot is free, hold it for
    /// [`SLOT_GRANT_MINUTES`], and tell the desk with a `slot_free` event.
    pub fn release_slot_and_grant_next(
        &self,
        agent_id: &str,
        default_window: u64,
        now: &str,
    ) -> Result<Option<SlotClaim>, StoreError> {
        let now = normalize_ts(now)?;
        self.expire_stale_slot_claims(&now)?;
        let cap = self
            .get_desk_parallelism(agent_id, default_window)?
            .max_parallel;
        let used = self.count_active_slots(agent_id)? + self.reserved_slots(agent_id, &now)?;
        if used >= cap {
            return Ok(None);
        }
        let next = self.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {CLAIM_COLUMNS} FROM slot_claims WHERE agent_id = ?1 AND status = 'waiting'
                     ORDER BY created_at, rowid LIMIT 1"
                ),
                params![agent_id],
                claim_row,
            )
            .optional()
        })?;
        let Some(next) = next else {
            return Ok(None);
        };
        let expires = shift(&now, chrono::Duration::minutes(SLOT_GRANT_MINUTES))?;
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE slot_claims SET status = 'granted', granted_at = ?2, expires_at = ?3 WHERE id = ?1",
                params![next.id, now, expires],
            )
        })?;
        let purpose = next
            .requested_for
            .as_deref()
            .map(|p| format!(" for {p}"))
            .unwrap_or_default();
        self.enqueue_desk_event(
            &NewDeskEvent {
                agent_id: agent_id.to_string(),
                kind: "slot_free".into(),
                source_ref: Some(next.id.clone()),
                priority: SLOT_FREE_PRIORITY,
                summary: format!(
                    "slot free{purpose} ({}/{cap} used): task.start before {expires} or the hold lapses",
                    used + 1
                ),
                payload: Some(json!({
                    "claim_id": next.id,
                    "requested_for": next.requested_for,
                    "expires_at": expires,
                    "cap": cap,
                })),
                wake: true,
                ..Default::default()
            },
            &now,
        )?;
        self.get_slot_claim(&next.id)
    }

    /// Count one tool round; returns the total and the rounds since the last report.
    pub fn record_session_tool_round(
        &self,
        session_id: &str,
        agent_id: Option<&str>,
        now: &str,
    ) -> Result<(u64, u64), StoreError> {
        let now = normalize_ts(now)?;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO session_progress (session_id, agent_id, tool_rounds, updated_at) VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET tool_rounds = tool_rounds + 1,
                     agent_id = COALESCE(?2, agent_id), updated_at = ?3",
                params![session_id, agent_id, now],
            )?;
            conn.query_row(
                "SELECT tool_rounds, last_reported_round FROM session_progress WHERE session_id = ?1",
                params![session_id],
                |r| {
                    let rounds = r.get::<_, i64>(0)?.max(0) as u64;
                    let reported = r.get::<_, i64>(1)?.max(0) as u64;
                    Ok((rounds, rounds.saturating_sub(reported)))
                },
            )
        })
    }

    pub fn mark_session_turn_start(
        &self,
        session_id: &str,
        agent_id: Option<&str>,
        now: &str,
    ) -> Result<(), StoreError> {
        let now = normalize_ts(now)?;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO session_progress (session_id, agent_id, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET turn_start_round = tool_rounds,
                     agent_id = COALESCE(?2, agent_id), updated_at = ?3",
                params![session_id, agent_id, now],
            )
            .map(|_| ())
        })
    }

    /// Rounds since the current turn began; `None` for a session never counted.
    pub fn rounds_since_turn_start(&self, session_id: &str) -> Result<Option<u64>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT tool_rounds - turn_start_round FROM session_progress WHERE session_id = ?1",
                params![session_id],
                |r| r.get::<_, i64>(0).map(|n| n.max(0) as u64),
            )
            .optional()
        })
    }

    /// A session's delta update: a `progress` event for `desk_agent`, ordered
    /// after the earlier ones, and the report mark moved to the current round.
    pub fn record_session_progress_report(
        &self,
        session_id: &str,
        desk_agent: &str,
        delta_summary: &str,
        wake: bool,
        now: &str,
    ) -> Result<DeskEvent, StoreError> {
        let now = normalize_ts(now)?;
        let summary = delta_summary.trim();
        if summary.is_empty() {
            return Err(StoreError::InvalidInput("summary must not be empty".into()));
        }
        let (rounds, since) = self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO session_progress (session_id, updated_at) VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO NOTHING",
                params![session_id, now],
            )?;
            let pair = conn.query_row(
                "SELECT tool_rounds, last_reported_round FROM session_progress WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get::<_, i64>(0)?.max(0) as u64, r.get::<_, i64>(1)?.max(0) as u64)),
            )?;
            conn.execute(
                "UPDATE session_progress SET last_reported_round = tool_rounds, updated_at = ?2 WHERE session_id = ?1",
                params![session_id, now],
            )?;
            Ok(pair)
        })?;
        self.enqueue_desk_event(
            &NewDeskEvent {
                agent_id: desk_agent.to_string(),
                kind: "progress".into(),
                source_ref: Some(format!("session:{session_id}")),
                summary: format!("session:{session_id} r{rounds}: {summary}"),
                payload: Some(json!({
                    "session": session_id,
                    "round": rounds,
                    "since_round": since,
                })),
                wake,
                ..Default::default()
            },
            &now,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-11-26T10:00:00Z";

    fn at(minutes: i64) -> String {
        shift(T0, chrono::Duration::minutes(minutes)).unwrap()
    }

    fn running_session(store: &Store, id: &str, agent: &str) {
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO sessions (id, workspace_key, workspace, status, created_at, updated_at, agent_id, kind, managed_by)
                     VALUES (?1, 'wk', '/tmp/wk', 'running', ?3, ?3, ?2, 'conversation', 'agent')",
                    params![id, agent, T0],
                )
            })
            .unwrap();
    }

    fn stop_session(store: &Store, id: &str) {
        store
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE sessions SET status = 'idle' WHERE id = ?1",
                    params![id],
                )
            })
            .unwrap();
    }

    #[test]
    fn auto_cap_scales_with_the_window() {
        assert_eq!(auto_parallel_cap(1_000_000), 6);
        assert_eq!(auto_parallel_cap(200_000), 6);
        assert_eq!(auto_parallel_cap(128_000), 4);
        assert_eq!(auto_parallel_cap(64_000), 2);
        assert_eq!(auto_parallel_cap(0), 2);
    }

    #[test]
    fn parallelism_defaults_to_auto_and_holds_a_manual_cap() {
        let store = Store::open_in_memory().unwrap();
        let auto = store.get_desk_parallelism("a", 128_000).unwrap();
        assert_eq!((auto.max_parallel, auto.source.as_str()), (4, "auto"));
        store
            .set_desk_parallelism("a", 3, "manual", None, None, T0)
            .unwrap();
        assert_eq!(
            store
                .get_desk_parallelism("a", 200_000)
                .unwrap()
                .max_parallel,
            3
        );
        store
            .set_desk_parallelism("a", 4, "auto", Some("opus"), Some(128_000), T0)
            .unwrap();
        let back = store.get_desk_parallelism("a", 200_000).unwrap();
        assert_eq!(
            (back.max_parallel, back.basis_profile.as_deref()),
            (6, Some("opus"))
        );
        assert!(
            store
                .set_desk_parallelism("a", 7, "manual", None, None, T0)
                .is_err()
        );
        assert!(
            store
                .set_desk_parallelism("a", 0, "manual", None, None, T0)
                .is_err()
        );
    }

    #[test]
    fn slot_claims_are_granted_fifo_expire_and_enqueue_slot_free() {
        let store = Store::open_in_memory().unwrap();
        store
            .set_desk_parallelism("a", 2, "manual", None, None, T0)
            .unwrap();
        running_session(&store, "s1", "a");
        running_session(&store, "s2", "a");
        running_session(&store, "inbox-a", "a");
        assert_eq!(store.count_active_slots("a").unwrap(), 2);

        let first = store
            .request_slot_claim("a", Some("deploy"), &at(0))
            .unwrap();
        let again = store
            .request_slot_claim("a", Some("deploy"), &at(0))
            .unwrap();
        assert_eq!(first.id, again.id);
        let second = store.request_slot_claim("a", Some("docs"), &at(1)).unwrap();
        assert!(
            store
                .release_slot_and_grant_next("a", 0, &at(2))
                .unwrap()
                .is_none()
        );

        stop_session(&store, "s1");
        let granted = store
            .release_slot_and_grant_next("a", 0, &at(3))
            .unwrap()
            .unwrap();
        assert_eq!(granted.id, first.id);
        assert_eq!(granted.status, "granted");
        assert_eq!(granted.expires_at.as_deref(), Some(at(13).as_str()));
        assert!(
            store
                .release_slot_and_grant_next("a", 0, &at(4))
                .unwrap()
                .is_none(),
            "the grant holds the free slot"
        );
        let events = store.list_open_desk_events("a", 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "slot_free");
        assert!(events[0].wake);
        assert!(
            events[0].summary.contains("for deploy"),
            "{}",
            events[0].summary
        );

        assert_eq!(store.expire_stale_slot_claims(&at(14)).unwrap(), 1);
        assert_eq!(
            store.get_slot_claim(&first.id).unwrap().unwrap().status,
            "expired"
        );
        let next = store
            .release_slot_and_grant_next("a", 0, &at(15))
            .unwrap()
            .unwrap();
        assert_eq!(next.id, second.id);
        let used = store.consume_slot_claim("a", &at(16)).unwrap().unwrap();
        assert_eq!(
            (used.id.as_str(), used.status.as_str()),
            (second.id.as_str(), "consumed")
        );
        assert!(store.consume_slot_claim("a", &at(16)).unwrap().is_none());
        assert!(store.agents_with_waiting_slot_claims().unwrap().is_empty());
    }

    #[test]
    fn tool_rounds_count_since_the_report_and_the_turn() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.rounds_since_turn_start("s").unwrap(), None);
        store.mark_session_turn_start("s", Some("a"), T0).unwrap();
        let mut nudges = Vec::new();
        for _ in 0..85 {
            let (rounds, since) = store.record_session_tool_round("s", Some("a"), T0).unwrap();
            if since > 0 && since % PROGRESS_EVERY_ROUNDS == 0 {
                nudges.push(rounds);
            }
        }
        assert_eq!(nudges, vec![40, 80]);
        assert_eq!(store.rounds_since_turn_start("s").unwrap(), Some(85));

        let first = store
            .record_session_progress_report("s", "desk", "built the parser", false, &at(1))
            .unwrap();
        assert_eq!(first.kind, "progress");
        assert!(!first.wake);
        assert_eq!(first.payload["round"], json!(85));
        let (_, since) = store.record_session_tool_round("s", None, &at(2)).unwrap();
        assert_eq!(since, 1);
        let second = store
            .record_session_progress_report("s", "desk", "tests pass", true, &at(3))
            .unwrap();
        assert_eq!(second.payload["since_round"], json!(85));
        assert!(second.wake);
        let open = store.list_open_desk_events("desk", 10).unwrap();
        let ids: Vec<&str> = open.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![first.id.as_str(), second.id.as_str()],
            "delta reports stay ordered"
        );

        store.mark_session_turn_start("s", None, &at(4)).unwrap();
        assert_eq!(store.rounds_since_turn_start("s").unwrap(), Some(0));
    }
}
