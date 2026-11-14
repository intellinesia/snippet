//! Group threads: board threads with scope `group` where several agents and
//! the human talk with turn-taking.
//!
//! A post is an ordinary `group_message` board event. Who gets woken is
//! recorded in `message_deliveries` like a direct message: `group` rows are
//! the immediate wakes, `group_deferred` rows carry a `retry_at` ten seconds
//! out and are skipped at fire time once anyone has replied to the post
//! (`group_replies` is the claim ledger, one reply per agent per post).
//! A deferred wake that finds a reply is still delivered, with the replies,
//! as context. Naming a member in the body (id, handle or display name) wakes
//! it like a mention. An agent's first post after the latest human post also
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
pub(crate) fn named_in_body(body: &str, members: &[(String, String, String)]) -> Vec<String> {
    let text = body.to_lowercase();
    let joined = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '/');
    let found = |term: &str| {
        let term = term.trim().to_lowercase();
        if term.chars().count() < 3 {
            return false;
        }
        text.match_indices(&term).any(|(at, m)| {
            let mut before = text[..at].chars().rev();
            let mut after = text[at + m.len()..].chars();
            let open = match before.next() {
                Some(c) => !(joined(c) || c == '.'),
                None => true,
            };
            // A trailing `.` ends a sentence unless a file extension follows.
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
        .filter(|(id, display, handle)| found(id) || found(handle) || found(display))
        .map(|(id, _, _)| id.clone())
        .collect()
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
    Ok(GroupSummary {
        id,
        title: row.get(1)?,
        members,
        lead: row.get(2)?,
        created_at: row.get(3)?,
        last_event_at: row.get(4)?,
        unread: row.get(5)?,
        last_message,
    })
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

        group_store_result(self.with_connection(|conn| {
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
                    if agents.iter().any(|a| a == mention)
                        && poster_agent != Some(mention.as_str())
                        && !mentioned.contains(mention)
                    {
                        mentioned.push(mention.clone());
                    }
                }
            }
            let names: Vec<(String, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT id, display_name, handle FROM agents WHERE id = ?1",
                )?;
                let mut out = Vec::new();
                for agent in &agents {
                    if let Some(row) = stmt
                        .query_row(params![agent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                        .optional()?
                    {
                        out.push(row);
                    }
                }
                out
            };
            for agent in named_in_body(body, &names) {
                if poster_agent != Some(agent.as_str()) && !mentioned.contains(&agent) {
                    mentioned.push(agent);
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
            Ok(())
        }))?;

        let mut payload = serde_json::json!({
            "body": body,
            "mentions": mentions,
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
            Ok(saved) => Ok(saved),
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

        let event = CoordinationEvent {
            event_id,
            thread_id: thread_id.to_string(),
            partition_key: format!("thread:{thread_id}"),
            sequence: 0,
            event_type: "group_message".into(),
            actor_kind: actor.0.to_string(),
            actor_id: actor.1.to_string(),
            payload_version: 1,
            payload: serde_json::json!({
                "body": "",
                "kind": "reaction",
                "reaction": reaction,
                "reply_to": target_event_id,
                "in_reply_to": target_event_id,
                "mentions": [],
                "from_kind": actor.0,
                "from_id": actor.1,
            }),
            causation_id: None,
            correlation_id: None,
            idempotency_key: idempotency_key.to_string(),
            created_at: created_at.to_string(),
        };
        self.append_event(&event)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

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
}
