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

/// The session a task's origin cards are mirrored from.
fn mirrored_session(task: &Task) -> &str {
    if task.session_id.trim().is_empty() {
        task.reporting_session.as_deref().unwrap_or("")
    } else {
        task.session_id.as_str()
    }
}

/// The chat that already shows `session`'s approvals, questions and results to
/// the user, as `(thread_id, title)`, when an in-progress task mirrors it.
pub fn origin_route(store: &Store, session: &str) -> Option<(String, String)> {
    store
        .tasks_worked_in(session, Some(&super::TaskStatus::InProgress))
        .ok()?
        .into_iter()
        .filter(|t| mirrored_session(t) == session)
        .find_map(|t| {
            let thread = t.origin_thread.as_deref().map(str::trim)?;
            (!thread.is_empty()).then(|| {
                let title = thread_title(store, thread).unwrap_or_else(|| "the chat".to_string());
                (thread.to_string(), title)
            })
        })
}

/// The title the user sees for a task's origin chat.
pub fn origin_title(store: &Store, task: &Task) -> Option<String> {
    let thread = task.origin_thread.as_deref().map(str::trim)?;
    (!thread.is_empty())
        .then(|| thread_title(store, thread).unwrap_or_else(|| "the chat".to_string()))
}

/// Whether a task result of this kind is posted to the task's origin chat.
pub fn result_mirrored(kind: &str) -> bool {
    RESULT_KINDS.contains(&kind)
}

/// Queueing behind another task's workspace is not an outcome, so it makes no card.
pub fn is_queue_note(message: &str) -> bool {
    message.starts_with("waiting on workspace owner(s)")
}

/// The desk an agent-managed session reports to when no task's origin chat
/// already carries its cards.
pub fn managing_desk(store: &Store, session: &str) -> Option<String> {
    if crate::mission_control::is_session_id(session)
        || crate::session::is_inbox_session_id(session)
    {
        return None;
    }
    let row = store.get_session_row(session).ok().flatten()?;
    if row.managed_by != "agent" {
        return None;
    }
    if origin_route(store, session).is_some() {
        return None;
    }
    Some(crate::session::owner_agent_id_for(
        &row.id,
        row.agent_id.as_deref(),
    ))
}

/// Tell the desk managing `session` something it may pass on to the user, once
/// per `key`. Nothing reaches the desk's chat with the user unless it posts it.
pub fn tell_managing_desk(
    store: &Store,
    session: &str,
    key: &str,
    body: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) -> Option<CoordinationEvent> {
    let desk = managing_desk(store, session)?;
    store
        .send_direct_message_typed(
            ("session", session),
            ("agent", desk.as_str()),
            body,
            key,
            &chrono::Utc::now().to_rfc3339(),
            None,
            None,
            None,
            extra,
        )
        .map_err(|e| eprintln!("[origin] desk notice failed for {session}: {e}"))
        .ok()
}

/// A session's title for a one-line card.
pub fn session_label(store: &Store, session: &str) -> String {
    store
        .get_session_row(session)
        .ok()
        .flatten()
        .and_then(|r| r.title)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| session.to_string())
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
    sweep_outcomes(store);
}

