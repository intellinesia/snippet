use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};

use crate::conversations::SessionRow;
use crate::coordination::types::{AgentKind, AgentStatus};
use crate::harness::HarnessEvent;
use crate::session::{SessionRole, SessionSidecar};
use crate::store::Store;

use super::{Shared, api_error, is_agent_desk};

const MAX_NOTE_CHARS: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PendingHandoff {
    pub(crate) to_agent: String,
    pub(crate) note: String,
    pub(crate) by: String,
    pub(crate) at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct HandoffOutcome {
    pub(crate) session: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) pending: bool,
}

#[derive(Debug)]
pub(crate) enum HandoffError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl HandoffError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::NotFound(m) | Self::BadRequest(m) | Self::Internal(m) => m,
        }
    }

    fn into_response(self) -> Response {
        match self {
            Self::NotFound(m) => api_error(StatusCode::NOT_FOUND, m),
            Self::BadRequest(m) => api_error(StatusCode::BAD_REQUEST, m),
            Self::Internal(m) => api_error(StatusCode::INTERNAL_SERVER_ERROR, m),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct HandoffReq {
    session: String,
    to_agent: String,
    #[serde(default)]
    note: String,
}

/// POST /session/handoff {session, to_agent, note?} -> {session, from, to, pending}
pub(crate) async fn handoff_route(
    State(d): State<Shared>,
    Json(req): Json<HandoffReq>,
) -> Response {
    match hand_off_session(&d, &req.session, &req.to_agent, &req.note, "human").await {
        Ok(out) => Json(out).into_response(),
        Err(err) => err.into_response(),
    }
}

pub(crate) fn validate_handoff_request(
    store: &Store,
    session: &str,
    to_agent: &str,
    note: &str,
    agent_hint: Option<&str>,
) -> Result<(SessionRow, String, String), HandoffError> {
    let id = crate::session::state_path_for_id(session)
        .map(|p| crate::session::session_id_for_state_path(&p))
        .unwrap_or_else(|| crate::conversations::canonical_session_id(session));
    if crate::mission_control::is_session_id(&id)
        || is_agent_desk(&id)
        || crate::session::is_lane_session_id(&id)
    {
        return Err(HandoffError::BadRequest(
            "cannot hand off Mission Control, an agent desk, or a lane".into(),
        ));
    }
    let row = store
        .get_session_row(&id)
        .map_err(|e| HandoffError::Internal(e.to_string()))?
        .ok_or_else(|| HandoffError::NotFound(format!("unknown session `{id}`")))?;
    if matches!(row.role.as_str(), "mission_control" | "coordination")
        || matches!(row.kind.as_str(), "mission_control" | "inbox")
    {
        return Err(HandoffError::BadRequest(
            "cannot hand off Mission Control, an agent desk, or a coordination session".into(),
        ));
    }
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(HandoffError::BadRequest(format!(
            "handoff note exceeds {MAX_NOTE_CHARS} characters"
        )));
    }
    let seated = crate::session::session_task_contexts(store)
        .seats
        .get(&row.id)
        .and_then(|(_, _, agent)| agent.clone());
    let bound = row.agent_id.as_deref().or(seated.as_deref()).or(agent_hint);
    let from = crate::session::owner_agent_id_for(&row.id, bound);

    let to = to_agent.trim();
    if to.is_empty() {
        return Err(HandoffError::BadRequest("to_agent is required".into()));
    }
    let agent = store
        .get_agent(to)
        .map_err(|e| HandoffError::Internal(e.to_string()))?
        .ok_or_else(|| HandoffError::NotFound(format!("unknown agent `{to}`")))?;
    if agent.status != AgentStatus::Active {
        return Err(HandoffError::BadRequest(format!(
            "agent `{to}` is not active"
        )));
    }
    if agent.kind != AgentKind::Worker {
        return Err(HandoffError::BadRequest(format!(
            "cannot hand off session to `{to}`"
        )));
    }
    if to == from {
        return Err(HandoffError::BadRequest(format!(
            "session is already owned by `{to}`"
        )));
    }
    Ok((row, from, to.to_string()))
}

fn emit_handoff_device_event(session: &str, status: &str, from: &str, to: &str, pending: bool) {
    crate::session::emit_device_event(serde_json::json!({
        "kind": "session_handoff",
        "session": session,
        "status": status,
        "from": from,
        "to": to,
        "pending": pending,
    }));
}

pub(crate) fn queue_pending_handoff_in_store(
    store: &Store,
    session: &str,
    to_agent: &str,
    note: &str,
    by: &str,
    agent_hint: Option<&str>,
) -> Result<HandoffOutcome, HandoffError> {
    let (row, from, to) = validate_handoff_request(store, session, to_agent, note, agent_hint)?;
    let at = crate::session::now_unix_secs();
    let pending = PendingHandoff {
        to_agent: to.clone(),
        note: note.to_string(),
        by: by.to_string(),
        at,
    };
    let encoded =
        serde_json::to_string(&pending).map_err(|e| HandoffError::Internal(e.to_string()))?;
    store
        .set_pending_handoff(&row.id, Some(&encoded))
        .map_err(|e| HandoffError::Internal(e.to_string()))?;
    let event = HarnessEvent::SessionHandoff {
        from: from.clone(),
        to: to.clone(),
        note: note.to_string(),
        by: by.to_string(),
        at,
        pending: true,
    };
    let now_rfc = chrono::Utc::now().to_rfc3339();
    store
        .append_conversation_events(&row.id, &[event], &now_rfc)
        .map_err(|e| HandoffError::Internal(e.to_string()))?;
    emit_handoff_device_event(&row.id, &row.status, &from, &to, true);
    Ok(HandoffOutcome {
        session: row.id,
        from,
        to,
        pending: true,
    })
}

pub(crate) fn queue_pending_handoff_from_tool(
    store: &Store,
    session: &str,
    agent_hint: Option<&str>,
    to_agent: &str,
    note: &str,
) -> Result<HandoffOutcome, HandoffError> {
    let (row, from, _) = validate_handoff_request(store, session, to_agent, note, agent_hint)?;
    queue_pending_handoff_in_store(store, &row.id, to_agent, note, &from, agent_hint)
}

pub(crate) async fn hand_off_session(
    d: &Shared,
    session: &str,
    to_agent: &str,
    note: &str,
    by: &str,
) -> Result<HandoffOutcome, HandoffError> {
    let (row, from, to) = validate_handoff_request(&d.store, session, to_agent, note, None)?;
    let busy = matches!(row.status.as_str(), "running" | "waiting_for_input")
        || crate::session::state_path_for_id(&row.id)
            .and_then(|sp| crate::session::read_session_meta(&sp))
            .is_some_and(|s| {
                matches!(
                    s.status,
                    crate::harness::HarnessStatus::Running
                        | crate::harness::HarnessStatus::WaitingForInput
                )
            });
    if busy {
        return queue_pending_handoff_in_store(&d.store, &row.id, &to, note, by, None);
    }
    apply_handoff_now(d, &row.id, &from, &to, note, by).await
}

pub(crate) fn handoff_envelope(from: &str, by: &str, note: &str) -> String {
    let by_label = if by == "human" { "the user" } else { by };
    let trimmed = note.trim();
    let instructions = if trimmed.is_empty() {
        format!("You now own this session. Its history above is {from}'s; continue from here.")
    } else {
        format!("You now own this session. Its history above is {from}'s; continue from the note.")
    };
    crate::envelope::Envelope::new("handoff")
        .attr("from", from)
        .attr("by", by_label)
        .child("handoff_note", trimmed)
        .child("note", instructions)
        .render()
}

pub(crate) async fn apply_handoff_now(
    d: &Shared,
    session: &str,
    from: &str,
    to_agent: &str,
    note: &str,
    by: &str,
) -> Result<HandoffOutcome, HandoffError> {
    let _ = d.store.set_pending_handoff(session, None);
    let (sp, _) = d
        .load_session_workspace_in(session)
        .map_err(HandoffError::BadRequest)?;
    let agent_id =
        (to_agent != crate::coordination::SNIPPET_AGENT_ID).then(|| to_agent.to_string());
    crate::session::write_session_sidecar_in(
        &d.store,
        &sp,
        &SessionSidecar {
            role: SessionRole::Standard,
            agent_id,
        },
    );
    let profile = d.resume_profile(session, &sp);
    let at = crate::session::now_unix_secs();
    let event = HarnessEvent::SessionHandoff {
        from: from.to_string(),
        to: to_agent.to_string(),
        note: note.to_string(),
        by: by.to_string(),
        at,
        pending: false,
    };
    let envelope = handoff_envelope(from, by, note);
    d.restart_session_with_profile(
        session,
        profile.as_deref(),
        false,
        Some(event),
        Some(envelope),
    )
    .await
    .map_err(HandoffError::Internal)?;
    let status = d
        .store
        .get_session_row(session)
        .ok()
        .flatten()
        .map(|r| r.status)
        .unwrap_or_else(|| "idle".to_string());
    emit_handoff_device_event(session, &status, from, to_agent, false);
    Ok(HandoffOutcome {
        session: session.to_string(),
        from: from.to_string(),
        to: to_agent.to_string(),
        pending: false,
    })
}

pub(crate) async fn apply_pending_handoff(d: &Shared, session: &str) {
    let Ok(Some(row)) = d.store.get_session_row(session) else {
        return;
    };
    if matches!(row.status.as_str(), "running" | "waiting_for_input") {
        return;
    }
    let Ok(Some(raw)) = d.store.take_pending_handoff(&row.id) else {
        return;
    };
    let Ok(pending) = serde_json::from_str::<PendingHandoff>(&raw) else {
        return;
    };
    let Ok((_, from, to)) =
        validate_handoff_request(&d.store, &row.id, &pending.to_agent, &pending.note, None)
    else {
        return;
    };
    if let Err(err) = apply_handoff_now(d, &row.id, &from, &to, &pending.note, &pending.by).await {
        eprintln!(
            "[handoff] could not apply pending hand-off on `{}`: {}",
            row.id,
            err.message()
        );
    }
}

pub(crate) fn apply_pending_handoff_if_due(session_id: &str) {
    let Some(d) = super::live_daemon() else {
        return;
    };
    let Ok(Some(row)) = d.store.get_session_row(session_id) else {
        return;
    };
    if row.pending_handoff.is_none()
        || matches!(row.status.as_str(), "running" | "waiting_for_input")
    {
        return;
    }
    let d = d.clone();
    let session_id = row.id;
    tokio::spawn(async move {
        apply_pending_handoff(&d, &session_id).await;
    });
}
