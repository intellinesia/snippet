//! Origin-chat routing: a task claimed from a chat reports its approvals,
//! questions, shared files and result back to that chat, so the human sees
//! the work where they asked for it. The desk that filed the work is never in
//! the delivery path — it only gets a quiet board note.
//!
//! `origin_thread` on a task names the chat (`group:<id>` or a `direct:<thread>`
//! id); `origin_attention` records the last approval/question id already
//! mirrored there, and each outcome marker carries `origin_reported`, so every
//! card posts exactly once.

use rusqlite::OptionalExtension;

use super::direct::LOCAL_HUMAN_ID;
use super::types::CoordinationEvent;
use super::{BoardEntryKind, NewBoardEntry, Store, StoreError, Task};

/// How far back the claim-time default looks for the chat that woke a desk.
const ORIGIN_LOOKBACK_HOURS: i64 = 6;

/// Outcome marker kinds that become a `task_result` card. `cancelled` and
/// informational markers (`plan`, `message`, offers) are not results.
const RESULT_KINDS: &[&str] = &["done", "failed", "blocked"];

/// The agent a task's origin cards post under: the roster member holding the
/// turn, then whoever is still seated, then the agent that filed it.
pub fn claiming_agent(store: &Store, task: &Task) -> Option<String> {
    let roster = store.list_task_agents(&task.id).unwrap_or_default();
    let open: Vec<_> = roster.iter().filter(|m| m.removed_at.is_none()).collect();
    open.iter()
        .find(|m| m.status == "active")
        .or_else(|| open.iter().find(|m| m.status == "done"))
        .or_else(|| open.first())
        .map(|m| m.agent_id.clone())
        .or_else(|| {
            (task.created_by_kind == "agent" && !task.created_by_id.is_empty())
                .then(|| task.created_by_id.clone())
        })
}

fn thread_scope(store: &Store, thread_id: &str) -> Result<Option<String>, StoreError> {
    store.with_connection(|conn| {
        conn.query_row(
            "SELECT scope FROM board_threads WHERE id = ?1",
            rusqlite::params![thread_id],
            |row| row.get(0),
        )
        .optional()
    })
}

fn thread_title(store: &Store, thread_id: &str) -> Option<String> {
    store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT title FROM board_threads WHERE id = ?1",
                rusqlite::params![thread_id],
                |row| row.get(0),
            )
            .optional()
        })
        .ok()
        .flatten()
}

/// Check a `report_to` value names a chat the given agent can post into: a
/// `group:<id>` it belongs to, or a `direct:<thread>` between it and the
/// human. `agent` is `None` for Mission Control, which routes on behalf of
/// the eventual claimant and only needs the thread to exist with the right
/// shape.
pub fn validate_report_to(
    store: &Store,
    report_to: &str,
    agent: Option<&str>,
) -> Result<(), String> {
    let thread = report_to.trim();
    if thread.starts_with("group:") && thread.len() > "group:".len() {
        let scope = thread_scope(store, thread).map_err(|e| e.to_string())?;
        if scope.as_deref() != Some("group") {
            return Err(format!("`{thread}` is not a group thread"));
        }
        if let Some(agent) = agent
            && !store
                .is_group_member(thread, "agent", agent)
                .map_err(|e| e.to_string())?
        {
            return Err(format!("`{thread}` is not a group `{agent}` belongs to"));
        }
        return Ok(());
    }
    if thread.starts_with("direct:") {
        let scope = thread_scope(store, thread).map_err(|e| e.to_string())?;
        if scope.as_deref() != Some("direct") {
            return Err(format!("`{thread}` is not a direct thread"));
        }
        let participants = store
            .list_thread_participants(thread)
            .map_err(|e| e.to_string())?;
        if !participants.iter().any(|(_, kind)| kind == "human") {
            return Err(format!("`{thread}` has no human participant"));
        }
        if let Some(agent) = agent
            && !participants
                .iter()
                .any(|(id, kind)| kind == "agent" && id == agent)
        {
            return Err(format!(
                "`{thread}` is not a direct thread between the human and `{agent}`"
            ));
        }
        return Ok(());
    }
    Err("`report_to` must be a `group:<id>` thread or a `direct:<thread>` id".into())
}