fn sweep_attention(store: &Store) {
    let Ok(tasks) = store.tasks_with_origin() else {
        return;
    };
    for task in tasks {
        if task.status != super::TaskStatus::InProgress {
            continue;
        }
        let session = mirrored_session(&task);
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
            let mut card = serde_json::json!({
                "session": session,
                "approval_id": attention.id,
                "tool": attention.payload.get("tool_name").cloned().unwrap_or(serde_json::Value::Null),
                "summary": attention.payload.get("summary").cloned().unwrap_or(serde_json::Value::Null),
                "index": attention.payload.get("index").cloned().unwrap_or(serde_json::Value::Null),
                "total": attention.payload.get("total").cloned().unwrap_or(serde_json::Value::Null),
                "task_id": task.id,
                "task_title": task.title,
                "session_title": attention.session_title,
            });
            // The card's headline, reason line and "Always allow" link.
            for field in ["title", "reason", "rule"] {
                if let Some(value) = attention.payload.get(field).filter(|v| !v.is_null()) {
                    card[field] = value.clone();
                }
            }
            (
                "approval",
                card,
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
        let mut card_event = None;
        match post_origin_event(store, &task, key, card, &body, false) {
            Ok(Some(event)) => {
                card_event = Some(event.event_id.clone());
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
                            "Task {} is waiting on the user: {}, shown in {}",
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
            if let Some(event_id) = card_event {
                t.origin_attention_event = Some(event_id);
            }
        });
    }
}

/// Stamp `outcome` onto every posted origin card whose session is no longer
/// waiting on that request, then push `coordination_event_updated` per stamp.
/// Runs every sweep and once at startup, where it doubles as the backfill for
/// cards posted before outcomes existed. Returns per-state counts.
pub fn sweep_outcomes(store: &Store) -> std::collections::BTreeMap<String, u64> {
    let mut counts = std::collections::BTreeMap::new();
    let Ok(tasks) = store.tasks_with_origin() else {
        return counts;
    };
    for task in tasks {
        let Some(thread) = task
            .origin_thread
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let Ok(cards) = store.unstamped_card_events(thread) else {
            continue;
        };
        for card in cards {
            let Some((key, attention_id, mut session)) = card_attention(&card.payload) else {
                continue;
            };
            if session.is_empty() {
                session = if task.session_id.trim().is_empty() {
                    task.reporting_session.clone().unwrap_or_default()
                } else {
                    task.session_id.clone()
                };
            }
            let live = !session.is_empty()
                && store
                    .current_attention_request(&session)
                    .ok()
                    .flatten()
                    .is_some_and(|a| a.id == attention_id);
            if live {
                continue;
            }
            let state = card_outcome(store, &session, key, &attention_id);
            let outcome = serde_json::json!({
                "state": state,
                "at": chrono::Utc::now().to_rfc3339(),
            });
            match store.stamp_event_card_outcome(&card.event_id, key, &outcome) {
                Ok(Some(_)) => {
                    *counts.entry(state.to_string()).or_default() += 1;
                    crate::session::emit_device_event(serde_json::json!({
                        "kind": "coordination_event_updated",
                        "thread_id": card.thread_id,
                        "event_id": card.event_id,
                    }));
                }
                _ => {}
            }
        }
    }
    counts
}

/// The card kind, attention id and session an unstamped origin card carries.
fn card_attention(payload: &serde_json::Value) -> Option<(&'static str, String, String)> {
    for (key, id_key) in [("approval", "approval_id"), ("question", "question_id")] {
        let Some(card) = payload.get(key) else {
            continue;
        };
        if let Some(id) = card.get(id_key).and_then(|v| v.as_str()) {
            let session = card
                .get("session")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            return Some((key, id.to_string(), session));
        }
    }
    None
}

/// The outcome a card's session log supports for the request `attention_id`
/// (`{ordinal}:{created_at}` of the approval_request / user_question event).
fn card_outcome(store: &Store, session: &str, key: &str, attention_id: &str) -> &'static str {
    if session.is_empty() {
        return "outdated";
    }
    let Some(ordinal) = attention_id
        .split(':')
        .next()
        .and_then(|o| o.parse::<i64>().ok())
    else {
        return "outdated";
    };
    let Ok(events) = store.session_events_from(session, ordinal - 1) else {
        return "outdated";
    };
    if key == "approval" {
        approval_outcome(&events, ordinal)
    } else {
        question_outcome(&events, ordinal)
    }
}

/// An approval's call is the `tool_call` right before its `approval_request`
/// event; the first decisive event after is its `tool_result` — `user_denied`
/// means denied, anything else answered means approved. Lane prompts and
/// interrupted waits have no such pair, so they read outdated.
fn approval_outcome(events: &[(i64, String, String)], ordinal: i64) -> &'static str {
    let Some(request) = events.iter().find(|e| e.0 == ordinal) else {
        return "outdated";
    };
    if request.1 != "approval_request" {
        return "outdated";
    }
    let Some(call) = events.iter().find(|e| e.0 == ordinal - 1) else {
        return "outdated";
    };
    if call.1 != "tool_call" {
        return "outdated";
    }
    for event in events.iter().filter(|e| e.0 > ordinal) {
        match event.1.as_str() {
            "tool_result" => {
                let denied = serde_json::from_str::<serde_json::Value>(&event.2)
                    .ok()
                    .and_then(|p| {
                        p.pointer("/result/error/code")
                            .and_then(|c| c.as_str().map(str::to_string))
                    })
                    .is_some_and(|code| code == "user_denied");
                return if denied { "denied" } else { "approved" };
            }
            "tool_call" | "user_question" | "approval_request" => return "outdated",
            _ => {}
        }
    }
    "outdated"
}

