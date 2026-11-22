//! Notices a desk was told about and is expected to pass on to the user. When
//! the desk stays silent past `due_at`, `serve::direct::desk_fallback_loop`
//! posts the notice itself, labelled as automatic.

use rusqlite::params;

use super::{Store, StoreError};

#[derive(Debug, Clone, PartialEq)]
pub struct DeskFallback {
    pub key: String,
    pub desk: String,
    pub session_id: String,
    /// `approval` or `question`.
    pub what: String,
    pub event_index: usize,
    pub body: String,
    pub extra: serde_json::Map<String, serde_json::Value>,
    pub created_at: String,
    pub due_at: String,
}

const COLUMNS: &str =
    "key, desk, session_id, what, event_index, body, extra_json, created_at, due_at";

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeskFallback> {
    let extra: String = row.get(6)?;
    Ok(DeskFallback {
        key: row.get(0)?,
        desk: row.get(1)?,
        session_id: row.get(2)?,
        what: row.get(3)?,
        event_index: row.get::<_, i64>(4)? as usize,
        body: row.get(5)?,
        extra: serde_json::from_str(&extra).unwrap_or_default(),
        created_at: row.get(7)?,
        due_at: row.get(8)?,
    })
}

impl Store {
    /// Idempotent per key: a re-persist of the same stop schedules nothing new.
    pub fn schedule_desk_fallback(&self, f: &DeskFallback) -> Result<(), StoreError> {
        let extra = serde_json::Value::Object(f.extra.clone()).to_string();
        self.with_connection(|conn| {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO desk_fallbacks ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
                ),
                params![
                    f.key,
                    f.desk,
                    f.session_id,
                    f.what,
                    f.event_index as i64,
                    f.body,
                    extra,
                    f.created_at,
                    f.due_at
                ],
            )
            .map(|_| ())
        })
    }

    pub fn due_desk_fallbacks(
        &self,
        now: &str,
        limit: u32,
    ) -> Result<Vec<DeskFallback>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM desk_fallbacks
                 WHERE done_at IS NULL AND due_at <= ?1 ORDER BY due_at LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![now, limit], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn finish_desk_fallback(&self, key: &str, now: &str) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE desk_fallbacks SET done_at = ?2 WHERE key = ?1",
                params![key, now],
            )
            .map(|_| ())
        })
    }
}
