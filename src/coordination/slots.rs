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
pub const SLOTS_PAUSED_PRIORITY: i64 = 60;
pub const PAUSE_SAFETY_NET_MINUTES: i64 = 10;
pub const RESTART_INTERRUPTED_PRIORITY: i64 = 40;

/// A live work session the desk could pause, with what protects it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseCandidate {
    pub session_id: String,
    pub task_ids: Vec<String>,
    pub priority: i64,
    pub last_progress_at: String,
    pub in_review: bool,
    pub waiting_for_input: bool,
}

/// The excess over `new_cap`, lowest priority and stalest progress first;
/// work in review or waiting on the user is never chosen.
pub fn select_sessions_to_pause(
    candidates: &[PauseCandidate],
    active: u32,
    new_cap: u32,
) -> Vec<PauseCandidate> {
    let excess = active.saturating_sub(new_cap) as usize;
    if excess == 0 {
        return Vec::new();
    }
    let mut eligible: Vec<&PauseCandidate> = candidates
        .iter()
        .filter(|c| !c.in_review && !c.waiting_for_input)
        .collect();
    eligible.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.last_progress_at.cmp(&b.last_progress_at))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    eligible.into_iter().take(excess).cloned().collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderChangeOutcome {
    pub parallelism: DeskParallelism,
    pub active: u32,
    pub paused: Vec<PauseCandidate>,
    pub event: Option<DeskEvent>,
}

/// An agent-managed work session a daemon restart stopped mid-task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartInterrupted {
    pub session_id: String,
    pub agent_id: String,
    pub task_ids: Vec<String>,
    pub restart_ordinal: i64,
}

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
    /// progress whose session is still active (not idle/stopped or user-owned),
    /// and agent-managed sessions running now. Desks and Lead never count.
    pub fn count_active_slots(&self, agent_id: &str) -> Result<u32, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM (
                     SELECT COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) AS sid
                     FROM task_agents ta
                     JOIN tasks t ON t.id = ta.task_id
                     LEFT JOIN sessions s ON s.id = COALESCE(NULLIF(ta.work_session_id, ''), t.session_id)
                     WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL AND t.status = 'in_progress'
                       AND (s.id IS NULL OR (s.managed_by = 'agent' AND s.status IN ('running', 'waiting_for_input')))
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

