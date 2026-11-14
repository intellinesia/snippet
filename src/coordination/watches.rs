//! Agent watches: durable rows for the shell scripts the daemon runs in the
//! background on an owner's behalf. The runner lives in `crate::serve::watches`;
//! this module is the store half — rows, events, and the view the app reads.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Store, StoreError};

pub const MAX_WATCHES_PER_OWNER: usize = 8;
pub const MIN_POLL_INTERVAL_SECS: i64 = 60;
pub const MAX_POLL_INTERVAL_SECS: i64 = 86_400;
pub const DEFAULT_POLL_INTERVAL_SECS: i64 = 300;
pub const DEFAULT_MAX_WAKES_PER_DAY: i32 = 48;
pub const MAX_WAKES_PER_DAY_CEILING: i32 = 288;
/// `pending` holds at most this many bytes of held lines.
pub const PENDING_CAP_BYTES: usize = 16 * 1024;

/// One `agent_watches` row.
#[derive(Debug, Clone)]
pub struct Watch {
    pub id: String,
    pub owner: String,
    pub name: String,
    pub mode: String,
    pub script_path: String,
    pub interval_secs: i64,
    pub filter: Option<String>,
    pub max_wakes_per_day: i32,
    pub quiet_start: Option<String>,
    pub quiet_end: Option<String>,
    pub enabled: bool,
    pub status: String,
    pub last_error: Option<String>,
    pub last_run_at: Option<String>,
    pub last_fired_at: Option<String>,
    pub last_output: Option<String>,
    pub pending: Option<String>,
    pub consecutive_failures: i32,
    pub created_at: String,
    pub updated_at: String,
}

/// One `agent_watch_events` row, newest first.
#[derive(Debug, Clone)]
pub struct WatchEvent {
    pub kind: String,
    pub at: String,
    pub excerpt: Option<String>,
}

/// Fields `create_watch` needs; everything else starts empty.
#[derive(Debug, Clone)]
pub struct NewWatch {
    pub owner: String,
    pub name: String,
    pub mode: String,
    pub script_path: String,
    pub interval_secs: i64,
    pub filter: Option<String>,
    pub max_wakes_per_day: i32,
    pub quiet_start: Option<String>,
    pub quiet_end: Option<String>,
}

/// Partial update; `Some(None)` clears a nullable column.
#[derive(Debug, Clone, Default)]
pub struct WatchPatch {
    pub mode: Option<String>,
    pub interval_secs: Option<i64>,
    pub filter: Option<Option<String>>,
    pub max_wakes_per_day: Option<i32>,
    pub quiet_start: Option<Option<String>>,
    pub quiet_end: Option<Option<String>>,
    pub enabled: Option<bool>,
}

/// Runtime columns the runner owns; `None` leaves the column alone.
#[derive(Debug, Clone, Default)]
pub struct WatchRuntime {
    pub status: Option<String>,
    pub last_error: Option<Option<String>>,
    pub last_run_at: Option<String>,
    pub last_output: Option<String>,
    pub pending: Option<Option<String>>,
    pub consecutive_failures: Option<i32>,
    pub last_fired_at: Option<String>,
}

/// `[a-z0-9_-]{1,40}` — also what makes `<name>.sh` a safe file name.
pub fn valid_watch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

pub fn valid_mode(mode: &str) -> bool {
    matches!(mode, "poll" | "stream")
}

/// RFC3339 at one-second precision; lexicographic order matches time order.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn watch_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Watch> {
    Ok(Watch {
        id: row.get(0)?,
        owner: row.get(1)?,
        name: row.get(2)?,
        mode: row.get(3)?,
        script_path: row.get(4)?,
        interval_secs: row.get(5)?,
        filter: row.get(6)?,
        max_wakes_per_day: row.get(7)?,
        quiet_start: row.get(8)?,
        quiet_end: row.get(9)?,
        enabled: row.get::<_, i64>(10)? != 0,
        status: row.get(11)?,
        last_error: row.get(12)?,
        last_run_at: row.get(13)?,
        last_fired_at: row.get(14)?,
        last_output: row.get(15)?,
        pending: row.get(16)?,
        consecutive_failures: row.get(17)?,
        created_at: row.get(18)?,
        updated_at: row.get(19)?,
    })
}

