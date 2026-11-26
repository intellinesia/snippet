//! Group threads: board threads with scope `group` where several agents and
//! the human talk with turn-taking.
//!
//! A post is an ordinary `group_message` board event. Who gets woken is
//! recorded in `message_deliveries` like a direct message: `group` rows are
//! the immediate wakes, `group_deferred` rows carry a `retry_at` ten seconds
//! out and are skipped at fire time once anyone has replied to the post
//! (`group_replies` is the claim ledger, one reply per agent per post), or
//! once any agent has posted in the thread since. Naming a member in the body
//! (id, handle, display name or nickname) wakes it like a mention; generic
//! words such as `snippet` count only as `@snippet`. A snoozed member gets no
//! optional wakes unless it is explicitly mentioned. Each human post opens or
//! joins a `group_rounds` row whose lead can fail over for that round only. An agent's first post after the latest human post also
//! reaches the other agents as a `group_peer` wake; later posts in that round
//! reach only whom they name, which bounds agents waking each other.

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::direct::{EVENT_COLUMNS_QUALIFIED, PendingDirectMessage, actor_ref, event_from_row};
use super::types::CoordinationEvent;
use super::{Store, StoreError};

/// How long a deferred group wake waits for someone else to answer first.
pub const GROUP_DEFER_SECONDS: i64 = 10;

/// How long a human's post may go unanswered before the lead is woken once
/// with an `[owed_reply]` nudge: the backstop when every member stays quiet.
pub const GROUP_UNANSWERED_SECONDS: i64 = 600;

/// Failed deliveries after which a group wake is given up on.
const MAX_GROUP_DELIVERY_ATTEMPTS: u32 = 12;

/// One participant of a group, in its wire form.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GroupMember {
    pub kind: String,
    pub id: String,
}

/// A group thread as listed for a reader.
#[derive(Debug, Clone, Serialize)]
pub struct GroupSummary {
    pub id: String,
    pub title: String,
    pub members: Vec<GroupMember>,
    pub lead: Option<String>,
    pub created_at: String,
    pub last_event_at: Option<String>,
    /// Events the human has not read yet.
    pub unread: i64,
    pub last_message: Option<GroupLastMessage>,
    /// Agents woken for this thread whose reply is still in flight.
    pub replying: Vec<String>,
}

/// The newest post in a group, for the list preview.
#[derive(Debug, Clone, Serialize)]
pub struct GroupLastMessage {
    pub from_kind: String,
    pub from_id: String,
    pub body: String,
    pub at: String,
}

/// Validation failures cross `with_connection` as `InvalidParameterName`
/// and are mapped back to [`StoreError::InvalidInput`] at the boundary.
/// Members named in `body` by id, handle or display name, as whole words.
/// Whether an agent's post mentions the user: `human` in `mentions` (with or
/// without a leading @), or `@human` in the text.
pub(crate) fn mentions_human(body: &str, mentions: &[String]) -> bool {
    if mentions.iter().any(|m| {
        m.trim()
            .trim_start_matches('@')
            .eq_ignore_ascii_case("human")
    }) {
        return true;
    }
    let text = body.to_lowercase();
    text.match_indices("@human").any(|(at, m)| {
        let open = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
        let close = text[at + m.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-')));
        open && close
    })
}

/// Words that are also ordinary prose (the product name, the lead's role):
/// they name an agent only as an explicit `@word`.
const GENERIC_TERMS: &[&str] = &["snippet", "lead"];

#[cfg(test)]
pub(crate) fn named_in_body(body: &str, members: &[(String, String, String)]) -> Vec<String> {
    let terms: Vec<(String, Vec<String>)> = members
        .iter()
        .map(|(id, display, handle)| (id.clone(), vec![id.clone(), handle.clone(), display.clone()]))
        .collect();
    named_by_terms(body, &terms)
}

/// Members named in `body` by any of their terms (id, handle, display name,
/// nickname) as whole words; a generic or short term counts only as `@term`.
pub(crate) fn named_by_terms(body: &str, members: &[(String, Vec<String>)]) -> Vec<String> {
    let text = body.to_lowercase();
    let joined = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '/');
    let found = |term: &str| {
        let term = term.trim().trim_start_matches('@').to_lowercase();
        if term.is_empty() {
            return false;
        }
        let generic = term.chars().count() < 3 || GENERIC_TERMS.contains(&term.as_str());
        text.match_indices(&term).any(|(at, m)| {
            let mut before = text[..at].chars().rev();
            let mut after = text[at + m.len()..].chars();
            let open = match before.next() {
                Some('@') => before.next().is_none_or(|c| !joined(c) && c != '.'),
                _ if generic => false,
                Some(c) => !(joined(c) || c == '.'),
                None => true,
            };
            let close = match after.next() {
                Some('.') => !after.next().is_some_and(|c| c.is_alphanumeric()),
                Some(c) => !joined(c),
                None => true,
            };
            open && close
        })
    };
    members
        .iter()
        .filter(|(_, terms)| terms.iter().any(|t| found(t)))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Whether `agent_id` is addressed explicitly: listed in `mentions` by id or
/// nickname, or written as `@id` / `@nickname` in the body.
pub(crate) fn explicitly_mentions(
    body: &str,
    mentions: &[String],
    agent_id: &str,
    nickname: Option<&str>,
) -> bool {
    let names: Vec<String> = std::iter::once(agent_id)
        .chain(nickname)
        .map(|n| n.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .collect();
    if mentions.iter().any(|m| {
        let m = m.trim().trim_start_matches('@').to_lowercase();
        m == "everyone" || names.contains(&m)
    }) {
        return true;
    }
    let text = body.to_lowercase();
    let joined = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '/');
    names.iter().any(|name| {
        let tag = format!("@{name}");
        text.match_indices(&tag).any(|(at, m)| {
            let open = text[..at].chars().next_back().is_none_or(|c| !joined(c));
            let mut after = text[at + m.len()..].chars();
            let close = match after.next() {
                Some('.') => !after.next().is_some_and(|c| c.is_alphanumeric()),
                Some(c) => !joined(c),
                None => true,
            };
            open && close
        })
    })
}

fn load_nicknames(
    conn: &rusqlite::Connection,
) -> Result<std::collections::HashMap<String, String>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT agent_id, nickname FROM agent_nicknames")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

fn invalid_input(message: String) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message)
}

fn group_store_result<T>(result: Result<T, StoreError>) -> Result<T, StoreError> {
    result.map_err(|error| match error {
        StoreError::Sqlite(rusqlite::Error::InvalidParameterName(message)) => {
            StoreError::InvalidInput(message)
        }
        other => other,
    })
}

