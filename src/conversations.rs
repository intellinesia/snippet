//! Durable conversations in the shared store.
//!
//! A conversation used to be one gzip'd MessagePack blob rewritten in full on
//! every step — so each persist re-serialized and recompressed the entire
//! transcript, and the cost grew with the history. Here a transcript is
//! **append-only rows**: adding a turn inserts one row, and nothing reads or
//! rewrites the messages it did not touch.
//!
//! The session's scalar state (status, goal, counters, lanes, checkpoints) is
//! kept as one JSON column on the `sessions` row. Those fields are small and
//! genuinely mutate together; splitting them into columns would add schema
//! churn for no read that needs it.


use rusqlite::{OptionalExtension, params};

use crate::harness::HarnessEvent;
use crate::llm::HarnessMessage;
use crate::store::{Store, StoreError};

static SESSION_CHANGES: std::sync::OnceLock<tokio::sync::watch::Sender<()>> = std::sync::OnceLock::new();

fn session_changes_tx() -> &'static tokio::sync::watch::Sender<()> {
    SESSION_CHANGES.get_or_init(|| tokio::sync::watch::channel(()).0)
}

/// Wakes `/attach` pushers when a write could change `conversation_fingerprint`.
pub fn session_changes() -> tokio::sync::watch::Receiver<()> {
    session_changes_tx().subscribe()
}

pub fn signal_session_change() {
    session_changes_tx().send_replace(());
}

struct SessionChangeSignal;

impl Drop for SessionChangeSignal {
    fn drop(&mut self) {
        signal_session_change();
    }
}

/// Schema for durable conversations.
///
/// The DDL lives with the data it describes. [`crate::store`] owns the
/// connection and calls each domain's `ensure`; keeping the session tables here
/// is what stops the store from reading like the coordination plane's private
/// database, which is exactly how the naming drifted.
///
/// Every statement is `IF NOT EXISTS`.
pub fn ensure_schema(connection: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY NOT NULL,
             workspace_key TEXT NOT NULL,
             workspace TEXT NOT NULL,
             title TEXT,
             status TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             tags_json TEXT NOT NULL DEFAULT '[]',
             -- The session's scalar state (status, goal, counters, lanes,
             -- checkpoints, queue), WITHOUT its two append-only logs. Those are
             -- rows here, so this column is small and its rewrite is cheap.
             state_json TEXT NOT NULL DEFAULT '{}',
             -- Last *user* activity, unix seconds. Drives the session list's
             -- order. Was a `.meta.json` sidecar, so a migrated session had no
             -- value and fell back to a file mtime that no longer exists.
             last_active INTEGER,
             -- Per-conversation model override (profile name). Was `.profile`.
             profile TEXT,
             -- 'mission_control' | 'standard'. Was `.role`.
             role TEXT NOT NULL DEFAULT 'standard',
             -- The agent working this session, if any. Was `.role`'s agent_id.
             agent_id TEXT,
             -- What kind of session this is: 'default' | 'conversation' |
             -- 'inbox' | 'mission_control'. This used to be parsed out of the
             -- id's suffix, which is the coupling opaque ids remove.
             kind TEXT NOT NULL DEFAULT 'conversation',
             -- Unix seconds when the session was settled (parked: not routed to,
             -- deleted 15 days later), or NULL while it is awake.
             settled_at INTEGER,
             -- 1 opts the session out of AUTOMATIC settling (manual settle still works).
             keep_awake INTEGER NOT NULL DEFAULT 0,
             -- JSON {files, added, removed} of the last turn that changed files, or NULL.
             last_changes TEXT,
             -- JSON {files, added, removed} summed over every turn since the session began.
             total_changes TEXT,
             -- Fresh token while the session is stopped idle; cleared by user input.
             stop_identity TEXT
         );
         CREATE INDEX IF NOT EXISTS sessions_workspace
             ON sessions(workspace_key, updated_at DESC);

         -- One row per message. A transcript is APPENDED to, never rewritten:
         -- the previous single-blob state file re-serialized and recompressed the
         -- whole conversation on every step, so its cost grew with history.
         CREATE TABLE IF NOT EXISTS session_messages (
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             role TEXT NOT NULL,
             payload_json TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (session_id, ordinal)
         );

         -- The display event log, append-only for the same reason as messages.
         CREATE TABLE IF NOT EXISTS session_events (
             session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             ordinal INTEGER NOT NULL,
             payload_json TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (session_id, ordinal)
         );

         -- Client request ids accepted by the daemon. Kept independently of a
         -- session row so pre-migration file sessions also get replay protection.
         CREATE TABLE IF NOT EXISTS request_nonces (
             session_id TEXT NOT NULL,
             nonce TEXT NOT NULL,
             accepted_at INTEGER NOT NULL,
             PRIMARY KEY (session_id, nonce)
         );"#,
    )?;
    connection.execute_batch(
        r#"CREATE TRIGGER IF NOT EXISTS session_stop_insert AFTER INSERT ON sessions
             WHEN NEW.status = 'idle'
             BEGIN UPDATE sessions SET stop_identity = lower(hex(randomblob(16))) WHERE id = NEW.id; END;
           CREATE TRIGGER IF NOT EXISTS session_stop_transition AFTER UPDATE OF status ON sessions
             WHEN OLD.status != NEW.status
             BEGIN UPDATE sessions SET stop_identity = CASE WHEN NEW.status = 'idle'
                 THEN lower(hex(randomblob(16))) ELSE NULL END WHERE id = NEW.id; END;
           CREATE TRIGGER IF NOT EXISTS session_stop_user_input AFTER INSERT ON session_messages
             WHEN NEW.role = 'user'
             BEGIN UPDATE sessions SET stop_identity = NULL WHERE id = NEW.session_id; END;"#,
    )?;
    Ok(())
}