/// The chat a desk's task should report to when `report_to` was not given:
/// the thread of the most recent human-facing delivery that woke this agent
/// in the last [`ORIGIN_LOOKBACK_HOURS`] hours — a group wake of any kind, or
/// a human's direct message. `None` keeps the old behaviour.
pub fn default_origin_thread(store: &Store, agent_id: &str, now: &str) -> Option<String> {
    let cutoff = chrono::DateTime::parse_from_rfc3339(now)
        .ok()?
        .checked_sub_signed(chrono::Duration::hours(ORIGIN_LOOKBACK_HOURS))?
        .to_rfc3339();
    store
        .with_connection(|conn| {
            conn.query_row(
                "SELECT e.thread_id
                 FROM message_deliveries d
                 JOIN board_events e ON e.event_id = d.event_id
                 JOIN board_threads t ON t.id = e.thread_id
                 WHERE d.recipient_id = ?1
                   AND d.delivered_at IS NOT NULL AND d.delivered_at >= ?2
                   AND (
                     d.recipient_kind IN ('group', 'group_deferred', 'group_peer', 'group_unanswered')
                     OR (d.recipient_kind = 'agent' AND e.actor_kind = 'human' AND t.scope = 'direct')
                   )
                 ORDER BY d.delivered_at DESC
                 LIMIT 1",
                rusqlite::params![agent_id, cutoff],
                |row| row.get(0),
            )
            .optional()
        })
        .ok()
        .flatten()
}

/// The other agent members of a group, for a result card's mentions.
fn other_group_agents(store: &Store, thread: &str, agent: &str) -> Result<Vec<String>, String> {
    let group = store
        .get_group(thread)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("group `{thread}` not found"))?;
    Ok(group
        .members
        .iter()
        .filter(|m| m.kind == "agent" && m.id != agent)
        .map(|m| m.id.clone())
        .collect())
}

/// The human participant id of a direct thread, for addressing the card.
fn human_participant(store: &Store, thread: &str) -> String {
    store
        .list_thread_participants(thread)
        .unwrap_or_default()
        .into_iter()
        .find(|(_, kind)| kind == "human")
        .map(|(id, _)| id)
        .unwrap_or_else(|| LOCAL_HUMAN_ID.to_string())
}

