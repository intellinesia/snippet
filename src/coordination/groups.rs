//! Group threads: board threads with scope `group` where several agents and
//! the human talk with turn-taking.
//!
//! A post is an ordinary `group_message` board event. Who gets woken is
//! recorded in `message_deliveries` like a direct message: `group` rows are
//! the immediate wakes, `group_deferred` rows carry a `retry_at` ten seconds
//! out and are skipped at fire time once anyone has replied to the post
//! (`group_replies` is the claim ledger, one reply per agent per post).

use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use super::direct::{EVENT_COLUMNS_QUALIFIED, PendingDirectMessage, actor_ref, event_from_row};
use super::types::CoordinationEvent;
use super::{Store, StoreError};

/// How long a deferred group wake waits for someone else to answer first.
pub const GROUP_DEFER_SECONDS: i64 = 10;

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
}

/// Validation failures cross `with_connection` as `InvalidParameterName`
/// and are mapped back to [`StoreError::InvalidInput`] at the boundary.
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
    Ok(GroupSummary {
        id,
        title: row.get(1)?,
        members,
        lead: row.get(2)?,
        created_at: row.get(3)?,
        last_event_at: row.get(4)?,
        unread: row.get(5)?,
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
                params![thread_id, lead, actor_ref(created_by.0, created_by.1), created_at],
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
        let event_id = uuid::Uuid::new_v4().to_string();
        let defer_at = chrono::DateTime::parse_from_rfc3339(created_at)
            .map(|at| (at + chrono::Duration::seconds(GROUP_DEFER_SECONDS)).to_rfc3339())
            .unwrap_or_else(|_| created_at.to_string());

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
                    if agents.iter().any(|a| a == mention) && !mentioned.contains(mention) {
                        mentioned.push(mention.clone());
                    }
                }
            }
            // An agent's post wakes only whom it names; a lead or deferred fallback would let agents wake each other forever.
            let immediate: Vec<String> = if !mentioned.is_empty() || poster_agent.is_some() {
                mentioned
            } else {
                match resolve_lead(&tx, thread_id, &agents)? {
                    Some(lead) if poster_agent != Some(lead.as_str()) => vec![lead],
                    _ => Vec::new(),
                }
            };
            let deferred: Vec<String> = agents
                .iter()
                .filter(|a| poster_agent.is_none() && !immediate.contains(a))
                .cloned()
                .collect();

            for agent in &immediate {
                tx.execute(
                    "INSERT OR IGNORE INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at)
                     VALUES (?1, 'group', ?2, ?3, ?4)",
                    params![event_id, agent, actor_ref("group", agent), created_at],
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

    /// Drop the scheduled wakes and reply claim of a post that never landed.
    fn discard_group_wakes(&self, event_id: &str) -> Result<(), StoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "DELETE FROM message_deliveries WHERE event_id = ?1
                 AND recipient_kind IN ('group', 'group_deferred', 'human')",
                params![event_id],
            )?;
            conn.execute(
                "DELETE FROM group_replies WHERE reply_event_id = ?1",
                params![event_id],
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

    /// Group wakes due for delivery, oldest first: immediate `group` rows and
    /// `group_deferred` rows whose claim window has opened.
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
                 WHERE d.recipient_kind IN ('group', 'group_deferred')
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
            let rows = stmt.query_map(
                params![limit, MAX_GROUP_DELIVERY_ATTEMPTS, now],
                |row| {
                    Ok(PendingDirectMessage {
                        event: event_from_row(row)?,
                        recipient_kind: row.get(13)?,
                        recipient_id: row.get(14)?,
                    })
                },
            )?;
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
            .create_group("build", ("human", "human"), &["a".into(), "b".into(), "c".into()], None, T0)
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
        assert!(group
            .members
            .iter()
            .any(|m| m.kind == "human" && m.id == "human"));
        assert!(db.is_group_member(&id, "agent", "a").unwrap());
        assert_eq!(db.list_groups().unwrap().len(), 1);
        assert!(db.get_group("direct:x").unwrap().is_none());
    }

    #[test]
    fn a_mention_wakes_only_the_mentioned_agent_immediately() {
        let (db, id) = three_agent_group();
        db.post_group_message(&id, ("human", "human"), "@b status?", &["b".into()], None, "k1", T0)
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
        db.post_group_message(&id, ("human", "human"), "@b hi", &["b".into()], None, "k2", T1)
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
    fn an_everyone_mention_wakes_every_agent_member() {
        let (db, id) = three_agent_group();
        db.post_group_message(&id, ("human", "human"), "@everyone ship it", &["everyone".into()], None, "k1", T0)
            .unwrap();
        let mut immediate: Vec<_> = pending(&db, T0)
            .into_iter()
            .filter(|p| p.recipient_kind == "group")
            .map(|p| p.recipient_id)
            .collect();
        immediate.sort();
        assert_eq!(immediate, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        // Unknown ids are ignored, not an error.
        db.post_group_message(&id, ("human", "human"), "@ghost hi", &["ghost".into(), "b".into()], None, "k2", T1)
            .unwrap();
    }

    #[test]
    fn an_agents_unmentioned_post_wakes_nobody() {
        let (db, id) = three_agent_group();
        db.set_group_lead(&id, "b").unwrap();
        db.post_group_message(&id, ("agent", "a"), "taking this", &[], None, "k1", T0)
            .unwrap();
        assert!(pending(&db, T0).is_empty());
        assert!(pending(&db, T2).is_empty());
        db.post_group_message(&id, ("agent", "a"), "@c over to you", &["c".into()], None, "k2", T1)
            .unwrap();
        assert_eq!(kinds(&pending(&db, T2)), vec![("group".into(), "c".into())]);
    }

    #[test]
    fn a_reply_claims_the_post_and_a_second_reply_is_refused() {
        let (db, id) = three_agent_group();
        let post = db
            .post_group_message(&id, ("human", "human"), "status?", &["a".into()], None, "k1", T0)
            .unwrap();
        let reply = db
            .post_group_message(&id, ("agent", "a"), "done", &[], Some(&post.event_id), "k2", T1)
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
        db.post_group_message(&id, ("agent", "b"), "same", &[], Some(&post.event_id), "k4", T1)
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
    fn scripted_three_agent_turn_taking() {
        let (db, id) = three_agent_group();
        let mut delivered: Vec<(String, String, String)> = Vec::new();
        fn deliver(db: &Store, now: &str, delivered: &mut Vec<(String, String, String)>) {
            for pending in pending(db, now) {
                let skip = pending.recipient_kind == "group_deferred"
                    && db.group_has_replies(&pending.event.event_id).unwrap();
                if !skip {
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
        db.post_group_message(&id, ("agent", "b"), "green", &[], Some(&p1.event_id), "k2", T1)
            .unwrap();
        deliver(&db, T1, &mut delivered);

        // An unmentioned follow-up: the lead — b, last addressed — is woken;
        // a and c wait out the claim window.
        let p2 = db
            .post_group_message(&id, ("human", "human"), "and the tests?", &[], None, "k3", T2)
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
        db.post_group_message(&id, ("agent", "b"), "they pass", &[], Some(&p2.event_id), "k4", T2)
            .unwrap();
        deliver(&db, "2026-01-01T00:00:31+00:00", &mut delivered);
        assert!(
            !delivered
                .iter()
                .any(|(e, k, _)| e == &p2.event_id && k == "group_deferred"),
            "deferred wakes for the follow-up skipped once b replied: {delivered:?}"
        );
    }
}