/// A session id from its state path: the `.json` file suffix is dropped.
pub fn canonical_session_id(id: &str) -> String {
    id.strip_suffix(".json").unwrap_or(id).to_string()
}

/// A conversation's row: identity plus the scalar state, without its messages.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    /// Durable session id (the state path relative to the workspaces root, so
    /// existing references keep resolving).
    pub id: String,
    /// Stable key of the workspace folder (see `config::workspace_key`).
    pub workspace_key: String,
    /// Absolute folder the session runs in.
    pub workspace: String,
    pub title: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// Last *user* activity, unix seconds. Orders the session list. Was a
    /// `.meta.json` sidecar, so a session listing it from disk had to have that
    /// file; here the row is the only thing the list needs.
    pub last_active: Option<i64>,
    /// Per-conversation model override (profile name). Was `.profile`.
    pub profile: Option<String>,
    /// `mission_control` or `standard`. Was `.role`.
    pub role: String,
    /// The agent working this session. Was `.role`'s `agent_id`.
    pub agent_id: Option<String>,
    /// `default` | `conversation` | `inbox` | `mission_control`.
    pub kind: String,
    /// When the session was settled, unix seconds; `None` while awake.
    pub settled_at: Option<i64>,
    /// Opted out of automatic settling.
    pub keep_awake: bool,
    /// JSON totals of the last turn that changed files.
    pub last_changes: Option<String>,
    /// JSON lifetime totals over all turns that changed files.
    pub total_changes: Option<String>,
}

/// Session metadata that used to live in sidecar files.
///
/// Bundled so the importer can carry all of it in one argument rather than
/// growing a parameter list, and so "no sidecars present" is a `Default` rather
/// than a pile of `None`s at every call site.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionExtras {
    /// Last *user* activity, unix seconds. Was `.meta.json`.
    pub last_active: Option<i64>,
    /// Model override (profile name). Was `.profile`.
    pub profile: Option<String>,
    /// `mission_control` or `standard`. Was `.role`.
    pub role: Option<String>,
    /// The agent working this session. Was `.role`'s `agent_id`.
    pub agent_id: Option<String>,
}

/// A stored message, with its position so ordering survives a round trip.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub ordinal: i64,
    pub role: String,
    pub message: HarnessMessage,
    pub created_at: String,
}

/// The message-kind discriminator stored beside each payload.
///
/// Kept explicit rather than derived from the enum so the column can be indexed
/// and filtered in SQL without deserializing every payload.
fn role_of(message: &HarnessMessage) -> &'static str {
    match message {
        HarnessMessage::User { .. } => "user",
        HarnessMessage::Assistant { .. } => "assistant",
        HarnessMessage::ToolResult { .. } => "tool_result",
        HarnessMessage::Summary { .. } => "summary",
        HarnessMessage::System { .. } => "system",
    }
}

