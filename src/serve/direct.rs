//! Direct messaging between coordination participants over HTTP.
//!
//! The durable half lives in [`crate::coordination::direct`]. This module is the
//! daemon's surface for it: the routes a client calls, and the loop that hands
//! each accepted message to its recipient's session.
//!
//! Delivery is pull-based from the store rather than pushed by the request
//! handler, so a message accepted just before a restart is still delivered after
//! it, and a reconnecting client cannot cause a second delivery.


use super::api_error;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::json;

use crate::harness::{HarnessState, LoopInput};
use crate::session::{
    SessionRole, SessionSidecar, state_path_for_id, write_session_sidecar,
};

use super::{Shared};

/// Recent messages included when a recipient is woken. Enough to read the
/// current exchange, small enough to keep the wake cheap.
pub(super) const DIRECT_HISTORY_ON_WAKE: u32 = 10;

/// How many pending deliveries one pass of the loop takes.
const DELIVER_BATCH: u32 = 50;

/// The session that receives a message addressed to `agent_id`.
///
/// Mission Control's own session IS its inbox — it is a singleton whose whole
/// job is being addressed. Every other agent gets a dedicated inbox session, so
/// a message never lands in whichever task session happens to be running.
pub(super) fn recipient_session_id(agent_id: &str) -> String {
    if agent_id == crate::mission_control::SESSION_ID {
        crate::mission_control::SESSION_ID.to_string()
    } else {
        crate::session::inbox_session_id(agent_id)
    }
}

/// The envelope handed to a recipient when a direct message arrives.
///
/// Field order matters: `body` is last, so a client or reader can take
/// everything up to the closing tag as the message — including newlines —
/// without the rules or history running into the sender's text.
fn direct_message_envelope(
    event: &crate::coordination::types::CoordinationEvent,
    history: &[crate::coordination::types::CoordinationEvent],
) -> String {
    let sender = format!("{}:{}", event.actor_kind, event.actor_id);
    if let Some(reaction) = event
        .payload
        .get("reaction")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        let target = event
            .payload
            .get("reply_to")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        return format!("[reaction] {sender} reacted {reaction} to your message {target}");
    }
    let body = event
        .payload
        .get("body")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let event_id_line = if event.event_id.is_empty() {
        String::new()
    } else {
        format!("event_id: {}\n", event.event_id)
    };
    let mut reply_lines = String::new();
    if let Some(target_id) = event
        .payload
        .get("reply_to")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        reply_lines.push_str(&format!("reply_to_message: {target_id}\n"));
        if let Some(target) = history.iter().find(|prior| prior.event_id == target_id) {
            let quoted: String = target
                .payload
                .get("body")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .replace(['\r', '\n'], " ")
                .chars()
                .take(160)
                .collect();
            reply_lines.push_str(&format!("quoted: {quoted}\n"));
        }
    }
    // When the message came from a SESSION, the reply belongs back in that
    // session — that is where the human is reading and where the exchange is
    // recorded. Stated as a `reply_to` line so the agent never has to infer it.
    let reply_to = event
        .payload
        .get("origin_session")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .map(|session| format!("session:{session}"))
        .unwrap_or_else(|| "human".to_string());
    let history: Vec<_> = history
        .iter()
        .filter(|prior| prior.payload.get("auto").is_none())
        .collect();
    let mut digest = String::new();
    if history.is_empty() {
        digest.push_str("history: (first message in this conversation)\n");
    } else {
        digest.push_str(&format!(
            "history: last {} message(s), oldest first\n",
            history.len()
        ));
        for prior in history {
            let prior_body = if let Some(r) = prior.payload.get("reaction").and_then(|v| v.as_str()) {
                let target = prior
                    .payload
                    .get("reply_to")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                format!("[reacted {r} to {target}]")
            } else {
                prior
                    .payload
                    .get("body")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            digest.push_str(&format!(
                "  {} [{}] {}: {}\n",
                prior.sequence, prior.actor_kind, prior.actor_id, prior_body
            ));
        }
    }
    format!(
        "[direct_message]\n{event_id_line}thread_id: {thread}\nfrom: {sender}\nfrom_kind: {kind}\nto: {to}\n\
         reply_to: {reply_to}\n{reply_lines}{tags}\
         note: {who} wrote directly; this is not a turn in your own conversation. \
         {answer} \
         Recent history only; use read_agent_thread for more.\n{history}body: {body}\n\
         [/direct_message]",
        event_id_line = event_id_line,
        thread = event.thread_id,
        sender = sender,
        who = if event.actor_kind == "human" { "The user" } else { "A colleague" },
        answer = if reply_to == "human" {
            "Your plain reply text is delivered to them automatically; use send_agent_message \
             only for extra messages."
        } else {
            "Answer with send_agent_message to `reply_to` above, which is where they're reading."
        },
        reply_to = reply_to,
        reply_lines = reply_lines,
        tags = crate::coordination::typed::envelope_lines(&event.payload),
        kind = event.actor_kind,
        to = event
            .payload
            .get("recipient")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
        history = digest,
        body = body,
    )
}

const HUMAN_DM_MARKER: &str = "[direct_message]";

/// The thread id of a `[direct_message]` envelope that came from the human and
/// expects its answer back in the human's DM thread.
pub(crate) fn human_dm_thread(text: &str) -> Option<String> {
    if !text.starts_with(HUMAN_DM_MARKER) {
        return None;
    }
    let has = |wanted: &str| text.lines().any(|line| line.trim() == wanted);
    if !(has("from_kind: human") && has("reply_to: human")) {
        return None;
    }
    Some(
        text.lines()
            .find_map(|line| line.trim().strip_prefix("thread_id:"))
            .map(|id| id.trim().to_string())
            .unwrap_or_default(),
    )
}

fn sent_to_human_ok(turn: &[crate::harness::HarnessEvent], from: usize) -> bool {
    use crate::harness::HarnessEvent as E;
    let mut sent = false;
    for (i, event) in turn.iter().enumerate().skip(from) {
        if let E::ToolCall { tool_name, arguments } = event
            && tool_name == "send_agent_message"
            && arguments
                .get("reaction")
                .and_then(|v| v.as_str())
                .is_none_or(|r| r.trim().is_empty())
            && matches!(
                arguments.get("recipient_agent_id").and_then(|v| v.as_str()).map(str::trim),
                Some("human" | "local" | "human:local")
            )
        {
            let failed = turn[i + 1..].iter().find_map(|e| match e {
                E::ToolResult { tool_name, result } if tool_name == "send_agent_message" => {
                    Some(result.get("error").is_some() || result.get("is_error") == Some(&json!(true)))
                }
                E::InvalidToolCall { tool_name, .. } if tool_name == "send_agent_message" => Some(true),
                _ => None,
            });
            if failed == Some(false) {
                sent = true;
            }
        }
    }
    sent
}

/// What to say to the human when a turn ends with `waiting` human messages
/// outstanding: `(turn start index, line)`, or None when they already got a reply.
pub(crate) fn turn_end_relay(
    waiting: usize,
    who: &str,
    events: &[crate::harness::HarnessEvent],
) -> Option<(usize, String)> {
    use crate::harness::HarnessEvent as E;
    if waiting == 0 {
        return None;
    }
    let start = events.iter().rposition(|e| matches!(e, E::UserInput { .. })).unwrap_or(0);
    let turn = events.get(start + 1..).unwrap_or(&[]);
    // A reply sent before the latest human message does not answer it.
    let last_dm = turn
        .iter()
        .rposition(|e| matches!(e, E::Steer { text } if human_dm_thread(text).is_some()))
        .map(|i| i + 1)
        .unwrap_or(0);
    if sent_to_human_ok(turn, last_dm) {
        return None;
    }
    let text = turn.iter().rev().find_map(|e| match e {
        E::AssistantText { text, .. } if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    });
    let line = match text {
        Some(text) => text,
        None => {
            eprintln!("[relay] no reply from {who}");
            let last_tool = turn
                .iter()
                .rev()
                .find_map(|e| match e {
                    E::ToolCall { tool_name, .. } => Some(tool_name.as_str()),
                    _ => None,
                })
                .unwrap_or("none");
            format!("{who} finished this turn without a reply; its last step was {last_tool}.")
        }
    };
    let line = if waiting >= 2 {
        format!("Answering your {waiting} messages:\n\n{line}")
    } else {
        line
    };
    Some((start, line))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn post_to_human(
    store: &crate::store::Store,
    agent_id: &str,
    session_id: &str,
    origin: Option<&str>,
    about_task: Option<&str>,
    key: &str,
    text: &str,
    extra: serde_json::Map<String, serde_json::Value>,
) {
    let mut fields = crate::coordination::typed::payload_fields(None, crate::coordination::typed::Urgency::Normal, None);
    fields.insert("channel".into(), json!("chat"));
    fields.extend(extra);
    match store.send_direct_message_typed(
        ("agent", agent_id),
        ("human", crate::coordination::LOCAL_HUMAN_ID),
        text,
        key,
        &chrono::Utc::now().to_rfc3339(),
        origin,
        None,
        about_task,
        fields,
    ) {
        Ok(saved) => crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved,
        })),
        Err(error) => eprintln!("relay desk reply for {session_id}: {error}"),
    }
}

/// The agent a session works for, when the session names one at all.
fn relay_agent_id(store: &crate::store::Store, session_id: &str) -> Option<String> {
    session_id
        .strip_prefix("inbox-")
        .map(str::to_string)
        .or_else(|| {
            store
                .get_session_row(session_id)
                .ok()
                .flatten()
                .and_then(|row| row.agent_id)
                .filter(|a| !a.is_empty())
        })
        // A task session with no agent row: the worker on its task's roster.
        .or_else(|| {
            let task = store.open_task_of_session(session_id).ok().flatten()?;
            store
                .list_task_agents(&task.id)
                .ok()?
                .into_iter()
                .find(|m| m.removed_at.is_none() && m.status == "active")
                .map(|m| m.agent_id)
        })
}

fn relay_agent_name(store: &crate::store::Store, agent_id: &str, session_id: &str) -> String {
    store
        .get_agent(agent_id)
        .ok()
        .flatten()
        .map(|a| a.display_name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| session_id.to_string())
}

fn relay_identity(store: &crate::store::Store, session_id: &str) -> (String, String) {
    let agent_id = relay_agent_id(store, session_id).unwrap_or_else(|| "snippet".to_string());
    let name = relay_agent_name(store, &agent_id, session_id);
    (agent_id, name)
}