/// Post `card` (`{key: value}` merged into the event payload) into the task's
/// origin thread as the claiming agent. `wake_agents` queues the other group
/// members, which a result card wants; attention and artifact mirrors pass
/// `false` so only the human's delivery row is written and no agent turn
/// runs. Returns the saved event, or `None` when the task has no origin or
/// no agent to speak as.
pub fn post_origin_event(
    store: &Store,
    task: &Task,
    key: &str,
    card: serde_json::Value,
    body: &str,
    wake_agents: bool,
) -> Result<Option<CoordinationEvent>, String> {
    let Some(thread) = task
        .origin_thread
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    let Some(agent) = claiming_agent(store, task) else {
        return Ok(None);
    };
    let now = chrono::Utc::now().to_rfc3339();
    let event_key = uuid::Uuid::new_v4().to_string();
    let mut extra = serde_json::Map::new();
    extra.insert(key.to_string(), card);
    let event = if thread.starts_with("group:") {
        let extra = serde_json::Value::Object(extra);
        if wake_agents {
            let mentions = other_group_agents(store, thread, &agent)?;
            store
                .post_group_message_ex(
                    thread,
                    ("agent", agent.as_str()),
                    body,
                    &mentions,
                    None,
                    &event_key,
                    &now,
                    Some(extra),
                )
                .map_err(|e| e.to_string())?
        } else {
            store
                .post_group_card(
                    thread,
                    ("agent", agent.as_str()),
                    body,
                    Some(extra),
                    &event_key,
                    &now,
                )
                .map_err(|e| e.to_string())?
        }
    } else if thread.starts_with("direct:") {
        extra.insert("target_thread".into(), serde_json::json!(thread));
        let human = human_participant(store, thread);
        store
            .send_direct_message_typed(
                ("agent", agent.as_str()),
                ("human", human.as_str()),
                body,
                &event_key,
                &now,
                None,
                None,
                Some(task.id.as_str()),
                extra,
            )
            .map_err(|e| e.to_string())?
    } else {
        return Ok(None);
    };
    crate::session::emit_device_event(serde_json::json!({
        "kind": "coordination_event",
        "event": event.clone(),
    }));
    Ok(Some(event))
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// One pass of the origin mirror, run on the dispatch tick: post each
/// InProgress task's current approval/question once per attention id, then
/// each outcome marker once. Errors are logged and retried next tick for
/// attention (a new id posts again); a posted marker or attention id is never
/// posted twice.
pub fn sweep(store: &Store) {
    sweep_attention(store);
    sweep_results(store);
}

fn sweep_attention(store: &Store) {
    let Ok(tasks) = store.tasks_with_origin() else {
        return;
    };
    for task in tasks {
        if task.status != super::TaskStatus::InProgress {
            continue;
        }
        let session = if task.session_id.trim().is_empty() {
            task.reporting_session.as_deref().unwrap_or("")
        } else {
            task.session_id.as_str()
        };
        if session.is_empty() {
            continue;
        }
        let Ok(Some(attention)) = store.current_attention_request(session) else {
            continue;
        };
        if task.origin_attention.as_deref() == Some(attention.id.as_str()) {
            continue;
        }
        let is_question = attention.kind == "user_question";
        let (key, card, body) = if is_question {
            let questions = attention
                .pending_question
                .clone()
                .or_else(|| attention.payload.get("questions").cloned())
                .unwrap_or(serde_json::Value::Null);
            let first_text = questions
                .get("questions")
                .and_then(|q| q.as_array())
                .and_then(|a| a.first())
                .and_then(|q| q.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            (
                "question",
                serde_json::json!({
                    "session": session,
                    "question_id": attention.id,
                    "questions": questions,
                    "task_id": task.id,
                    "task_title": task.title,
                }),
                format!(
                    "\"{}\" is waiting on a question: {}",
                    task.title,
                    clip(first_text, 160)
                ),
            )
        } else {
            (
                "approval",
                serde_json::json!({
                    "session": session,
                    "approval_id": attention.id,
                    "tool": attention.payload.get("tool_name").cloned().unwrap_or(serde_json::Value::Null),
                    "summary": attention.payload.get("summary").cloned().unwrap_or(serde_json::Value::Null),
                    "index": attention.payload.get("index").cloned().unwrap_or(serde_json::Value::Null),
                    "total": attention.payload.get("total").cloned().unwrap_or(serde_json::Value::Null),
                    "task_id": task.id,
                    "task_title": task.title,
                    "session_title": attention.session_title,
                }),
                format!(
                    "\"{}\" is waiting on an approval: {}",
                    task.title,
                    clip(
                        attention
                            .payload
                            .get("summary")
                            .and_then(|s| s.as_str())
                            .unwrap_or(""),
                        160
                    )
                ),
            )
        };
        match post_origin_event(store, &task, key, card, &body, false) {
            Ok(Some(event)) => {
                let agent = event.actor_id.clone();
                let shown = thread_title(store, &task.origin_thread.clone().unwrap_or_default())
                    .unwrap_or_else(|| "the chat".to_string());
                let _ = store.record_board_entry(
                    &agent,
                    BoardEntryKind::Noted,
                    &NewBoardEntry {
                        session_id: Some(session),
                        workspace: None,
                        summary: &format!(
                            "Task {} is waiting on the boss: {}, shown in {}",
                            task.title,
                            if is_question { "question" } else { "approval" },
                            shown
                        ),
                        correlation_id: Some(task.id.as_str()),
                        created_at: &event.created_at,
                    },
                );
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "[origin] attention mirror failed for task {}: {error}",
                    task.id
                );
            }
        }
        // Consume the attention id either way: a card that cannot post (the
        // agent left the group, the thread is gone) must not retry every tick.
        let attention_id = attention.id.clone();
        let _ = store.update_task_in(&task.id, &chrono::Utc::now().to_rfc3339(), |t| {
            t.origin_attention = Some(attention_id.clone());
        });
    }
}

fn sweep_results(store: &Store) {
    let Ok(tasks) = store.tasks_with_origin() else {
        return;
    };
    for task in tasks {
        let pending: Vec<_> = task
            .notifications
            .iter()
            .filter(|m| {
                m.target == "mission_control"
                    && !m.origin_reported
                    && RESULT_KINDS.contains(&m.kind.as_str())
            })
            .cloned()
            .collect();
        for marker in pending {
            let card = serde_json::json!({
                "task_id": task.id,
                "title": task.title,
                "status": marker.kind,
                "summary": marker.message,
                "session_id": task.session_id,
            });
            let verb = match marker.kind.as_str() {
                "done" => "finished",
                "failed" => "failed",
                _ => "was stopped",
            };
            let body = format!(
                "\"{}\" {}: {}",
                task.title,
                verb,
                clip(&marker.message, 300)
            );
            if let Err(error) = post_origin_event(store, &task, "task_result", card, &body, true) {
                eprintln!(
                    "[origin] result mirror failed for task {}: {error}",
                    task.id
                );
            }
            // Once only: mark the marker reported whether the post landed or
            // not, so a gone thread cannot retry forever.
            let _ = store.update_task_in(&task.id, &chrono::Utc::now().to_rfc3339(), |t| {
                if let Some(n) = t.notifications.iter_mut().find(|n| {
                    !n.origin_reported
                        && n.target == "mission_control"
                        && n.kind == marker.kind
                        && n.message == marker.message
                }) {
                    n.origin_reported = true;
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::{
        BoardQuery, TaskStatus,
        types::{Agent, AgentKind, AgentStatus},
    };

    fn db() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn agent(store: &Store, id: &str) {
        store
            .create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: Vec::new(),
            })
            .unwrap();
    }

    fn task(id: &str) -> Task {
        Task::filed_by_human(
            id.into(),
            format!("Task {id}"),
            "do the thing".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        )
    }

    #[test]
    fn default_origin_picks_the_latest_wake_within_six_hours() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group(
                "g",
                ("agent", "a"),
                &["a".into()],
                None,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let now = "2026-01-01T12:00:00Z";
        // An old wake is out of the window.
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at, delivered_at)
                     VALUES ('e0', 'group', 'a', 'group:a', '2026-01-01T04:00:00Z', '2026-01-01T04:00:00Z')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO board_events
                     (event_id, thread_id, partition_key, sequence, event_type, actor_kind, actor_id,
                      payload_version, payload_json, idempotency_key, created_at)
                     VALUES ('e0', ?1, 'p', 1, 'group_message', 'human', 'human', 1, '{}', 'k0', '2026-01-01T04:00:00Z')",
                    rusqlite::params![group.id],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(default_origin_thread(&store, "a", now), None);
        // A fresh group wake resolves to its thread.
        store
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO message_deliveries
                     (event_id, recipient_kind, recipient_id, recipient, queued_at, delivered_at)
                     VALUES ('e1', 'group_deferred', 'a', 'group_deferred:a', ?2, ?2)",
                    rusqlite::params![group.id, "2026-01-01T11:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO board_events
                     (event_id, thread_id, partition_key, sequence, event_type, actor_kind, actor_id,
                      payload_version, payload_json, idempotency_key, created_at)
                     VALUES ('e1', ?1, 'p', 2, 'group_message', 'human', 'human', 1, '{}', 'k1', '2026-01-01T11:00:00Z')",
                    rusqlite::params![group.id],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            default_origin_thread(&store, "a", now).as_deref(),
            Some(group.id.as_str())
        );
    }

    #[test]
    fn default_origin_ignores_agent_authored_direct_mail() {
        let store = db();
        agent(&store, "a");
        agent(&store, "b");
        // agent b -> agent a: a recipient row exists but the sender is not human.
        store
            .send_direct_message_typed(
                ("agent", "b"),
                ("agent", "a"),
                "hi",
                "k1",
                "2026-01-01T11:00:00Z",
                None,
                None,
                None,
                serde_json::Map::new(),
            )
            .unwrap();
        assert_eq!(
            default_origin_thread(&store, "a", "2026-01-01T12:00:00Z"),
            None
        );
        // human -> agent a resolves to the direct thread once delivered.
        let ev = store
            .send_direct_message_typed(
                ("human", LOCAL_HUMAN_ID),
                ("agent", "a"),
                "yo",
                "k2",
                "2026-01-01T11:30:00Z",
                None,
                None,
                None,
                serde_json::Map::new(),
            )
            .unwrap();
        store
            .mark_direct_delivered(&ev.event_id, "agent", "a", "2026-01-01T11:30:05Z")
            .unwrap();
        assert_eq!(
            default_origin_thread(&store, "a", "2026-01-01T12:00:00Z").as_deref(),
            Some(ev.thread_id.as_str())
        );
    }

    #[test]
    fn report_to_validation() {
        let store = db();
        agent(&store, "a");
        agent(&store, "b");
        let group = store
            .create_group(
                "g",
                ("agent", "a"),
                &["a".into()],
                None,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        // member passes, non-member and nonsense fail.
        assert!(validate_report_to(&store, &group.id, Some("a")).is_ok());
        assert!(validate_report_to(&store, &group.id, Some("b")).is_err());
        assert!(validate_report_to(&store, "group:nope", Some("a")).is_err());
        assert!(validate_report_to(&store, "task:t1", Some("a")).is_err());
        // direct thread between human and a.
        let ev = store
            .send_direct_message_typed(
                ("human", LOCAL_HUMAN_ID),
                ("agent", "a"),
                "hi",
                "k1",
                "2026-01-01T00:00:00Z",
                None,
                None,
                None,
                serde_json::Map::new(),
            )
            .unwrap();
        assert!(validate_report_to(&store, &ev.thread_id, Some("a")).is_ok());
        assert!(validate_report_to(&store, &ev.thread_id, Some("b")).is_err());
        // Mission Control (no agent) only needs the shape to hold.
        assert!(validate_report_to(&store, &ev.thread_id, None).is_ok());
        assert!(validate_report_to(&store, &group.id, None).is_ok());
    }

    #[test]
    fn result_card_posts_once_per_marker() {
        let store = db();
        agent(&store, "a");
        agent(&store, "b");
        let group = store
            .create_group(
                "g",
                ("agent", "a"),
                &["a".into(), "b".into()],
                None,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let mut t = task("t1");
        t.origin_thread = Some(group.id.clone());
        t.status = TaskStatus::InProgress;
        t.session_id = "s1".into();
        store.create_task(&t).unwrap();
        let now = "2026-01-01T01:00:00Z";
        store
            .add_task_agent_full("t1", "a", "implementer", None, "", "active", now)
            .unwrap();
        store
            .update_task_in("t1", now, |t| {
                t.notifications
                    .push(crate::coordination::NotificationMarker {
                        target: "mission_control".into(),
                        kind: "done".into(),
                        message: "shipped it".into(),
                        delivered: false,
                        origin_reported: false,
                    });
            })
            .unwrap();
        sweep(&store);
        sweep(&store);
        let events = store.events_for_thread(&group.id, 0, 50).unwrap();
        let results: Vec<_> = events
            .iter()
            .filter(|e| e.payload.get("task_result").is_some())
            .collect();
        assert_eq!(results.len(), 1, "the result card posts exactly once");
        let card = &results[0].payload["task_result"];
        assert_eq!(card["task_id"], "t1");
        assert_eq!(card["status"], "done");
        assert_eq!(card["summary"], "shipped it");
        assert_eq!(results[0].actor_id, "a");
        // The other member was mentioned, so agents are woken with it.
        let wakes: i64 = store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM message_deliveries
                     WHERE event_id = ?1 AND recipient_kind = 'group' AND recipient_id = 'b'",
                    rusqlite::params![results[0].event_id],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(wakes, 1);
    }

    #[test]
    fn attention_card_posts_once_and_writes_a_board_note() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group(
                "g",
                ("agent", "a"),
                &["a".into()],
                None,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let mut t = task("t1");
        t.origin_thread = Some(group.id.clone());
        t.status = TaskStatus::InProgress;
        t.session_id = "worker".into();
        store.create_task(&t).unwrap();
        let now = "2026-01-01T01:00:00Z";
        store
            .add_task_agent_full("t1", "a", "implementer", None, "", "active", now)
            .unwrap();
        // A session waiting on an approval.
        store
            .save_session_scalar(
                "worker",
                "workspace",
                "/workspace",
                None,
                "waiting_for_input",
                "{}",
                now,
                now,
            )
            .unwrap();
        store
            .append_conversation_events(
                "worker",
                &[crate::harness::HarnessEvent::ApprovalRequest {
                    tool_name: "bash".into(),
                    summary: "run the deploy".into(),
                    index: 1,
                    total: 1,
                }],
                now,
            )
            .unwrap();
        sweep(&store);
        sweep(&store);
        let events = store.events_for_thread(&group.id, 0, 50).unwrap();
        let cards: Vec<_> = events
            .iter()
            .filter(|e| e.payload.get("approval").is_some())
            .collect();
        assert_eq!(cards.len(), 1, "the approval card posts once");
        let card = &cards[0].payload["approval"];
        assert_eq!(card["session"], "worker");
        assert_eq!(card["tool"], "bash");
        assert_eq!(card["task_id"], "t1");
        // Human-only delivery: no agent wake rows for the card.
        let wakes: i64 = store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM message_deliveries
                     WHERE event_id = ?1 AND recipient_kind != 'human'",
                    rusqlite::params![cards[0].event_id],
                    |row| row.get(0),
                )
            })
            .unwrap();
        assert_eq!(wakes, 0);
        // The claiming agent got a quiet board note.
        let board = store.read_board("a", &BoardQuery::default(), 10).unwrap();
        assert!(
            board
                .iter()
                .any(|e| e.summary.contains("waiting on the boss"))
        );
        // A task with no origin posts nothing.
        let mut plain = task("t2");
        plain.status = TaskStatus::InProgress;
        plain.session_id = "worker".into();
        store.create_task(&plain).unwrap();
        store
            .add_task_agent_full("t2", "a", "implementer", None, "", "active", now)
            .unwrap();
        let before = store.events_for_thread(&group.id, 0, 50).unwrap().len();
        sweep(&store);
        assert_eq!(
            store.events_for_thread(&group.id, 0, 50).unwrap().len(),
            before
        );
    }
}