impl Store {
    /// Create a session with its initial transcript, in one transaction.
    ///
    /// The messages are written in the same transaction as the row so a session
    /// can never exist with a partially-written history.
    pub fn create_conversation(
        &self,
        session: &SessionRow,
        messages: &[HarnessMessage],
        tags_json: &str,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status,
                    created_at, updated_at, tags_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    session.id,
                    session.workspace_key,
                    session.workspace,
                    session.title,
                    session.status,
                    session.created_at,
                    session.updated_at,
                    tags_json,
                ],
            )?;
            for (ordinal, message) in messages.iter().enumerate() {
                insert_message(
                    &tx,
                    &session.id,
                    ordinal as i64,
                    message,
                    &session.updated_at,
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Append messages to the end of a transcript.
    ///
    /// This is the hot path: one INSERT per message, no read of what is already
    /// stored. `ordinal` continues from the current maximum, computed inside the
    /// same transaction so two writers cannot collide on a position.
    pub fn append_conversation_messages(
        &self,
        session_id: &str,
        messages: &[HarnessMessage],
        now: &str,
    ) -> Result<Vec<i64>, StoreError> {
        let _signal = SessionChangeSignal;
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM session_messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )?;
            let mut written = Vec::with_capacity(messages.len());
            for message in messages {
                insert_message(&tx, session_id, next, message, now)?;
                written.push(next);
                next += 1;
            }
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, now],
            )?;
            tx.commit()?;
            Ok(written)
        })
    }

    /// Replace the transcript from `from_ordinal` onward.
    ///
    /// Compaction is the one legitimate rewrite: it folds a span of history into
    /// a summary. Scoping the delete to `>= from_ordinal` keeps everything
    /// before the cut byte-for-byte, instead of rewriting the whole table.
    pub fn replace_conversation_tail(
        &self,
        session_id: &str,
        from_ordinal: i64,
        messages: &[HarnessMessage],
        now: &str,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM session_messages WHERE session_id = ?1 AND ordinal >= ?2",
                params![session_id, from_ordinal],
            )?;
            for (offset, message) in messages.iter().enumerate() {
                insert_message(&tx, session_id, from_ordinal + offset as i64, message, now)?;
            }
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, now],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Load a session's row, without its messages.
    /// A session by its id — or by the path-shaped id it used to have.
    ///
    /// THE lookup. Every reader funnels here, so accepting both forms in one
    /// place is what lets an id captured before the rewrite (in a message
    /// payload, a client cache, a log) still resolve to its session instead of
    /// silently matching nothing.
    pub fn get_session_row(&self, id: &str) -> Result<Option<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
            ))?;
            stmt.query_row(params![id], session_row_from).optional()
        })
    }

    /// Whether a session's transcript holds `needle` at or after `since`, optionally
    /// only in what the session itself asked for rather than what it was shown.
    pub fn session_mentions(&self, session_id: &str, since: &str, needle: &str, calls_only: bool) -> Result<bool, StoreError> {
        let encoded = serde_json::to_string(needle).unwrap_or_default();
        let encoded = encoded.trim_matches('"').to_string();
        let row = self.get_session_row(session_id)?;
        let id = row.map_or_else(|| session_id.to_string(), |row| row.id);
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_events
                 WHERE session_id = ?1 AND created_at >= ?2 AND instr(payload_json, ?3) > 0
                   AND (?4 = 0 OR payload_json LIKE '{\"kind\":\"tool_call\"%'))",
                params![id, since, encoded, calls_only],
                |row| row.get(0),
            )
        })
    }

    /// Load a session's transcript in order.
    pub fn load_conversation_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<HarnessMessage>, StoreError> {
        let rows = self.load_conversation_rows(session_id, None)?;
        Ok(rows.into_iter().map(|row| row.message).collect())
    }

    /// Load at most the newest `limit` messages, in transcript order.
    ///
    /// Selection happens in SQL (newest-first with a LIMIT, reversed after), so
    /// opening a long conversation does not read the history it is not showing.
    pub fn load_recent_conversation(
        &self,
        session_id: &str,
        limit: u32,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.load_conversation_rows(session_id, Some(limit))
    }

    fn load_conversation_rows(
        &self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<StoredMessage>, StoreError> {
        self.with_connection(|conn| {
            let mut rows: Vec<StoredMessage> = match limit {
                Some(limit) => {
                    let mut stmt = conn.prepare(
                        "SELECT ordinal, role, payload_json, created_at
                         FROM session_messages WHERE session_id = ?1
                         ORDER BY ordinal DESC LIMIT ?2",
                    )?;
                    let mapped = stmt.query_map(params![session_id, limit], message_row_from)?;
                    mapped.collect::<Result<Vec<_>, _>>()?
                }
                None => {
                    let mut stmt = conn.prepare(
                        "SELECT ordinal, role, payload_json, created_at
                         FROM session_messages WHERE session_id = ?1
                         ORDER BY ordinal",
                    )?;
                    let mapped = stmt.query_map(params![session_id], message_row_from)?;
                    mapped.collect::<Result<Vec<_>, _>>()?
                }
            };
            if limit.is_some() {
                // The LIMIT took the newest rows; restore transcript order.
                rows.reverse();
            }
            Ok(rows)
        })
    }

    /// How many messages a session holds, without reading any of them.
    pub fn conversation_message_count(&self, session_id: &str) -> Result<i64, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT COALESCE((SELECT ordinal + 1 FROM session_messages WHERE session_id = ?1 ORDER BY ordinal DESC LIMIT 1), 0)",
                params![session_id],
                |row| row.get(0),
            )
        })
    }

    /// Every session for a workspace, newest activity first — the session list.
    ///
    /// Reads only the `sessions` rows: the previous implementation had to open
    /// (and for older sessions, decompress) every transcript just to enumerate.
    pub fn list_conversations_for_workspace(
        &self,
        workspace_key: &str,
    ) -> Result<Vec<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions
                 WHERE workspace_key = ?1
                 ORDER BY updated_at DESC"
            ))?;
            let rows = stmt.query_map(params![workspace_key], session_row_from)?;
            rows.collect()
        })
    }

    /// Delete a session and its transcript.
    pub fn delete_conversation(&self, id: &str) -> Result<bool, StoreError> {
        let _signal = SessionChangeSignal;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM request_nonces WHERE session_id = ?1",
                params![id],
            )?;
            let deleted = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])? == 1;
            tx.commit()?;
            Ok(deleted)
        })
    }

    /// Every session in the store, newest first — the device-wide catalog.
    ///
    /// The catalog used to be a `read_dir` walk of the workspaces tree, which
    /// meant a session living only in the store was invisible to it. The store is
    /// the inventory now; the walk survives only for sessions not yet moved.
    pub fn list_all_sessions(&self) -> Result<Vec<SessionRow>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions
                 ORDER BY COALESCE(last_active, 0) DESC, updated_at DESC"
            ))?;
            let rows = stmt.query_map([], session_row_from)?;
            rows.collect()
        })
    }

    /// Record a session's last *user* activity.
    ///
    /// Separate from the scalar save because it is written on a different
    /// schedule: opening or attaching must NOT move it (the list would jump every
    /// time a chat was opened), so only a real user message calls this.
    pub fn set_session_last_active(&self, id: &str, last_active: i64) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET last_active = ?2, settled_at = NULL WHERE id = ?1",
                params![id, last_active],
            )?;
            Ok(())
        })
    }

    /// Settle (`Some(at)`) or wake (`None`) a session. Returns whether a row changed.
    pub fn set_session_settled(&self, id: &str, settled_at: Option<i64>) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET settled_at = ?2 WHERE id = ?1 AND settled_at IS NOT ?2",
                params![id, settled_at],
            )?;
            Ok(changed > 0)
        })
    }

    /// Record a turn that changed files: it becomes the session's last-turn totals and is added
    /// to its lifetime totals. Read-modify-write on the one store connection.
    pub fn record_turn_changes(&self, id: &str, files: u64, added: u64, removed: u64) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            let current: Option<String> = conn
                .query_row(
                    "SELECT total_changes FROM sessions WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let sum = |key: &str| {
                current
                    .as_deref()
                    .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                    .and_then(|v| v.get(key).and_then(serde_json::Value::as_u64))
                    .unwrap_or(0)
            };
            let last = serde_json::json!({"files": files, "added": added, "removed": removed});
            let total = serde_json::json!({
                "files": sum("files") + files,
                "added": sum("added") + added,
                "removed": sum("removed") + removed,
            });
            conn.execute(
                "UPDATE sessions SET last_changes = ?2, total_changes = ?3 WHERE id = ?1",
                params![id, last.to_string(), total.to_string()],
            )?;
            Ok(())
        })
    }

    /// Replace a session's lifetime totals with a figure measured from its worktree.
    pub fn set_total_changes(&self, id: &str, total: &crate::session_stats::Changes) -> Result<(), StoreError> {
        let json = serde_json::to_string(total).unwrap_or_default();
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET total_changes = ?2 WHERE id = ?1",
                params![id, json],
            )?;
            Ok(())
        })
    }

    /// Opt a session in or out of automatic settling. Returns whether it exists.
    pub fn set_session_keep_awake(&self, id: &str, on: bool) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET keep_awake = ?2 WHERE id = ?1",
                params![id, on as i64],
            )?;
            Ok(changed > 0)
        })
    }

    /// Ids of sessions settled strictly before `cutoff` (unix seconds).
    pub fn sessions_settled_before(&self, cutoff: i64) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt =
                conn.prepare("SELECT id FROM sessions WHERE settled_at IS NOT NULL AND settled_at < ?1")?;
            let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
            rows.collect()
        })
    }

    /// Set (or clear) a session's model override. Was the `.profile` sidecar.
    pub fn set_session_profile(&self, id: &str, profile: Option<&str>) -> Result<(), StoreError> {
        let value = profile.map(str::trim).filter(|p| !p.is_empty());
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET profile = ?2 WHERE id = ?1",
                params![id, value],
            )?;
            Ok(())
        })
    }

    /// Set a session's kind and the agent working it. Was the `.role` sidecar.
    pub fn set_session_role(
        &self,
        id: &str,
        role: &str,
        agent_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET role = ?2, agent_id = ?3 WHERE id = ?1",
                params![id, role, agent_id],
            )?;
            Ok(())
        })
    }

    /// Import a whole session — scalar state plus both logs — in one transaction.
    ///
    /// Unlike [`Self::create_conversation`] this takes the session's real scalar
    /// (counters, goal, checkpoints, lanes) and its event log, which is what a
    /// migration has and a new session does not. One transaction so an
    /// interrupted import leaves no half-written session behind: either the row
    /// and its full history are there or nothing is.
    ///
    /// Re-importing the same id replaces it, so a migration can be re-run safely
    /// after a failure without duplicating messages.
    pub fn import_session(
        &self,
        id: &str,
        workspace_key: &str,
        state: &crate::harness::HarnessState,
        extras: &SessionExtras,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        let scalar = crate::harness::scalar_json(state).map_err(StoreError::ScalarEncode)?;
        let status = crate::session::status_str(state.status);
        let role = extras
            .role
            .clone()
            .unwrap_or_else(|| "standard".to_string());
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status,
                    created_at, updated_at, tags_json, state_json,
                    last_active, profile, role, agent_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'[]',?8,?9,?10,?11,?12)
                 ON CONFLICT(id) DO UPDATE SET
                    workspace_key = excluded.workspace_key,
                    workspace = excluded.workspace,
                    title = excluded.title,
                    status = excluded.status,
                    updated_at = excluded.updated_at,
                    state_json = excluded.state_json,
                    last_active = excluded.last_active,
                    profile = excluded.profile,
                    role = excluded.role,
                    agent_id = excluded.agent_id",
                params![
                    id,
                    workspace_key,
                    state.workspace,
                    state.title,
                    status,
                    state.created_at,
                    state.updated_at,
                    scalar,
                    extras.last_active,
                    extras.profile,
                    role,
                    extras.agent_id,
                ],
            )?;
            // Replace rather than append: an import states the whole truth, so a
            // re-run must not stack a second copy of every message.
            tx.execute(
                "DELETE FROM session_messages WHERE session_id = ?1",
                params![id],
            )?;
            tx.execute(
                "DELETE FROM session_events WHERE session_id = ?1",
                params![id],
            )?;
            for (ordinal, message) in state.messages.iter().enumerate() {
                insert_message(&tx, id, ordinal as i64, message, &state.updated_at)?;
            }
            for (ordinal, event) in state.events.iter().enumerate() {
                insert_event(&tx, id, ordinal as i64, event, &state.updated_at)?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}

