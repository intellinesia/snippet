use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::harness::{PlanStatus, PlanStep};
use crate::store::{Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatPlan {
    pub session_id: String,
    pub agent_id: String,
    pub steps: Vec<PlanStep>,
    pub updated_at: String,
    pub last_change_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled_flagged_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatWake {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub due_at: String,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_in: Option<String>,
    pub created_at: String,
}

fn encode_steps(steps: &[PlanStep]) -> Result<String, StoreError> {
    serde_json::to_string(steps)
        .map_err(|e| StoreError::AppDecode(format!("encode seat plan: {e}")))
}

fn decode_steps(raw: &str) -> Vec<PlanStep> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn seat_plan_from_row(row: &rusqlite::Row<'_>) -> Result<SeatPlan, rusqlite::Error> {
    let steps_json: String = row.get(2)?;
    Ok(SeatPlan {
        session_id: row.get(0)?,
        agent_id: row.get(1)?,
        steps: decode_steps(&steps_json),
        updated_at: row.get(3)?,
        last_change_at: row.get(4)?,
        stalled_flagged_at: row.get(5)?,
    })
}

fn seat_wake_from_row(row: &rusqlite::Row<'_>) -> Result<SeatWake, rusqlite::Error> {
    Ok(SeatWake {
        id: row.get(0)?,
        session_id: row.get(1)?,
        agent_id: row.get(2)?,
        due_at: row.get(3)?,
        note: row.get(4)?,
        reply_in: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn rfc3339_utc(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

impl Store {
    pub fn save_plan(
        &self,
        session_id: &str,
        agent_id: &str,
        steps: &[PlanStep],
        now: &str,
    ) -> Result<(), StoreError> {
        let steps_json = encode_steps(steps)?;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let existing: Option<(String, String, Option<String>)> = tx
                .query_row(
                    "SELECT steps_json, last_change_at, stalled_flagged_at
                     FROM seat_plans WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let unchanged = existing
                .as_ref()
                .and_then(|(old_json, _, _)| serde_json::from_str::<Vec<PlanStep>>(old_json).ok())
                .is_some_and(|old_steps| old_steps.as_slice() == steps);
            let (last_change_at, stalled_flagged_at) = match existing {
                Some((_, prev_change, prev_flagged)) if unchanged => (prev_change, prev_flagged),
                _ => (now.to_string(), None),
            };
            tx.execute(
                "INSERT INTO seat_plans
                 (session_id, agent_id, steps_json, updated_at, last_change_at, stalled_flagged_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id) DO UPDATE SET
                     agent_id = excluded.agent_id,
                     steps_json = excluded.steps_json,
                     updated_at = excluded.updated_at,
                     last_change_at = excluded.last_change_at,
                     stalled_flagged_at = excluded.stalled_flagged_at",
                params![
                    session_id,
                    agent_id,
                    steps_json,
                    now,
                    last_change_at,
                    stalled_flagged_at
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn plan_for_session(&self, session_id: &str) -> Result<Option<SeatPlan>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT session_id, agent_id, steps_json, updated_at, last_change_at, stalled_flagged_at
                 FROM seat_plans WHERE session_id = ?1",
                params![session_id],
                seat_plan_from_row,
            )
            .optional()
        })
    }

    pub fn plans_for_agent(&self, agent_id: &str) -> Result<Vec<SeatPlan>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT session_id, agent_id, steps_json, updated_at, last_change_at, stalled_flagged_at
                 FROM seat_plans WHERE agent_id = ?1
                 ORDER BY updated_at DESC, session_id",
            )?;
            let rows = stmt.query_map(params![agent_id], seat_plan_from_row)?;
            rows.collect()
        })
    }

    /// Steps with `wake_at <= now` that are not done. Clears `wake_at` on each
    /// matched step so it fires once.
    pub fn due_wakes(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(String, String, PlanStep)>, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let rows: Vec<(String, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT session_id, agent_id, steps_json
                     FROM seat_plans
                     ORDER BY updated_at, session_id",
                )?;
                let mapped =
                    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                mapped.collect::<Result<Vec<_>, _>>()?
            };
            let mut due = Vec::new();
            for (session_id, agent_id, steps_json) in rows {
                let mut steps = decode_steps(&steps_json);
                let mut changed = false;
                for step in &mut steps {
                    if step.status == PlanStatus::Done {
                        continue;
                    }
                    let is_due = step
                        .wake_at
                        .as_deref()
                        .and_then(rfc3339_utc)
                        .is_some_and(|at| at <= now);
                    if is_due {
                        due.push((session_id.clone(), agent_id.clone(), step.clone()));
                        step.wake_at = None;
                        changed = true;
                    }
                }
                if changed {
                    let updated_json = serde_json::to_string(&steps)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    tx.execute(
                        "UPDATE seat_plans SET steps_json = ?2 WHERE session_id = ?1",
                        params![session_id, updated_json],
                    )?;
                }
            }
            tx.commit()?;
            Ok(due)
        })
    }

    pub fn steps_waiting_on(
        &self,
        task_or_lane_ref: &str,
    ) -> Result<Vec<(String, String, PlanStep)>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT session_id, agent_id, steps_json
                 FROM seat_plans
                 ORDER BY updated_at, session_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut matches = Vec::new();
            for row in rows {
                let (session_id, agent_id, steps_json) = row?;
                for step in decode_steps(&steps_json) {
                    if step.status != PlanStatus::Done
                        && step.waiting_on.as_deref() == Some(task_or_lane_ref)
                    {
                        matches.push((session_id.clone(), agent_id.clone(), step));
                    }
                }
            }
            Ok(matches)
        })
    }

    /// Plans with an `in_progress` step and `last_change_at` older than
    /// `threshold_secs`, not yet flagged for this quiet spell.
    pub fn stale_in_progress(
        &self,
        now: DateTime<Utc>,
        threshold_secs: i64,
    ) -> Result<Vec<(String, String, PlanStep)>, StoreError> {
        let stamp = now.to_rfc3339();
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let rows: Vec<(String, String, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT session_id, agent_id, steps_json, last_change_at
                     FROM seat_plans
                     WHERE stalled_flagged_at IS NULL
                     ORDER BY last_change_at, session_id",
                )?;
                let mapped = stmt.query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?;
                mapped.collect::<Result<Vec<_>, _>>()?
            };
            let mut stale = Vec::new();
            for (session_id, agent_id, steps_json, last_change_at) in rows {
                let Some(changed_at) = rfc3339_utc(&last_change_at) else {
                    continue;
                };
                if (now - changed_at).num_seconds() < threshold_secs {
                    continue;
                }
                let Some(step) = decode_steps(&steps_json)
                    .into_iter()
                    .find(|s| s.status == PlanStatus::InProgress)
                else {
                    continue;
                };
                tx.execute(
                    "UPDATE seat_plans SET stalled_flagged_at = ?2 WHERE session_id = ?1",
                    params![session_id, stamp],
                )?;
                stale.push((session_id, agent_id, step));
            }
            tx.commit()?;
            Ok(stale)
        })
    }

    pub fn schedule_seat_wake(
        &self,
        session_id: &str,
        agent_id: &str,
        due_at: &str,
        note: &str,
        reply_in: Option<&str>,
        now: &str,
    ) -> Result<SeatWake, StoreError> {
        let wake = SeatWake {
            id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            due_at: due_at.to_string(),
            note: note.to_string(),
            reply_in: reply_in.map(str::to_string),
            created_at: now.to_string(),
        };
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO seat_wakes (id, session_id, agent_id, due_at, note, reply_in, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    wake.id,
                    wake.session_id,
                    wake.agent_id,
                    wake.due_at,
                    wake.note,
                    wake.reply_in,
                    wake.created_at
                ],
            )?;
            Ok(())
        })?;
        Ok(wake)
    }

    pub fn take_due_seat_wakes(&self, now: DateTime<Utc>) -> Result<Vec<SeatWake>, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let rows: Vec<SeatWake> = {
                let mut stmt = tx.prepare(
                    "SELECT id, session_id, agent_id, due_at, note, reply_in, created_at
                     FROM seat_wakes
                     ORDER BY due_at, created_at, id",
                )?;
                let mapped = stmt.query_map([], seat_wake_from_row)?;
                mapped.collect::<Result<Vec<_>, _>>()?
            };
            let mut due = Vec::new();
            for wake in rows {
                if rfc3339_utc(&wake.due_at).is_some_and(|at| at <= now) {
                    tx.execute("DELETE FROM seat_wakes WHERE id = ?1", params![wake.id])?;
                    due.push(wake);
                }
            }
            tx.commit()?;
            Ok(due)
        })
    }

    pub fn list_seat_wakes(&self, session_id: Option<&str>) -> Result<Vec<SeatWake>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, agent_id, due_at, note, reply_in, created_at
                 FROM seat_wakes
                 WHERE (?1 IS NULL OR session_id = ?1)
                 ORDER BY due_at, created_at, id",
            )?;
            let rows = stmt.query_map(params![session_id], seat_wake_from_row)?;
            rows.collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str, status: PlanStatus) -> PlanStep {
        PlanStep {
            step: text.to_string(),
            status,
            reply_in: None,
            waiting_on: None,
            wake_at: None,
        }
    }

    #[test]
    fn save_plan_updates_last_change_at_only_on_change() {
        let store = Store::open_in_memory().unwrap();
        let steps = vec![step("investigate", PlanStatus::InProgress)];
        store
            .save_plan("sess-1", "agent-a", &steps, "2026-11-14T00:00:00Z")
            .unwrap();
        let first = store.plan_for_session("sess-1").unwrap().unwrap();
        assert_eq!(first.last_change_at, "2026-11-14T00:00:00Z");

        store
            .save_plan("sess-1", "agent-a", &steps, "2026-11-14T01:00:00Z")
            .unwrap();
        let same = store.plan_for_session("sess-1").unwrap().unwrap();
        assert_eq!(same.updated_at, "2026-11-14T01:00:00Z");
        assert_eq!(same.last_change_at, "2026-11-14T00:00:00Z");

        let changed_steps = vec![
            step("investigate", PlanStatus::Done),
            step("implement", PlanStatus::InProgress),
        ];
        store
            .save_plan("sess-1", "agent-a", &changed_steps, "2026-11-14T02:00:00Z")
            .unwrap();
        let changed = store.plan_for_session("sess-1").unwrap().unwrap();
        assert_eq!(changed.last_change_at, "2026-11-14T02:00:00Z");
        assert_eq!(store.plans_for_agent("agent-a").unwrap().len(), 1);
    }

    #[test]
    fn due_wakes_returns_due_step_once_and_skips_done() {
        let store = Store::open_in_memory().unwrap();
        let mut s1 = step("check deploy", PlanStatus::Pending);
        s1.wake_at = Some("2026-11-14T01:00:00Z".into());
        s1.reply_in = Some("direct:agent:a|human:local".into());

        let mut s2 = step("already done", PlanStatus::Done);
        s2.wake_at = Some("2026-11-14T00:30:00Z".into());

        let mut s3 = step("later", PlanStatus::Pending);
        s3.wake_at = Some("2026-11-14T05:00:00Z".into());

        store
            .save_plan("sess-1", "agent-a", &[s1, s2, s3], "2026-11-14T00:00:00Z")
            .unwrap();

        let now = rfc3339_utc("2026-11-14T02:00:00Z").unwrap();
        let first = store.due_wakes(now).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "sess-1");
        assert_eq!(first[0].1, "agent-a");
        assert_eq!(first[0].2.step, "check deploy");

        // Second call must not fire again because wake_at was cleared.
        assert!(store.due_wakes(now).unwrap().is_empty());
        let saved = store.plan_for_session("sess-1").unwrap().unwrap();
        assert_eq!(saved.steps[0].wake_at, None);
        assert_eq!(
            saved.steps[2].wake_at.as_deref(),
            Some("2026-11-14T05:00:00Z")
        );
        // Clearing wake_at preserves last_change_at.
        assert_eq!(saved.last_change_at, "2026-11-14T00:00:00Z");
    }

    #[test]
    fn steps_waiting_on_finds_step_by_task_ref() {
        let store = Store::open_in_memory().unwrap();
        let mut s1 = step("wait for build", PlanStatus::InProgress);
        s1.waiting_on = Some("task:t-42".into());
        let mut s2 = step("finished wait", PlanStatus::Done);
        s2.waiting_on = Some("task:t-42".into());
        let mut s3 = step("other task", PlanStatus::Pending);
        s3.waiting_on = Some("task:t-99".into());

        store
            .save_plan("sess-1", "agent-a", &[s1, s2], "2026-11-14T00:00:00Z")
            .unwrap();
        store
            .save_plan("sess-2", "agent-b", &[s3], "2026-11-14T00:00:00Z")
            .unwrap();

        let waiting = store.steps_waiting_on("task:t-42").unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].0, "sess-1");
        assert_eq!(waiting[0].2.step, "wait for build");
    }

    #[test]
    fn stale_in_progress_flags_once_per_quiet_spell() {
        let store = Store::open_in_memory().unwrap();
        let steps = vec![step("stuck step", PlanStatus::InProgress)];
        store
            .save_plan("sess-1", "agent-a", &steps, "2026-11-14T00:00:00Z")
            .unwrap();

        let t1 = rfc3339_utc("2026-11-14T01:30:00Z").unwrap();
        assert!(store.stale_in_progress(t1, 7200).unwrap().is_empty());

        let t2 = rfc3339_utc("2026-11-14T02:05:00Z").unwrap();
        let first = store.stale_in_progress(t2, 7200).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "sess-1");
        assert_eq!(first[0].2.step, "stuck step");

        // Same quiet spell: must not flag again.
        let t3 = rfc3339_utc("2026-11-14T03:00:00Z").unwrap();
        assert!(store.stale_in_progress(t3, 7200).unwrap().is_empty());

        // Saving identical steps keeps the stall flag; changing steps clears it.
        store
            .save_plan("sess-1", "agent-a", &steps, "2026-11-14T03:05:00Z")
            .unwrap();
        assert!(store.stale_in_progress(t3, 7200).unwrap().is_empty());

        let next_steps = vec![
            step("stuck step", PlanStatus::Done),
            step("new step", PlanStatus::InProgress),
        ];
        store
            .save_plan("sess-1", "agent-a", &next_steps, "2026-11-14T03:10:00Z")
            .unwrap();
        let t4 = rfc3339_utc("2026-11-14T05:15:00Z").unwrap();
        let second = store.stale_in_progress(t4, 7200).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].2.step, "new step");
    }
}