fn group_from_row(
    conn: &rusqlite::Connection,
    row: &rusqlite::Row<'_>,
) -> Result<GroupSummary, rusqlite::Error> {
    let id: String = row.get(0)?;
    let members = conn
        .prepare(
            "SELECT actor_id, actor_kind FROM board_participants
             WHERE thread_id = ?1 ORDER BY actor_id",
        )?
        .query_map(params![id], |r| {
            Ok(GroupMember {
                kind: r.get(1)?,
                id: r.get(0)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let last_message = conn
        .query_row(
            "SELECT actor_kind, actor_id, payload_json, created_at FROM board_events
             WHERE thread_id = ?1 AND event_type = 'group_message'
               AND json_extract(payload_json, '$.reaction') IS NULL
             ORDER BY sequence DESC LIMIT 1",
            params![id],
            |r| {
                let payload: String = r.get(2)?;
                let body = serde_json::from_str::<serde_json::Value>(&payload)
                    .ok()
                    .and_then(|v| v.get("body").and_then(|b| b.as_str()).map(str::to_owned))
                    .unwrap_or_default();
                Ok(GroupLastMessage {
                    from_kind: r.get(0)?,
                    from_id: r.get(1)?,
                    body,
                    at: r.get(3)?,
                })
            },
        )
        .optional()?;
    let replying = replying_agents(conn, &id)?;
    Ok(GroupSummary {
        id,
        title: row.get(1)?,
        members,
        lead: row.get(2)?,
        created_at: row.get(3)?,
        last_event_at: row.get(4)?,
        unread: row.get(5)?,
        last_message,
        replying,
    })
}

/// Agents woken for a group thread whose reply is still in flight: a wake was
/// delivered for a post they have not answered since, and their inbox session
/// is mid-turn. A member told to stay quiet goes idle and drops off the list;
/// one paused on the user is waiting, not replying. Only the last 200 events
/// are scanned, since a reply in flight answers a recent post.
fn replying_agents(
    conn: &rusqlite::Connection,
    thread_id: &str,
) -> Result<Vec<String>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT d.recipient_id
         FROM message_deliveries d
         JOIN board_events e ON e.event_id = d.event_id
         JOIN sessions s ON s.id = CASE
             WHEN d.recipient_id = 'lead' THEN 'lead'
             ELSE 'inbox-' || d.recipient_id END
         WHERE e.thread_id = ?1
           AND d.recipient_kind IN ('group', 'group_deferred', 'group_peer', 'group_unanswered')
           AND d.delivered_at IS NOT NULL
           AND s.status = 'running'
           AND e.sequence > (SELECT COALESCE(MAX(sequence), 0) - 200
                             FROM board_events WHERE thread_id = ?1)
           AND NOT EXISTS (
               SELECT 1 FROM board_events r
               WHERE r.thread_id = ?1 AND r.event_type = 'group_message'
                 AND r.actor_kind = 'agent' AND r.actor_id = d.recipient_id
                 AND r.sequence > e.sequence)
         ORDER BY d.recipient_id",
    )?;
    let rows = stmt.query_map(params![thread_id], |row| row.get(0))?;
    rows.collect()
}

const GROUP_COLUMNS: &str = "t.id, t.title,
        (SELECT lead_agent FROM group_meta m WHERE m.thread_id = t.id),
        t.created_at,
        (SELECT MAX(e.created_at) FROM board_events e WHERE e.thread_id = t.id),
        (SELECT COUNT(*) FROM message_deliveries d
          JOIN board_events e2 ON e2.event_id = d.event_id
          WHERE e2.thread_id = t.id
            AND d.recipient_kind = 'human' AND d.recipient_id = 'human'
            AND d.read_at IS NULL)";

/// The lead a no-mention post goes to: the configured lead while it is still
/// a member, else the agent the human last addressed, else the first agent.
fn resolve_lead(
    conn: &rusqlite::Connection,
    thread_id: &str,
    agents: &[String],
) -> Result<Option<String>, rusqlite::Error> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT lead_agent FROM group_meta WHERE thread_id = ?1",
            params![thread_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(Some(lead)) = stored
        && agents.iter().any(|a| a == &lead)
    {
        return Ok(Some(lead));
    }
    let mut stmt = conn.prepare(
        "SELECT payload_json FROM board_events
         WHERE thread_id = ?1 AND actor_kind = 'human' AND event_type = 'group_message'
         ORDER BY sequence DESC LIMIT 20",
    )?;
    let rows = stmt.query_map(params![thread_id], |row| row.get::<_, String>(0))?;
    for payload in rows {
        let payload: serde_json::Value = serde_json::from_str(&payload?).unwrap_or_default();
        let mut mentions = payload
            .get("mentions")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str());
        if let Some(lead) = mentions.find(|m| agents.iter().any(|a| a.as_str() == *m)) {
            return Ok(Some(lead.to_string()));
        }
    }
    Ok(agents.first().cloned())
}

impl Store {
    /// Create a group thread. The human is always a member; `members` are
    /// agent ids. Returns the new group.
    pub fn create_group(
        &self,
        title: &str,
        created_by: (&str, &str),
        members: &[String],
        lead: Option<&str>,
        created_at: &str,
    ) -> Result<GroupSummary, StoreError> {
        let thread_id = format!("group:{}", uuid::Uuid::new_v4());
        if let Some(lead) = lead
            && !members.iter().any(|m| m == lead)
        {
            return Err(StoreError::InvalidInput(format!(
                "lead `{lead}` is not one of the group's members"
            )));
        }
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO board_threads (id, scope, subject_id, title, created_at)
                 VALUES (?1, 'group', NULL, ?2, ?3)",
                params![thread_id, title, created_at],
            )?;
            for member in members {
                tx.execute(
                    "INSERT OR IGNORE INTO board_participants (thread_id, actor_id, actor_kind)
                     VALUES (?1, ?2, 'agent')",
                    params![thread_id, member],
                )?;
            }
            tx.execute(
                "INSERT OR IGNORE INTO board_participants (thread_id, actor_id, actor_kind)
                 VALUES (?1, 'human', 'human')",
                params![thread_id],
            )?;
            tx.execute(
                "INSERT INTO group_meta (thread_id, lead_agent, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    thread_id,
                    lead,
                    actor_ref(created_by.0, created_by.1),
                    created_at
                ],
            )?;
            tx.commit()?;
            Ok(())
        })?;
        self.get_group(&thread_id)?.ok_or_else(|| {
            StoreError::InvalidInput(format!("group `{thread_id}` vanished during create"))
        })
    }

    /// Every group thread, most recently created first.
    pub fn list_groups(&self) -> Result<Vec<GroupSummary>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {GROUP_COLUMNS} FROM board_threads t
                 WHERE t.scope = 'group' ORDER BY t.created_at DESC"
            ))?;
            let rows = stmt.query_map([], |row| group_from_row(conn, row))?;
            rows.collect()
        })
    }

    /// Agents woken for a room whose reply is still in flight. Groups answer
    /// from their wake deliveries; a task room's wakes are not recorded per
    /// agent, so this is the seated members whose work session is mid-turn.
    pub fn room_replying(&self, thread_id: &str) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let scope: Option<String> = conn
                .query_row(
                    "SELECT scope FROM board_threads WHERE id = ?1",
                    params![thread_id],
                    |row| row.get(0),
                )
                .optional()?;
            match scope.as_deref() {
                Some("group") => replying_agents(conn, thread_id),
                Some("task") => {
                    let mut stmt = conn.prepare(
                        "SELECT ta.agent_id
                         FROM task_agents ta
                         JOIN tasks t ON t.id = ta.task_id
                         JOIN sessions s ON s.id = ta.work_session_id
                         WHERE t.thread_id = ?1
                           AND ta.removed_at IS NULL
                           AND ta.work_session_id IS NOT NULL
                           AND ta.work_session_id != ''
                           AND s.status = 'running'
                         ORDER BY ta.agent_id",
                    )?;
                    let rows = stmt.query_map(params![thread_id], |row| row.get(0))?;
                    rows.collect()
                }
                _ => Ok(Vec::new()),
            }
        })
    }

    /// One group thread, or `None` when the id is not a group.
    pub fn get_group(&self, thread_id: &str) -> Result<Option<GroupSummary>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {GROUP_COLUMNS} FROM board_threads t
                 WHERE t.id = ?1 AND t.scope = 'group'"
            ))?;
            let mut rows = stmt.query_map(params![thread_id], |row| group_from_row(conn, row))?;
            rows.next().transpose()
        })
    }

    /// Rename a group.
    pub fn set_group_title(&self, thread_id: &str, title: &str) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE board_threads SET title = ?2 WHERE id = ?1 AND scope = 'group'",
                params![thread_id, title],
            )?;
            Ok(())
        })
    }

    /// Set the group's lead agent. The lead must be an agent participant.
    pub fn set_group_lead(&self, thread_id: &str, agent_id: &str) -> Result<(), StoreError> {
        group_store_result(self.with_connection(|conn| {
            let member: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM board_participants
                     WHERE thread_id = ?1 AND actor_kind = 'agent' AND actor_id = ?2",
                    params![thread_id, agent_id],
                    |row| row.get(0),
                )
                .optional()?;
            if member.is_none() {
                return Err(invalid_input(format!(
                    "`{agent_id}` is not an agent in this group"
                )));
            }
            conn.execute(
                "UPDATE group_meta SET lead_agent = ?2 WHERE thread_id = ?1",
                params![thread_id, agent_id],
            )?;
            Ok(())
        }))
    }

    /// Add an agent to a group.
    pub fn add_group_member(&self, thread_id: &str, agent_id: &str) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO board_participants (thread_id, actor_id, actor_kind)
                 VALUES (?1, ?2, 'agent')",
                params![thread_id, agent_id],
            )?;
            Ok(())
        })
    }

    /// Remove an agent from a group. The human is never removable; removing
    /// the lead clears the lead so the default resolution takes over.
    pub fn remove_group_member(&self, thread_id: &str, agent_id: &str) -> Result<(), StoreError> {
        if agent_id == "human" {
            return Err(StoreError::InvalidInput(
                "the human is always a member of a group".into(),
            ));
        }
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM board_participants
                 WHERE thread_id = ?1 AND actor_kind = 'agent' AND actor_id = ?2",
                params![thread_id, agent_id],
            )?;
            conn.execute(
                "UPDATE group_meta SET lead_agent = NULL
                 WHERE thread_id = ?1 AND lead_agent = ?2",
                params![thread_id, agent_id],
            )?;
            Ok(())
        })
    }

    /// Delete a group with its posts and their pending wakes. Returns whether it existed.
    pub fn delete_group(&self, thread_id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            for table in ["message_deliveries", "group_replies"] {
                let column = if table == "group_replies" {
                    "post_event_id"
                } else {
                    "event_id"
                };
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE {column} IN
                         (SELECT event_id FROM board_events WHERE thread_id = ?1)"
                    ),
                    params![thread_id],
                )?;
            }
            let removed = tx.execute(
                "DELETE FROM board_threads WHERE id = ?1 AND scope = 'group'",
                params![thread_id],
            )?;
            tx.commit()?;
            Ok(removed > 0)
        })
    }

    /// Whether `(kind, id)` sits in the group's participant list.
    pub fn is_group_member(
        &self,
        thread_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM board_participants
                 WHERE thread_id = ?1 AND actor_kind = ?2 AND actor_id = ?3)",
                params![thread_id, kind, id],
                |row| row.get(0),
            )
        })
    }

    /// Post to a group thread and schedule its turn-taking wakes.
    ///
    /// Mentioned agents are queued for immediate delivery; with no mentions
    /// the lead is, unless the poster IS the lead. Every other agent gets a
    /// deferred wake [`GROUP_DEFER_SECONDS`] out, which the delivery loop
    /// drops once anyone has replied to the post. An agent reply
    /// (`in_reply_to`) claims its one reply per post here, so a second reply
    /// to the same post is refused before the event is written.
    #[allow(clippy::too_many_arguments)]
    pub fn post_group_message(
        &self,
        thread_id: &str,
        actor: (&str, &str),
        body: &str,
        mentions: &[String],
        in_reply_to: Option<&str>,
        idempotency_key: &str,
        created_at: &str,
    ) -> Result<CoordinationEvent, StoreError> {
        self.post_group_message_inner(
            thread_id,
            actor,
            body,
            mentions,
            in_reply_to,
            idempotency_key,
            created_at,
            None,
            true,
        )
    }

    /// [`Self::post_group_message`] carrying extra payload keys (approval,
    /// question, task_result, artifact cards) merged into the event payload.
    #[allow(clippy::too_many_arguments)]
    pub fn post_group_message_ex(
        &self,
        thread_id: &str,
        actor: (&str, &str),
        body: &str,
        mentions: &[String],
        in_reply_to: Option<&str>,
        idempotency_key: &str,
        created_at: &str,
        extra: Option<serde_json::Value>,
    ) -> Result<CoordinationEvent, StoreError> {
        self.post_group_message_inner(
            thread_id,
            actor,
            body,
            mentions,
            in_reply_to,
            idempotency_key,
            created_at,
            extra,
            true,
        )
    }

    /// A card post that wakes nobody: the event lands on the thread with only
    /// the human delivery row, so the app renders it live without any agent
    /// turn. Used for approval/question/artifact mirrors.
    #[allow(clippy::too_many_arguments)]
    pub fn post_group_card(
        &self,
        thread_id: &str,
        actor: (&str, &str),
        body: &str,
        extra: Option<serde_json::Value>,
        idempotency_key: &str,
        created_at: &str,
    ) -> Result<CoordinationEvent, StoreError> {
        self.post_group_message_inner(
            thread_id,
            actor,
            body,
            &[],
            None,
            idempotency_key,
            created_at,
            extra,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn post_group_message_inner(
        &self,
        thread_id: &str,
        actor: (&str, &str),
        body: &str,
        mentions: &[String],
        in_reply_to: Option<&str>,
        idempotency_key: &str,
        created_at: &str,
        extra: Option<serde_json::Value>,
        wake_agents: bool,
    ) -> Result<CoordinationEvent, StoreError> {
        let event_id = uuid::Uuid::new_v4().to_string();
        let after = |secs: i64| {
            chrono::DateTime::parse_from_rfc3339(created_at)
                .map(|at| (at + chrono::Duration::seconds(secs)).to_rfc3339())
                .unwrap_or_else(|_| created_at.to_string())
        };
        let defer_at = after(GROUP_DEFER_SECONDS);
        let nudge_at = after(GROUP_UNANSWERED_SECONDS);

        let round_lead = group_store_result(self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let scope: Option<String> = tx
                .query_row(
                    "SELECT scope FROM board_threads WHERE id = ?1",
                    params![thread_id],
                    |row| row.get(0),
                )
                .optional()?;
            if scope.as_deref() != Some("group") {
                return Err(invalid_input(format!(
                    "`{thread_id}` is not a group thread"
                )));
            }
            let member: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM board_participants
                     WHERE thread_id = ?1 AND actor_kind = ?2 AND actor_id = ?3",
                    params![thread_id, actor.0, actor.1],
                    |row| row.get(0),
                )
                .optional()?;
            if member.is_none() {
                return Err(invalid_input(format!(
                    "{}:{} is not a member of this group",
                    actor.0, actor.1
                )));
            }
            if let Some(post_id) = in_reply_to {
                let kind: Option<String> = tx
                    .query_row(
                        "SELECT event_type FROM board_events
                         WHERE event_id = ?1 AND thread_id = ?2",
                        params![post_id, thread_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                match kind.as_deref() {
                    Some("group_message") => {}
                    _ => {
                        return Err(invalid_input(format!(
                            "`{post_id}` is not a post in this group"
                        )));
                    }
                }
                if actor.0 != "human" {
                    let claimed = tx.execute(
                        "INSERT OR IGNORE INTO group_replies
                         (post_event_id, agent_id, reply_event_id, claimed_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![post_id, actor.1, event_id, created_at],
                    )?;
                    if claimed == 0 {
                        return Err(invalid_input(format!(
                            "{} already replied to this post",
                            actor.1
                        )));
                    }
                }
            }

            let agents: Vec<String> = tx
                .prepare(
                    "SELECT actor_id FROM board_participants
                     WHERE thread_id = ?1 AND actor_kind = 'agent' ORDER BY actor_id",
                )?
                .query_map(params![thread_id], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let poster_agent = (actor.0 == "agent").then_some(actor.1);
            let nicknames = load_nicknames(&tx)?;
            let everyone = mentions.iter().any(|m| m == "everyone");
            let mut mentioned: Vec<String> = Vec::new();
            if everyone {
                for agent in &agents {
                    if poster_agent != Some(agent.as_str()) {
                        mentioned.push(agent.clone());
                    }
                }
            } else {
                for mention in mentions {
                    let bare = mention.trim().trim_start_matches('@');
                    let resolved = agents.iter().find(|a| a.as_str() == bare).or_else(|| {
                        agents.iter().find(|a| {
                            nicknames
                                .get(a.as_str())
                                .is_some_and(|n| n.trim().eq_ignore_ascii_case(bare))
                        })
                    });
                    if let Some(agent) = resolved
                        && poster_agent != Some(agent.as_str())
                        && !mentioned.contains(agent)
                    {
                        mentioned.push(agent.clone());
                    }
                }
            }
            let names: Vec<(String, Vec<String>)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, display_name, handle FROM agents WHERE id = ?1",
                )?;
                let mut out = Vec::new();
                for agent in &agents {
                    let mut terms: Vec<String> = match stmt
                        .query_row(params![agent], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                        })
                        .optional()?
                    {
                        Some((id, display, handle)) => vec![id, handle, display],
                        None => Vec::new(),
                    };
                    if let Some(nick) = nicknames.get(agent) {
                        terms.push(nick.clone());
                    }
                    if !terms.is_empty() {
                        out.push((agent.clone(), terms));
                    }
                }
                out
            };
            for agent in named_by_terms(body, &names) {
                if poster_agent != Some(agent.as_str()) && !mentioned.contains(&agent) {
                    mentioned.push(agent);
                }
            }
            for agent in &agents {
                if poster_agent != Some(agent.as_str())
                    && explicitly_mentions(
                        body,
                        mentions,
                        agent,
                        nicknames.get(agent).map(String::as_str),
                    )
                {
                    tx.execute(
                        "DELETE FROM group_snoozes
                         WHERE agent_id = ?1 AND thread_id = ?2 AND until_at IS NULL",
                        params![agent, thread_id],
                    )?;
                }
            }
            // An agent's first post in a round (since the latest human post)
            // reaches the other agents; later ones only whom they name, so
            // agents cannot keep waking each other.
            let peers: Vec<String> = match poster_agent {
                Some(poster) => {
                    let earlier: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM board_events
                         WHERE thread_id = ?1 AND event_type = 'group_message'
                           AND actor_kind = 'agent' AND actor_id = ?2
                           AND sequence > COALESCE((
                               SELECT MAX(sequence) FROM board_events
                               WHERE thread_id = ?1 AND event_type = 'group_message'
                                 AND actor_kind = 'human'), 0)",
                        params![thread_id, poster],
                        |row| row.get(0),
                    )?;
                    if earlier == 0 {
                        agents
                            .iter()
                            .filter(|a| a.as_str() != poster && !mentioned.contains(a))
                            .cloned()
                            .collect()
                    } else {
                        Vec::new()
                    }
                }
                None => Vec::new(),
            };
            let lead = resolve_lead(&tx, thread_id, &agents)?;
            let immediate: Vec<String> = if !mentioned.is_empty() || poster_agent.is_some() {
                mentioned
            } else {
                match lead.as_deref() {
                    Some(lead) if poster_agent != Some(lead) => vec![lead.to_string()],
                    _ => Vec::new(),
                }
            };
            let deferred: Vec<String> = agents
                .iter()
                .filter(|a| poster_agent.is_none() && !immediate.contains(a))
                .cloned()
                .collect();
            let round_lead = if actor.0 == "human" && wake_agents {
                match lead.as_deref() {
                    Some(lead) if immediate.iter().any(|a| a == lead) => Some(lead.to_string()),
                    _ => immediate.first().cloned(),
                }
            } else {
                None
            };

            if wake_agents {
            for agent in &immediate {
                tx.execute(
                    "INSERT OR IGNORE INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at)
                     VALUES (?1, 'group', ?2, ?3, ?4)",
                    params![event_id, agent, actor_ref("group", agent), created_at],
                )?;
            }
            for agent in &peers {
                tx.execute(
                    "INSERT OR IGNORE INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at)
                     VALUES (?1, 'group_peer', ?2, ?3, ?4)",
                    params![event_id, agent, actor_ref("group_peer", agent), created_at],
                )?;
            }
            for agent in &deferred {
                tx.execute(
                    "INSERT OR IGNORE INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at, retry_at)
                     VALUES (?1, 'group_deferred', ?2, ?3, ?4, ?5)",
                    params![event_id, agent, actor_ref("group_deferred", agent), created_at, defer_at],
                )?;
            }
            if let (None, Some(lead)) = (poster_agent, lead.as_deref()) {
                tx.execute(
                    "INSERT OR IGNORE INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at, retry_at)
                     VALUES (?1, 'group_unanswered', ?2, ?3, ?4, ?5)",
                    params![event_id, lead, actor_ref("group_unanswered", lead), created_at, nudge_at],
                )?;
            }
            }
            let own_read = (actor.0 == "human").then_some(created_at);
            tx.execute(
                "INSERT OR IGNORE INTO message_deliveries
                 (event_id, recipient_kind, recipient_id, recipient, queued_at, delivered_at, read_at)
                 VALUES (?1, 'human', 'human', ?2, ?3, ?3, ?4)",
                params![event_id, actor_ref("human", "human"), created_at, own_read],
            )?;
            tx.commit()?;
            Ok(round_lead)
        }))?;

        let mut stored: Vec<String> = mentions
            .iter()
            .filter(|m| {
                !m.trim()
                    .trim_start_matches('@')
                    .eq_ignore_ascii_case("human")
            })
            .cloned()
            .collect();
        if actor.0 == "agent" && mentions_human(body, mentions) {
            stored.push("human".into());
        }
        let mut payload = serde_json::json!({
            "body": body,
            "mentions": stored,
            "from_kind": actor.0,
            "from_id": actor.1,
        });
        if let Some(post_id) = in_reply_to {
            payload["in_reply_to"] = serde_json::json!(post_id);
        }
        if let Some(extra) = extra
            && let (Some(dst), Some(src)) = (payload.as_object_mut(), extra.as_object())
        {
            for (key, value) in src {
                dst.insert(key.clone(), value.clone());
            }
        }
        let event = CoordinationEvent {
            event_id,
            thread_id: thread_id.to_string(),
            partition_key: format!("thread:{thread_id}"),
            sequence: 0,
            event_type: "group_message".into(),
            actor_kind: actor.0.to_string(),
            actor_id: actor.1.to_string(),
            payload_version: 1,
            payload,
            causation_id: None,
            correlation_id: None,
            idempotency_key: idempotency_key.to_string(),
            created_at: created_at.to_string(),
        };
        match self.append_event(&event) {
            Ok(saved) => {
                if let Some(lead) = round_lead.as_deref() {
                    self.open_or_coalesce_group_round(thread_id, lead, &saved.event_id, created_at)?;
                } else if actor.0 == "agent" {
                    self.ack_group_round(thread_id, actor.1, created_at)?;
                }
                Ok(saved)
            }
            Err(error) => {
                let _ = self.discard_group_wakes(&event.event_id);
                Err(error)
            }
        }
    }

    /// React to a post in a group. The reaction is an event on the thread with
    /// no body, visible to everyone who reads it, and it wakes nobody: a group
    /// post is not an awaited question, so a reaction there only acknowledges.
    pub fn post_group_reaction(
        &self,
        thread_id: &str,
        actor: (&str, &str),
        target_event_id: &str,
        reaction: &str,
        idempotency_key: &str,
        created_at: &str,
    ) -> Result<CoordinationEvent, StoreError> {
        self.post_group_reaction_ex(
            thread_id,
            actor,
            target_event_id,
            reaction,
            false,
            idempotency_key,
            created_at,
        )
    }

    /// `removed` records taking a reaction back; readers fold events in order.
    pub fn post_group_reaction_ex(
        &self,
        thread_id: &str,
        actor: (&str, &str),
        target_event_id: &str,
        reaction: &str,
        removed: bool,
        idempotency_key: &str,
        created_at: &str,
    ) -> Result<CoordinationEvent, StoreError> {
        let reaction = reaction.trim();
        if reaction.is_empty() {
            return Err(StoreError::InvalidInput("`reaction` is required".into()));
        }
        let event_id = uuid::Uuid::new_v4().to_string();
        group_store_result(self.with_connection(|conn| {
            let scope: Option<String> = conn
                .query_row(
                    "SELECT scope FROM board_threads WHERE id = ?1",
                    params![thread_id],
                    |row| row.get(0),
                )
                .optional()?;
            if scope.as_deref() != Some("group") {
                return Err(invalid_input(format!(
                    "`{thread_id}` is not a group thread"
                )));
            }
            let member: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM board_participants
                     WHERE thread_id = ?1 AND actor_kind = ?2 AND actor_id = ?3",
                    params![thread_id, actor.0, actor.1],
                    |row| row.get(0),
                )
                .optional()?;
            if member.is_none() {
                return Err(invalid_input(format!(
                    "{}:{} is not a member of this group",
                    actor.0, actor.1
                )));
            }
            let target: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT event_type, json_extract(payload_json, '$.reaction') FROM board_events
                     WHERE event_id = ?1 AND thread_id = ?2",
                    params![target_event_id, thread_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if !matches!(target, Some((ref kind, None)) if kind == "group_message") {
                return Err(invalid_input(format!(
                    "`{target_event_id}` is not a post in this group"
                )));
            }
            if actor.0 == "human" {
                conn.execute(
                    "INSERT OR IGNORE INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at, delivered_at, read_at)
                     VALUES (?1, 'human', 'human', ?2, ?3, ?3, ?3)",
                    params![event_id, actor_ref("human", "human"), created_at],
                )?;
            }
            Ok(())
        }))?;

        let mut payload = serde_json::json!({
                "body": "",
                "kind": "reaction",
                "reaction": reaction,
                "reply_to": target_event_id,
                "in_reply_to": target_event_id,
                "mentions": [],
                "from_kind": actor.0,
                "from_id": actor.1,
        });
        if removed {
            payload["removed"] = serde_json::json!(true);
        }
        let event = CoordinationEvent {
            event_id,
            thread_id: thread_id.to_string(),
            partition_key: format!("thread:{thread_id}"),
            sequence: 0,
            event_type: "group_message".into(),
            actor_kind: actor.0.to_string(),
            actor_id: actor.1.to_string(),
            payload_version: 1,
            payload,
            causation_id: None,
            correlation_id: None,
            idempotency_key: idempotency_key.to_string(),
            created_at: created_at.to_string(),
        };
        let saved = self.append_event(&event)?;
        if actor.0 == "agent" && !removed {
            self.ack_group_round(thread_id, actor.1, created_at)?;
        }
        Ok(saved)
    }

    /// Drop the scheduled wakes and reply claim of a post that never landed.
    fn discard_group_wakes(&self, event_id: &str) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM message_deliveries WHERE event_id = ?1
                 AND recipient_kind IN ('group', 'group_deferred', 'group_peer',
                                        'group_unanswered', 'human')",
                params![event_id],
            )?;
            conn.execute(
                "DELETE FROM group_replies WHERE reply_event_id = ?1",
                params![event_id],
            )?;
            Ok(())
        })
    }

    /// Who leads the current round: the set lead, else the agent the human last
    /// addressed, else the first agent.
    pub fn group_round_lead(&self, thread_id: &str) -> Result<Option<String>, StoreError> {
        self.with_connection(|conn| {
            let agents: Vec<String> = conn
                .prepare(
                    "SELECT actor_id FROM board_participants
                     WHERE thread_id = ?1 AND actor_kind = 'agent' ORDER BY actor_id",
                )?
                .query_map(params![thread_id], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            resolve_lead(conn, thread_id, &agents)
        })
    }

    /// Agents woken at once for `post_event_id` (the named ones, or the lead).
    pub fn group_immediate_recipients(
        &self,
        post_event_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT recipient_id FROM message_deliveries
                 WHERE event_id = ?1 AND recipient_kind = 'group' ORDER BY recipient_id",
            )?;
            let rows = stmt
                .query_map(params![post_event_id], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
    }

    /// Push one agent's deferred wake for a post to `retry_at`, while whoever
    /// was woken first is still working on it.
    pub fn postpone_group_delivery(
        &self,
        post_event_id: &str,
        agent_id: &str,
        retry_at: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE message_deliveries SET retry_at = ?3
                 WHERE event_id = ?1 AND recipient_kind = 'group_deferred'
                   AND recipient_id = ?2 AND delivered_at IS NULL",
                params![post_event_id, agent_id, retry_at],
            )?;
            Ok(())
        })
    }

    /// Whether anyone has claimed a reply to `post_event_id`. The deferred
    /// wake checks this when it fires: one answer closes the window for all.
    pub fn group_has_replies(&self, post_event_id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM group_replies WHERE post_event_id = ?1)",
                params![post_event_id],
                |row| row.get(0),
            )
        })
    }

    /// Replies claimed for a post so far, oldest first.
    pub fn group_post_replies(
        &self,
        post_event_id: &str,
    ) -> Result<Vec<CoordinationEvent>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS_QUALIFIED} FROM group_replies r
                 JOIN board_events e ON e.event_id = r.reply_event_id
                 WHERE r.post_event_id = ?1 ORDER BY e.sequence"
            ))?;
            let rows = stmt.query_map(params![post_event_id], event_from_row)?;
            rows.collect()
        })
    }

    /// Group wakes due for delivery, oldest first: immediate `group` rows,
    /// `group_deferred` rows whose claim window has opened, and
    /// `group_unanswered` lead nudges whose grace period has run out.
    pub fn list_pending_group_deliveries(
        &self,
        limit: u32,
        now: &str,
    ) -> Result<Vec<PendingDirectMessage>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS_QUALIFIED}, d.recipient_kind, d.recipient_id
                 FROM message_deliveries d
                 JOIN board_events e ON e.event_id = d.event_id
                 JOIN board_threads t ON t.id = e.thread_id AND t.scope = 'group'
                 WHERE d.recipient_kind IN ('group', 'group_deferred', 'group_peer', 'group_unanswered')
                   AND d.delivered_at IS NULL
                   AND d.attempts < ?2
                   AND (d.retry_at IS NULL OR d.retry_at <= ?3)
                   AND NOT EXISTS (
                        SELECT 1 FROM agents a
                        WHERE a.id = d.recipient_id
                          AND a.status IN ('paused', 'disabled'))
                 ORDER BY e.created_at, e.sequence
                 LIMIT ?1"
            ))?;
            let rows = stmt.query_map(params![limit, MAX_GROUP_DELIVERY_ATTEMPTS, now], |row| {
                Ok(PendingDirectMessage {
                    event: event_from_row(row)?,
                    recipient_kind: row.get(13)?,
                    recipient_id: row.get(14)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Mark the human's group deliveries read up to `up_to_sequence`.
    pub fn mark_group_read(
        &self,
        thread_id: &str,
        up_to_sequence: u64,
        at: &str,
    ) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE message_deliveries SET read_at = ?1
                 WHERE recipient_kind = 'human' AND recipient_id = 'human'
                   AND read_at IS NULL
                   AND event_id IN (SELECT event_id FROM board_events
                                    WHERE thread_id = ?2 AND sequence <= ?3)",
                params![at, thread_id, up_to_sequence],
            )?;
            Ok(())
        })
    }
}