/// At turn end, tell the human something if they were waiting; then clear the wait.
pub(crate) fn relay_desk_reply(
    store: &crate::store::Store,
    session_id: &str,
    state: &mut crate::harness::HarnessState,
) {
    if state.human_waiting.is_empty() {
        return;
    }
    let (agent_id, name) = relay_identity(store, session_id);
    let relay = turn_end_relay(state.human_waiting.len(), &name, &state.events);
    state.human_waiting.clear();
    let Some((start, text)) = relay else { return };
    let key = format!("desk-relay:{session_id}:{start}");
    let origin = session_id.starts_with("inbox-").then_some(session_id);
    let task = store.open_task_of_session(session_id).ok().flatten();
    let about_task = task.as_ref().map(|t| t.id.as_str());
    let kind = crate::coordination::reach::Kind::from_reply(&text, None, false);
    crate::coordination::reach::reach_with_key(
        store,
        &agent_id,
        kind,
        &name,
        &text,
        false,
        origin,
        about_task,
        Some(&key),
        Default::default(),
    );
}

/// When a session pauses on `ask_user`, put the question in the agent's chat so
/// the human can answer it there. Keyed on the `UserQuestion` event's index, so
/// a re-persist never posts the same question twice.
pub(crate) fn relay_question(
    store: &crate::store::Store,
    session_id: &str,
    state: &crate::harness::HarnessState,
) {
    if session_id == crate::mission_control::SESSION_ID {
        return;
    }
    let Some(question) = state.pending_question.as_ref() else {
        return;
    };
    let Some(agent_id) = relay_agent_id(store, session_id) else {
        return;
    };
    let Some(index) = state
        .events
        .iter()
        .rposition(|e| matches!(e, crate::harness::HarnessEvent::UserQuestion { .. }))
    else {
        return;
    };
    let Some(questions) = question.get("questions").and_then(|q| q.as_array()) else {
        return;
    };
    let body = questions
        .iter()
        .filter_map(|q| {
            let text = q.get("text").and_then(|t| t.as_str())?.trim();
            if text.is_empty() {
                return None;
            }
            Some(
                match q
                    .get("header")
                    .and_then(|h| h.as_str())
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                {
                    Some(header) => format!("{header}: {text}"),
                    None => text.to_string(),
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if body.is_empty() {
        return;
    }
    let name = relay_agent_name(store, &agent_id, session_id);
    let key = format!("ask:{session_id}:{index}");
    let origin = session_id.starts_with("inbox-").then_some(session_id);
    let task = store.open_task_of_session(session_id).ok().flatten();
    let about_task = task.as_ref().map(|t| t.id.as_str());
    let mut extra = serde_json::Map::new();
    extra.insert("kind".into(), json!("question"));
    extra.insert("auto".into(), json!("ask"));
    extra.insert(
        "question".into(),
        json!({
            "session": session_id,
            "questions": questions,
            "asked_at": chrono::Utc::now().to_rfc3339(),
        }),
    );
    crate::coordination::reach::reach_with_key(
        store,
        &agent_id,
        crate::coordination::reach::Kind::Question,
        &name,
        &body,
        false,
        origin,
        about_task,
        Some(&key),
        extra,
    );
}

/// The newest relayed `ask_user` question for `session_id`, when its agent's
/// chat thread carries one.
pub(super) fn latest_question_event(
    store: &crate::store::Store,
    session_id: &str,
) -> Option<crate::coordination::types::CoordinationEvent> {
    let agent_id = relay_agent_id(store, session_id)?;
    let thread = crate::coordination::direct_thread_id(
        ("agent", agent_id.as_str()),
        ("human", crate::coordination::LOCAL_HUMAN_ID),
    );
    store
        .recent_direct_thread_events(&thread, 50)
        .ok()?
        .into_iter()
        .rev()
        .find(|event| {
            event.payload.get("auto").and_then(|v| v.as_str()) == Some("ask")
                && event
                    .payload
                    .get("question")
                    .and_then(|q| q.get("session"))
                    .and_then(|v| v.as_str())
                    == Some(session_id)
        })
}

/// The human's answer to a relayed question, posted back onto the agent's chat
/// thread. Keyed on the question event, so chat and session-screen answers of
/// the same question post once. Returns the saved event id.
pub(super) fn record_question_answer(
    store: &crate::store::Store,
    session_id: &str,
    question_event_id: &str,
    text: &str,
    answers: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    let agent_id = relay_agent_id(store, session_id)?;
    let mut extra = serde_json::Map::new();
    extra.insert("channel".into(), json!("chat"));
    extra.insert("auto".into(), json!("answer"));
    extra.insert("answer_to".into(), json!(question_event_id));
    if let Some(answers) = answers.filter(|a| !a.is_empty()) {
        extra.insert("answers".into(), json!(answers));
    }
    let key = format!("answer:{question_event_id}");
    let origin = session_id.starts_with("inbox-").then_some(session_id);
    let task = store.open_task_of_session(session_id).ok().flatten();
    let about_task = task.as_ref().map(|t| t.id.as_str());
    match store.send_direct_message_typed(
        ("human", crate::coordination::LOCAL_HUMAN_ID),
        ("agent", agent_id.as_str()),
        text,
        &key,
        &chrono::Utc::now().to_rfc3339(),
        origin,
        None,
        about_task,
        extra,
    ) {
        Ok(saved) => {
            // The session already took this as its answer; a queued delivery would wake the agent a second time.
            let now = chrono::Utc::now().to_rfc3339();
            let _ = store.mark_direct_delivered(&saved.event_id, "agent", &agent_id, &now);
            let _ = store.mark_direct_read_event(&saved.event_id, &now);
            crate::session::emit_device_event(json!({
                "kind": "coordination_event",
                "event": saved.clone(),
            }));
            Some(saved.event_id)
        }
        Err(error) => {
            eprintln!("record question answer for {session_id}: {error}");
            None
        }
    }
}

/// Shutdown notice for a session that was mid-turn or had a human waiting.
pub(crate) fn relay_restart(store: &crate::store::Store, session_id: &str, state: &crate::harness::HarnessState) {
    if state.human_waiting.is_empty() {
        return;
    }
    let (agent_id, name) = relay_identity(store, session_id);
    let text = format!("Snippet restarted while {name} was answering; it will pick this up.");
    let key = format!("restart-relay:{session_id}:{}", state.events.len());
    let origin = session_id.starts_with("inbox-").then_some(session_id);
    post_to_human(store, &agent_id, session_id, origin, None, &key, &text, Default::default());
}

/// Create (or re-bind) an agent's durable inbox session.
///
/// The row is written once; the sidecar is re-written every time because it is
/// what makes a resume come back as this agent rather than as a plain session.
pub(super) async fn ensure_agent_inbox(d: &Shared, agent_id: &str) -> Result<(), String> {
    let session_id = crate::session::inbox_session_id(agent_id);
    let path = state_path_for_id(&session_id)
        .ok_or_else(|| format!("invalid inbox session id `{session_id}`"))?;
    let agent = d
        .store
        .get_agent(agent_id)
        .map_err(|error| format!("look up agent `{agent_id}`: {error}"))?
        .ok_or_else(|| format!("unknown agent `{agent_id}`"))?;
    let home = crate::coordination::AgentHome::new(
        crate::coordination::agents_root(&d.mission_control_root),
        agent_id,
    )
    .map_err(|error| error.to_string())?;
    let identity = format!(
        "# {name}\n\nYou are {name}, a durable agent in this device's coordination \
         directory. Direct messages from the human and from peer agents arrive in this \
         inbox. Answer each one where it came from: the envelope's `reply_to` names that \n\
         session, and that is where the sender is reading — not this inbox. Your shell \n\
         is read-only: answer and look things up yourself, and send anything that changes \n\
         files, has side effects or takes long to `mission-control` as a task request.\n",
        name = agent.display_name,
    );
    home.ensure_layout(&identity)
        .map_err(|error| error.to_string())?;

    let sidecar = SessionSidecar {
        role: SessionRole::Standard,
        agent_id: Some(agent_id.to_string()),
    };
    if d.store.has_conversation(&session_id).unwrap_or(false) {
        write_session_sidecar(&path, &sidecar);
        return Ok(());
    }

    let state = HarnessState::blank(
        home.root().display().to_string(),
        Some(format!("{} inbox", agent.display_name)),
    );
    d.store
        .import_session(
            &session_id,
            &crate::config::workspace_key(home.root()),
            &state,
            &crate::conversations::SessionExtras::default(),
        )
        .map_err(|error| format!("create inbox session: {error}"))?;
    write_session_sidecar(&path, &sidecar);
    Ok(())
}

/// The canonical session id a stored delivery recipient names.
fn canonical_session_id(id: &str) -> Option<String> {
    let path = state_path_for_id(id)?;
    Some(crate::session::session_id_for_state_path(&path))
}

const STEER_NOTE_PREFIX: &str = "steer: the user wrote while you were working; let it shape your very next step and acknowledge it in one line. ";
const STOP_NOTE_PREFIX: &str = "stop: the user asked you to stop; report where you stopped and what is left, then wait.";

/// Whether a human message body asks the running session to stop right now.
pub(crate) fn is_stop_command(body: &str) -> bool {
    let trimmed = body
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_punctuation() || matches!(c, '…' | '—' | '–'))
        .trim()
        .to_lowercase();
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    let normalized = words.join(" ");
    matches!(
        normalized.as_str(),
        "stop"
            | "stop now"
            | "stop that"
            | "hold on"
            | "wait"
            | "pause"
            | "cancel that"
            | "abort"
    ) || (normalized.starts_with("stop ") && words.len() <= 6)
}

fn body_of<'a>(payload: &'a serde_json::Value, envelope: &'a str) -> &'a str {
    if let Some(body) = payload.get("body").and_then(|v| v.as_str()) {
        return body;
    }
    if let Some((_, rest)) = envelope.split_once("\nbody: ") {
        return rest.strip_suffix("\n[/direct_message]").unwrap_or(rest);
    }
    envelope
}

fn prefix_envelope_note(envelope: String, prefix: &str) -> String {
    let spaced = if prefix.ends_with(' ') {
        prefix.to_string()
    } else {
        format!("{prefix} ")
    };
    if envelope.contains("\nnote: ") {
        envelope.replacen("\nnote: ", &format!("\nnote: {spaced}"), 1)
    } else if envelope.starts_with("note: ") {
        envelope.replacen("note: ", &format!("note: {spaced}"), 1)
    } else {
        format!("note: {prefix}\n{envelope}")
    }
}

/// Hand a message to a session by its urgency and sender. Urgent ends the
/// current run, then delivers. Normal from the human steers a busy session (or
/// interrupts first on a stop command); normal from an agent queues while busy.
pub(super) async fn deliver_by_urgency(
    d: &Shared,
    session_id: &str,
    envelope: String,
    payload: &serde_json::Value,
    from_human: bool,
) {
    let from_human = from_human
        && payload.get("reaction").is_none()
        && payload.get("kind").and_then(|v| v.as_str()) != Some("reaction");
    let busy = d
        .store
        .get_session_row(session_id)
        .ok()
        .flatten()
        .map(|row| matches!(row.status.as_str(), "running" | "waiting_for_input"))
        .unwrap_or_else(|| {
            crate::session::state_path_for_id(session_id)
                .and_then(|path| crate::session::read_session_meta(&path))
                .is_some_and(|meta| {
                    matches!(
                        meta.status,
                        crate::harness::HarnessStatus::Running
                            | crate::harness::HarnessStatus::WaitingForInput
                    )
                })
        });
    match crate::coordination::typed::urgency_of(payload) {
        crate::coordination::typed::Urgency::Urgent => {
            if busy {
                d.deliver(session_id, LoopInput::Interrupt).await;
            }
            d.deliver(session_id, LoopInput::UserMessage(envelope)).await;
        }
        crate::coordination::typed::Urgency::Normal if busy && from_human => {
            if is_stop_command(body_of(payload, &envelope)) {
                let envelope = prefix_envelope_note(envelope, STOP_NOTE_PREFIX);
                d.deliver(session_id, LoopInput::Interrupt).await;
                d.deliver(session_id, LoopInput::UserMessage(envelope)).await;
            } else {
                let envelope = prefix_envelope_note(envelope, STEER_NOTE_PREFIX);
                d.deliver(session_id, LoopInput::UserMessage(envelope)).await;
            }
        }
        crate::coordination::typed::Urgency::Normal if busy => {
            let item = crate::harness::QueuedInput { id: uuid::Uuid::new_v4().to_string(), text: envelope };
            d.deliver(session_id, LoopInput::Queue(item)).await;
        }
        crate::coordination::typed::Urgency::Normal => {
            crate::session::wake_settled(session_id);
            d.deliver(session_id, LoopInput::UserMessage(envelope)).await;
        }
    }
}

/// Hand a human message to the agent's single in-progress task session instead
/// of its desk. False when there is no such task or its session will not start.
async fn route_to_working_task(
    d: &Shared,
    event: &crate::coordination::types::CoordinationEvent,
    history: &[crate::coordination::types::CoordinationEvent],
    agent_id: &str,
    desk_session_id: &str,
) -> bool {
    let Ok(Some(task)) = d.store.working_task_of_agent(agent_id) else {
        return false;
    };
    let Some((tx, _path, _stream)) = d.ensure_live(&task.session_id).await else {
        eprintln!("[coordination] route to task session failed, desk instead");
        return false;
    };
    drop(tx);
    let prefix = format!("note: about task {} ({}); routed to the session doing it. ", task.id, task.title);
    let envelope = direct_message_envelope(event, history).replacen("note: ", &prefix, 1);
    deliver_by_urgency(d, &task.session_id, envelope, &event.payload, event.actor_kind == "human").await;
    let body = event.payload.get("body").and_then(|v| v.as_str()).unwrap_or_default();
    let quoted: String = body.chars().take(120).collect();
    let notice = format!(
        "{quoted}  routed to task {}'s session; its reply goes to the user directly.",
        task.id
    );
    super::record_agent_message(d, desk_session_id, "daemon", &notice, false).await;
    true
}

async fn deliver_direct(
    d: &Shared,
    pending: &crate::coordination::PendingDirectMessage,
) -> Result<(), String> {
    let event = &pending.event;
    // The session already took this answer as LoopInput::Answer.
    if event.payload.get("auto").and_then(|v| v.as_str()) == Some("answer") {
        return Ok(());
    }

    // A reply addressed to a SESSION comes back to where the question was asked.
    // Recorded as a NOTICE: it lands in the transcript and in the model's next
    // context, but the session's agent is NOT woken. Waking it would spend a turn
    // on a reply whose own envelope says not to act on it — and would start a
    // cold session just to hand it a notice.
    if pending.recipient_kind == "session" {
        // A session that no longer resolves cannot be retried into existence:
        // resolution is a pure lookup, so retrying every 500ms would only storm
        // the log. Report it once and let the delivery row be marked done.
        let Some(session_id) = canonical_session_id(pending.recipient_id.as_str()) else {
            eprintln!(
                "[coordination] direct message {event_id} addressed to unknown session `{session}`",
                event_id = event.event_id,
                session = pending.recipient_id
            );
            return Ok(());
        };
        let body = event
            .payload
            .get("body")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        match event.payload.get("notice").and_then(|v| v.as_str()) {
            Some("task_brief") => {
                d.deliver(&session_id, LoopInput::UserMessage(body.to_string())).await;
                return Ok(());
            }
            Some(_) => {
                super::record_agent_message(d, &session_id, &event.actor_id, body, false).await;
                return Ok(());
            }
            None => {}
        }
        if event.payload.get("reaction").is_some() {
            let envelope = direct_message_envelope(event, &[]);
            if let Some(lane) = reply_lane(d, event, &session_id).as_deref()
                && crate::lanes::deliver_to_lane(&session_id, lane, envelope.clone())
            {
                return Ok(());
            }
            deliver_by_urgency(d, &session_id, envelope, &event.payload, false).await;
            return Ok(());
        }
        if event.payload.get("about_task").and_then(|v| v.as_str()).is_some() {
            let Some((tx, _path, _stream)) = d.ensure_live(&session_id).await else {
                return Err(format!("could not start session `{session_id}`"));
            };
            let history: Vec<_> = d
                .store
                .recent_direct_thread_events(&event.thread_id, DIRECT_HISTORY_ON_WAKE + 1)
                .unwrap_or_default()
                .into_iter()
                .filter(|prior| prior.sequence < event.sequence)
                .collect();
            drop(tx);
            deliver_by_urgency(
                d,
                &session_id,
                direct_message_envelope(event, &history),
                &event.payload,
                event.actor_kind == "human",
            )
            .await;
            return Ok(());
        }
        let lane = reply_lane(d, event, &session_id);
        if let Some(lane) = lane.as_deref() {
            let envelope = format!(
                "[agent_reply]\nfrom: {}:{}\nthread_id: {}\nnote: this answers the message you sent; carry on with your piece, using it. No reply is needed unless you have a follow-up.\nbody: {body}\n[/agent_reply]",
                event.actor_kind, event.actor_id, event.thread_id
            );
            if crate::lanes::deliver_to_lane(&session_id, lane, envelope) {
                return Ok(());
            }
        }
        if answers_own_message(d, event, &session_id) {
            let note = match lane.as_deref() {
                Some(lane) => format!(
                    "this answers a message your lane {lane} sent, and that lane has finished. Use it yourself, or pass it on with delegate_task (lane_id {lane}) if the lane should act on it."
                ),
                None => "this answers the message this session sent; carry on with what you were doing, using it. No reply is needed unless you have a follow-up.".to_string(),
            };
            let envelope = format!(
                "[agent_reply]\nfrom: {}:{}\nthread_id: {}\nnote: {note}\nbody: {body}\n[/agent_reply]",
                event.actor_kind, event.actor_id, event.thread_id
            );
            d.deliver(&session_id, LoopInput::UserMessage(envelope)).await;
            return Ok(());
        }
        // By the session it came from, not the actor's name: agent `snippet`
        // must still reach a session called `snippet-service-…`.
        let from = event
            .payload
            .get("origin_session")
            .and_then(|v| v.as_str())
            .or((event.actor_kind == "session").then_some(event.actor_id.as_str()));
        let from_self = from.is_some_and(|from| session_id.starts_with(from) || from.starts_with(session_id.as_str()));
        if !from_self && !crate::session::is_lane_session_id(&session_id) {
            let reply_to = if event.actor_kind == "agent" { event.actor_id.clone() } else { "human".to_string() };
            let envelope = format!(
                "[direct_message]\nthread_id: {}\nfrom: {}:{}\nreply_to: {reply_to}\n{tags}note: this was sent to this session directly; act on it or answer it with send_agent_message to reply_to.\nbody: {body}\n[/direct_message]",
                event.thread_id, event.actor_kind, event.actor_id,
                tags = crate::coordination::typed::envelope_lines(&event.payload)
            );
            deliver_by_urgency(d, &session_id, envelope, &event.payload, event.actor_kind == "human").await;
            return Ok(());
        }
        if crate::session::is_lane_session_id(&session_id)
            && let Some((parent, lane)) = session_id.rsplit_once("/lanes/") {
                let envelope = format!(
                    "[direct_message]\nthread_id: {}\nfrom: {}:{}\n{tags}note: sent to this lane while it runs; act on it now.\nbody: {body}\n[/direct_message]",
                    event.thread_id, event.actor_kind, event.actor_id,
                    tags = crate::coordination::typed::envelope_lines(&event.payload)
                );
                let reached = crate::lanes::deliver_to_lane(parent, lane, envelope);
                eprintln!("[coordination] message to lane {lane} of {parent}: live mailbox reached = {reached}");
                if reached {
                    return Ok(());
                }
            }
        super::record_agent_message(d, &session_id, &event.actor_id, body, false).await;
        return Ok(());
    }

    if pending.recipient_kind != "agent" {
        return Ok(());
    }
    let agent_id = pending.recipient_id.as_str();
    if agent_id != crate::mission_control::SESSION_ID {
        ensure_agent_inbox(d, agent_id).await?;
    }
    let session_id = recipient_session_id(agent_id);
    let Some((tx, _path, _stream)) = d.ensure_live(&session_id).await else {
        return Err(format!("could not start inbox session `{session_id}`"));
    };
    if event.payload.get("reaction").is_some() {
        let envelope = direct_message_envelope(event, &[]);
        drop(tx);
        deliver_by_urgency(d, &session_id, envelope, &event.payload, false).await;
        return Ok(());
    }
    // Exclude the message being delivered: the envelope carries it as `body`.
    let history: Vec<_> = d
        .store
        .recent_direct_thread_events(&event.thread_id, DIRECT_HISTORY_ON_WAKE + 1)
        .unwrap_or_default()
        .into_iter()
        .filter(|prior| prior.sequence < event.sequence)
        .collect();
    let envelope = direct_message_envelope(event, &history);
    drop(tx);
    // The user is talking to the agent: steer a running desk turn rather than
    // wait behind it, as an agent's reply already does.
    if event.actor_kind == "human" {
        if !route_to_working_task(d, event, &history, agent_id, &session_id).await {
            deliver_by_urgency(d, &session_id, envelope, &event.payload, true).await;
        }
    } else {
        deliver_by_urgency(d, &session_id, envelope, &event.payload, false).await;
    }
    Ok(())
}

/// The lane that sent the latest message from this session on the thread, when
/// a lane rather than the session itself is the one waiting for the answer.
fn reply_lane(d: &crate::serve::Daemon, event: &crate::coordination::types::CoordinationEvent, session_id: &str) -> Option<String> {
    asked_from_session(d, event, session_id)?
        .payload
        .get("origin_lane")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The message from this session that `event` answers: the latest one sent
/// from it to the replier.
fn asked_from_session(
    d: &crate::serve::Daemon,
    event: &crate::coordination::types::CoordinationEvent,
    session_id: &str,
) -> Option<crate::coordination::types::CoordinationEvent> {
    let replier = crate::coordination::actor_ref(&event.actor_kind, &event.actor_id);
    d.store
        .latest_message_from_session(session_id, &replier, &event.created_at)
        .ok()
        .flatten()
}

/// Whether this session's own agent started the conversation being answered,
/// as opposed to a person writing from the session.
fn answers_own_message(d: &crate::serve::Daemon, event: &crate::coordination::types::CoordinationEvent, session_id: &str) -> bool {
    if asked_from_session(d, event, session_id).is_some() {
        return true;
    }
    d.store
        .recent_direct_thread_events(&event.thread_id, 20)
        .unwrap_or_default()
        .iter()
        .any(|prior| {
            prior.sequence < event.sequence
                && prior.actor_kind != "human"
                && prior.payload.get("origin_session").and_then(|v| v.as_str()) == Some(session_id)
        })
}

/// Deliver every accepted-but-undelivered direct message.
///
/// The same shape as the assignment dispatcher: read the pending set, hand each
/// one over, and record the outcome durably. A failure is counted rather than
/// swallowed, so a permanently undeliverable message is visible instead of being
/// retried in silence forever.
pub(super) async fn direct_dispatch_loop(d: Shared) {
    loop {
        let now = chrono::Utc::now().to_rfc3339();
        match d.store.list_pending_direct_deliveries(DELIVER_BATCH, &now) {
            Ok(pending) => {
                for message in pending {
                    let at = chrono::Utc::now().to_rfc3339();
                    match deliver_direct(&d, &message).await {
                        Ok(()) => {
                            if d
                                .store
                                .mark_direct_delivered(
                                    &message.event.event_id,
                                    &message.recipient_kind,
                                    &message.recipient_id,
                                    &at,
                                )
                                .is_ok()
                            {
                                crate::session::emit_device_event(json!({
                                    "kind": "coordination_event",
                                    "event": {
                                        "event_type": "direct_message.delivered",
                                        "event_id": message.event.event_id,
                                        "thread_id": message.event.thread_id,
                                    },
                                }));
                            }
                        }
                        Err(error) => {
                            eprintln!("[coordination] direct message not delivered: {error}");
                            let _ = d.store.record_direct_delivery_failure(
                                &message.event.event_id,
                                &message.recipient_kind,
                                &message.recipient_id,
                                &error,
                                chrono::Utc::now(),
                            );
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("[coordination] could not list pending direct messages: {error}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[derive(Deserialize)]
pub(super) struct DirectSendReq {
    /// `human` or `agent`. A session may only send as itself, so an agent sender
    /// is validated against the directory and the human sender id is fixed.
    from_kind: String,
    from_id: String,
    /// `agent`, `human`, or `session`. A `session` recipient is how a reply goes
    /// back to the chat that asked the question.
    to_kind: String,
    to_id: String,
    #[serde(default)]
    body: String,
    /// The session this message was asked FROM, when it came from one. Recorded
    /// on the event so the exchange is traceable.
    #[serde(default)]
    origin_session: Option<String>,
    #[serde(default)]
    idempotency_key: String,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    reaction: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// POST /coordination/direct/messages — send one direct message.
///
/// Answers 202 rather than 200: the message is durably accepted, and the
/// recipient is woken by the delivery loop, not by this request.
pub(super) async fn direct_send_message(
    State(d): State<Shared>,
    Json(req): Json<DirectSendReq>,
) -> Response {
    let (reply_to, reaction) = match crate::coordination::typed::parse_reply_and_reaction(
        req.reply_to.as_deref(),
        req.reaction.as_deref(),
    ) {
        Ok(parsed) => parsed,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
    };
    let mut extra = serde_json::Map::new();
    if let Some(reaction_name) = reaction.as_ref() {
        if let Some(k) = req.kind.as_deref().map(str::trim).filter(|k| !k.is_empty())
            && k != "reaction"
        {
            return api_error(StatusCode::BAD_REQUEST, "reaction kind must be reaction");
        }
        crate::coordination::typed::apply_reply_and_reaction(
            &mut extra,
            reply_to.as_deref(),
            Some(reaction_name.as_str()),
        );
    } else {
        if req.body.trim().is_empty() {
            return api_error(StatusCode::BAD_REQUEST, "body is required");
        }
        if let Some(k) = req.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            let (parsed_kind, _) = match crate::coordination::typed::parse(Some(k), None) {
                Ok(parsed) => parsed,
                Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
            };
            if let Some(kind_str) = parsed_kind {
                extra.insert("kind".into(), json!(kind_str));
            }
        }
        crate::coordination::typed::apply_reply_and_reaction(&mut extra, reply_to.as_deref(), None);
    }
    let from = (req.from_kind.trim(), req.from_id.trim());
    let to = (req.to_kind.trim(), req.to_id.trim());
    if from.1.is_empty() || to.1.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "from_id and to_id are required");
    }
    // A caller may not send as just any identity. The local human is the only
    // human; an agent sender must be a real directory entry. This is what stops
    // the API from impersonating another agent.
    match from.0 {
        "human" => {
            if from.1 != crate::coordination::LOCAL_HUMAN_ID {
                return api_error(StatusCode::FORBIDDEN, "unknown human sender");
            }
        }
        "agent" => match d.store.get_agent(from.1) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return api_error(StatusCode::NOT_FOUND, "unknown sender agent");
            }
            Err(error) => {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("look up sender: {error}"));
            }
        },
        _ => return api_error(StatusCode::BAD_REQUEST, "from_kind must be human or agent"),
    }
    if to.0 != "agent" && to.0 != "human" && to.0 != "session" {
        return api_error(StatusCode::BAD_REQUEST, "to_kind must be agent, human, or session");
    }
    // A human recipient is the local operator — the only human this device knows.
    // An agent replying to the person who messaged it is the same direct-message
    // path, just the other direction, so it needs no separate endpoint.
    match to.0 {
        "human" => {
            if to.1 != crate::coordination::LOCAL_HUMAN_ID {
                return api_error(StatusCode::NOT_FOUND, "unknown human recipient");
            }
        }
        // A SESSION recipient puts the message back where the question was asked,
        // so a dispatch from inside a chat is answerable in that chat.
        "session" => {
            if crate::session::state_path_for_id(to.1).is_none()
                || crate::session::read_session_meta(
                    &crate::session::state_path_for_id(to.1).expect("checked above"),
                )
                .is_none()
            {
                return api_error(StatusCode::NOT_FOUND, "unknown recipient session");
            }
        }
        _ => match d.store.get_agent(to.1) {
            Ok(Some(_)) => {}
            Ok(None) => return api_error(StatusCode::NOT_FOUND, "unknown recipient agent"),
            Err(error) => {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("look up recipient: {error}"));
            }
        },
    }

    let now = chrono::Utc::now().to_rfc3339();
    let key = if req.idempotency_key.trim().is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        req.idempotency_key.trim().to_string()
    };
    // The session a message was ASKED FROM, when the caller names one. Recorded
    // so the exchange is traceable and a reply knows where to go.
    let origin = req
        .origin_session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let prepared_body = if reaction.is_some() {
        String::new()
    } else {
        match super::transcribe::prepare_message(&d, req.body.trim().to_string()).await {
            Ok(t) => t,
            Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
        }
    };
    if to.0 == "human" {
        extra.insert("channel".into(), json!("chat"));
        if from.0 == "agent" && reaction.is_none() && from.1 != crate::mission_control::SESSION_ID {
            crate::coordination::reach::apply_outbound_human(&d.store, from.1, &prepared_body, &mut extra);
        }
    }
    match d.store.send_direct_message_typed(
        from,
        to,
        &prepared_body,
        &key,
        &now,
        origin,
        None,
        None,
        extra,
    ) {
        Ok(saved) => {
            let _ = d.coordination_events.send(saved.clone());
            crate::session::emit_device_event(serde_json::json!({
                "kind": "coordination_event",
                "event": saved.clone(),
            }));
            // Record what this session SENT, so a later reply has something to
            // attach to. Without it the answer would appear with no question
            // before it and the model could not tell what was asked.
            if let Some(origin) = origin
                && reaction.is_none()
            {
                super::record_agent_message(&d, origin, to.1, &prepared_body, true).await;
            }
            (StatusCode::ACCEPTED, Json(saved)).into_response()
        }
        Err(error) => api_error(StatusCode::CONFLICT, format!("send direct message: {error}")),
    }
}

#[derive(Deserialize)]
pub(super) struct DirectActorQuery {
    actor_kind: String,
    actor_id: String,
}

/// GET /coordination/direct/threads?actor_kind=&actor_id= — one participant's
/// conversations, with unread counts.
pub(super) async fn direct_list_threads(
    State(d): State<Shared>,
    Query(q): Query<DirectActorQuery>,
) -> Response {
    match d.store.list_direct_threads(&q.actor_kind, &q.actor_id) {
        Ok(threads) => Json(threads).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("list direct threads: {error}")),
    }
}

#[derive(Deserialize)]
pub(super) struct DirectMessagesQuery {
    actor_kind: String,
    actor_id: String,
    peer_kind: String,
    peer_id: String,
    #[serde(default)]
    after_sequence: u64,
    #[serde(default = "default_direct_limit")]
    limit: u32,
}
fn default_direct_limit() -> u32 {
    50
}

/// GET /coordination/direct/messages — one page of the conversation between two
/// participants, oldest first. The thread is derived from the pair, so a client
/// never needs to know or remember a thread id.
pub(super) async fn direct_list_messages(
    State(d): State<Shared>,
    Query(q): Query<DirectMessagesQuery>,
) -> Response {
    let from = (q.actor_kind.as_str(), q.actor_id.as_str());
    let to = (q.peer_kind.as_str(), q.peer_id.as_str());
    let thread_id = crate::coordination::direct_thread_id(from, to);
    match d
        .store
        .direct_thread_events(&thread_id, q.after_sequence, q.limit.clamp(1, 200))
    {
        Ok(events) => {
            let event_ids: Vec<String> =
                events.iter().map(|event| event.event_id.clone()).collect();
            let deliveries = match d.store.direct_deliveries(&event_ids) {
                Ok(deliveries) => deliveries
                    .into_iter()
                    .map(|(event_id, (delivered_at, read_at))| {
                        (
                            event_id,
                            json!({"delivered_at": delivered_at, "read_at": read_at}),
                        )
                    })
                    .collect::<serde_json::Map<String, serde_json::Value>>(),
                Err(error) => {
                    return api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("read direct deliveries: {error}"),
                    )
                }
            };
            Json(json!({
                "thread_id": thread_id,
                "events": events,
                "deliveries": deliveries,
            }))
            .into_response()
        }
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("read direct messages: {error}")),
    }
}

#[derive(Deserialize)]
pub(super) struct DirectReadReq {
    actor_kind: String,
    actor_id: String,
    peer_kind: String,
    peer_id: String,
}

/// POST /coordination/direct/read — mark a conversation read for one side.
pub(super) async fn direct_mark_read(
    State(d): State<Shared>,
    Json(req): Json<DirectReadReq>,
) -> Response {
    let thread_id = crate::coordination::direct_thread_id(
        (req.actor_kind.as_str(), req.actor_id.as_str()),
        (req.peer_kind.as_str(), req.peer_id.as_str()),
    );
    let at = chrono::Utc::now().to_rfc3339();
    match d.store.mark_direct_read(
        &req.actor_kind,
        &req.actor_id,
        &thread_id,
        &at,
    ) {
        Ok(()) => Json(json!({"thread_id": thread_id, "read_at": at})).into_response(),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("mark read: {error}")),
    }
}

#[derive(Deserialize)]
pub(super) struct DirectAnswerReq {
    session: String,
    question_event_id: String,
    text: String,
    /// Per-question answers, keyed by question id or index.
    #[serde(default)]
    answers: serde_json::Map<String, serde_json::Value>,
}

/// POST /coordination/direct/answer — answer a relayed `ask_user` question from
/// the agent's chat. The session gets `LoopInput::Answer` exactly as the
/// session-screen path delivers it, and the exchange lands on the chat thread.
pub(super) async fn direct_answer(
    State(d): State<Shared>,
    Json(req): Json<DirectAnswerReq>,
) -> Response {
    let session = req.session.trim();
    if session.is_empty()
        || req.question_event_id.trim().is_empty()
        || req.text.trim().is_empty()
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "session, question_event_id and text are required",
        );
    }
    let waiting = d
        .store
        .get_session_row(session)
        .ok()
        .flatten()
        .is_some_and(|row| row.status == "waiting_for_input")
        && d
            .store
            .load_session_scalar(session)
            .ok()
            .flatten()
            .and_then(|scalar| serde_json::from_str::<serde_json::Value>(&scalar).ok())
            .is_some_and(|state| {
                state
                    .get("pending_question")
                    .is_some_and(|q| !q.is_null())
            });
    if !waiting {
        return api_error(
            StatusCode::CONFLICT,
            "no question is waiting in that session",
        );
    }
    if latest_question_event(&d.store, session)
        .is_some_and(|q| q.event_id != req.question_event_id.trim())
    {
        return api_error(
            StatusCode::CONFLICT,
            "that question is no longer the one waiting in that session",
        );
    }
    crate::speech::cancel_all("interrupted");
    d.deliver(session, LoopInput::Answer(req.text.trim().to_string()))
        .await;
    match record_question_answer(
        &d.store,
        session,
        req.question_event_id.trim(),
        req.text.trim(),
        Some(&req.answers),
    ) {
        Some(event_id) => Json(json!({"event_id": event_id})).into_response(),
        None => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not record the answer",
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::HarnessEvent as E;

    fn dm(reply_to: &str) -> E {
        E::UserInput {
            text: format!("[direct_message]\nthread_id: t\nfrom: human:local\nfrom_kind: human\nto: agent:a\nreply_to: {reply_to}\nbody: hi\n[/direct_message]"),
        }
    }
    fn say(text: &str) -> E {
        E::AssistantText { text: text.into(), quiet: false }
    }
    fn send(to: &str, ok: bool) -> Vec<E> {
        vec![
            E::ToolCall { tool_name: "send_agent_message".into(), arguments: serde_json::json!({"recipient_agent_id": to, "body": "x"}) },
            E::ToolResult {
                tool_name: "send_agent_message".into(),
                result: if ok { serde_json::json!({"to": "human:local"}) } else { serde_json::json!({"error": "no"}) },
            },
        ]
    }

    #[test]
    fn relays_plain_reply_to_human_dm() {
        let events = vec![dm("human"), say("draft"), say("Yes, boss.")];
        assert_eq!(super::turn_end_relay(1, "Scout", &events), Some((0, "Yes, boss.".into())));
    }

    #[test]
    fn no_relay_when_tool_sent_to_human() {
        for to in ["human", "local", "human:local"] {
            let mut events = vec![dm("human")];
            events.extend(send(to, true));
            events.push(say("done"));
            assert_eq!(super::turn_end_relay(1, "Scout", &events), None, "{to}");
        }
    }

    #[test]
    fn relays_when_tool_send_failed_or_went_elsewhere() {
        let mut events = vec![dm("human")];
        events.extend(send("human", false));
        events.push(say("answer"));
        assert!(super::turn_end_relay(1, "Scout", &events).is_some());
        let mut events = vec![dm("human")];
        events.extend(send("mission-control", true));
        events.push(say("answer"));
        assert!(super::turn_end_relay(1, "Scout", &events).is_some());
    }

    #[test]
    fn tool_call_only_turn_gets_fixed_line() {
        let events = vec![dm("human"), E::ToolCall { tool_name: "bash".into(), arguments: serde_json::json!({}) }];
        assert_eq!(
            super::turn_end_relay(1, "Scout", &events),
            Some((0, "Scout finished this turn without a reply; its last step was bash.".into()))
        );
        assert_eq!(super::turn_end_relay(1, "Scout", &[dm("human"), say("  ")]).map(|r| r.1.contains("without a reply")), Some(true));
    }

    #[test]
    fn nothing_waiting_means_no_relay() {
        assert_eq!(super::turn_end_relay(0, "Scout", &[dm("human"), say("x")]), None);
    }

    #[test]
    fn steered_second_message_is_prefixed() {
        let steer = E::Steer { text: "[direct_message]\nthread_id: t\nfrom_kind: human\nreply_to: human\nbody: and?\n[/direct_message]".into() };
        let events = vec![dm("human"), steer, say("Both answered.")];
        assert_eq!(
            super::turn_end_relay(2, "Scout", &events),
            Some((0, "Answering your 2 messages:\n\nBoth answered.".into()))
        );
    }

    #[test]
    fn reply_before_a_later_steer_does_not_cover_it() {
        let steer = E::Steer { text: "[direct_message]\nthread_id: t\nfrom_kind: human\nreply_to: human\nbody: and?\n[/direct_message]".into() };
        let mut events = vec![dm("human")];
        events.extend(send("human", true));
        events.push(steer);
        events.push(say("late"));
        assert!(super::turn_end_relay(2, "Scout", &events).is_some());
    }

    #[test]
    fn parses_human_dm_envelope() {
        assert_eq!(super::human_dm_thread(&match dm("human") { E::UserInput { text } => text, _ => unreachable!() }), Some("t".into()));
        assert_eq!(super::human_dm_thread("[direct_message]\nfrom_kind: human\nreply_to: session:x\n[/direct_message]"), None);
        assert_eq!(super::human_dm_thread("hello"), None);
    }

    #[test]
    fn old_state_json_loads_without_human_waiting() {
        let w: crate::harness::HumanWait = serde_json::from_str(r#"{"thread_id":"t","since":"s"}"#).unwrap();
        assert!(!w.steered);
    }

    #[test]
    fn only_latest_turn_counts() {
        let mut events = vec![dm("human")];
        events.extend(send("human", true));
        events.push(E::UserInput { text: "other".into() });
        events.push(say("x"));
        assert_eq!(super::turn_end_relay(1, "Scout", &events), Some((3, "x".into())));
    }
    use super::*;
    use crate::coordination::types::CoordinationEvent;

    fn event(sequence: u64, actor_id: &str, body: &str, recipient: &str) -> CoordinationEvent {
        CoordinationEvent {
            event_id: format!("e{sequence}"),
            thread_id: "direct:agent:a|human:local".into(),
            partition_key: "thread:direct:agent:a|human:local".into(),
            sequence,
            event_type: "direct_message.sent".into(),
            actor_kind: "human".into(),
            actor_id: actor_id.into(),
            payload_version: 1,
            payload: json!({"body": body, "recipient": recipient}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: format!("k{sequence}"),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn mission_control_uses_its_own_session_not_an_inbox() {
        assert_eq!(
            recipient_session_id(crate::mission_control::SESSION_ID),
            crate::mission_control::SESSION_ID
        );
    }

    #[test]
    fn other_agents_get_a_dedicated_inbox_session() {
        let id = recipient_session_id("snippet");
        assert!(crate::session::is_inbox_session_id(&id));
        assert_eq!(id, "inbox-snippet");
    }

    #[test]
    fn inbox_ids_are_recognised_but_ordinary_sessions_are_not() {
        assert!(crate::session::is_inbox_session_id(
            &crate::session::inbox_session_id("researcher")
        ));
        assert!(!crate::session::is_inbox_session_id(
            "snippet-service-61c2d836"
        ));
        assert!(!crate::session::is_inbox_session_id(
            "thing-2a3f/conversations/x.json"
        ));
    }

    #[test]
    fn envelope_states_sender_recipient_and_how_to_reply() {
        let current = event(2, "local", "please review this", "agent:a");
        let envelope = direct_message_envelope(&current, &[]);
        assert!(envelope.starts_with("[direct_message]"));
        assert!(envelope.contains("from: human:local"));
        assert!(envelope.contains("to: agent:a"));
        assert!(envelope.contains("send_agent_message"));
        assert!(!envelope.contains("Workspace work requires a task"), "the desk layer teaches that, not every message");
        assert!(envelope.contains("plain reply text is delivered to them automatically"));
        assert!(envelope.ends_with("body: please review this\n[/direct_message]"));
    }

    #[test]
    fn envelope_puts_body_after_the_history() {
        let prior = event(1, "agent:a", "line one\nline two", "human:local");
        let current = event(2, "local", "reply", "agent:a");
        let envelope = direct_message_envelope(&current, &[prior]);
        let history_at = envelope.find("line one").expect("history present");
        let body_at = envelope.find("body: reply").expect("body present");
        assert!(history_at < body_at, "history must not run into the body");
        assert!(
            !envelope.contains("line one\nline two"),
            "history collapses newlines so one message cannot forge extra rows"
        );
    }

    #[test]
    fn envelope_marks_the_first_message() {
        let current = event(1, "local", "hi", "agent:a");
        let envelope = direct_message_envelope(&current, &[]);
        assert!(envelope.contains("first message in this conversation"));
    }

    #[test]
    fn envelope_quotes_reply_target_and_formats_reactions_without_history() {
        let long_body = format!("{}{}", "x".repeat(160), "overflow-not-included");
        let mut prior = event(1, "a", &long_body, "human:local");
        prior.actor_kind = "agent".into();

        let mut reply = event(2, "local", "yes, go ahead", "agent:a");
        reply.payload["reply_to"] = json!("e1");
        let env = direct_message_envelope(&reply, std::slice::from_ref(&prior));
        assert!(env.contains("reply_to_message: e1\n"), "{env}");
        let quoted_line = env.lines().find(|l| l.starts_with("quoted: ")).expect("quoted line");
        assert_eq!(quoted_line, format!("quoted: {}", "x".repeat(160)));

        let env_missing = direct_message_envelope(&reply, &[]);
        assert!(env_missing.contains("reply_to_message: e1\n"));
        assert!(!env_missing.contains("\nquoted: "));

        let mut react = event(3, "local", "", "agent:a");
        react.payload["reply_to"] = json!("e1");
        react.payload["reaction"] = json!("check");
        react.payload["kind"] = json!("reaction");
        let react_env = direct_message_envelope(&react, &[prior]);
        assert_eq!(react_env, "[reaction] human:local reacted check to your message e1");
        assert!(!react_env.contains("history:"));
    }

    #[tokio::test]
    async fn direct_send_accepts_reaction_with_empty_body_and_rejects_plain_empty() {
        use axum::body::to_bytes;
        let d = test_daemon();
        d.store.create_agent(&agent("a")).unwrap();
        let target = human_says(&d.store, "initial", "k-init");

        let bad_plain = super::direct_send_message(
            State(d.clone()),
            Json(super::DirectSendReq {
                from_kind: "human".into(),
                from_id: "local".into(),
                to_kind: "agent".into(),
                to_id: "a".into(),
                body: "".into(),
                origin_session: None,
                idempotency_key: "k-empty".into(),
                reply_to: None,
                reaction: None,
                kind: None,
            }),
        )
        .await;
        assert_eq!(bad_plain.status(), StatusCode::BAD_REQUEST);

        let bad_react = super::direct_send_message(
            State(d.clone()),
            Json(super::DirectSendReq {
                from_kind: "human".into(),
                from_id: "local".into(),
                to_kind: "agent".into(),
                to_id: "a".into(),
                body: "".into(),
                origin_session: None,
                idempotency_key: "k-no-target".into(),
                reply_to: None,
                reaction: Some("check".into()),
                kind: Some("reaction".into()),
            }),
        )
        .await;
        assert_eq!(bad_react.status(), StatusCode::BAD_REQUEST);

        let ok_react = super::direct_send_message(
            State(d.clone()),
            Json(super::DirectSendReq {
                from_kind: "human".into(),
                from_id: "local".into(),
                to_kind: "agent".into(),
                to_id: "a".into(),
                body: "".into(),
                origin_session: None,
                idempotency_key: "k-react".into(),
                reply_to: Some(target.event_id.clone()),
                reaction: Some("check".into()),
                kind: Some("reaction".into()),
            }),
        )
        .await;
        assert_eq!(ok_react.status(), StatusCode::ACCEPTED);
        let bytes = to_bytes(ok_react.into_body(), 16 * 1024).await.unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved["payload"]["reply_to"], target.event_id);
        assert_eq!(saved["payload"]["reaction"], "check");
        assert_eq!(saved["payload"]["kind"], "reaction");
        assert_eq!(saved["payload"]["body"], "");

        let mut mc_rx = fake_busy_session(&d, crate::mission_control::SESSION_ID).await;
        let mut react_extra = serde_json::Map::new();
        crate::coordination::typed::apply_reply_and_reaction(
            &mut react_extra,
            Some(&target.event_id),
            Some("eyes"),
        );
        let react_event = d
            .store
            .send_direct_message_typed(
                ("human", "local"),
                ("agent", crate::mission_control::SESSION_ID),
                "",
                "k-mc-react",
                "2026-01-01T00:00:05Z",
                None,
                None,
                None,
                react_extra,
            )
            .unwrap();
        super::deliver_direct(
            &d,
            &crate::coordination::PendingDirectMessage {
                event: react_event,
                recipient_kind: "agent".into(),
                recipient_id: crate::mission_control::SESSION_ID.into(),
            },
        )
        .await
        .unwrap();
        match mc_rx.try_recv().expect("reaction queued on busy session") {
            LoopInput::Queue(item) => {
                assert_eq!(
                    item.text,
                    format!("[reaction] human:local reacted eyes to your message {}", target.event_id)
                );
            }
            other => panic!("expected Queue for reaction on busy session, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn direct_messages_page_carries_agent_delivery_state() {
        use axum::body::to_bytes;
        let d = test_daemon();
        d.store.create_agent(&agent("a")).unwrap();
        let event = human_says(&d.store, "hello", "k-page-deliveries");
        d.store
            .mark_direct_delivered(&event.event_id, "agent", "a", "2026-01-01T00:00:01Z")
            .unwrap();
        d.store
            .mark_direct_read_event(&event.event_id, "2026-01-01T00:00:02Z")
            .unwrap();

        let response = super::direct_list_messages(
            State(d),
            Query(super::DirectMessagesQuery {
                actor_kind: "human".into(),
                actor_id: "local".into(),
                peer_kind: "agent".into(),
                peer_id: "a".into(),
                after_sequence: 0,
                limit: 50,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let page: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            page["deliveries"][event.event_id.as_str()],
            json!({"delivered_at": "2026-01-01T00:00:01Z", "read_at": "2026-01-01T00:00:02Z"})
        );
    }

    fn test_daemon() -> super::Shared {
        std::sync::Arc::new(crate::serve::Daemon {
            config: std::sync::Mutex::new(crate::config::SnippetConfig::default()),
            config_path: std::path::PathBuf::new(),
            token: String::new(),
            hostname: "test".into(),
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            shells: crate::term::SessionTerms::new(std::env::temp_dir()),
            git_write: tokio::sync::Mutex::new(()),
            seen_nonces: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_hidden: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_revision: std::sync::atomic::AtomicU64::new(0),
            mission_control_root: tempfile::tempdir().expect("tempdir").keep(),
            store: crate::store::Store::open_in_memory().unwrap(),
            coordination_events: tokio::sync::broadcast::channel(16).0,
            recurring_root: tempfile::tempdir().expect("tempdir").keep(),
        })
    }

    async fn fake_session(d: &super::Shared, id: &str) -> tokio::sync::mpsc::UnboundedReceiver<LoopInput> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let live = crate::serve::LiveSession {
            input_tx: tx,
            join: tokio::spawn(std::future::pending()),
            state_path: std::path::PathBuf::new(),
            profile: None,
            stream: std::sync::Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default())),
            terms: crate::term::SessionTerms::new(std::env::temp_dir()),
        };
        d.sessions.lock().await.insert(id.to_string(), live);
        rx
    }

    fn agent(id: &str) -> crate::coordination::types::Agent {
        use crate::coordination::types::{Agent, AgentKind, AgentStatus};
        Agent { id: id.into(), display_name: "Scout".into(), handle: id.into(), kind: AgentKind::Worker, status: AgentStatus::Active, capabilities: vec![] }
    }

    fn human_says(store: &crate::store::Store, body: &str, key: &str) -> crate::coordination::types::CoordinationEvent {
        store
            .send_direct_message_typed(("human", "local"), ("agent", "a"), body, key, "2026-01-01T00:00:00Z", None, None, None, Default::default())
            .unwrap()
    }

    #[test]
    fn the_ack_is_not_the_model_replying_and_stays_out_of_history() {
        let mut ack = event(2, "a", "Scout has your message and is working on it.", "human:local");
        ack.payload["auto"] = json!("ack");
        let envelope = direct_message_envelope(&event(3, "local", "and?", "agent:a"), &[ack]);
        assert!(envelope.contains("first message in this conversation"));
        assert!(!envelope.contains("has your message"));
        let events = vec![
            E::UserInput { text: "[direct_message]\nthread_id: t\nfrom_kind: human\nreply_to: human\nbody: hi\n[/direct_message]".into() },
            E::ToolCall { tool_name: "bash".into(), arguments: json!({}) },
        ];
        assert!(super::turn_end_relay(1, "Scout", &events).is_some());
    }

    fn working_task(store: &crate::store::Store, id: &str, session: &str) {
        let mut task = crate::coordination::Task::filed_by_human(id.into(), format!("Task {id}"), "d".into(), "s1".into(), 0, "2026-01-01T00:00:00Z".into());
        task.status = crate::coordination::TaskStatus::InProgress;
        task.session_id = session.into();
        store.create_task(&task).unwrap();
        store.add_task_agent_full(id, "a", "implementer", Some(session), "", "active", "2026-01-01T00:00:00Z").unwrap();
    }

    #[tokio::test]
    async fn human_dm_goes_to_the_single_working_task_session() {
        let d = test_daemon();
        d.store.create_agent(&agent("a")).unwrap();
        working_task(&d.store, "t1", "task-session");
        let mut task_rx = fake_session(&d, "task-session").await;
        let mut desk_rx = fake_session(&d, "inbox-a").await;
        let event = human_says(&d.store, "how is it going", "k1");

        assert!(super::route_to_working_task(&d, &event, &[], "a", "inbox-a").await);

        match task_rx.try_recv().expect("task session got the message") {
            LoopInput::UserMessage(text) | LoopInput::Queue(crate::harness::QueuedInput { text, .. }) => {
                assert!(text.contains("note: about task t1 (Task t1); routed to the session doing it. The user wrote directly"), "{text}");
                assert!(text.contains("from_kind: human") && text.contains("reply_to: human"));
                assert_eq!(crate::serve::human_dm_thread(&text), Some(event.thread_id.clone()));
            }
            _ => panic!("unexpected input"),
        }
        match desk_rx.try_recv().expect("desk got a notice") {
            LoopInput::Notice(crate::harness::HarnessEvent::AgentMessage { body, .. }) => {
                assert!(body.contains("how is it going") && body.contains("routed to task t1's session"), "{body}");
            }
            _ => panic!("desk should only get a notice"),
        }
    }

    #[tokio::test]
    async fn two_working_tasks_leave_the_message_with_the_desk() {
        let d = test_daemon();
        d.store.create_agent(&agent("a")).unwrap();
        working_task(&d.store, "t1", "s-one");
        working_task(&d.store, "t2", "s-two");
        let mut rx = fake_session(&d, "s-one").await;
        let event = human_says(&d.store, "hello", "k1");
        assert!(!super::route_to_working_task(&d, &event, &[], "a", "inbox-a").await);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_reply_from_a_task_session_carries_the_task() {
        let store = crate::store::Store::open_in_memory().unwrap();
        store.create_agent(&agent("a")).unwrap();
        working_task(&store, "t1", "task-session");
        let mut state = crate::harness::HarnessState::blank("/tmp", None);
        state.human_waiting.push(crate::harness::HumanWait { thread_id: "t".into(), since: "s".into(), steered: true, event_index: 0 });
        state.events.push(E::UserInput { text: "[direct_message]\nthread_id: t\nfrom_kind: human\nreply_to: human\nbody: hi\n[/direct_message]".into() });
        state.events.push(E::AssistantText { text: "Almost done.".into(), quiet: false });
        super::relay_desk_reply(&store, "task-session", &mut state);
        // No session row: the identity comes from the task's roster, not the fallback.
        let thread = crate::coordination::direct::direct_thread_id(("agent", "a"), ("human", "local"));
        let sent = store.recent_direct_thread_events(&thread, 5).unwrap();
        assert_eq!(sent.len(), 1, "reply goes out as the task's worker");
        assert_eq!(sent[0].payload["about_task"], "t1");
        assert_eq!(sent[0].payload["body"], "Almost done.");
    }

    #[test]
    fn stop_commands_are_recognised() {
        for cmd in [
            "stop",
            "Stop now.",
            "hold on",
            "stop the deploy",
            "stop that",
            "wait!",
            "pause",
            "cancel that.",
            "abort",
        ] {
            assert!(super::is_stop_command(cmd), "expected stop command: {cmd}");
        }
        for non_stop in [
            "stop words are tricky in this sentence and keep going anyway",
            "please continue",
            "stopping by to check status",
            "",
        ] {
            assert!(!super::is_stop_command(non_stop), "expected non-stop: {non_stop}");
        }
    }

    async fn fake_busy_session(
        d: &super::Shared,
        id: &str,
    ) -> tokio::sync::mpsc::UnboundedReceiver<LoopInput> {
        let mut state = HarnessState::blank("/tmp", None);
        state.status = crate::harness::HarnessStatus::Running;
        d.store
            .import_session(id, "ws", &state, &crate::conversations::SessionExtras::default())
            .unwrap();
        fake_session(d, id).await
    }

    #[tokio::test]
    async fn deliver_by_urgency_steers_human_queues_agent_and_interrupts_on_stop() {
        let d = test_daemon();
        let mut rx = fake_busy_session(&d, "busy-s").await;

        let human_ev = event(1, "local", "use sqlite instead", "agent:a");
        let human_env = direct_message_envelope(&human_ev, &[]);
        super::deliver_by_urgency(&d, "busy-s", human_env, &human_ev.payload, true).await;
        match rx.try_recv().expect("human normal message delivered") {
            LoopInput::UserMessage(text) => {
                assert!(
                    text.contains("note: steer: the user wrote while you were working; let it shape your very next step and acknowledge it in one line."),
                    "{text}"
                );
            }
            other => panic!("expected UserMessage steer, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());

        let mut agent_ev = event(2, "peer", "status update", "agent:a");
        agent_ev.actor_kind = "agent".into();
        let agent_env = direct_message_envelope(&agent_ev, &[]);
        super::deliver_by_urgency(&d, "busy-s", agent_env, &agent_ev.payload, false).await;
        match rx.try_recv().expect("agent normal message queued") {
            LoopInput::Queue(item) => {
                assert!(item.text.contains("body: status update"), "{}", item.text);
                assert!(!item.text.contains("note: steer:"));
            }
            other => panic!("expected Queue for agent normal message, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());

        let stop_ev = event(3, "local", "stop now", "agent:a");
        let stop_env = direct_message_envelope(&stop_ev, &[]);
        super::deliver_by_urgency(&d, "busy-s", stop_env, &stop_ev.payload, true).await;
        assert!(matches!(rx.try_recv().expect("interrupt first"), LoopInput::Interrupt));
        match rx.try_recv().expect("stop message second") {
            LoopInput::UserMessage(text) => {
                assert!(
                    text.contains("note: stop: the user asked you to stop; report where you stopped and what is left, then wait."),
                    "{text}"
                );
            }
            other => panic!("expected UserMessage after Interrupt, got {other:?}"),
        }
        assert!(rx.try_recv().is_err());

        d.store.create_agent(&agent("a")).unwrap();
        working_task(&d.store, "t1", "busy-s");
        let _desk_rx = fake_session(&d, "inbox-a").await;
        let routed_ev = human_says(&d.store, "check edge cases too", "k-route");
        assert!(super::route_to_working_task(&d, &routed_ev, &[], "a", "inbox-a").await);
        match rx.try_recv().expect("busy task session steered") {
            LoopInput::UserMessage(text) => {
                assert!(text.contains("note: steer: the user wrote while you were working;"), "{text}");
                assert!(text.contains("about task t1 (Task t1); routed to the session doing it."), "{text}");
            }
            other => panic!("expected UserMessage steer on busy task session, got {other:?}"),
        }
    }

    fn force_quiet_hours(store: &crate::store::Store, quiet: bool) {
        use chrono::Timelike;
        let now_utc_hour = chrono::Utc::now().hour() as i32;
        // Shift user local clock to 02:00 (inside 22:00..08:00) or 13:00 (outside 22:00..08:00).
        let target_hour = if quiet { 2 } else { 13 };
        let mut diff_hours = target_hour - now_utc_hour;
        if diff_hours > 12 {
            diff_hours -= 24;
        } else if diff_hours < -12 {
            diff_hours += 24;
        }
        crate::mission_autonomy::update_in(store, |st| {
            st.settings.quiet_start = Some("22:00".into());
            st.settings.quiet_end = Some("08:00".into());
            st.settings.utc_offset_minutes = Some(diff_hours * 60);
        })
        .unwrap();
    }

    #[test]
    fn worker_question_in_quiet_hours_posts_chat_with_held_push_and_no_ping() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let agent_id = "reach-quiet-worker";
        store
            .create_agent(&crate::coordination::types::Agent {
                id: agent_id.into(),
                display_name: "Quiet Worker".into(),
                handle: agent_id.into(),
                kind: crate::coordination::types::AgentKind::Worker,
                status: crate::coordination::types::AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        let mut task = crate::coordination::Task::filed_by_human(
            "t-quiet".into(),
            "Task quiet".into(),
            "d".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        task.status = crate::coordination::TaskStatus::InProgress;
        task.session_id = "quiet-task-session".into();
        store.create_task(&task).unwrap();
        store
            .add_task_agent_full(
                "t-quiet",
                agent_id,
                "implementer",
                Some("quiet-task-session"),
                "",
                "active",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();

        force_quiet_hours(&store, true);
        assert_eq!(crate::mission_autonomy::quiet_and_call_in(&store), (true, false));

        let mut dev_rx = crate::session::subscribe_device_events();
        let mut state = crate::harness::HarnessState::blank("/tmp", None);
        state.human_waiting.push(crate::harness::HumanWait {
            thread_id: "t".into(),
            since: "s".into(),
            steered: false,
            event_index: 0,
        });
        state.events.push(E::UserInput {
            text: "[direct_message]\nthread_id: t\nfrom_kind: human\nreply_to: human\nbody: status?\n[/direct_message]".into(),
        });
        state.events.push(E::AssistantText {
            text: "Should I migrate the production schema now?".into(),
            quiet: false,
        });

        super::relay_desk_reply(&store, "quiet-task-session", &mut state);

        let thread =
            crate::coordination::direct::direct_thread_id(("agent", agent_id), ("human", "local"));
        let sent = store.recent_direct_thread_events(&thread, 10).unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].payload["channel"], "chat");
        assert_eq!(sent[0].payload["kind"], "question");
        assert_eq!(sent[0].payload["held"], json!(["push"]));

        let mut chat_events = 0usize;
        let mut ping_events = 0usize;
        while let Ok(ev) = dev_rx.try_recv() {
            if ev["kind"] == "ping" && ev["agent"] == agent_id {
                ping_events += 1;
            }
            if ev["kind"] == "coordination_event" && ev["event"]["actor_id"] == agent_id {
                assert_eq!(ev["event"]["payload"]["channel"], "chat");
                assert_eq!(ev["event"]["payload"]["held"], json!(["push"]));
                chat_events += 1;
            }
        }
        assert_eq!(chat_events, 1, "expected exactly one chat coordination_event");
        assert_eq!(ping_events, 0, "expected no ping device event in quiet hours");
        assert_eq!(crate::mission_autonomy::load_from(&store).held_pings.len(), 1);

        // Outside quiet hours, a worker question emits both chat and a tagged ping event.
        force_quiet_hours(&store, false);
        assert_eq!(crate::mission_autonomy::quiet_and_call_in(&store), (false, false));
        state.human_waiting.push(crate::harness::HumanWait {
            thread_id: "t".into(),
            since: "s2".into(),
            steered: false,
            event_index: 2,
        });
        state.events.push(E::UserInput {
            text: "[direct_message]\nthread_id: t\nfrom_kind: human\nreply_to: human\nbody: next?\n[/direct_message]".into(),
        });
        state.events.push(E::AssistantText {
            text: "Ready to cut the release?".into(),
            quiet: false,
        });
        super::relay_desk_reply(&store, "quiet-task-session", &mut state);

        let mut day_pings = Vec::new();
        while let Ok(ev) = dev_rx.try_recv() {
            if ev["kind"] == "ping" && ev["agent"] == agent_id {
                day_pings.push(ev);
            }
        }
        assert_eq!(day_pings.len(), 1);
        assert_eq!(day_pings[0]["channel"], "push");
        assert_eq!(day_pings[0]["agent"], agent_id);
    }

    #[tokio::test]
    async fn agent_reach_routes_tool_and_non_mc_ping_user_work_end_to_end() {
        use axum::body::to_bytes;
        use crate::tools::{Tool, ToolContext};

        let d = test_daemon();
        let agent_id = "reach-route-worker";
        d.store
            .create_agent(&crate::coordination::types::Agent {
                id: agent_id.into(),
                display_name: "Route Worker".into(),
                handle: agent_id.into(),
                kind: crate::coordination::types::AgentKind::Worker,
                status: crate::coordination::types::AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();

        let missing = crate::serve::agent_routes::get_agent_reach(
            State(d.clone()),
            axum::extract::Path("ghost".into()),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let get_resp = crate::serve::agent_routes::get_agent_reach(
            State(d.clone()),
            axum::extract::Path(agent_id.into()),
        )
        .await;
        assert_eq!(get_resp.status(), StatusCode::OK);
        let initial: serde_json::Value =
            serde_json::from_slice(&to_bytes(get_resp.into_body(), 16 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(initial["agent_id"], agent_id);
        assert_eq!(initial["policy"]["question"], json!(["push", "chat"]));
        assert_eq!(initial["channels"]["push"]["enabled"], true);
        assert_eq!(initial["channels"]["voice"]["enabled"], false);

        let put_resp = crate::serve::agent_routes::put_agent_reach(
            State(d.clone()),
            axum::extract::Path(agent_id.into()),
            Json(json!({
                "channels": { "push": { "enabled": false, "quiet_hours": true } },
                "policy": { "question": ["push", "chat"], "result": ["chat"] }
            })),
        )
        .await;
        assert_eq!(put_resp.status(), StatusCode::OK);
        let updated: serde_json::Value =
            serde_json::from_slice(&to_bytes(put_resp.into_body(), 16 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(updated["channels"]["push"]["enabled"], false);

        // Verify reach_policy tool and non-Mission-Control ping_user on a file-backed store.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("reach-tool.sqlite");
        let file_store = crate::store::Store::open(&db_path).unwrap();
        file_store
            .create_agent(&crate::coordination::types::Agent {
                id: agent_id.into(),
                display_name: "Route Worker".into(),
                handle: agent_id.into(),
                kind: crate::coordination::types::AgentKind::Worker,
                status: crate::coordination::types::AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        force_quiet_hours(&file_store, false);

        let ctx = ToolContext::new(dir.path().to_path_buf())
            .unwrap()
            .with_store_path(db_path.clone())
            .with_agent_id(agent_id);

        let tool = crate::mission_tools::ReachPolicyTool;
        let got = tool.execute(&ctx, json!({"action": "get"})).await.unwrap();
        assert_eq!(got.value["data"]["agent_id"], agent_id);
        assert_eq!(got.value["data"]["channels"]["push"]["enabled"], true);

        let set_res = tool
            .execute(
                &ctx,
                json!({"action": "set", "channel": "push", "enabled": false}),
            )
            .await
            .unwrap();
        assert_eq!(set_res.value["data"]["channels"]["push"]["enabled"], false);

        let mut dev_rx = crate::session::subscribe_device_events();
        let ping_res = crate::mission_tools::PingUser
            .execute(
                &ctx,
                json!({"title": "Need input", "message": "Pick option A or B?", "kind": "decision"}),
            )
            .await
            .unwrap();
        assert_eq!(ping_res.value["data"]["ping"], "disabled");
        assert_eq!(ping_res.value["data"]["used"], json!(["chat"]));
        while let Ok(ev) = dev_rx.try_recv() {
            assert!(
                !(ev["kind"] == "ping" && ev["agent"] == agent_id),
                "disabled push must not emit ping"
            );
        }

        let desk_id = crate::session::minimal_inbox_identity(agent_id);
        assert!(desk_id.contains("Your channels and the kinds of message each carries are in reach_policy;"));
    }

    fn waiting_question_state() -> crate::harness::HarnessState {
        let mut state = crate::harness::HarnessState::blank("/tmp", None);
        state.status = crate::harness::HarnessStatus::WaitingForInput;
        state.pending_question = Some(json!({
            "questions": [
                {"id": "q0", "text": "Ship now or wait for review?", "header": "Deploy", "answer_kind": {"kind": "yes_no"}},
                {"id": "q1", "text": "Which channel?", "answer_kind": {"kind": "free_text"}},
            ],
            "context": null,
        }));
        state.events.push(E::UserQuestion {
            questions: state.pending_question.clone().unwrap(),
        });
        state
    }

    fn import_waiting_session(
        store: &crate::store::Store,
        id: &str,
        agent_id: &str,
        state: &crate::harness::HarnessState,
    ) {
        store
            .import_session(
                id,
                "wk",
                state,
                &crate::conversations::SessionExtras {
                    agent_id: Some(agent_id.to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn thread_events(
        store: &crate::store::Store,
        agent_id: &str,
    ) -> Vec<crate::coordination::types::CoordinationEvent> {
        let thread = crate::coordination::direct_thread_id(
            ("agent", agent_id),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        store.recent_direct_thread_events(&thread, 50).unwrap()
    }

    #[test]
    fn relay_question_posts_once_per_question_event() {
        let store = crate::store::Store::open_in_memory().unwrap();
        store.create_agent(&agent("a")).unwrap();
        let state = waiting_question_state();
        import_waiting_session(&store, "sess-1", "a", &state);
        super::relay_question(&store, "sess-1", &state);
        super::relay_question(&store, "sess-1", &state);
        let asks: Vec<_> = thread_events(&store, "a")
            .into_iter()
            .filter(|e| e.payload.get("auto").and_then(|v| v.as_str()) == Some("ask"))
            .collect();
        assert_eq!(asks.len(), 1);
        let ask = &asks[0];
        assert_eq!(ask.payload["kind"], "question");
        assert_eq!(ask.payload["body"], "Deploy: Ship now or wait for review?\nWhich channel?");
        assert_eq!(ask.payload["question"]["session"], "sess-1");
        assert_eq!(
            ask.payload["question"]["questions"],
            state.pending_question.as_ref().unwrap()["questions"]
        );
        assert!(ask.payload["question"]["asked_at"].as_str().is_some());
        assert_eq!(ask.idempotency_key, "ask:sess-1:0");
    }

    #[test]
    fn relay_question_skips_mission_control_and_agentless_sessions() {
        let store = crate::store::Store::open_in_memory().unwrap();
        store.create_agent(&agent("a")).unwrap();
        let state = waiting_question_state();
        super::relay_question(&store, crate::mission_control::SESSION_ID, &state);
        super::relay_question(&store, "sess-unknown", &state);
        assert!(thread_events(&store, "a").is_empty());
        // The relay_identity fallback must not post as `snippet` either.
        let snippet_thread = crate::coordination::direct_thread_id(
            ("agent", "snippet"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        assert!(store
            .recent_direct_thread_events(&snippet_thread, 50)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn direct_answer_conflicts_when_no_question_waits() {
        let d = test_daemon();
        let res = super::direct_answer(
            State(d.clone()),
            Json(super::DirectAnswerReq {
                session: "sess-1".into(),
                question_event_id: "e1".into(),
                text: "yes".into(),
                answers: Default::default(),
            }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn direct_answer_delivers_and_records_on_the_thread() {
        let d = test_daemon();
        let store = d.store.clone();
        store.create_agent(&agent("a")).unwrap();
        let state = waiting_question_state();
        import_waiting_session(&store, "sess-1", "a", &state);
        super::relay_question(&store, "sess-1", &state);
        let ask = thread_events(&store, "a")
            .into_iter()
            .find(|e| e.payload.get("auto").and_then(|v| v.as_str()) == Some("ask"))
            .unwrap();
        let mut rx = fake_session(&d, "sess-1").await;
        let mut answers = serde_json::Map::new();
        answers.insert("q0".into(), json!("wait"));
        let res = super::direct_answer(
            State(d.clone()),
            Json(super::DirectAnswerReq {
                session: "sess-1".into(),
                question_event_id: ask.event_id.clone(),
                text: "wait for review".into(),
                answers,
            }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        match rx.try_recv().expect("answer delivered to session") {
            LoopInput::Answer(text) => assert_eq!(text, "wait for review"),
            other => panic!("expected LoopInput::Answer, got {other:?}"),
        }
        let events = thread_events(&store, "a");
        let answer = events
            .iter()
            .find(|e| e.payload.get("auto").and_then(|v| v.as_str()) == Some("answer"))
            .expect("human answer event on the thread");
        assert_eq!(answer.actor_kind, "human");
        assert_eq!(answer.payload["body"], "wait for review");
        assert_eq!(answer.payload["answer_to"], ask.event_id);
        assert_eq!(answer.payload["answers"]["q0"], "wait");
        assert_eq!(answer.idempotency_key, format!("answer:{}", ask.event_id));
    }

    #[tokio::test]
    async fn session_screen_answer_mirrors_to_the_thread_once() {
        let d = test_daemon();
        let store = d.store.clone();
        store.create_agent(&agent("a")).unwrap();
        let state = waiting_question_state();
        import_waiting_session(&store, "sess-1", "a", &state);
        super::relay_question(&store, "sess-1", &state);
        // What the ws `answer` frame does: find the relayed question, mirror it.
        for _ in 0..2 {
            let question = super::latest_question_event(&store, "sess-1").unwrap();
            super::record_question_answer(&store, "sess-1", &question.event_id, "ship it", None);
        }
        let ask = thread_events(&store, "a")
            .into_iter()
            .find(|e| e.payload.get("auto").and_then(|v| v.as_str()) == Some("ask"))
            .unwrap();
        let answers: Vec<_> = thread_events(&store, "a")
            .into_iter()
            .filter(|e| e.payload.get("auto").and_then(|v| v.as_str()) == Some("answer"))
            .collect();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].payload["answer_to"], ask.event_id);
        assert_eq!(answers[0].payload["body"], "ship it");
    }

}