const SESSION_COLUMNS: &str = "id, workspace_key, workspace, title, status, created_at, \
     updated_at, last_active, profile, role, agent_id, kind, settled_at, keep_awake, last_changes, total_changes";

impl Store {
    /// Save a session's scalar state, creating the row on first write.
    ///
    /// Upsert rather than insert-or-fail: a session created before this store
    /// existed has no row, and the first persist must be able to establish one.
    /// `state_json` deliberately excludes the two append-only logs, so rewriting
    /// it on every persist stays cheap regardless of conversation length.
    #[allow(clippy::too_many_arguments)]
    pub fn save_session_scalar(
        &self,
        id: &str,
        workspace_key: &str,
        workspace: &str,
        title: Option<&str>,
        status: &str,
        scalar_json: &str,
        created_at: &str,
        now: &str,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status,
                created_at, updated_at, state_json, role)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
                workspace = excluded.workspace,
                title = excluded.title,
                status = excluded.status,
                updated_at = excluded.updated_at,
                state_json = excluded.state_json",
                params![
                    id,
                    workspace_key,
                    workspace,
                    title,
                    status,
                    created_at,
                    now,
                    scalar_json,
                    if crate::mission_control::is_session_id(id) { "mission_control" } else { "standard" }
                ],
            )?;
            Ok(())
        })
    }

    /// Whether the parent's lane records list `lane` as running.
    pub fn session_lane_recorded_running(&self, parent: &str, lane: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM sessions s, json_each(s.state_json, '$.lanes') l
                     WHERE s.id = ?1
                       AND json_extract(l.value, '$.id') = ?2
                       AND json_extract(l.value, '$.status') = 'running')",
                params![parent, lane],
                |row| row.get(0),
            )
        })
    }

    /// Set the row's status and the status held in its state JSON, nothing else.
    pub fn set_session_status(&self, id: &str, status: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let n = conn.execute(
                "UPDATE sessions SET status = ?2,
                     state_json = CASE WHEN json_valid(state_json) THEN json_set(state_json, '$.status', ?2) ELSE state_json END
                 WHERE id = ?1",
                params![id, status],
            )?;
            Ok(n > 0)
        })
    }

    pub fn session_has_running_lanes(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM sessions s, json_each(s.state_json, '$.lanes') l
                     WHERE s.id = ?1
                       AND json_extract(l.value, '$.status') = 'running')",
                params![id],
                |row| row.get(0),
            )
        })
    }

    pub fn session_has_watches(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM sessions s, json_each(s.state_json, '$.watches') w
                     WHERE s.id = ?1)",
                params![id],
                |row| row.get(0),
            )
        })
    }

    pub fn current_stop_identity(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT stop_identity FROM sessions WHERE id = ?1 AND status = 'idle'",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            ).optional().map(Option::flatten)
        })
    }

    pub fn current_attention(&self, id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        self.with_connection(|conn| {
            let request = conn.query_row(
                "SELECT e.ordinal, e.created_at, json_extract(e.payload_json, '$.kind'), e.payload_json
                 FROM sessions s JOIN session_events e ON e.session_id = s.id
                 WHERE s.id = ?1 AND s.status = 'waiting_for_input'
                   AND json_extract(e.payload_json, '$.kind') IN ('user_question', 'approval_request')
                   AND e.ordinal = (SELECT MAX(ordinal) FROM session_events
                       WHERE session_id = s.id AND json_extract(payload_json, '$.kind')
                           IN ('user_question', 'approval_request'))
                   AND ((json_extract(e.payload_json, '$.kind') = 'user_question'
                         AND json_extract(s.state_json, '$.pending_question') IS NOT NULL
                         AND json_extract(s.state_json, '$.pending_question') = json_extract(e.payload_json, '$.questions'))
                        OR (json_extract(e.payload_json, '$.kind') = 'approval_request'
                            AND json_extract(s.state_json, '$.pending_question') IS NULL))",
                params![id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
            ).optional()?;
            Ok(request.map(|(ordinal, created_at, kind, payload)| {
                let mut attention = serde_json::json!({
                    "type": if kind == "user_question" { "question" } else { "approval" },
                    "id": format!("{ordinal}:{created_at}")
                });
                let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
                attention_detail(&mut attention, &payload);
                approval_call_detail(conn, id, ordinal, &payload, &mut attention);
                attention
            }))
        })
    }

    /// The agent's last words this turn, for the notification when it stops.
    pub fn last_assistant_excerpt(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            let text = conn.query_row(
                "SELECT json_extract(payload_json, '$.text') FROM session_events
                 WHERE session_id = ?1 AND json_extract(payload_json, '$.kind') = 'assistant_text'
                   AND ordinal > COALESCE((SELECT MAX(ordinal) FROM session_events
                       WHERE session_id = ?1 AND json_extract(payload_json, '$.kind') IN ('user_input', 'steer')), -1)
                 ORDER BY ordinal DESC LIMIT 1",
                params![id],
                |row| row.get::<_, Option<String>>(0),
            ).optional().map(Option::flatten)?;
            Ok(text.map(|t| clip_keeping_lines(&t, 700)).filter(|t| !t.is_empty()))
        })
    }

    /// Read a session's stored scalar state, if this store has the session.
    pub fn load_session_scalar(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            conn
                .query_row(
                    "SELECT state_json FROM sessions WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()
        })
    }

    /// Whether this store has a row for the session. Drives DB-vs-file selection on
    /// read, so a session that has not been migrated is still served from disk.
    pub fn has_conversation(&self, id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(count > 0)
        })
    }
}