/// How long a round's lead has to engage before the round fails over.
pub const GROUP_ROUND_ACK_SECONDS: i64 = 90;

/// One turn of a group conversation: the human's post (or burst of posts) and
/// the agent leading the answer. Failover moves only this row's lead; the
/// group's configured lead in `group_meta` never changes.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GroupRound {
    pub round_id: String,
    pub thread_id: String,
    pub lead_agent: String,
    pub opened_at: String,
    pub latest_event_id: String,
    pub lead_acked_at: Option<String>,
    pub lead_decision: Option<String>,
    pub failover_count: i64,
    pub status: String,
}

/// A member's quiet period in one group; `until_at` None means until pinged.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GroupSnooze {
    pub thread_id: String,
    pub until_at: Option<String>,
    pub created_at: String,
}

const ROUND_COLUMNS: &str = "round_id, thread_id, lead_agent, opened_at, latest_event_id, lead_acked_at, lead_decision, failover_count, status";

fn round_from_row(row: &rusqlite::Row<'_>) -> Result<GroupRound, rusqlite::Error> {
    Ok(GroupRound {
        round_id: row.get(0)?,
        thread_id: row.get(1)?,
        lead_agent: row.get(2)?,
        opened_at: row.get(3)?,
        latest_event_id: row.get(4)?,
        lead_acked_at: row.get(5)?,
        lead_decision: row.get(6)?,
        failover_count: row.get(7)?,
        status: row.get(8)?,
    })
}