const WATCH_COLUMNS: &str = "id, owner, name, mode, script_path, interval_secs, filter,
    max_wakes_per_day, quiet_start, quiet_end, enabled, status, last_error,
    last_run_at, last_fired_at, last_output, pending, consecutive_failures,
    created_at, updated_at";

fn invalid_input(message: String) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message)
}

fn watch_result<T>(result: Result<T, StoreError>) -> Result<T, StoreError> {
    result.map_err(|error| match error {
        StoreError::Sqlite(rusqlite::Error::InvalidParameterName(message)) => {
            StoreError::InvalidInput(message)
        }
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::InvalidInput("a watch with that name already exists".into())
        }
        other => other,
    })
}

fn validate_quiet(start: Option<&str>, end: Option<&str>) -> Result<(), String> {
    if start.is_some() != end.is_some() {
        return Err("quiet hours need both quiet_start and quiet_end".into());
    }
    for clock in [start, end].into_iter().flatten() {
        if !crate::mission_autonomy::valid_clock(clock) {
            return Err(format!("quiet hours need HH:MM, got `{clock}`"));
        }
    }
    Ok(())
}

/// The directory a watch's script and state live in: the agent home's
/// `watches/`, or Mission Control's own home for owner `mission-control`.
pub fn watches_dir(mission_control_root: &Path, owner: &str) -> PathBuf {
    if owner == crate::mission_control::SESSION_ID {
        return mission_control_root.join("watches");
    }
    super::AgentHome::new(super::agents_root(mission_control_root), owner)
        .map(|home| home.watches_path())
        .unwrap_or_else(|_| mission_control_root.join("watches"))
}

pub fn script_path(mission_control_root: &Path, owner: &str, name: &str) -> PathBuf {
    watches_dir(mission_control_root, owner).join(format!("{name}.sh"))
}

pub fn state_dir(mission_control_root: &Path, owner: &str, name: &str) -> PathBuf {
    watches_dir(mission_control_root, owner).join(format!("{name}.state"))
}

/// Remove the script and state dir; missing pieces are not an error.
pub fn remove_watch_files(mission_control_root: &Path, owner: &str, name: &str) {
    let _ = std::fs::remove_file(script_path(mission_control_root, owner, name));
    let _ = std::fs::remove_dir_all(state_dir(mission_control_root, owner, name));
}