impl Store {
    /// Cheap change-detector: the row's `updated_at` plus the two log lengths.
    ///
    /// Lets a poller notice a change without loading the transcript. The attach
    /// stream needs this because it re-reads on a timer, and loading a long
    /// conversation just to discover nothing changed would defeat the point of
    /// moving it into the database.
    pub fn conversation_fingerprint(&self, id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            conn
                .query_row(
                    "SELECT s.updated_at,
                            COALESCE((SELECT ordinal + 1 FROM session_messages WHERE session_id = s.id ORDER BY ordinal DESC LIMIT 1), 0),
                            COALESCE((SELECT ordinal + 1 FROM session_events WHERE session_id = s.id ORDER BY ordinal DESC LIMIT 1), 0)
                     FROM sessions s WHERE s.id = ?1",
                    params![id],
                    |row| {
                        let updated: String = row.get(0)?;
                        let messages: i64 = row.get(1)?;
                        let events: i64 = row.get(2)?;
                        Ok(format!("{updated}|{messages}|{events}"))
                    },
                )
                .optional()
        })
    }

    /// Replace a transcript wholesale.
    ///
    /// The escape hatch for the writers that genuinely rewrite history —
    /// compaction, checkpoint rewind, interrupt rollback. Scoped as delete-all +
    /// insert rather than a diff, because after one of those the two lists have
    /// little in common and diffing would cost more than rewriting.
    pub fn replace_conversation_messages(
        &self,
        session_id: &str,
        messages: &[HarnessMessage],
        now: &str,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM session_messages WHERE session_id = ?1",
                params![session_id],
            )?;
            for (ordinal, message) in messages.iter().enumerate() {
                insert_message(&tx, session_id, ordinal as i64, message, now)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Replace the event log wholesale. Same escape hatch as messages.
    pub fn replace_conversation_events(
        &self,
        session_id: &str,
        events: &[HarnessEvent],
        now: &str,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM session_events WHERE session_id = ?1",
                params![session_id],
            )?;
            for (ordinal, event) in events.iter().enumerate() {
                insert_event(&tx, session_id, ordinal as i64, event, now)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Atomically claim a client request id. `false` means it was already
    /// accepted, so a reconnect must not deliver the same input again.
    pub fn claim_request_nonce(
        &self,
        session_id: &str,
        nonce: &str,
        accepted_at: i64,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let inserted = tx.execute(
                "INSERT INTO request_nonces (session_id, nonce, accepted_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id, nonce) DO NOTHING",
                params![session_id, nonce, accepted_at],
            )? == 1;
            if inserted {
                tx.execute(
                    "DELETE FROM request_nonces
                     WHERE session_id = ?1
                       AND nonce NOT IN (
                           SELECT nonce FROM request_nonces
                           WHERE session_id = ?1
                           ORDER BY accepted_at DESC, rowid DESC
                           LIMIT 512
                       )",
                    params![session_id],
                )?;
            }
            tx.commit()?;
            Ok(inserted)
        })
    }

    /// Append events to the log, continuing from the current maximum ordinal.
    pub fn append_conversation_events(
        &self,
        session_id: &str,
        events: &[HarnessEvent],
        now: &str,
    ) -> Result<(), StoreError> {
        let _signal = SessionChangeSignal;
        if events.is_empty() {
            return Ok(());
        }
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut next: i64 = tx.query_row(
                "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM session_events WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )?;
            for event in events {
                insert_event(&tx, session_id, next, event, now)?;
                next += 1;
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Load the full event log in order.
    pub fn load_conversation_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<HarnessEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM session_events
                 WHERE session_id = ?1 ORDER BY ordinal",
            )?;
            let rows = stmt.query_map(params![session_id], |row| {
                let raw: String = row.get(0)?;
                serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })?;
            rows.collect()
        })
    }

    pub fn load_conversation_events_from(
        &self,
        session_id: &str,
        from: usize,
    ) -> Result<(Vec<HarnessEvent>, usize), StoreError> {
        self.with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COALESCE((SELECT ordinal + 1 FROM session_events WHERE session_id = ?1 ORDER BY ordinal DESC LIMIT 1), 0)",
                params![session_id],
                |row| row.get(0),
            )?;
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM session_events
                 WHERE session_id = ?1 ORDER BY ordinal LIMIT -1 OFFSET ?2",
            )?;
            let offset = i64::try_from(from).unwrap_or(i64::MAX);
            let rows = stmt.query_map(params![session_id, offset], |row| {
                let raw: String = row.get(0)?;
                serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })?;
            let events = rows.collect::<Result<Vec<HarnessEvent>, _>>()?;
            Ok((events, count as usize))
        })
    }

    pub fn load_conversation_event_page(
        &self,
        session_id: &str,
        before: Option<usize>,
        size: usize,
    ) -> Result<(Vec<HarnessEvent>, usize, usize), StoreError> {
        self.with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COALESCE((SELECT ordinal + 1 FROM session_events WHERE session_id = ?1 ORDER BY ordinal DESC LIMIT 1), 0)",
                params![session_id],
                |row| row.get(0),
            )?;
            let count = count as usize;
            let end = before.unwrap_or(count).min(count);
            let start = end.saturating_sub(size);
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM session_events
                 WHERE session_id = ?1 ORDER BY ordinal LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(params![session_id, (end - start) as i64, start as i64], |row| {
                let raw: String = row.get(0)?;
                serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
                })
            })?;
            Ok((rows.collect::<Result<Vec<HarnessEvent>, _>>()?, start, end))
        })
    }
}