/// A question is answered by the next `user_input` event; another request
/// first means it was superseded, and nothing means the wait ended another way.
fn question_outcome(events: &[(i64, String, String)], ordinal: i64) -> &'static str {
    let Some(request) = events.iter().find(|e| e.0 == ordinal) else {
        return "outdated";
    };
    if request.1 != "user_question" {
        return "outdated";
    }
    for event in events.iter().filter(|e| e.0 > ordinal) {
        match event.1.as_str() {
            "user_input" => return "answered",
            "user_question" | "approval_request" | "tool_call" | "tool_result" => return "outdated",
            _ => {}
        }
    }
    "outdated"
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
                    && !is_queue_note(&m.message)
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
                    title: Some("Deploy staging".into()),
                    reason: Some("The fix needs a live check.".into()),
                    rule: Some(crate::harness::allow_rules::AllowRule {
                        id: "bash:make deploy".into(),
                        label: "make deploy".into(),
                    }),
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
        assert_eq!(card["title"], "Deploy staging");
        assert_eq!(card["reason"], "The fix needs a live check.");
        assert_eq!(
            card["rule"],
            serde_json::json!({"id": "bash:make deploy", "label": "make deploy"})
        );
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
                .any(|e| e.summary.contains("waiting on the user"))
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

    use crate::harness::HarnessEvent;
    use serde_json::json;

    const T0: &str = "2026-01-01T01:00:00Z";

    fn origin_task(store: &Store, group: &str, id: &str, session: &str) {
        let mut t = task(id);
        t.origin_thread = Some(group.to_string());
        t.status = TaskStatus::InProgress;
        t.session_id = session.into();
        store.create_task(&t).unwrap();
        store
            .add_task_agent_full(id, "a", "implementer", None, "", "active", T0)
            .unwrap();
    }

    fn session(store: &Store, id: &str, status: &str, state: &str) {
        store
            .save_session_scalar(id, "wk", "/w", None, status, state, T0, T0)
            .unwrap();
    }

    fn approval_events() -> Vec<HarnessEvent> {
        vec![
            HarnessEvent::ToolCall {
                tool_name: "bash".into(),
                arguments: json!({"command": "ls"}),
            },
            HarnessEvent::ApprovalRequest {
                tool_name: "bash".into(),
                summary: "run ls".into(),
                index: 1,
                total: 1,
                title: None,
                reason: None,
                rule: None,
            },
        ]
    }

    fn card_outcome_of(store: &Store, thread: &str) -> Vec<serde_json::Value> {
        store
            .unstamped_card_events(thread)
            .unwrap()
            .iter()
            .map(|e| e.payload.clone())
            .collect()
    }

    fn stamped_outcomes(store: &Store, thread: &str) -> Vec<serde_json::Value> {
        store
            .events_for_thread(thread, 0, 100)
            .unwrap()
            .iter()
            .filter_map(|e| {
                e.payload
                    .get("approval")
                    .or_else(|| e.payload.get("question"))
                    .and_then(|c| c.get("outcome"))
                    .cloned()
            })
            .collect()
    }

    #[test]
    fn approved_call_stamps_approved() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group("g", ("agent", "a"), &["a".into()], None, T0)
            .unwrap()
            .id;
        origin_task(&store, &group, "t1", "worker");
        session(&store, "worker", "waiting_for_input", "{}");
        store
            .append_conversation_events("worker", &approval_events(), T0)
            .unwrap();
        sweep(&store);
        // The user approves: the call's tool_result lands and the wait ends.
        store
            .append_conversation_events(
                "worker",
                &[HarnessEvent::ToolResult {
                    tool_name: "bash".into(),
                    result: json!({"status": "success", "data": {"output": "ok"}}),
                }],
                "2026-01-01T01:05:00Z",
            )
            .unwrap();
        session(&store, "worker", "idle", "{}");
        sweep(&store);
        let outcomes = stamped_outcomes(&store, &group);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["state"], "approved");
        assert!(outcomes[0]["at"].as_str().unwrap().contains('T'));
        // A second sweep changes nothing.
        sweep(&store);
        assert_eq!(stamped_outcomes(&store, &group), outcomes);
    }

    #[test]
    fn denied_call_stamps_denied() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group("g", ("agent", "a"), &["a".into()], None, T0)
            .unwrap()
            .id;
        origin_task(&store, &group, "t1", "worker");
        session(&store, "worker", "waiting_for_input", "{}");
        store
            .append_conversation_events("worker", &approval_events(), T0)
            .unwrap();
        sweep(&store);
        store
            .append_conversation_events(
                "worker",
                &[HarnessEvent::ToolResult {
                    tool_name: "bash".into(),
                    result: json!({"status": "error", "error": {"code": "user_denied", "message": "no"}}),
                }],
                "2026-01-01T01:05:00Z",
            )
            .unwrap();
        session(&store, "worker", "idle", "{}");
        sweep(&store);
        let outcomes = stamped_outcomes(&store, &group);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["state"], "denied");
    }

    #[test]
    fn newer_request_supersedes_and_stamps_update_frame() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group("g", ("agent", "a"), &["a".into()], None, T0)
            .unwrap()
            .id;
        origin_task(&store, &group, "t1", "worker");
        session(&store, "worker", "waiting_for_input", "{}");
        store
            .append_conversation_events("worker", &approval_events(), T0)
            .unwrap();
        sweep(&store);
        let mut rx = crate::session::subscribe_device_events();
        // A second approval request arrives before the first is answered.
        store
            .append_conversation_events(
                "worker",
                &[HarnessEvent::ApprovalRequest {
                    tool_name: "write".into(),
                    summary: "write a file".into(),
                    index: 1,
                    total: 1,
                    title: None,
                    reason: None,
                    rule: None,
                }],
                "2026-01-01T01:05:00Z",
            )
            .unwrap();
        sweep(&store);
        let outcomes = stamped_outcomes(&store, &group);
        assert_eq!(outcomes.len(), 1, "only the superseded card is stamped");
        assert_eq!(outcomes[0]["state"], "outdated");
        // The live card has no outcome yet.
        let live = store
            .events_for_thread(&group, 0, 100)
            .unwrap()
            .iter()
            .filter(|e| {
                e.payload
                    .get("approval")
                    .is_some_and(|c| c.get("outcome").is_none())
            })
            .count();
        assert_eq!(live, 1);
        // The update frame named the stamped event.
        let stamped_id = store
            .events_for_thread(&group, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| {
                e.payload
                    .get("approval")
                    .and_then(|c| c.get("outcome"))
                    .is_some()
            })
            .unwrap()
            .event_id;
        let mut saw_frame = false;
        while let Ok(ev) = rx.try_recv() {
            if ev["kind"] == "coordination_event_updated"
                && ev["event_id"] == stamped_id
                && ev["thread_id"] == group
            {
                saw_frame = true;
            }
        }
        assert!(saw_frame, "coordination_event_updated frame emitted");
    }

    #[test]
    fn answered_question_stamps_answered() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group("g", ("agent", "a"), &["a".into()], None, T0)
            .unwrap()
            .id;
        origin_task(&store, &group, "t1", "worker");
        let questions = json!({"questions": [{"id": "q0", "text": "pick one", "answer_kind": {"kind": "free_text"}}]});
        let state = json!({"pending_question": questions}).to_string();
        session(&store, "worker", "waiting_for_input", &state);
        store
            .append_conversation_events(
                "worker",
                &[HarnessEvent::UserQuestion {
                    questions: questions.clone(),
                }],
                T0,
            )
            .unwrap();
        sweep(&store);
        store
            .append_conversation_events(
                "worker",
                &[HarnessEvent::UserInput {
                    text: "the second one".into(),
                }],
                "2026-01-01T01:05:00Z",
            )
            .unwrap();
        session(&store, "worker", "idle", &state);
        sweep(&store);
        let outcomes = stamped_outcomes(&store, &group);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["state"], "answered");
    }

    #[test]
    fn unanswered_question_and_gone_session_stamp_outdated() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group("g", ("agent", "a"), &["a".into()], None, T0)
            .unwrap()
            .id;
        origin_task(&store, &group, "t1", "worker");
        let questions = json!({"questions": [{"id": "q0", "text": "pick one", "answer_kind": {"kind": "free_text"}}]});
        let state = json!({"pending_question": questions}).to_string();
        session(&store, "worker", "waiting_for_input", &state);
        store
            .append_conversation_events(
                "worker",
                &[HarnessEvent::UserQuestion {
                    questions: questions.clone(),
                }],
                T0,
            )
            .unwrap();
        sweep(&store);
        // The wait ended without an answer (interrupt, cancel).
        session(&store, "worker", "idle", "{}");
        sweep(&store);
        let outcomes = stamped_outcomes(&store, &group);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["state"], "outdated");
        // A card whose session row is gone stamps outdated too.
        let gone = post_origin_event(
            &store,
            &store
                .list_tasks(None, None)
                .unwrap()
                .into_iter()
                .find(|t| t.id == "t1")
                .unwrap(),
            "approval",
            json!({"session": "missing", "approval_id": "9:2026-01-01T00:00:00Z"}),
            "waiting",
            false,
        )
        .unwrap()
        .unwrap();
        let _ = gone;
        let counts = sweep_outcomes(&store);
        assert_eq!(counts.get("outdated"), Some(&1));
    }

    #[test]
    fn backfill_stamps_old_cards_and_leaves_the_live_one() {
        let store = db();
        agent(&store, "a");
        let group = store
            .create_group("g", ("agent", "a"), &["a".into()], None, T0)
            .unwrap()
            .id;
        let mut t = task("t1");
        t.origin_thread = Some(group.clone());
        t.status = TaskStatus::InProgress;
        t.session_id = "s1".into();
        store.create_task(&t).unwrap();
        store
            .add_task_agent_full("t1", "a", "implementer", None, "", "active", T0)
            .unwrap();
        let t = store.list_tasks(None, None).unwrap().remove(0);
        // An answered approval and a still-live question, posted as cards.
        session(&store, "s1", "idle", "{}");
        store
            .append_conversation_events(
                "s1",
                &[
                    HarnessEvent::ToolCall {
                        tool_name: "bash".into(),
                        arguments: json!({}),
                    },
                    HarnessEvent::ApprovalRequest {
                        tool_name: "bash".into(),
                        summary: "run it".into(),
                        index: 1,
                        total: 1,
                        title: None,
                        reason: None,
                        rule: None,
                    },
                    HarnessEvent::ToolResult {
                        tool_name: "bash".into(),
                        result: json!({"status": "success", "data": {}}),
                    },
                ],
                T0,
            )
            .unwrap();
        post_origin_event(
            &store,
            &t,
            "approval",
            json!({"session": "s1", "approval_id": format!("1:{T0}"), "task_id": "t1"}),
            "waiting on approval",
            false,
        )
        .unwrap();
        let questions = json!({"questions": [{"id": "q0", "text": "pick", "answer_kind": {"kind": "free_text"}}]});
        let state = json!({"pending_question": questions}).to_string();
        session(&store, "s2", "waiting_for_input", &state);
        store
            .append_conversation_events(
                "s2",
                &[HarnessEvent::UserQuestion {
                    questions: questions.clone(),
                }],
                T0,
            )
            .unwrap();
        post_origin_event(
            &store,
            &t,
            "question",
            json!({"session": "s2", "question_id": format!("0:{T0}"), "task_id": "t1"}),
            "waiting on a question",
            false,
        )
        .unwrap();
        let counts = sweep_outcomes(&store);
        assert_eq!(counts.get("approved"), Some(&1));
        let outcomes = stamped_outcomes(&store, &group);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0]["state"], "approved");
        // The live question card is untouched and still discoverable as unstamped.
        assert_eq!(card_outcome_of(&store, &group).len(), 1);
        // Running it again is a no-op.
        assert!(sweep_outcomes(&store).is_empty());
    }
}
