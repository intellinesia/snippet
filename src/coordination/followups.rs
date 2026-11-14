//! One-shot self wakes for desks and work sessions: `schedule_wake` stores a
//! row, `serve::followups` delivers it when due.

use rusqlite::{OptionalExtension, params};

use super::{Store, StoreError};

pub const MAX_PENDING_PER_SESSION: i64 = 20;
pub const NOTE_MAX_CHARS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followup {
    pub id: String,
    pub session_id: String,
    pub owner: Option<String>,
    pub due_at: String,
    pub note: String,
    pub created_at: String,
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Followup> {
    Ok(Followup {
        id: row.get(0)?,
        session_id: row.get(1)?,
        owner: row.get(2)?,
        due_at: row.get(3)?,
        note: row.get(4)?,
        created_at: row.get(5)?,
    })
}

const COLUMNS: &str = "id, session_id, owner, due_at, note, created_at";

impl Store {
    pub fn schedule_followup(
        &self,
        session_id: &str,
        owner: Option<&str>,
        due_at: &str,
        note: &str,
        now: &str,
    ) -> Result<Followup, StoreError> {
        let note: String = note.trim().chars().take(NOTE_MAX_CHARS).collect();
        if note.is_empty() {
            return Err(StoreError::InvalidInput(
                "note must say what to do when the wake is due".into(),
            ));
        }
        let pending: i64 = self.with_connection(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM session_followups
                 WHERE session_id = ?1 AND delivered_at IS NULL",
                params![session_id],
                |r| r.get(0),
            )?)
        })?;
        if pending >= MAX_PENDING_PER_SESSION {
            return Err(StoreError::InvalidInput(format!(
                "at most {MAX_PENDING_PER_SESSION} pending wakes per session; cancel one first"
            )));
        }
        let followup = Followup {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            owner: owner.map(str::to_string),
            due_at: due_at.to_string(),
            note,
            created_at: now.to_string(),
        };
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO session_followups (id, session_id, owner, due_at, note, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    followup.id,
                    followup.session_id,
                    followup.owner,
                    followup.due_at,
                    followup.note,
                    followup.created_at
                ],
            )?;
            Ok(())
        })?;
        Ok(followup)
    }

    pub fn pending_followups(&self, session_id: &str) -> Result<Vec<Followup>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM session_followups
                 WHERE session_id = ?1 AND delivered_at IS NULL ORDER BY due_at"
            ))?;
            let rows = stmt
                .query_map(params![session_id], row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Drop a pending wake; only the session that set it may.
    pub fn cancel_followup(&self, session_id: &str, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "DELETE FROM session_followups
                 WHERE id = ?1 AND session_id = ?2 AND delivered_at IS NULL",
                params![id, session_id],
            )? > 0)
        })
    }

    pub fn due_followups(&self, now: &str, limit: u32) -> Result<Vec<Followup>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM session_followups
                 WHERE delivered_at IS NULL AND due_at <= ?1 ORDER BY due_at LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![now, limit], row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn mark_followup_delivered(
        &self,
        id: &str,
        at: &str,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE session_followups SET delivered_at = ?2, error = ?3 WHERE id = ?1",
                params![id, at, error],
            )?;
            Ok(())
        })
    }

    pub fn get_followup(&self, id: &str) -> Result<Option<Followup>, StoreError> {
        self.with_connection(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM session_followups WHERE id = ?1"),
                    params![id],
                    row,
                )
                .optional()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-11-14T10:00:00+00:00";
    const T1: &str = "2026-11-14T10:30:00+00:00";
    const T2: &str = "2026-11-14T11:00:00+00:00";

    #[test]
    fn a_followup_is_due_once_and_only_after_its_time() {
        let db = Store::open_in_memory().unwrap();
        let f = db
            .schedule_followup("inbox-a", Some("a"), T1, "check the deploy", T0)
            .unwrap();
        assert!(db.due_followups(T0, 10).unwrap().is_empty());
        assert_eq!(db.due_followups(T2, 10).unwrap(), vec![f.clone()]);
        db.mark_followup_delivered(&f.id, T2, None).unwrap();
        assert!(db.due_followups(T2, 10).unwrap().is_empty());
        assert!(db.pending_followups("inbox-a").unwrap().is_empty());
    }

    #[test]
    fn only_the_setting_session_can_cancel_and_notes_are_required() {
        let db = Store::open_in_memory().unwrap();
        assert!(db.schedule_followup("s", None, T1, "  ", T0).is_err());
        let f = db
            .schedule_followup("s", None, T1, "look again", T0)
            .unwrap();
        assert!(!db.cancel_followup("other", &f.id).unwrap());
        assert!(db.cancel_followup("s", &f.id).unwrap());
        assert!(db.get_followup(&f.id).unwrap().is_none());
    }

    #[test]
    fn pending_wakes_per_session_are_capped() {
        let db = Store::open_in_memory().unwrap();
        for _ in 0..MAX_PENDING_PER_SESSION {
            db.schedule_followup("s", None, T1, "x", T0).unwrap();
        }
        assert!(db.schedule_followup("s", None, T1, "x", T0).is_err());
        assert!(db.schedule_followup("t", None, T1, "x", T0).is_ok());
    }
}