fn insert_event(
    conn: &rusqlite::Connection,
    session_id: &str,
    ordinal: i64,
    event: &HarnessEvent,
    now: &str,
) -> Result<(), rusqlite::Error> {
    let payload = serde_json::to_string(event)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO session_events (session_id, ordinal, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![session_id, ordinal, payload, now],
    )?;
    Ok(())
}

fn session_row_from(row: &rusqlite::Row<'_>) -> Result<SessionRow, rusqlite::Error> {
    Ok(SessionRow {
        id: row.get(0)?,
        workspace_key: row.get(1)?,
        workspace: row.get(2)?,
        title: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        last_active: row.get(7)?,
        profile: row.get(8)?,
        role: row.get(9)?,
        agent_id: row.get(10)?,
        kind: row.get(11)?,
        settled_at: row.get(12)?,
        keep_awake: row.get::<_, i64>(13)? != 0,
        last_changes: row.get(14)?,
        total_changes: row.get(15)?,
    })
}

fn message_row_from(row: &rusqlite::Row<'_>) -> Result<StoredMessage, rusqlite::Error> {
    let payload: String = row.get(2)?;
    let message: HarnessMessage = serde_json::from_str(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(StoredMessage {
        ordinal: row.get(0)?,
        role: row.get(1)?,
        message,
        created_at: row.get(3)?,
    })
}

fn insert_message(
    conn: &rusqlite::Connection,
    session_id: &str,
    ordinal: i64,
    message: &HarnessMessage,
    now: &str,
) -> Result<(), rusqlite::Error> {
    let payload = serde_json::to_string(message)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "INSERT INTO session_messages (session_id, ordinal, role, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, ordinal, role_of(message), payload, now],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn session(id: &str) -> SessionRow {
        SessionRow {
            id: id.into(),
            workspace_key: "wk1".into(),
            workspace: "/code/thing".into(),
            title: Some("Thing".into()),
            status: "idle".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            last_active: None,
            profile: None,
            role: "standard".into(),
            agent_id: None,
            kind: "conversation".into(),
            settled_at: None,
            keep_awake: false,
            last_changes: None,
            total_changes: None,
        }
    }

    fn user(text: &str) -> HarnessMessage {
        HarnessMessage::User {
            content: text.into(),
        }
    }

    #[test]
    fn request_nonces_are_idempotent_and_bounded() {
        let db = db();
        assert!(db.claim_request_nonce("s1", "first", 1).unwrap());
        assert!(!db.claim_request_nonce("s1", "first", 2).unwrap());
        for i in 0..513 {
            assert!(
                db.claim_request_nonce("s1", &format!("nonce-{i}"), i + 3)
                    .unwrap()
            );
        }
        let count: i64 = db
            .with_connection(|connection| {
                connection.query_row(
                    "SELECT COUNT(*) FROM request_nonces WHERE session_id = 's1'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(count, 512);
        assert!(db.claim_request_nonce("s1", "first", 600).unwrap());
    }

    #[test]
    fn appending_grows_the_transcript_without_rewriting_it() {
        let db = db();
        db.create_conversation(&session("s1"), &[user("one")], "[]")
            .unwrap();
        assert_eq!(db.conversation_message_count("s1").unwrap(), 1);

        // Two separate appends: the second must continue the ordinals, not
        // restart them, so the transcript order survives.
        db.append_conversation_messages("s1", &[user("two")], "2026-01-01T00:00:01Z")
            .unwrap();
        db.append_conversation_messages("s1", &[user("three")], "2026-01-01T00:00:02Z")
            .unwrap();

        assert_eq!(db.conversation_message_count("s1").unwrap(), 3);
        let loaded = db.load_conversation_messages("s1").unwrap();
        assert_eq!(loaded, vec![user("one"), user("two"), user("three")]);
    }

    #[test]
    fn recent_load_reads_only_the_tail() {
        let db = db();
        let all: Vec<_> = (0..10).map(|i| user(&format!("m{i}"))).collect();
        db.create_conversation(&session("s1"), &all, "[]").unwrap();

        let recent = db.load_recent_conversation("s1", 3).unwrap();
        assert_eq!(recent.len(), 3);
        // Newest selected, transcript order preserved.
        assert_eq!(recent[0].message, user("m7"));
        assert_eq!(recent[2].message, user("m9"));
    }

    #[test]
    fn compaction_replaces_only_the_tail() {
        let db = db();
        let all: Vec<_> = (0..6).map(|i| user(&format!("m{i}"))).collect();
        db.create_conversation(&session("s1"), &all, "[]").unwrap();

        // Fold everything from ordinal 3 into one summary.
        db.replace_conversation_tail(
            "s1",
            3,
            &[HarnessMessage::Summary {
                kind: "compact".into(),
                content: "folded".into(),
            }],
            "2026-01-01T00:00:03Z",
        )
        .unwrap();

        let loaded = db.load_conversation_messages("s1").unwrap();
        assert_eq!(loaded.len(), 4, "3 kept + 1 summary");
        assert_eq!(loaded[0], user("m0"));
        assert_eq!(loaded[2], user("m2"));
        assert!(matches!(loaded[3], HarnessMessage::Summary { .. }));
    }

    #[test]
    fn the_session_list_does_not_touch_transcripts() {
        let db = db();
        db.create_conversation(&session("s1"), &[user("a")], "[]")
            .unwrap();
        db.create_conversation(
            &SessionRow {
                id: "s2".into(),
                updated_at: "2026-01-02T00:00:00Z".into(),
                ..session("s2")
            },
            &[user("b")],
            "[]",
        )
        .unwrap();

        let rows = db.list_conversations_for_workspace("wk1").unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["s2", "s1"],
            "newest activity first"
        );
    }

    #[test]
    fn deleting_a_session_takes_its_transcript() {
        let db = db();
        db.create_conversation(&session("s1"), &[user("a")], "[]")
            .unwrap();
        assert!(db.delete_conversation("s1").unwrap());
        assert_eq!(db.conversation_message_count("s1").unwrap(), 0);
        assert!(db.get_session_row("s1").unwrap().is_none());
    }

    #[test]
    fn deleting_a_session_takes_its_events_too() {
        // `create_conversation` writes messages but no events, so the event-log
        // cascade is only exercised by a full import — which is also the path the
        // migration uses.
        let db = db();
        let mut state = crate::harness::HarnessState::blank("/code/thing", Some("Thing".into()));
        state.messages = vec![user("one"), user("two")];
        state.events = vec![HarnessEvent::UserInput { text: "one".into() }];
        db.import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        assert_eq!(db.conversation_message_count("s1").unwrap(), 2);
        assert_eq!(db.load_conversation_events("s1").unwrap().len(), 1);

        assert!(db.delete_conversation("s1").unwrap());

        // Both logs must go with the row. A leftover message or event would keep
        // the deleted conversation readable through the log tables.
        assert_eq!(db.conversation_message_count("s1").unwrap(), 0);
        assert!(db.load_conversation_events("s1").unwrap().is_empty());
        assert!(db.get_session_row("s1").unwrap().is_none());
    }

    #[test]
    fn re_importing_replaces_instead_of_duplicating() {
        // The migration is re-runnable, so importing the same id twice must not
        // stack a second copy of every message.
        let db = db();
        let mut state = crate::harness::HarnessState::blank("/code/thing", Some("Thing".into()));
        state.messages = vec![user("one")];
        db.import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        db.import_session("s1", "wk1", &state, &SessionExtras::default())
            .unwrap();
        assert_eq!(db.conversation_message_count("s1").unwrap(), 1);
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    format!("{}…", text.chars().take(max).collect::<String>().trim_end())
}

fn clip_keeping_lines(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max).collect::<String>().trim_end())
}

/// The label and real command of the call being approved. A lane's request or a
/// parallel call has a different call before it, and a wrong one on an approval
/// misleads, so the call must match the command in the summary.
fn approval_call_detail(
    conn: &rusqlite::Connection,
    id: &str,
    ordinal: i64,
    payload: &serde_json::Value,
    attention: &mut serde_json::Value,
) -> Option<()> {
    if payload["kind"] != "approval_request" || payload["tool_name"] != "bash" {
        return None;
    }
    let call: String = conn.query_row(
        "SELECT payload_json FROM session_events
         WHERE session_id = ?1 AND ordinal = ?2 AND json_extract(payload_json, '$.kind') = 'tool_call'",
        params![id, ordinal - 1],
        |row| row.get(0),
    ).ok()?;
    let call: serde_json::Value = serde_json::from_str(&call).ok()?;
    let command = call["arguments"]["command"].as_str()?;
    let head = clip(command, 40);
    if !payload["summary"].as_str()?.contains(head.trim_end_matches('…')) {
        return None;
    }
    attention["command"] = clip_keeping_lines(command, 2000).into();
    let label = call["arguments"]["label"].as_str().unwrap_or("").trim();
    if !label.is_empty() {
        attention["label"] = clip(label, 120).into();
    }
    Some(())
}

/// What a notification needs to show the request without opening the chat.
fn attention_detail(attention: &mut serde_json::Value, payload: &serde_json::Value) {
    if payload["kind"] == "approval_request" {
        attention["tool"] = payload["tool_name"].clone();
        if let Some(summary) = payload["summary"].as_str() {
            attention["text"] = clip(summary, 400).into();
        }
        return;
    }
    let Some(questions) = payload["questions"]["questions"].as_array() else { return };
    let Some(first) = questions.first() else { return };
    if let Some(text) = first["text"].as_str() {
        attention["text"] = clip(text, 600).into();
    }
    if questions.len() > 1 {
        attention["more"] = (questions.len() - 1).into();
        return;
    }
    let kind = &first["answer_kind"];
    let choices: Vec<serde_json::Value> = match kind["kind"].as_str() {
        Some("single_choice") => kind["choices"].as_array().map(|cs| {
            cs.iter()
                .filter_map(|c| {
                    let value = c["value"].as_str()?;
                    let label = c["label"].as_str().unwrap_or(value);
                    Some(serde_json::json!({"label": clip(label, 60), "value": value}))
                })
                .take(4)
                .collect()
        }).unwrap_or_default(),
        Some("yes_no") => vec![
            serde_json::json!({"label": "Yes", "value": "yes"}),
            serde_json::json!({"label": "No", "value": "no"}),
        ],
        _ => Vec::new(),
    };
    if !choices.is_empty() {
        attention["choices"] = choices.into();
    }
}