fn open_round(
    conn: &rusqlite::Connection,
    thread_id: &str,
) -> Result<Option<GroupRound>, rusqlite::Error> {
    conn.query_row(
        &format!(
            "SELECT {ROUND_COLUMNS} FROM group_rounds
             WHERE thread_id = ?1 AND status = 'open'
             ORDER BY opened_at DESC LIMIT 1"
        ),
        params![thread_id],
        round_from_row,
    )
    .optional()
}

fn ts(at: &str) -> Result<String, StoreError> {
    super::desk_inbox::normalize_ts(at)
}

impl Store {
    /// Snooze `agent_id` in a group until `until_at`, or until it is pinged.
    pub fn set_group_snooze(
        &self,
        agent_id: &str,
        thread_id: &str,
        until_at: Option<&str>,
        now: &str,
    ) -> Result<GroupSnooze, StoreError> {
        let now = ts(now)?;
        let until = until_at.map(ts).transpose()?;
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO group_snoozes (agent_id, thread_id, until_at, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent_id, thread_id) DO UPDATE
                 SET until_at = excluded.until_at, created_at = excluded.created_at",
                params![agent_id, thread_id, until, now],
            )
        })?;
        Ok(GroupSnooze {
            thread_id: thread_id.to_string(),
            until_at: until,
            created_at: now,
        })
    }

    pub fn clear_group_snooze(&self, agent_id: &str, thread_id: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM group_snoozes WHERE agent_id = ?1 AND thread_id = ?2",
                params![agent_id, thread_id],
            )
            .map(|n| n > 0)
        })
    }

    pub fn is_group_snoozed(
        &self,
        agent_id: &str,
        thread_id: &str,
        now: &str,
    ) -> Result<bool, StoreError> {
        let now = ts(now)?;
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM group_snoozes
                 WHERE agent_id = ?1 AND thread_id = ?2
                   AND (until_at IS NULL OR until_at > ?3))",
                params![agent_id, thread_id, now],
                |row| row.get(0),
            )
        })
    }

    /// The agent's snoozes still in force, soonest to lapse first.
    pub fn list_active_group_snoozes(
        &self,
        agent_id: &str,
        now: &str,
    ) -> Result<Vec<GroupSnooze>, StoreError> {
        let now = ts(now)?;
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT thread_id, until_at, created_at FROM group_snoozes
                 WHERE agent_id = ?1 AND (until_at IS NULL OR until_at > ?2)
                 ORDER BY until_at IS NULL, until_at, thread_id",
            )?;
            let rows = stmt.query_map(params![agent_id, now], |r| {
                Ok(GroupSnooze {
                    thread_id: r.get(0)?,
                    until_at: r.get(1)?,
                    created_at: r.get(2)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Whether a snooze holds back this wake: optional wakes (`group_deferred`,
    /// `group_peer`) unless the agent is explicitly mentioned, and the lead's
    /// `group` wake for a post that does not name it at all.
    pub fn group_wake_snoozed(
        &self,
        pending: &PendingDirectMessage,
        now: &str,
    ) -> Result<bool, StoreError> {
        if !matches!(pending.recipient_kind.as_str(), "group" | "group_deferred" | "group_peer") {
            return Ok(false);
        }
        let agent = pending.recipient_id.as_str();
        if !self.is_group_snoozed(agent, &pending.event.thread_id, now)? {
            return Ok(false);
        }
        let payload = &pending.event.payload;
        let body = payload.get("body").and_then(|v| v.as_str()).unwrap_or_default();
        let mentions: Vec<String> = payload
            .get("mentions")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let nickname = self.agent_nicknames()?.remove(agent);
        if explicitly_mentions(body, &mentions, agent, nickname.as_deref()) {
            return Ok(false);
        }
        if pending.recipient_kind == "group" {
            let mut terms = vec![agent.to_string()];
            if let Some(known) = self.get_agent(agent)? {
                terms.push(known.handle);
                terms.push(known.display_name);
            }
            terms.extend(nickname);
            return Ok(named_by_terms(body, &[(agent.to_string(), terms)]).is_empty());
        }
        Ok(true)
    }

    /// The thread's open round, if any.
    pub fn open_group_round(&self, thread_id: &str) -> Result<Option<GroupRound>, StoreError> {
        self.with_connection(|conn| open_round(conn, thread_id))
    }

    /// Fold a human post into the thread's open round while nobody has
    /// engaged with it yet (a burst is one round); otherwise close that round
    /// and open a new one led by `lead_agent`.
    pub fn open_or_coalesce_group_round(
        &self,
        thread_id: &str,
        lead_agent: &str,
        event_id: &str,
        now: &str,
    ) -> Result<GroupRound, StoreError> {
        let now = ts(now)?;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            if let Some(round) = open_round(&tx, thread_id)? {
                let answered: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM board_events r
                     WHERE r.thread_id = ?1 AND r.event_type = 'group_message'
                       AND r.actor_kind = 'agent'
                       AND json_extract(r.payload_json, '$.reaction') IS NULL
                       AND r.sequence > COALESCE(
                           (SELECT sequence FROM board_events WHERE event_id = ?2), 0))",
                    params![thread_id, round.latest_event_id],
                    |row| row.get(0),
                )?;
                if round.lead_acked_at.is_none() && !answered {
                    tx.execute(
                        "UPDATE group_rounds SET latest_event_id = ?2 WHERE round_id = ?1",
                        params![round.round_id, event_id],
                    )?;
                    tx.commit()?;
                    return Ok(GroupRound {
                        latest_event_id: event_id.to_string(),
                        ..round
                    });
                }
                tx.execute(
                    "UPDATE group_rounds SET status = 'closed' WHERE round_id = ?1",
                    params![round.round_id],
                )?;
            }
            let raw = uuid::Uuid::new_v4().simple().to_string();
            let round = GroupRound {
                round_id: format!("round-{}", &raw[..12]),
                thread_id: thread_id.to_string(),
                lead_agent: lead_agent.to_string(),
                opened_at: now.clone(),
                latest_event_id: event_id.to_string(),
                lead_acked_at: None,
                lead_decision: None,
                failover_count: 0,
                status: "open".into(),
            };
            tx.execute(
                "INSERT INTO group_rounds
                 (round_id, thread_id, lead_agent, opened_at, latest_event_id, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'open')",
                params![
                    round.round_id,
                    round.thread_id,
                    round.lead_agent,
                    round.opened_at,
                    round.latest_event_id
                ],
            )?;
            tx.commit()?;
            Ok(round)
        })
    }

    /// The round lead engaged (posted or reacted): its failover clock stops.
    pub fn ack_group_round(
        &self,
        thread_id: &str,
        agent_id: &str,
        now: &str,
    ) -> Result<bool, StoreError> {
        let now = ts(now)?;
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE group_rounds SET lead_acked_at = ?3
                 WHERE thread_id = ?1 AND lead_agent = ?2 AND status = 'open'
                   AND lead_acked_at IS NULL",
                params![thread_id, agent_id, now],
            )
            .map(|n| n > 0)
        })
    }

    /// Close the thread's open round on its lead's word, recording the
    /// decision. Fenced: only the current round lead releases. The round's
    /// pending owed-reply nudges are dropped, since the lead has settled it.
    pub fn release_group_round(
        &self,
        thread_id: &str,
        by_agent: &str,
        decision: Option<&str>,
        now: &str,
    ) -> Result<Option<GroupRound>, StoreError> {
        let now = ts(now)?;
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let Some(round) = open_round(&tx, thread_id)? else {
                return Ok(None);
            };
            let changed = tx.execute(
                "UPDATE group_rounds
                 SET status = 'released', lead_acked_at = COALESCE(lead_acked_at, ?3),
                     lead_decision = ?4
                 WHERE round_id = ?1 AND lead_agent = ?2 AND status = 'open'",
                params![round.round_id, by_agent, now, decision],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            tx.execute(
                "UPDATE message_deliveries SET delivered_at = ?2
                 WHERE recipient_kind = 'group_unanswered' AND delivered_at IS NULL
                   AND event_id IN (SELECT event_id FROM board_events WHERE thread_id = ?1)",
                params![thread_id, now],
            )?;
            let released = tx.query_row(
                &format!("SELECT {ROUND_COLUMNS} FROM group_rounds WHERE round_id = ?1"),
                params![round.round_id],
                round_from_row,
            )?;
            tx.commit()?;
            Ok(Some(released))
        })
    }

    /// Hand one round to `next_lead`, only while `expected_lead` still holds
    /// it unacknowledged. The group's configured lead is left alone.
    pub fn failover_group_round(
        &self,
        round_id: &str,
        expected_lead: &str,
        next_lead: &str,
        now: &str,
    ) -> Result<bool, StoreError> {
        let now = ts(now)?;
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE group_rounds
                 SET lead_agent = ?3, opened_at = ?4, failover_count = failover_count + 1
                 WHERE round_id = ?1 AND lead_agent = ?2 AND status = 'open'
                   AND lead_acked_at IS NULL",
                params![round_id, expected_lead, next_lead, now],
            )
            .map(|n| n > 0)
        })
    }

    /// Park a round nobody can take, so its one notice is posted once.
    pub fn stall_group_round(&self, round_id: &str, expected_lead: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "UPDATE group_rounds SET status = 'stalled'
                 WHERE round_id = ?1 AND lead_agent = ?2 AND status = 'open'
                   AND lead_acked_at IS NULL",
                params![round_id, expected_lead],
            )
            .map(|n| n > 0)
        })
    }

    /// Open rounds whose lead has not engaged yet, oldest first.
    pub fn list_unacked_group_rounds(&self) -> Result<Vec<GroupRound>, StoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ROUND_COLUMNS} FROM group_rounds
                 WHERE status = 'open' AND lead_acked_at IS NULL ORDER BY opened_at"
            ))?;
            let rows = stmt.query_map([], round_from_row)?;
            rows.collect()
        })
    }

    /// Whether any agent has posted in the thread after `after_seq`
    /// (reactions aside): one reply answers a whole burst.
    pub fn has_agent_reply_after(&self, thread_id: &str, after_seq: u64) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM board_events
                 WHERE thread_id = ?1 AND event_type = 'group_message'
                   AND actor_kind = 'agent' AND sequence > ?2
                   AND json_extract(payload_json, '$.reaction') IS NULL)",
                params![thread_id, after_seq as i64],
                |row| row.get(0),
            )
        })
    }

    /// Whether a deferred wake for `event` is moot: the post was claimed, or
    /// any agent has spoken in the thread since.
    pub fn group_post_answered(&self, event: &CoordinationEvent) -> Result<bool, StoreError> {
        Ok(self.group_has_replies(&event.event_id)?
            || self.has_agent_reply_after(&event.thread_id, event.sequence)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    #[test]
    fn human_mentions_by_field_or_text() {
        let none: Vec<String> = Vec::new();
        assert!(mentions_human("", &["human".into()]));
        assert!(mentions_human("", &["@Human".into()]));
        assert!(mentions_human("@human which one?", &none));
        assert!(mentions_human("Need you, @human.", &none));
        assert!(!mentions_human("", &["@boss".into()]));
        assert!(!mentions_human("@boss which one?", &none));
        assert!(!mentions_human("@humans assemble", &none));
        assert!(!mentions_human("mail me@human.com", &none));
        assert!(!mentions_human("the humanist view", &none));
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

    const T0: &str = "2026-01-01T00:00:00+00:00";
    const T1: &str = "2026-01-01T00:00:01+00:00";
    const T2: &str = "2026-01-01T00:00:20+00:00";

    fn three_agent_group() -> (Store, String) {
        let db = Store::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            db.create_agent(&worker(id)).unwrap();
        }
        let group = db
            .create_group(
                "build",
                ("human", "human"),
                &["a".into(), "b".into(), "c".into()],
                None,
                T0,
            )
            .unwrap();
        (db, group.id)
    }

    fn pending(db: &Store, now: &str) -> Vec<PendingDirectMessage> {
        db.list_pending_group_deliveries(50, now).unwrap()
    }

    fn kinds(pending: &[PendingDirectMessage]) -> Vec<(String, String)> {
        pending
            .iter()
            .map(|p| (p.recipient_kind.clone(), p.recipient_id.clone()))
            .collect()
    }

    #[test]
    fn create_group_lists_members_lead_and_creator() {
        let (db, id) = three_agent_group();
        let group = db.get_group(&id).unwrap().unwrap();
        assert_eq!(group.title, "build");
        assert_eq!(group.lead, None);
        assert_eq!(group.members.len(), 4);
        assert!(
            group
                .members
                .iter()
                .any(|m| m.kind == "human" && m.id == "human")
        );
        assert!(db.is_group_member(&id, "agent", "a").unwrap());
        assert_eq!(db.list_groups().unwrap().len(), 1);
        assert!(db.get_group("direct:x").unwrap().is_none());
    }

    #[test]
    fn a_group_reaction_lands_on_the_thread_and_wakes_nobody() {
        let (db, id) = three_agent_group();
        let post = db
            .post_group_message(&id, ("agent", "a"), "shipped", &[], None, "k1", T0)
            .unwrap();
        let before = db.list_pending_group_deliveries(50, T2).unwrap().len();
        let reaction = db
            .post_group_reaction(
                &id,
                ("human", "human"),
                &post.event_id,
                "thumbs_up",
                "r1",
                T2,
            )
            .unwrap();
        assert_eq!(reaction.payload["reaction"], "thumbs_up");
        assert_eq!(reaction.payload["reply_to"], post.event_id);
        assert_eq!(reaction.payload["body"], "");
        assert_eq!(
            db.list_pending_group_deliveries(50, T2).unwrap().len(),
            before,
            "a reaction schedules no group wake"
        );
        let last = db.list_groups().unwrap()[0].last_message.clone().unwrap();
        assert_eq!(last.body, "shipped", "the preview skips reactions");
        let events = db.events_for_thread(&id, 0, 10).unwrap();
        assert_eq!(events.len(), 2, "the reaction is an event on the thread");

        let on_reaction =
            db.post_group_reaction(&id, ("agent", "b"), &reaction.event_id, "check", "r2", T2);
        assert!(
            on_reaction.is_err(),
            "reactions target posts, not other reactions"
        );
        let outsider =
            db.post_group_reaction(&id, ("agent", "ghost"), &post.event_id, "check", "r3", T2);
        assert!(outsider.is_err(), "only members react");
        let removed = db
            .post_group_reaction_ex(
                &id,
                ("human", "human"),
                &post.event_id,
                "thumbs_up",
                true,
                "r4",
                T2,
            )
            .unwrap();
        assert_eq!(removed.payload["removed"], true);
        assert!(reaction.payload.get("removed").is_none());
        assert_eq!(
            db.list_pending_group_deliveries(50, T2).unwrap().len(),
            before,
            "a removal wakes nobody either"
        );
    }

    #[test]
    fn list_groups_carries_the_newest_post() {
        let (db, id) = three_agent_group();
        assert!(db.list_groups().unwrap()[0].last_message.is_none());
        db.post_group_message(&id, ("human", "human"), "first", &[], None, "k1", T0)
            .unwrap();
        db.post_group_message(&id, ("agent", "a"), "second", &[], None, "k2", T2)
            .unwrap();
        let last = db.list_groups().unwrap()[0].last_message.clone().unwrap();
        assert_eq!((last.from_id.as_str(), last.body.as_str()), ("a", "second"));
    }

    #[test]
    fn delete_group_removes_posts_and_pending_wakes() {
        let (db, id) = three_agent_group();
        db.post_group_message(&id, ("human", "human"), "hi", &[], None, "k1", T0)
            .unwrap();
        assert!(!pending(&db, T2).is_empty());
        assert!(db.delete_group(&id).unwrap());
        assert!(db.get_group(&id).unwrap().is_none());
        assert!(pending(&db, T2).is_empty());
        assert!(!db.delete_group(&id).unwrap());
    }

    #[test]
    fn a_mention_wakes_only_the_mentioned_agent_immediately() {
        let (db, id) = three_agent_group();
        db.post_group_message(
            &id,
            ("human", "human"),
            "@b status?",
            &["b".into()],
            None,
            "k1",
            T0,
        )
        .unwrap();
        let now = pending(&db, T0);
        assert_eq!(kinds(&now), vec![("group".to_string(), "b".to_string())]);
        let later = pending(&db, T2);
        let mut all: Vec<_> = kinds(&later);
        all.sort();
        assert_eq!(
            all,
            vec![
                ("group".into(), "b".into()),
                ("group_deferred".into(), "a".into()),
                ("group_deferred".into(), "c".into()),
            ]
        );
    }

    #[test]
    fn the_round_lead_follows_whom_the_human_addressed() {
        let (db, id) = three_agent_group();
        assert_eq!(db.group_round_lead(&id).unwrap().as_deref(), Some("a"));
        db.post_group_message(
            &id,
            ("human", "human"),
            "@b hi",
            &["b".into()],
            None,
            "k1",
            T0,
        )
        .unwrap();
        assert_eq!(db.group_round_lead(&id).unwrap().as_deref(), Some("b"));
        db.set_group_lead(&id, "c").unwrap();
        assert_eq!(db.group_round_lead(&id).unwrap().as_deref(), Some("c"));
    }

    #[test]
    fn no_mention_wakes_the_lead_and_defers_the_rest() {
        let (db, id) = three_agent_group();
        db.post_group_message(&id, ("human", "human"), "hello", &[], None, "k1", T0)
            .unwrap();
        // No human has addressed anyone yet: the first agent participant leads.
        assert_eq!(
            kinds(&pending(&db, T0)),
            vec![("group".to_string(), "a".to_string())]
        );
        // The human addresses b; the next unmentioned post goes to b.
        db.post_group_message(
            &id,
            ("human", "human"),
            "@b hi",
            &["b".into()],
            None,
            "k2",
            T1,
        )
        .unwrap();
        let p3 = db
            .post_group_message(&id, ("human", "human"), "and?", &[], None, "k3", T1)
            .unwrap();
        let immediate: Vec<_> = pending(&db, T1)
            .into_iter()
            .filter(|p| p.recipient_kind == "group" && p.event.event_id == p3.event_id)
            .map(|p| p.recipient_id)
            .collect();
        assert_eq!(immediate, vec!["b".to_string()]);
    }

    #[test]
    fn an_unanswered_human_post_nudges_the_lead_after_the_grace_period() {
        let (db, id) = three_agent_group();
        let post = db
            .post_group_message(&id, ("human", "human"), "anyone?", &[], None, "k1", T0)
            .unwrap();
        let later = (chrono::DateTime::parse_from_rfc3339(T0).unwrap()
            + chrono::Duration::seconds(GROUP_UNANSWERED_SECONDS + 1))
        .to_rfc3339();
        let nudges = |at: &str| {
            pending(&db, at)
                .into_iter()
                .filter(|p| p.recipient_kind == "group_unanswered")
                .map(|p| (p.event.event_id.clone(), p.recipient_id))
                .collect::<Vec<_>>()
        };
        assert!(
            nudges(T1).is_empty(),
            "nudge must wait out the grace period"
        );
        assert_eq!(
            nudges(&later),
            vec![(post.event_id.clone(), "a".to_string())]
        );
        // An agent's own post never queues a nudge: agents must not wake each other forever.
        db.post_group_message(&id, ("agent", "b"), "fyi", &[], None, "k2", T0)
            .unwrap();
        assert_eq!(nudges(&later).len(), 1);
    }

    #[test]
    fn an_everyone_mention_wakes_every_agent_member() {
        let (db, id) = three_agent_group();
        db.post_group_message(
            &id,
            ("human", "human"),
            "@everyone ship it",
            &["everyone".into()],
            None,
            "k1",
            T0,
        )
        .unwrap();
        let mut immediate: Vec<_> = pending(&db, T0)
            .into_iter()
            .filter(|p| p.recipient_kind == "group")
            .map(|p| p.recipient_id)
            .collect();
        immediate.sort();
        assert_eq!(
            immediate,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // Unknown ids are ignored, not an error.
        db.post_group_message(
            &id,
            ("human", "human"),
            "@ghost hi",
            &["ghost".into(), "b".into()],
            None,
            "k2",
            T1,
        )
        .unwrap();
    }

    #[test]
    fn an_agents_first_post_in_a_round_reaches_peers_then_only_named() {
        let (db, id) = three_agent_group();
        db.set_group_lead(&id, "b").unwrap();
        db.post_group_message(&id, ("agent", "a"), "taking this", &[], None, "k1", T0)
            .unwrap();
        assert_eq!(
            kinds(&pending(&db, T0)),
            vec![
                ("group_peer".into(), "b".into()),
                ("group_peer".into(), "c".into())
            ]
        );
        for p in pending(&db, T0) {
            db.mark_direct_delivered(&p.event.event_id, &p.recipient_kind, &p.recipient_id, T0)
                .unwrap();
        }
        db.post_group_message(
            &id,
            ("agent", "a"),
            "@c over to you",
            &["c".into()],
            None,
            "k2",
            T1,
        )
        .unwrap();
        assert_eq!(kinds(&pending(&db, T2)), vec![("group".into(), "c".into())]);
        db.post_group_message(
            &id,
            ("agent", "b"),
            "@b note to self",
            &["b".into()],
            None,
            "k3",
            T2,
        )
        .unwrap();
        let after = kinds(&pending(&db, T2));
        assert!(
            !after.iter().any(|(_, a)| a == "b"),
            "an agent never wakes itself: {after:?}"
        );
        assert!(
            after.contains(&("group_peer".into(), "a".into())),
            "{after:?}"
        );
    }

    #[test]
    fn a_reply_claims_the_post_and_a_second_reply_is_refused() {
        let (db, id) = three_agent_group();
        let post = db
            .post_group_message(
                &id,
                ("human", "human"),
                "status?",
                &["a".into()],
                None,
                "k1",
                T0,
            )
            .unwrap();
        let reply = db
            .post_group_message(
                &id,
                ("agent", "a"),
                "done",
                &[],
                Some(&post.event_id),
                "k2",
                T1,
            )
            .unwrap();
        assert_eq!(reply.payload["in_reply_to"], post.event_id);
        assert!(db.group_has_replies(&post.event_id).unwrap());
        let second = db.post_group_message(
            &id,
            ("agent", "a"),
            "again",
            &[],
            Some(&post.event_id),
            "k3",
            T1,
        );
        assert!(second.is_err(), "one reply per agent per post");
        // A different agent may still reply to the same post.
        db.post_group_message(
            &id,
            ("agent", "b"),
            "same",
            &[],
            Some(&post.event_id),
            "k4",
            T1,
        )
        .unwrap();
    }

    #[test]
    fn replies_must_name_a_post_in_the_same_group() {
        let (db, id) = three_agent_group();
        let err = db
            .post_group_message(&id, ("agent", "a"), "hi", &[], Some("nope"), "k1", T0)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
        let err = db
            .post_group_message(&id, ("agent", "ghost"), "hi", &[], None, "k2", T0)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    }

    /// The spec's scripted scenario with a fake deliverer: pending rows are
    /// the wake list; delivery marks them done, and a deferred wake is
    /// skipped once the post has a reply.
    #[test]
    fn naming_a_member_in_the_body_counts_as_a_mention() {
        let m = vec![
            (
                "x-scout".to_string(),
                "X Scout".to_string(),
                "x_scout".to_string(),
            ),
            (
                "snippet".to_string(),
                "Snippet".to_string(),
                "snippet".to_string(),
            ),
        ];
        assert_eq!(
            named_in_body("`x-scout` can take triage", &m),
            vec!["x-scout"]
        );
        assert_eq!(named_in_body("Ask X Scout.", &m), vec!["x-scout"]);
        assert_eq!(named_in_body("@x_scout over to you", &m), vec!["x-scout"]);
        assert!(named_in_body("see ~/code/snippet-service and snippet.db", &m).is_empty());
        assert!(named_in_body("the x-scouting job", &m).is_empty());
    }

    #[test]
    fn scripted_three_agent_turn_taking() {
        let (db, id) = three_agent_group();
        let mut delivered: Vec<(String, String, String)> = Vec::new();
        fn deliver(db: &Store, now: &str, delivered: &mut Vec<(String, String, String)>) {
            for pending in pending(db, now) {
                {
                    delivered.push((
                        pending.event.event_id.clone(),
                        pending.recipient_kind.clone(),
                        pending.recipient_id.clone(),
                    ));
                }
                db.mark_direct_delivered(
                    &pending.event.event_id,
                    &pending.recipient_kind,
                    &pending.recipient_id,
                    now,
                )
                .unwrap();
            }
        }

        // The human asks b directly: only b is woken.
        let p1 = db
            .post_group_message(
                &id,
                ("human", "human"),
                "@b what is the build status",
                &["b".into()],
                None,
                "k1",
                T0,
            )
            .unwrap();
        deliver(&db, T0, &mut delivered);
        assert_eq!(
            delivered,
            vec![(p1.event_id.clone(), "group".into(), "b".into())]
        );

        // b answers the mention.
        db.post_group_message(
            &id,
            ("agent", "b"),
            "green",
            &[],
            Some(&p1.event_id),
            "k2",
            T1,
        )
        .unwrap();
        deliver(&db, T1, &mut delivered);

        // An unmentioned follow-up: the lead — b, last addressed — is woken;
        // a and c wait out the claim window.
        let p2 = db
            .post_group_message(
                &id,
                ("human", "human"),
                "and the tests?",
                &[],
                None,
                "k3",
                T2,
            )
            .unwrap();
        deliver(&db, T2, &mut delivered);
        assert!(
            delivered
                .iter()
                .any(|(e, k, a)| e == &p2.event_id && k == "group" && a == "b"),
            "the lead takes the unmentioned post: {delivered:?}"
        );

        // b replies inside the window; when it closes, a's and c's deferred
        // wakes find a reply and are skipped.
        db.post_group_message(
            &id,
            ("agent", "b"),
            "they pass",
            &[],
            Some(&p2.event_id),
            "k4",
            T2,
        )
        .unwrap();
        deliver(&db, "2026-01-01T00:00:31+00:00", &mut delivered);
        let mut deferred: Vec<&str> = delivered
            .iter()
            .filter(|(e, k, _)| e == &p2.event_id && k == "group_deferred")
            .map(|(_, _, a)| a.as_str())
            .collect();
        deferred.sort();
        assert_eq!(
            deferred,
            vec!["a", "c"],
            "a and c still get the follow-up, as context, once b replied: {delivered:?}"
        );
        assert_eq!(db.group_post_replies(&p2.event_id).unwrap().len(), 1);
    }

    fn session(db: &Store, id: &str, status: &str) {
        db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, title, status, created_at, updated_at)
                 VALUES (?1, 'k', '/tmp', ?1, ?2, ?3, ?3)",
                rusqlite::params![id, status, T0],
            )
        })
        .unwrap();
    }

    #[test]
    fn replying_is_a_delivered_wake_with_no_answer_and_a_busy_session() {
        let (db, id) = three_agent_group();
        let post = db
            .post_group_message(
                &id,
                ("human", "human"),
                "@b status?",
                &["b".into()],
                None,
                "k1",
                T0,
            )
            .unwrap();
        // Delivered, but b has no inbox session yet.
        db.mark_direct_delivered(&post.event_id, "group", "b", T0)
            .unwrap();
        assert!(db.room_replying(&id).unwrap().is_empty());
        // The deferred wakes for a and c are still pending: not replying.
        session(&db, "inbox-b", "running");
        assert_eq!(db.room_replying(&id).unwrap(), vec!["b".to_string()]);
        assert_eq!(
            db.get_group(&id).unwrap().unwrap().replying,
            vec!["b".to_string()]
        );
        // b answers; its own later post closes the obligation.
        db.post_group_message(
            &id,
            ("agent", "b"),
            "all green",
            &[],
            Some(&post.event_id),
            "k2",
            T1,
        )
        .unwrap();
        assert!(db.room_replying(&id).unwrap().is_empty());
        // An idle session is not replying even with an unanswered wake.
        let p2 = db
            .post_group_message(
                &id,
                ("human", "human"),
                "@c ping",
                &["c".into()],
                None,
                "k3",
                T2,
            )
            .unwrap();
        db.mark_direct_delivered(&p2.event_id, "group", "c", T2)
            .unwrap();
        session(&db, "inbox-c", "idle");
        assert!(db.room_replying(&id).unwrap().is_empty());
        db.with_connection(|conn| {
            conn.execute(
                "UPDATE sessions SET status = 'running' WHERE id = 'inbox-c'",
                [],
            )
        })
        .unwrap();
        assert_eq!(db.room_replying(&id).unwrap(), vec!["c".to_string()]);
    }

    #[test]
    fn room_replying_on_a_task_room_is_the_busy_seated_members() {
        let db = Store::open_in_memory().unwrap();
        let mut task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "T".into(),
            "Brief".into(),
            "s1".into(),
            0,
            T0.into(),
        );
        task.status = crate::coordination::TaskStatus::InProgress;
        db.create_task(&task).unwrap();
        db.create_agent(&worker("w")).unwrap();
        db.add_task_agent_full("t1", "w", "worker", Some("ws-1"), "", "active", T0)
            .unwrap();
        session(&db, "ws-1", "running");
        assert_eq!(
            db.room_replying(&task.thread_id).unwrap(),
            vec!["w".to_string()]
        );
        db.with_connection(|conn| {
            conn.execute("UPDATE sessions SET status = 'idle' WHERE id = 'ws-1'", [])
        })
        .unwrap();
        assert!(db.room_replying(&task.thread_id).unwrap().is_empty());
    }
}