fn payload_strings(payload: &serde_json::Value, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl Store {
    /// The agent's live work sessions, one row each, with their tasks.
    pub fn pause_candidates(&self, agent_id: &str) -> Result<Vec<PauseCandidate>, StoreError> {
        type Row = (String, Option<String>, Option<String>, i64, String, String);
        let rows: Vec<Row> = self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sid, task_id, task_status, priority, progress, session_status FROM (
                     SELECT COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) AS sid, t.id AS task_id,
                            t.status AS task_status, t.priority AS priority,
                            COALESCE(t.last_progress_at, t.updated_at) AS progress,
                            COALESCE(s.status, '') AS session_status
                     FROM task_agents ta JOIN tasks t ON t.id = ta.task_id
                     LEFT JOIN sessions s ON s.id = COALESCE(NULLIF(ta.work_session_id, ''), t.session_id)
                     WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL AND t.status IN ('in_progress', 'in_review')
                     UNION ALL
                     SELECT s.id, NULL, NULL, 0, s.updated_at, s.status FROM sessions s
                     WHERE s.agent_id = ?1 AND s.status IN ('running', 'waiting_for_input') AND s.managed_by = 'agent'
                       AND s.kind NOT IN ('inbox', 'lead')
                 ) WHERE sid IS NOT NULL AND sid != '' AND sid NOT LIKE 'inbox-%' AND sid != 'lead'",
            )?;
            stmt.query_map(params![agent_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        let mut out: Vec<PauseCandidate> = Vec::new();
        for (sid, task_id, task_status, priority, progress, session_status) in rows {
            let index = match out.iter().position(|c| c.session_id == sid) {
                Some(i) => i,
                None => {
                    out.push(PauseCandidate {
                        session_id: sid,
                        task_ids: Vec::new(),
                        priority: i64::MIN,
                        last_progress_at: String::new(),
                        in_review: false,
                        waiting_for_input: false,
                    });
                    out.len() - 1
                }
            };
            let c = &mut out[index];
            if let Some(task) = task_id
                && !c.task_ids.contains(&task)
            {
                c.task_ids.push(task);
            }
            c.priority = c.priority.max(priority);
            if progress > c.last_progress_at {
                c.last_progress_at = progress;
            }
            c.in_review |= task_status.as_deref() == Some("in_review");
            c.waiting_for_input |= session_status == "waiting_for_input";
        }
        Ok(out)
    }

    /// A desk's provider changed: the cap returns to auto for the new window,
    /// and work over it is parked (tasks to blocked) behind a `slots_paused` event.
    pub fn apply_desk_provider_change(
        &self,
        agent_id: &str,
        from_profile: &str,
        to_profile: &str,
        window: u64,
        desk_only: bool,
        now: &str,
    ) -> Result<ProviderChangeOutcome, StoreError> {
        let now = normalize_ts(now)?;
        let cap = auto_parallel_cap(window);
        let parallelism = self.set_desk_parallelism(
            agent_id,
            cap,
            "auto",
            Some(to_profile).filter(|p| !p.is_empty()),
            Some(window),
            &now,
        )?;
        let active = self.count_active_slots(agent_id)?;
        let paused = if active > cap {
            select_sessions_to_pause(&self.pause_candidates(agent_id)?, active, cap)
        } else {
            Vec::new()
        };
        if paused.is_empty() {
            return Ok(ProviderChangeOutcome {
                parallelism,
                active,
                paused,
                event: None,
            });
        }
        let task_ids: Vec<String> = paused.iter().flat_map(|c| c.task_ids.clone()).collect();
        let session_ids: Vec<String> = paused.iter().map(|c| c.session_id.clone()).collect();
        for task in &task_ids {
            self.update_task_in(task, &now, |t| {
                if t.status == super::tasks::TaskStatus::InProgress {
                    t.status = super::tasks::TaskStatus::Blocked;
                }
            })?;
        }
        let reasoning = format!(
            "provider_change: {from_profile} -> {to_profile}, desk cap now {cap}; this session was paused to fit it"
        );
        for session in &session_ids {
            let event = crate::harness::HarnessEvent::SystemDecision {
                step: "slot_paused".into(),
                reasoning: reasoning.clone(),
            };
            let _ = self.append_conversation_events(session, &[event], &now);
        }
        let list = |ids: &[String]| {
            if ids.is_empty() {
                "none".to_string()
            } else {
                ids.join(",")
            }
        };
        let event = self.enqueue_desk_event(
            &NewDeskEvent {
                agent_id: agent_id.to_string(),
                kind: "slots_paused".into(),
                source_ref: Some(format!("provider_change:{now}")),
                priority: SLOTS_PAUSED_PRIORITY,
                summary: format!(
                    "provider {from_profile} -> {to_profile}: cap now {cap} ({active} active), paused tasks {} (sessions {}). Ack and requeue each with slot.claim or event.create; unacked after {PAUSE_SAFETY_NET_MINUTES}m the daemon claims slots for them.",
                    list(&task_ids),
                    list(&session_ids)
                ),
                payload: Some(json!({
                    "reason": "provider_change",
                    "from": from_profile,
                    "to": to_profile,
                    "cap": cap,
                    "active": active,
                    "desk_only": desk_only,
                    "task_ids": task_ids,
                    "session_ids": session_ids,
                })),
                wake: true,
                ..Default::default()
            },
            &now,
        )?;
        Ok(ProviderChangeOutcome {
            parallelism,
            active,
            paused,
            event: Some(event),
        })
    }

    /// A `slots_paused` event left unacked past [`PAUSE_SAFETY_NET_MINUTES`]
    /// gets a slot claim per paused task (or bare session), once.
    pub fn run_slots_paused_safety_net(&self, now: &str) -> Result<Vec<SlotClaim>, StoreError> {
        let now = normalize_ts(now)?;
        let cutoff = shift(&now, -chrono::Duration::minutes(PAUSE_SAFETY_NET_MINUTES))?;
        let stale: Vec<DeskEvent> = self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id FROM desk_inbox WHERE kind = 'slots_paused' AND resolved_at IS NULL
                 AND created_at <= ?1 AND json_extract(payload_json, '$.safety_net_at') IS NULL
                 ORDER BY created_at",
            )?;
            stmt.query_map(params![cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })?
        .into_iter()
        .filter_map(|(id, agent)| self.get_desk_event(&agent, &id).ok().flatten())
        .collect();
        let mut claims = Vec::new();
        for event in stale {
            let tasks = payload_strings(&event.payload, "task_ids");
            let mut purposes: Vec<String> = tasks.iter().map(|t| format!("task:{t}")).collect();
            if purposes.is_empty() {
                purposes = payload_strings(&event.payload, "session_ids")
                    .iter()
                    .map(|s| format!("session:{s}"))
                    .collect();
            }
            let mut ids = Vec::new();
            for purpose in purposes {
                let claim = self.request_slot_claim(&event.agent_id, Some(&purpose), &now)?;
                ids.push(claim.id.clone());
                claims.push(claim);
            }
            let mut payload = match event.payload.clone() {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            payload.insert("safety_net_at".into(), json!(now));
            payload.insert("safety_net_claims".into(), json!(ids));
            self.with_connection(|conn| {
                conn.execute(
                    "UPDATE desk_inbox SET payload_json = ?2, updated_at = ?3 WHERE id = ?1",
                    params![
                        event.id,
                        serde_json::Value::Object(payload).to_string(),
                        now
                    ],
                )
            })?;
        }
        Ok(claims)
    }

    /// Desk inbox sessions that were running when the daemon restarted.
    pub fn restart_interrupted_inboxes(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT s.id, COALESCE(NULLIF(s.agent_id, ''), SUBSTR(s.id, 7))
                 FROM sessions s
                 LEFT JOIN session_events e ON e.session_id = s.id
                  AND e.ordinal = (SELECT MAX(ordinal) FROM session_events WHERE session_id = s.id)
                 WHERE s.id LIKE 'inbox-%'
                   AND (s.status = 'running'
                        OR (json_extract(e.payload_json, '$.kind') = 'system_decision'
                            AND json_extract(e.payload_json, '$.step') = 'daemon_restart'))
                 ORDER BY s.id",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
    }

    /// Agent-managed work sessions whose last event is a `daemon_restart`
    /// notice and that still hold a task in progress.
    pub fn restart_interrupted_sessions(&self) -> Result<Vec<RestartInterrupted>, StoreError> {
        let stopped: Vec<(String, String, i64)> = self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.agent_id, e.ordinal FROM sessions s
                 JOIN session_events e ON e.session_id = s.id
                  AND e.ordinal = (SELECT MAX(ordinal) FROM session_events WHERE session_id = s.id)
                 WHERE s.managed_by = 'agent' AND s.agent_id IS NOT NULL AND s.agent_id != ''
                   AND s.kind NOT IN ('inbox', 'lead') AND s.id NOT LIKE 'inbox-%' AND s.id != 'lead'
                   AND json_extract(e.payload_json, '$.kind') = 'system_decision'
                   AND json_extract(e.payload_json, '$.step') = 'daemon_restart'
                 ORDER BY s.id",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })?;
        let mut out = Vec::new();
        for (session_id, agent_id, restart_ordinal) in stopped {
            let task_ids: Vec<String> = self.with_connection(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT t.id FROM tasks t
                     LEFT JOIN task_agents ta ON ta.task_id = t.id AND ta.removed_at IS NULL
                     WHERE t.status = 'in_progress' AND (t.session_id = ?1 OR ta.work_session_id = ?1)
                     ORDER BY t.id",
                )?;
                stmt.query_map(params![session_id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?;
            if !task_ids.is_empty() {
                out.push(RestartInterrupted {
                    session_id,
                    agent_id,
                    task_ids,
                    restart_ordinal,
                });
            }
        }
        Ok(out)
    }

    /// One `restart_interrupted` event per session and restart; `None` when
    /// this restart was already reported.
    pub fn enqueue_restart_interrupted(
        &self,
        hit: &RestartInterrupted,
        now: &str,
    ) -> Result<Option<DeskEvent>, StoreError> {
        let source = format!("restart:{}:{}", hit.session_id, hit.restart_ordinal);
        let seen = self.with_connection(|conn| {
            conn.query_row(
                "SELECT 1 FROM desk_inbox WHERE agent_id = ?1 AND source_ref = ?2 LIMIT 1",
                params![hit.agent_id, source],
                |_| Ok(()),
            )
            .optional()
        })?;
        if seen.is_some() {
            return Ok(None);
        }
        let tasks = hit.task_ids.join(",");
        self.enqueue_desk_event(
            &NewDeskEvent {
                agent_id: hit.agent_id.clone(),
                kind: "restart_interrupted".into(),
                source_ref: Some(source),
                priority: RESTART_INTERRUPTED_PRIORITY,
                summary: format!(
                    "session:{} was stopped by a Snippet restart mid-turn with tasks {tasks} in progress. Resume it (dm the session to continue) or re-dispatch the tasks.",
                    hit.session_id
                ),
                payload: Some(json!({
                    "session": hit.session_id,
                    "task_ids": hit.task_ids,
                    "restart_ordinal": hit.restart_ordinal,
                })),
                wake: true,
                ..Default::default()
            },
            now,
        )
        .map(Some)
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

    fn session(store: &Store, id: &str, agent: &str, status: &str, managed_by: &str) {
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO sessions (id, workspace_key, workspace, status, created_at, updated_at, agent_id, kind, managed_by)
                     VALUES (?1, 'wk', '/tmp/wk', ?3, ?4, ?4, ?2, 'conversation', ?5)",
                    params![id, agent, status, T0, managed_by],
                )
            })
            .unwrap();
    }

    fn agent(store: &Store, id: &str) {
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO agents (id, display_name, handle, kind, status, role, capabilities_json, created_at, updated_at)
                     VALUES (?1, ?1, ?1, 'agent', 'active', 'worker', '[]', ?2, ?2)",
                    params![id, T0],
                )
            })
            .unwrap();
    }

    fn task(
        store: &Store,
        id: &str,
        agent: &str,
        session: &str,
        status: &str,
        priority: i64,
        progress: &str,
    ) {
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO tasks (id, title, description, status, priority, created_by_kind, created_by_id,
                         created_at, updated_at, thread_id, session_id, last_progress_at)
                     VALUES (?1, ?1, '', ?2, ?3, 'agent', 'desk', ?4, ?4, ?5, ?6, ?7)",
                    params![id, status, priority, T0, format!("task:{id}"), session, progress],
                )?;
                conn.execute(
                    "INSERT INTO task_agents (task_id, agent_id, work_session_id, added_at) VALUES (?1, ?2, ?3, ?4)",
                    params![id, agent, session, T0],
                )
            })
            .unwrap();
    }

    fn task_status(store: &Store, id: &str) -> String {
        store
            .with_connection(|conn| {
                conn.query_row("SELECT status FROM tasks WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })
            })
            .unwrap()
    }

    fn candidate(id: &str, priority: i64, progress: &str) -> PauseCandidate {
        PauseCandidate {
            session_id: id.into(),
            task_ids: vec![format!("t-{id}")],
            priority,
            last_progress_at: progress.into(),
            in_review: false,
            waiting_for_input: false,
        }
    }

    #[test]
    fn pause_selection_takes_the_excess_lowest_priority_and_stalest_first() {
        let review = PauseCandidate {
            in_review: true,
            ..candidate("review", -5, &at(0))
        };
        let waiting = PauseCandidate {
            waiting_for_input: true,
            ..candidate("waiting", -5, &at(0))
        };
        let all = vec![
            candidate("high", 5, &at(0)),
            candidate("low-fresh", 0, &at(9)),
            candidate("low-stale", 0, &at(1)),
            review,
            waiting,
            candidate("mid", 2, &at(0)),
        ];
        let ids = |picked: Vec<PauseCandidate>| -> Vec<String> {
            picked.into_iter().map(|c| c.session_id).collect()
        };
        assert_eq!(
            ids(select_sessions_to_pause(&all, 6, 3)),
            vec!["low-stale", "low-fresh", "mid"]
        );
        assert!(select_sessions_to_pause(&all, 3, 3).is_empty());
        assert!(select_sessions_to_pause(&all, 2, 3).is_empty());
        assert_eq!(
            ids(select_sessions_to_pause(&all, 6, 0)),
            vec!["low-stale", "low-fresh", "mid", "high"],
            "review and waiting work is never paused"
        );
    }

    fn crowded_desk(store: &Store) {
        agent(store, "a");
        for (sid, prio, minute) in [("s1", 3, 0), ("s2", 0, 5), ("s3", 0, 1), ("s4", 1, 0)] {
            session(store, sid, "a", "running", "agent");
            task(
                store,
                &format!("t{}", &sid[1..]),
                "a",
                sid,
                "in_progress",
                prio,
                &at(minute),
            );
        }
        session(store, "s5", "a", "waiting_for_input", "agent");
        task(store, "t5", "a", "s5", "in_progress", -9, &at(0));
        session(store, "s6", "a", "idle", "agent");
        task(store, "t6", "a", "s6", "in_review", -9, &at(0));
    }

    #[test]
    fn provider_change_resets_to_auto_and_pauses_the_excess() {
        let store = Store::open_in_memory().unwrap();
        crowded_desk(&store);
        store
            .set_desk_parallelism("a", 6, "manual", Some("big"), Some(1_000_000), T0)
            .unwrap();
        assert_eq!(store.count_active_slots("a").unwrap(), 5);

        let calm = store
            .apply_desk_provider_change("a", "big", "huge", 1_000_000, true, T0)
            .unwrap();
        assert_eq!(calm.parallelism.source, "auto");
        assert_eq!(calm.parallelism.max_parallel, 6);
        assert!(calm.paused.is_empty() && calm.event.is_none());
        assert!(store.list_open_desk_events("a", 10).unwrap().is_empty());

        let out = store
            .apply_desk_provider_change("a", "huge", "small", 64_000, true, &at(1))
            .unwrap();
        assert_eq!(
            (
                out.parallelism.max_parallel,
                out.parallelism.source.as_str(),
                out.parallelism.basis_profile.as_deref(),
                out.parallelism.basis_window
            ),
            (2, "auto", Some("small"), Some(64_000))
        );
        assert_eq!(out.active, 5);
        let paused: Vec<&str> = out.paused.iter().map(|c| c.session_id.as_str()).collect();
        assert_eq!(paused, vec!["s3", "s2", "s4"]);
        for (t, want) in [
            ("t1", "in_progress"),
            ("t2", "blocked"),
            ("t3", "blocked"),
            ("t4", "blocked"),
            ("t5", "in_progress"),
            ("t6", "in_review"),
        ] {
            assert_eq!(task_status(&store, t), want, "{t}");
        }
        let event = out.event.unwrap();
        assert_eq!(
            (event.kind.as_str(), event.priority, event.wake),
            ("slots_paused", 60, true)
        );
        assert_eq!(event.payload["task_ids"], json!(["t3", "t2", "t4"]));
        assert_eq!(event.payload["cap"], json!(2));
        assert!(event.summary.contains("huge -> small"), "{}", event.summary);
        let stored = store.get_desk_parallelism("a", 64_000).unwrap();
        assert_eq!(stored.source, "auto");
        let notes = store.load_conversation_events("s2").unwrap();
        assert!(matches!(
            notes.last(),
            Some(crate::harness::HarnessEvent::SystemDecision { step, .. }) if step == "slot_paused"
        ));
    }

    #[test]
    fn unacked_pause_gets_slot_claims_once_after_ten_minutes() {
        let store = Store::open_in_memory().unwrap();
        crowded_desk(&store);
        let out = store
            .apply_desk_provider_change("a", "huge", "small", 64_000, true, T0)
            .unwrap();
        let event = out.event.unwrap();
        assert!(
            store
                .run_slots_paused_safety_net(&at(9))
                .unwrap()
                .is_empty()
        );
        let claims = store.run_slots_paused_safety_net(&at(10)).unwrap();
        let purposes: Vec<&str> = claims
            .iter()
            .filter_map(|c| c.requested_for.as_deref())
            .collect();
        assert_eq!(purposes, vec!["task:t3", "task:t2", "task:t4"]);
        assert!(claims.iter().all(|c| c.status == "waiting"));
        assert!(
            store
                .run_slots_paused_safety_net(&at(11))
                .unwrap()
                .is_empty(),
            "fires once"
        );
        let settled = store.get_desk_event("a", &event.id).unwrap().unwrap();
        assert_eq!(
            settled.payload["safety_net_claims"]
                .as_array()
                .map(Vec::len),
            Some(3)
        );

        let other = Store::open_in_memory().unwrap();
        crowded_desk(&other);
        let paused = other
            .apply_desk_provider_change("a", "huge", "small", 64_000, true, T0)
            .unwrap()
            .event
            .unwrap();
        other
            .ack_desk_events("a", std::slice::from_ref(&paused.id), &at(1))
            .unwrap();
        assert!(
            other
                .run_slots_paused_safety_net(&at(30))
                .unwrap()
                .is_empty(),
            "acked pauses are left alone"
        );
    }

    fn restart_note(store: &Store, session: &str, now: &str) {
        let event = crate::harness::HarnessEvent::SystemDecision {
            step: "daemon_restart".into(),
            reasoning: "restarted".into(),
        };
        store
            .append_conversation_events(session, &[event], now)
            .unwrap();
    }

    #[test]
    fn restart_continuation_reports_interrupted_agent_work_once_per_restart() {
        let store = Store::open_in_memory().unwrap();
        agent(&store, "a");
        session(&store, "r1", "a", "idle", "agent");
        task(&store, "t1", "a", "r1", "in_progress", 0, T0);
        session(&store, "r2", "a", "idle", "human");
        task(&store, "t2", "a", "r2", "in_progress", 0, T0);
        session(&store, "r3", "a", "idle", "agent");
        task(&store, "t3", "a", "r3", "done", 0, T0);
        session(&store, "r4", "a", "idle", "agent");
        task(&store, "t4", "a", "r4", "in_progress", 0, T0);
        for s in ["r1", "r2", "r3", "r4"] {
            restart_note(&store, s, T0);
        }
        store
            .append_conversation_events(
                "r4",
                &[crate::harness::HarnessEvent::SystemDecision {
                    step: "other".into(),
                    reasoning: "moved on".into(),
                }],
                &at(1),
            )
            .unwrap();

        let hits = store.restart_interrupted_sessions().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            (hits[0].session_id.as_str(), hits[0].agent_id.as_str()),
            ("r1", "a")
        );
        assert_eq!(hits[0].task_ids, vec!["t1".to_string()]);

        let first = store
            .enqueue_restart_interrupted(&hits[0], &at(2))
            .unwrap()
            .unwrap();
        assert_eq!(
            (first.kind.as_str(), first.wake),
            ("restart_interrupted", true)
        );
        assert!(
            store
                .enqueue_restart_interrupted(&hits[0], &at(3))
                .unwrap()
                .is_none()
        );
        store
            .ack_desk_events("a", std::slice::from_ref(&first.id), &at(4))
            .unwrap();
        assert!(
            store
                .enqueue_restart_interrupted(&hits[0], &at(5))
                .unwrap()
                .is_none(),
            "acked stays deduped"
        );

        restart_note(&store, "r1", &at(6));
        let again = store.restart_interrupted_sessions().unwrap();
        assert!(
            store
                .enqueue_restart_interrupted(&again[0], &at(7))
                .unwrap()
                .is_some(),
            "a new restart reports again"
        );
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