impl Store {
    pub fn create_watch(&self, new: &NewWatch) -> Result<Watch, StoreError> {
        watch_result(self.with_connection(|conn| {
            if !valid_watch_name(&new.name) {
                return Err(invalid_input("name must match [a-z0-9_-]{1,40}".into()));
            }
            if !valid_mode(&new.mode) {
                return Err(invalid_input("mode must be poll or stream".into()));
            }
            if new.interval_secs <= 0 {
                return Err(invalid_input("interval_secs must be positive".into()));
            }
            if !(1..=MAX_WAKES_PER_DAY_CEILING).contains(&new.max_wakes_per_day) {
                return Err(invalid_input(format!(
                    "max_wakes_per_day must be 1 to {MAX_WAKES_PER_DAY_CEILING}"
                )));
            }
            validate_quiet(new.quiet_start.as_deref(), new.quiet_end.as_deref())
                .map_err(invalid_input)?;
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM agent_watches WHERE owner = ?1",
                params![new.owner],
                |row| row.get(0),
            )?;
            if count as usize >= MAX_WATCHES_PER_OWNER {
                return Err(invalid_input(format!(
                    "at most {MAX_WATCHES_PER_OWNER} watches per owner"
                )));
            }
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agent_watches WHERE owner = ?1 AND name = ?2)",
                params![new.owner, new.name],
                |row| row.get(0),
            )?;
            if exists {
                return Err(invalid_input(
                    "a watch with that name already exists".into(),
                ));
            }
            let id = uuid::Uuid::new_v4().to_string();
            let now = now_rfc3339();
            conn.execute(
                "INSERT INTO agent_watches
                 (id, owner, name, mode, script_path, interval_secs, filter,
                  max_wakes_per_day, quiet_start, quiet_end, enabled, status,
                  created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,'idle',?11,?11)",
                params![
                    id,
                    new.owner,
                    new.name,
                    new.mode,
                    new.script_path,
                    new.interval_secs,
                    new.filter,
                    new.max_wakes_per_day,
                    new.quiet_start,
                    new.quiet_end,
                    now,
                ],
            )?;
            self.watch_by_id_conn(conn, &id)
        }))
    }

    fn watch_by_id_conn(
        &self,
        conn: &rusqlite::Connection,
        id: &str,
    ) -> Result<Watch, rusqlite::Error> {
        conn.query_row(
            &format!("SELECT {WATCH_COLUMNS} FROM agent_watches WHERE id = ?1"),
            params![id],
            watch_from_row,
        )
    }

    pub fn watch_by_id(&self, id: &str) -> Result<Option<Watch>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!("SELECT {WATCH_COLUMNS} FROM agent_watches WHERE id = ?1"),
                params![id],
                watch_from_row,
            )
            .optional()
        })
    }

    pub fn get_watch(&self, owner: &str, name: &str) -> Result<Option<Watch>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {WATCH_COLUMNS} FROM agent_watches WHERE owner = ?1 AND name = ?2"
                ),
                params![owner, name],
                watch_from_row,
            )
            .optional()
        })
    }

    pub fn list_watches(&self, owner: Option<&str>) -> Result<Vec<Watch>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {WATCH_COLUMNS} FROM agent_watches
                 WHERE (?1 IS NULL OR owner = ?1) ORDER BY created_at, name"
            ))?;
            let rows = stmt.query_map(params![owner], watch_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
    }

    /// Partial update; bumps `updated_at` so the runner restarts the watch.
    pub fn update_watch(&self, id: &str, patch: &WatchPatch) -> Result<Option<Watch>, StoreError> {
        watch_result(self.with_connection(|conn| {
            let current = conn
                .query_row(
                    &format!("SELECT {WATCH_COLUMNS} FROM agent_watches WHERE id = ?1"),
                    params![id],
                    watch_from_row,
                )
                .optional()?;
            let Some(watch) = current else {
                return Ok(None);
            };
            let mode = patch.mode.clone().unwrap_or_else(|| watch.mode.clone());
            if !valid_mode(&mode) {
                return Err(invalid_input("mode must be poll or stream".into()));
            }
            let interval = patch.interval_secs.unwrap_or(watch.interval_secs);
            if interval <= 0 {
                return Err(invalid_input("interval_secs must be positive".into()));
            }
            let cap = patch.max_wakes_per_day.unwrap_or(watch.max_wakes_per_day);
            if !(1..=MAX_WAKES_PER_DAY_CEILING).contains(&cap) {
                return Err(invalid_input(format!(
                    "max_wakes_per_day must be 1 to {MAX_WAKES_PER_DAY_CEILING}"
                )));
            }
            let quiet_start = patch
                .quiet_start
                .clone()
                .unwrap_or_else(|| watch.quiet_start.clone());
            let quiet_end = patch
                .quiet_end
                .clone()
                .unwrap_or_else(|| watch.quiet_end.clone());
            validate_quiet(quiet_start.as_deref(), quiet_end.as_deref()).map_err(invalid_input)?;
            let now = now_rfc3339();
            conn.execute(
                "UPDATE agent_watches SET
                    mode = ?2, interval_secs = ?3, filter = ?4, max_wakes_per_day = ?5,
                    quiet_start = ?6, quiet_end = ?7, enabled = ?8, updated_at = ?9
                 WHERE id = ?1",
                params![
                    id,
                    mode,
                    interval,
                    patch.filter.clone().unwrap_or_else(|| watch.filter.clone()),
                    cap,
                    quiet_start,
                    quiet_end,
                    patch.enabled.unwrap_or(watch.enabled) as i64,
                    now,
                ],
            )?;
            self.watch_by_id_conn(conn, id).map(Some)
        }))
    }

    pub fn delete_watch(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute("DELETE FROM agent_watches WHERE id = ?1", params![id])? == 1)
        })
    }

    /// Runner-owned runtime columns; `None` fields stay untouched.
    /// Runtime state only; leaves `updated_at` alone, since the runner reads a
    /// changed `updated_at` as an edit and restarts the script.
    pub fn set_watch_runtime(&self, id: &str, runtime: &WatchRuntime) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let mut sets = Vec::new();
            let mut values: Vec<rusqlite::types::Value> = Vec::new();
            let mut bind = |column: &str, value: rusqlite::types::Value| {
                sets.push(format!("{column} = ?{}", values.len() + 1));
                values.push(value);
            };
            if let Some(status) = &runtime.status {
                bind("status", status.clone().into());
            }
            if let Some(last_error) = &runtime.last_error {
                bind(
                    "last_error",
                    last_error
                        .clone()
                        .map(Into::into)
                        .unwrap_or(rusqlite::types::Value::Null),
                );
            }
            if let Some(last_run_at) = &runtime.last_run_at {
                bind("last_run_at", last_run_at.clone().into());
            }
            if let Some(last_fired_at) = &runtime.last_fired_at {
                bind("last_fired_at", last_fired_at.clone().into());
            }
            if let Some(last_output) = &runtime.last_output {
                bind("last_output", last_output.clone().into());
            }
            if let Some(pending) = &runtime.pending {
                bind(
                    "pending",
                    pending
                        .clone()
                        .map(Into::into)
                        .unwrap_or(rusqlite::types::Value::Null),
                );
            }
            if let Some(failures) = runtime.consecutive_failures {
                bind("consecutive_failures", failures.into());
            }
            if sets.is_empty() {
                return Ok(());
            }
            let sql = format!(
                "UPDATE agent_watches SET {} WHERE id = ?{}",
                sets.join(", "),
                values.len() + 1
            );
            let mut stmt_values = values;
            stmt_values.push(id.to_string().into());
            conn.execute(&sql, rusqlite::params_from_iter(stmt_values))?;
            Ok(())
        })
    }

    pub fn record_watch_event(
        &self,
        watch_id: &str,
        kind: &str,
        excerpt: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO agent_watch_events (watch_id, kind, at, excerpt)
                 VALUES (?1, ?2, ?3, ?4)",
                params![watch_id, kind, now_rfc3339(), excerpt],
            )?;
            Ok(())
        })
    }

    pub fn list_watch_events(
        &self,
        watch_id: &str,
        limit: u32,
    ) -> Result<Vec<WatchEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, at, excerpt FROM agent_watch_events
                 WHERE watch_id = ?1 ORDER BY at DESC, id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![watch_id, limit.clamp(1, 200) as i64], |row| {
                Ok(WatchEvent {
                    kind: row.get(0)?,
                    at: row.get(1)?,
                    excerpt: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
    }

    /// 'fired' events at or after `since` (same RFC3339 clock as `now_rfc3339`).
    pub fn count_watch_fires_since(&self, watch_id: &str, since: &str) -> Result<i64, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM agent_watch_events
                 WHERE watch_id = ?1 AND kind = 'fired' AND at >= ?2",
                params![watch_id, since],
                |row| row.get(0),
            )
        })
    }

    /// The view the app and the `watch` tool share.
    pub fn watch_view(&self, watch: &Watch) -> Result<Value, StoreError> {
        let day_ago = (chrono::Utc::now() - chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let fires_last_day = self.count_watch_fires_since(&watch.id, &day_ago)?;
        Ok(json!({
            "id": watch.id,
            "owner": watch.owner,
            "name": watch.name,
            "mode": watch.mode,
            "interval_minutes": watch.interval_secs / 60,
            "filter": watch.filter,
            "max_wakes_per_day": watch.max_wakes_per_day,
            "quiet_start": watch.quiet_start,
            "quiet_end": watch.quiet_end,
            "enabled": watch.enabled,
            "status": watch.status,
            "last_error": watch.last_error,
            "last_run_at": watch.last_run_at,
            "last_fired_at": watch.last_fired_at,
            "fires_last_day": fires_last_day,
            "script_path": watch.script_path,
            "created_at": watch.created_at,
            "updated_at": watch.updated_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    fn new_watch(owner: &str, name: &str) -> NewWatch {
        NewWatch {
            owner: owner.into(),
            name: name.into(),
            mode: "poll".into(),
            script_path: "/tmp/x.sh".into(),
            interval_secs: 300,
            filter: None,
            max_wakes_per_day: 48,
            quiet_start: None,
            quiet_end: None,
        }
    }

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
    fn create_list_update_delete_round_trip() {
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&worker("a")).unwrap();
        let watch = db.create_watch(&new_watch("a", "inbox")).unwrap();
        assert_eq!(watch.status, "idle");
        assert!(watch.enabled);

        let listed = db.list_watches(Some("a")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "inbox");
        assert!(db.list_watches(Some("b")).unwrap().is_empty());
        assert_eq!(db.list_watches(None).unwrap().len(), 1);
        assert!(db.get_watch("a", "inbox").unwrap().is_some());
        assert!(db.get_watch("a", "other").unwrap().is_none());

        let updated = db
            .update_watch(
                &watch.id,
                &WatchPatch {
                    enabled: Some(false),
                    filter: Some(Some("error".into())),
                    ..WatchPatch::default()
                },
            )
            .unwrap()
            .unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.filter.as_deref(), Some("error"));
        assert!(updated.updated_at >= watch.updated_at);

        assert!(db.delete_watch(&watch.id).unwrap());
        assert!(db.watch_by_id(&watch.id).unwrap().is_none());
        assert!(!db.delete_watch(&watch.id).unwrap());
    }

    #[test]
    fn ninth_watch_is_refused_and_names_are_unique_per_owner() {
        let db = Store::open_in_memory().unwrap();
        for i in 0..MAX_WATCHES_PER_OWNER {
            db.create_watch(&new_watch("a", &format!("w{i}"))).unwrap();
        }
        let err = db.create_watch(&new_watch("a", "w9")).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
        // A different owner has its own budget and namespace.
        db.create_watch(&new_watch("b", "w0")).unwrap();
        let dup = db.create_watch(&new_watch("a", "w0")).unwrap_err();
        assert!(matches!(dup, StoreError::InvalidInput(_)), "{dup}");
    }

    #[test]
    fn runtime_and_events_persist() {
        let db = Store::open_in_memory().unwrap();
        let watch = db.create_watch(&new_watch("a", "inbox")).unwrap();
        db.with_connection(|c| {
            c.execute(
                "UPDATE agent_watches SET updated_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
                [&watch.id],
            )
        })
        .unwrap();
        db.set_watch_runtime(
            &watch.id,
            &WatchRuntime {
                status: Some("running".into()),
                last_run_at: Some(now_rfc3339()),
                last_output: Some("one\ntwo".into()),
                consecutive_failures: Some(0),
                ..WatchRuntime::default()
            },
        )
        .unwrap();
        let watch = db.watch_by_id(&watch.id).unwrap().unwrap();
        assert_eq!(watch.status, "running");
        assert_eq!(watch.last_output.as_deref(), Some("one\ntwo"));
        assert_eq!(watch.updated_at, "2000-01-01T00:00:00Z");

        db.record_watch_event(&watch.id, "fired", Some("hello"))
            .unwrap();
        db.record_watch_event(&watch.id, "held", None).unwrap();
        let events = db.list_watch_events(&watch.id, 20).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "held");
        assert_eq!(
            db.count_watch_fires_since(&watch.id, "2000-01-01T00:00:00Z")
                .unwrap(),
            1
        );
        assert_eq!(
            db.count_watch_fires_since(&watch.id, "2999-01-01T00:00:00Z")
                .unwrap(),
            0
        );

        // Deleting the watch cascades its events.
        db.delete_watch(&watch.id).unwrap();
        assert!(db.list_watch_events(&watch.id, 20).unwrap().is_empty());
    }

    #[test]
    fn patch_validates_quiet_hours_pair() {
        let db = Store::open_in_memory().unwrap();
        let watch = db.create_watch(&new_watch("a", "inbox")).unwrap();
        let err = db
            .update_watch(
                &watch.id,
                &WatchPatch {
                    quiet_start: Some(Some("22:00".into())),
                    ..WatchPatch::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
        let ok = db
            .update_watch(
                &watch.id,
                &WatchPatch {
                    quiet_start: Some(Some("22:00".into())),
                    quiet_end: Some(Some("08:00".into())),
                    ..WatchPatch::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(ok.quiet_start.as_deref(), Some("22:00"));
    }

    #[test]
    fn invalid_names_and_modes_are_refused() {
        assert!(valid_watch_name("inbox-1_x"));
        assert!(!valid_watch_name("Bad Name"));
        assert!(!valid_watch_name(""));
        assert!(!valid_watch_name(&"a".repeat(41)));
        let db = Store::open_in_memory().unwrap();
        let mut bad = new_watch("a", "ok");
        bad.name = "NOT OK".into();
        assert!(db.create_watch(&bad).is_err());
        let mut bad = new_watch("a", "ok");
        bad.mode = "webhook".into();
        assert!(db.create_watch(&bad).is_err());
    }
}