#[cfg(test)]
mod round_tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    const T0: &str = "2026-01-01T00:00:00+00:00";
    const T1: &str = "2026-01-01T00:00:01+00:00";
    const T2: &str = "2026-01-01T00:00:02+00:00";

    fn agent(id: &str, display: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: display.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    fn group(ids: &[(&str, &str)], lead: &str) -> (Store, String) {
        let db = Store::open_in_memory().unwrap();
        for (id, display) in ids {
            db.create_agent(&agent(id, display)).unwrap();
        }
        let members: Vec<String> = ids.iter().map(|(id, _)| id.to_string()).collect();
        let g = db
            .create_group("build", ("human", "human"), &members, Some(lead), T0)
            .unwrap();
        (db, g.id)
    }

    fn immediate(db: &Store, event_id: &str) -> Vec<String> {
        db.group_immediate_recipients(event_id).unwrap()
    }

    fn human(db: &Store, g: &str, body: &str, at: &str) -> CoordinationEvent {
        db.post_group_message(g, ("human", "human"), body, &[], None, &uuid::Uuid::new_v4().to_string(), at)
            .unwrap()
    }

    fn pending_for(db: &Store, event_id: &str, kind: &str, agent: &str) -> PendingDirectMessage {
        db.list_pending_group_deliveries(100, "2999-01-01T00:00:00Z")
            .unwrap()
            .into_iter()
            .find(|p| p.event.event_id == event_id && p.recipient_kind == kind && p.recipient_id == agent)
            .unwrap_or_else(|| panic!("no {kind} wake for {agent}"))
    }

    #[test]
    fn nicknames_route_mentions_and_bare_snippet_is_prose() {
        let (db, g) = group(
            &[("mobile-design", "Mobile Design"), ("snippet", "Snippet"), ("x-scout", "X Scout")],
            "x-scout",
        );
        db.set_agent_nickname("mobile-design", Some("Sashy")).unwrap();

        let p = human(&db, &g, "Sashy, can you redo the onboarding screen?", T0);
        assert_eq!(immediate(&db, &p.event_id), vec!["mobile-design"]);

        let p = db
            .post_group_message(&g, ("human", "human"), "thoughts?", &["@sashy".into()], None, "k-nick", T1)
            .unwrap();
        assert_eq!(immediate(&db, &p.event_id), vec!["mobile-design"]);

        let p = human(&db, &g, "the snippet app crashes on launch", T1);
        assert_eq!(immediate(&db, &p.event_id), vec!["x-scout"], "bare snippet is the product");

        let p = human(&db, &g, "@snippet please look at the crash", T2);
        assert_eq!(immediate(&db, &p.event_id), vec!["snippet"]);

        let terms = vec![("snippet".to_string(), vec!["snippet".to_string(), "Snippet".to_string()])];
        assert!(named_by_terms("Snippet is slow today.", &terms).is_empty());
        assert_eq!(named_by_terms("ask @Snippet.", &terms), vec!["snippet"]);
        assert!(named_by_terms("mail me@snippet.dev", &terms).is_empty());
    }

    #[test]
    fn any_later_agent_reply_answers_a_whole_burst() {
        let (db, g) = group(&[("a", "a"), ("b", "b")], "a");
        let p1 = human(&db, &g, "first thought", T0);
        let p2 = human(&db, &g, "and another", T1);
        assert!(!db.group_post_answered(&p1).unwrap());
        assert!(!db.group_post_answered(&p2).unwrap());
        let _ = pending_for(&db, &p1.event_id, "group_unanswered", "a");
        let _ = pending_for(&db, &p1.event_id, "group_deferred", "b");

        db.post_group_message(&g, ("agent", "a"), "covering both", &[], Some(&p2.event_id), "r1", T2)
            .unwrap();
        assert!(db.group_post_answered(&p1).unwrap(), "the owed-reply nudge for p1 is moot");
        assert!(db.group_post_answered(&p2).unwrap(), "the deferred wake for p2 is moot");
        assert!(!db.has_agent_reply_after(&g, p2.sequence + 1).unwrap());
    }

    #[test]
    fn a_burst_is_one_round_and_a_reply_starts_the_next() {
        let (db, g) = group(&[("a", "a"), ("b", "b")], "a");
        let p1 = human(&db, &g, "one", T0);
        let p2 = human(&db, &g, "two", T1);
        let round = db.open_group_round(&g).unwrap().unwrap();
        assert_eq!(round.latest_event_id, p2.event_id);
        assert_eq!(db.list_unacked_group_rounds().unwrap().len(), 1);
        db.post_group_message(&g, ("agent", "a"), "done", &[], Some(&p1.event_id), "r", T2)
            .unwrap();
        assert!(db.list_unacked_group_rounds().unwrap().is_empty(), "the lead's post acks the round");
        human(&db, &g, "three", T2);
        let next = db.open_group_round(&g).unwrap().unwrap();
        assert_ne!(next.round_id, round.round_id);
    }

    #[test]
    fn snoozes_hold_optional_wakes_but_not_explicit_mentions() {
        let (db, g) = group(&[("a", "a"), ("b", "b"), ("c", "c")], "a");
        let now = "2026-01-01T00:00:05Z";
        db.set_group_snooze("b", &g, None, T0).unwrap();
        db.set_group_snooze("c", &g, Some("2026-01-01T01:00:00Z"), T0).unwrap();
        assert_eq!(db.list_active_group_snoozes("b", now).unwrap().len(), 1);

        let p = human(&db, &g, "status?", T1);
        assert!(db.group_wake_snoozed(&pending_for(&db, &p.event_id, "group_deferred", "b"), now).unwrap());
        assert!(db.group_wake_snoozed(&pending_for(&db, &p.event_id, "group_deferred", "c"), now).unwrap());
        assert!(
            !db.group_wake_snoozed(&pending_for(&db, &p.event_id, "group_deferred", "c"), "2026-01-01T02:00:00Z")
                .unwrap(),
            "a timed snooze lapses"
        );
        assert!(!db.group_wake_snoozed(&pending_for(&db, &p.event_id, "group_unanswered", "a"), now).unwrap());

        let mut addressed = pending_for(&db, &p.event_id, "group_deferred", "c");
        addressed.event.payload["body"] = serde_json::json!("@c can you check?");
        assert!(!db.group_wake_snoozed(&addressed, now).unwrap(), "@mention gets through");

        db.set_group_snooze("a", &g, None, T0).unwrap();
        let p = human(&db, &g, "anyone around?", T1);
        assert!(
            db.group_wake_snoozed(&pending_for(&db, &p.event_id, "group", "a"), now).unwrap(),
            "a snoozed lead is not woken for a post that does not name it"
        );
        let named = human(&db, &g, "@a over to you", T1);
        assert!(!db.group_wake_snoozed(&pending_for(&db, &named.event_id, "group", "a"), now).unwrap());

        let p = human(&db, &g, "@b and @c, please weigh in", T2);
        assert_eq!(immediate(&db, &p.event_id), vec!["b", "c"]);
        assert!(!db.is_group_snoozed("b", &g, now).unwrap(), "a ping ends an until-pinged snooze");
        assert!(db.is_group_snoozed("c", &g, now).unwrap(), "a timed snooze outlasts a ping");
    }

    #[test]
    fn failover_moves_one_round_and_never_the_group_lead() {
        let (db, g) = group(&[("a", "a"), ("b", "b"), ("c", "c")], "a");
        human(&db, &g, "help", T0);
        let round = db.open_group_round(&g).unwrap().unwrap();
        assert_eq!(round.lead_agent, "a");

        assert!(!db.failover_group_round(&round.round_id, "b", "c", T1).unwrap(), "fenced on the expected lead");
        assert!(db.failover_group_round(&round.round_id, "a", "b", T1).unwrap());
        assert!(!db.failover_group_round(&round.round_id, "a", "c", T1).unwrap(), "a stale failover loses");
        let moved = db.open_group_round(&g).unwrap().unwrap();
        assert_eq!((moved.lead_agent.as_str(), moved.failover_count), ("b", 1));
        assert_eq!(db.get_group(&g).unwrap().unwrap().lead.as_deref(), Some("a"));

        assert!(db.release_group_round(&g, "a", None, T2).unwrap().is_none(), "only the round lead releases");
        let released = db.release_group_round(&g, "b", Some("wait for CI"), T2).unwrap().unwrap();
        assert_eq!(released.status, "released");
        assert_eq!(released.lead_decision.as_deref(), Some("wait for CI"));
        assert!(!db.failover_group_round(&round.round_id, "b", "c", T2).unwrap());

        human(&db, &g, "next one", T2);
        assert_eq!(db.open_group_round(&g).unwrap().unwrap().lead_agent, "a");
        let stalled = db.open_group_round(&g).unwrap().unwrap();
        assert!(db.stall_group_round(&stalled.round_id, "a").unwrap());
        assert!(!db.stall_group_round(&stalled.round_id, "a").unwrap(), "one notice per round");
    }
}
