//! Group threads over HTTP, and the loop that hands each scheduled group
//! wake to its agent's session.
//!
//! The durable half — membership, posts, reply claims and the wake schedule —
//! lives in [`crate::coordination::groups`]. This module is the app's surface
//! for it plus the delivery half, which reuses the direct-message wake path so
//! a restart loses nothing.

use super::api_error;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post, put};
use serde::Deserialize;

use super::Shared;

/// How many pending group deliveries one pass of the loop takes.
const GROUP_DELIVER_BATCH: u32 = 50;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{id}", put(update_group).delete(delete_group))
        .route("/groups/{id}/events", get(group_events))
        .route("/groups/{id}/post", post(post_group))
}

#[derive(Deserialize)]
struct GroupCreateReq {
    title: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    lead: Option<String>,
}

#[derive(Deserialize)]
struct GroupUpdateReq {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    lead: Option<String>,
    #[serde(default)]
    add_members: Option<Vec<String>>,
    #[serde(default)]
    remove_members: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct GroupEventsQuery {
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct GroupPostReq {
    #[serde(default)]
    body: String,
    #[serde(default)]
    mentions: Vec<String>,
    #[serde(default)]
    in_reply_to: Option<String>,
    #[serde(default)]
    reaction: Option<String>,
}

fn store_error(status: StatusCode, error: crate::store::StoreError) -> Response {
    match error {
        crate::store::StoreError::InvalidInput(message) => {
            api_error(StatusCode::BAD_REQUEST, message)
        }
        other => api_error(status, format!("group: {other}")),
    }
}

/// `id` names a real agent, or Mission Control's fixed session id.
fn known_agent(d: &Shared, id: &str) -> bool {
    id == crate::mission_control::SESSION_ID || d.store.get_agent(id).ok().flatten().is_some()
}

/// GET /groups — every group thread with the human's unread count.
async fn list_groups(State(d): State<Shared>) -> Response {
    match d.store.list_groups() {
        Ok(groups) => Json(groups).into_response(),
        Err(error) => store_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

/// POST /groups {title, members, lead?} — create a group; the human joins
/// automatically. Answers the new group object.
async fn create_group(State(d): State<Shared>, Json(req): Json<GroupCreateReq>) -> Response {
    let title = req.title.trim();
    if title.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title is required");
    }
    if req.members.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "members must name at least one agent",
        );
    }
    let mut members: Vec<String> = Vec::new();
    for member in &req.members {
        let member = member.trim();
        if member.is_empty() {
            continue;
        }
        if !known_agent(&d, member) {
            return api_error(StatusCode::BAD_REQUEST, format!("unknown agent `{member}`"));
        }
        if !members.iter().any(|m| m == member) {
            members.push(member.to_string());
        }
    }
    if members.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "members must name at least one agent",
        );
    }
    let lead = req.lead.as_deref().map(str::trim).filter(|l| !l.is_empty());
    if let Some(lead) = lead
        && !members.iter().any(|m| m == lead)
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!("lead `{lead}` is not one of the group's members"),
        );
    }
    let now = chrono::Utc::now().to_rfc3339();
    match d
        .store
        .create_group(title, ("human", "human"), &members, lead, &now)
    {
        Ok(group) => {
            crate::session::emit_device_event(serde_json::json!({
                "kind": "group_changed",
                "group": group,
            }));
            notify_added(&d, &group, members);
            Json(group).into_response()
        }
        Err(error) => store_error(StatusCode::CONFLICT, error),
    }
}

/// DELETE /groups/:id — delete the group, its posts and any pending wakes.
async fn delete_group(
    State(d): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match d.store.delete_group(&id) {
        Ok(true) => {
            crate::session::emit_device_event(serde_json::json!({
                "kind": "group_deleted",
                "id": id,
            }));
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no such group"),
        Err(error) => store_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

fn group_added_envelope(group: &crate::coordination::GroupSummary) -> String {
    let members: Vec<&str> = group
        .members
        .iter()
        .filter(|m| m.kind == "agent")
        .map(|m| m.id.as_str())
        .collect();
    format!(
        "[group_added]\nthread_id: {}\ntitle: {}\nmembers: {}\nlead: {}\n\
         note: you were added to this group chat. Nothing to do now: do not post and write \
         nothing, just end your turn. Posts reach you as [group_message]; answer with group_thread post.\n\
         [/group_added]",
        group.id,
        group.title,
        members.join(", "),
        group.lead.as_deref().unwrap_or("none"),
    )
}

/// Tell each newly added agent's desk it joined the group, in the background.
fn notify_added(d: &Shared, group: &crate::coordination::GroupSummary, agents: Vec<String>) {
    if agents.is_empty() {
        return;
    }
    let d = d.clone();
    let envelope = group_added_envelope(group);
    tokio::spawn(async move {
        for agent_id in agents {
            if agent_id != crate::mission_control::SESSION_ID
                && let Err(error) = super::direct::ensure_agent_inbox(&d, &agent_id).await
            {
                eprintln!("[coordination] group notice for {agent_id} not delivered: {error}");
                continue;
            }
            let session_id = super::direct::recipient_session_id(&agent_id);
            if d.ensure_live(&session_id).await.is_none() {
                continue;
            }
            super::direct::deliver_by_urgency(
                &d,
                &session_id,
                envelope.clone(),
                &serde_json::json!({}),
                false,
            )
            .await;
        }
    });
}

/// PUT /groups/:id — rename, re-lead, add or remove members. Answers the
/// updated group object.
async fn update_group(
    State(d): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<GroupUpdateReq>,
) -> Response {
    let Some(_) = d.store.get_group(&id).ok().flatten() else {
        return api_error(StatusCode::NOT_FOUND, "no such group");
    };
    for member in req.add_members.iter().flatten() {
        let member = member.trim();
        if !member.is_empty() && !known_agent(&d, member) {
            return api_error(StatusCode::BAD_REQUEST, format!("unknown agent `{member}`"));
        }
    }
    let mut added = Vec::new();
    for member in req.add_members.iter().flatten() {
        let member = member.trim();
        if member.is_empty()
            || d.store
                .is_group_member(&id, "agent", member)
                .unwrap_or(false)
        {
            continue;
        }
        if let Err(error) = d.store.add_group_member(&id, member) {
            return store_error(StatusCode::CONFLICT, error);
        }
        added.push(member.to_string());
    }
    for member in req.remove_members.iter().flatten() {
        let member = member.trim();
        if member.is_empty() {
            continue;
        }
        if member == "human" {
            return api_error(StatusCode::BAD_REQUEST, "the human cannot leave a group");
        }
        if let Err(error) = d.store.remove_group_member(&id, member) {
            return store_error(StatusCode::CONFLICT, error);
        }
    }
    if let Some(lead) = req.lead.as_deref().map(str::trim).filter(|l| !l.is_empty())
        && let Err(error) = d.store.set_group_lead(&id, lead)
    {
        return store_error(StatusCode::BAD_REQUEST, error);
    }
    if let Some(title) = req
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        && let Err(error) = d.store.set_group_title(&id, title)
    {
        return store_error(StatusCode::CONFLICT, error);
    }
    match d.store.get_group(&id) {
        Ok(Some(group)) => {
            crate::session::emit_device_event(serde_json::json!({
                "kind": "group_changed",
                "group": group,
            }));
            notify_added(&d, &group, added);
            Json(group).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such group"),
        Err(error) => store_error(StatusCode::INTERNAL_SERVER_ERROR, error),
    }
}

/// GET /groups/:id/events?after=<sequence>&limit= — one page of the group's
/// posts, oldest first. Reading the page marks it read for the human.
async fn group_events(
    State(d): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<GroupEventsQuery>,
) -> Response {
    if d.store.get_group(&id).ok().flatten().is_none() {
        return api_error(StatusCode::NOT_FOUND, "no such group");
    }
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    match d.store.events_for_thread(&id, after, limit) {
        Ok(events) => {
            let up_to = events.last().map(|e| e.sequence).unwrap_or(after);
            let now = chrono::Utc::now().to_rfc3339();
            let _ = d.store.mark_group_read(&id, up_to, &now);
            Json(events).into_response()
        }
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("group events: {error}"),
        ),
    }
}

/// POST /groups/:id/post {body, mentions?, in_reply_to?, reaction?} — the human
/// posts to the group; turn-taking is scheduled by the store. With `reaction`
/// the body is empty and `in_reply_to` names the post reacted to. Answers the
/// created event.
async fn post_group(
    State(d): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<GroupPostReq>,
) -> Response {
    let body = req.body.trim();
    let now = chrono::Utc::now().to_rfc3339();
    let in_reply_to = req
        .in_reply_to
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let reaction = req
        .reaction
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let posted = if let Some(reaction) = reaction {
        let Some(target) = in_reply_to else {
            return api_error(StatusCode::BAD_REQUEST, "a reaction needs in_reply_to");
        };
        d.store.post_group_reaction(
            &id,
            ("human", "human"),
            target,
            reaction,
            &uuid::Uuid::new_v4().to_string(),
            &now,
        )
    } else {
        if body.is_empty() {
            return api_error(StatusCode::BAD_REQUEST, "body is required");
        }
        let mentions: Vec<String> = req
            .mentions
            .iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect();
        let body = match super::transcribe::prepare_message(&d, body.to_string()).await {
            Ok(text) => text,
            Err(error) => return api_error(StatusCode::BAD_REQUEST, error),
        };
        d.store.post_group_message(
            &id,
            ("human", "human"),
            &body,
            &mentions,
            in_reply_to,
            &uuid::Uuid::new_v4().to_string(),
            &now,
        )
    };
    match posted {
        Ok(saved) => {
            let _ = d.coordination_events.send(saved.clone());
            crate::session::emit_device_event(serde_json::json!({
                "kind": "coordination_event",
                "event": saved.clone(),
            }));
            Json(saved).into_response()
        }
        Err(error) => store_error(StatusCode::CONFLICT, error),
    }
}

/// The `[group_message]` envelope handed to a woken agent. `history` is the
/// prior slice of the thread, rendered by the same digest the DM envelope uses.
/// `members` pairs each agent in the group with the one-line summary of what
/// it is for, so a woken agent can tell who is better placed for what.
fn group_message_envelope(
    event: &crate::coordination::types::CoordinationEvent,
    title: &str,
    reply_expected: bool,
    history: &[crate::coordination::types::CoordinationEvent],
    members: &[(String, String)],
) -> String {
    let body = event
        .payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let mentions: Vec<&str> = event
        .payload
        .get("mentions")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .collect();
    let mut envelope = format!(
        "[group_message]\nthread_id: {}\ntitle: {}\nfrom: {}:{}\n",
        event.thread_id, title, event.actor_kind, event.actor_id
    );
    if !members.is_empty() {
        let line: Vec<String> = members
            .iter()
            .map(|(id, summary)| {
                let summary: String = summary.chars().take(120).collect();
                if summary.is_empty() {
                    id.clone()
                } else {
                    format!("{id} ({summary})")
                }
            })
            .collect();
        envelope.push_str(&format!("members: {}\n", line.join("; ")));
    }
    if !mentions.is_empty() {
        envelope.push_str(&format!("mentions: {}\n", mentions.join(", ")));
    }
    if let Some(post_id) = event.payload.get("in_reply_to").and_then(|v| v.as_str()) {
        envelope.push_str(&format!("in_reply_to: {post_id}\n"));
    }
    let digest = super::direct::history_digest(history);
    envelope.push_str(&format!(
        "reply_expected: {}\nnote: {} Recent history only; use read_chat for more.\n\
         {digest}body: {body}\n[/group_message]",
        if reply_expected { "true" } else { "false" },
        if reply_expected {
            "this post is waiting on the group; answer in the thread with \
             group_thread post, setting in_reply_to to the event you answer. \
             One reply per agent per post. Before doing work from it, say in one line \
             what you are taking; hand the parts another member is better placed for \
             to them by naming them, instead of doing everything yourself."
        } else {
            "join only if you add something: an answer, a relevant view, a correction. \
             If someone has already answered or said they are taking this, do not redo it; \
             take only what is yours and say so in one line first. \
             Otherwise end your turn without writing anything, not even that you are staying quiet."
        }
    ));
    envelope
}

/// The nudge the lead gets when a human's post has had no reply from anyone
/// in the group for [`crate::coordination::GROUP_UNANSWERED_SECONDS`].
fn group_unanswered_envelope(
    event: &crate::coordination::types::CoordinationEvent,
    waiting_minutes: i64,
) -> String {
    let body = event
        .payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    format!(
        "[owed_reply]\nfrom: {}:{}\nthread_id: {}\nevent_id: {}\nwaiting_minutes: {waiting_minutes}\n\
         note: nobody in the group has answered this post and you lead the group. \
         Answer it with group_thread post (in_reply_to this event), or name in one line \
         the member who should and mention them.\nbody: {body}\n[/owed_reply]",
        event.actor_kind, event.actor_id, event.thread_id, event.event_id
    )
}

/// What a deferred wake did: handed over, found moot, or pushed back because
/// the first responder is still mid-turn and may claim it.
enum GroupWake {
    Delivered,
    Skipped,
    Postponed,
}

/// How long after a post the deferred members keep waiting for the first
/// responder to finish; past this the wake goes out regardless.
const GROUP_CLAIM_WAIT_MAX_SECS: i64 = 300;
const GROUP_CLAIM_RECHECK_SECS: i64 = 15;

/// Whether any agent woken first for this post is still running its turn.
async fn first_responder_busy(d: &Shared, event_id: &str) -> bool {
    let first = d
        .store
        .group_immediate_recipients(event_id)
        .unwrap_or_default();
    for agent_id in first {
        let session_id = super::direct::recipient_session_id(&agent_id);
        let busy = d
            .store
            .get_session_row(&session_id)
            .ok()
            .flatten()
            .is_some_and(|row| matches!(row.status.as_str(), "running" | "waiting_for_input"));
        if busy {
            return true;
        }
    }
    false
}

/// Hand one scheduled group wake to its agent's session. A deferred wake
/// whose post already has a reply is skipped — that is the claim window.
async fn deliver_group(
    d: &Shared,
    pending: &crate::coordination::PendingDirectMessage,
) -> Result<GroupWake, String> {
    let event = &pending.event;
    let agent_id = pending.recipient_id.as_str();
    if pending.recipient_kind == "group_unanswered" {
        if d.store.group_has_replies(&event.event_id).unwrap_or(false) {
            return Ok(GroupWake::Skipped);
        }
        let waiting = chrono::DateTime::parse_from_rfc3339(&event.created_at)
            .map(|at| (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_minutes())
            .unwrap_or_default();
        let envelope = group_unanswered_envelope(event, waiting);
        if agent_id != crate::mission_control::SESSION_ID {
            super::direct::ensure_agent_inbox(d, agent_id).await?;
        }
        let session_id = super::direct::recipient_session_id(agent_id);
        let Some((tx, _path, _stream)) = d.ensure_live(&session_id).await else {
            return Err(format!("could not start inbox session `{session_id}`"));
        };
        drop(tx);
        super::direct::deliver_by_urgency(d, &session_id, envelope, &event.payload, false).await;
        return Ok(GroupWake::Delivered);
    }
    if pending.recipient_kind == "group_deferred" {
        if d.store.group_has_replies(&event.event_id).unwrap_or(false) {
            return Ok(GroupWake::Skipped);
        }
        let age = chrono::DateTime::parse_from_rfc3339(&event.created_at)
            .map(|at| (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(i64::MAX);
        if age < GROUP_CLAIM_WAIT_MAX_SECS && first_responder_busy(d, &event.event_id).await {
            let retry_at = (chrono::Utc::now()
                + chrono::Duration::seconds(GROUP_CLAIM_RECHECK_SECS))
            .to_rfc3339();
            let _ = d
                .store
                .postpone_group_delivery(&event.event_id, agent_id, &retry_at);
            return Ok(GroupWake::Postponed);
        }
    }
    let group = d.store.get_group(&event.thread_id).ok().flatten();
    let title = group
        .as_ref()
        .map(|group| group.title.clone())
        .unwrap_or_default();
    let root = crate::coordination::agents_root(&d.mission_control_root);
    let members: Vec<(String, String)> = group
        .as_ref()
        .map(|group| {
            group
                .members
                .iter()
                .filter(|m| m.kind == "agent")
                .map(|m| {
                    let summary = crate::coordination::AgentHome::new(root.clone(), &m.id)
                        .ok()
                        .and_then(|home| home.read_identity().ok())
                        .map(|text| crate::coordination_tools::identity_summary(&text))
                        .unwrap_or_default();
                    (m.id.clone(), summary)
                })
                .collect()
        })
        .unwrap_or_default();
    // The wake carries the same bounded history a DM wake does, minus the post
    // being delivered and the daemon's own acknowledgements.
    let history = d
        .store
        .events_before(
            &event.thread_id,
            event.sequence,
            super::direct::DIRECT_HISTORY_ON_WAKE,
        )
        .unwrap_or_default();
    let envelope = group_message_envelope(
        event,
        &title,
        pending.recipient_kind == "group",
        &history,
        &members,
    );
    if agent_id != crate::mission_control::SESSION_ID {
        super::direct::ensure_agent_inbox(d, agent_id).await?;
    }
    let session_id = super::direct::recipient_session_id(agent_id);
    let Some((tx, _path, _stream)) = d.ensure_live(&session_id).await else {
        return Err(format!("could not start inbox session `{session_id}`"));
    };
    drop(tx);
    super::direct::deliver_by_urgency(
        d,
        &session_id,
        envelope,
        &event.payload,
        event.actor_kind == "human",
    )
    .await;
    Ok(GroupWake::Delivered)
}

/// Deliver every scheduled-but-undelivered group wake. Same shape as the
/// direct-message loop: read the pending set, hand each over, record the
/// outcome durably.
pub async fn group_dispatch_loop(d: Shared) {
    loop {
        let now = chrono::Utc::now().to_rfc3339();
        match d
            .store
            .list_pending_group_deliveries(GROUP_DELIVER_BATCH, &now)
        {
            Ok(pending) => {
                for message in pending {
                    let at = chrono::Utc::now().to_rfc3339();
                    match deliver_group(&d, &message).await {
                        Ok(GroupWake::Postponed) => {}
                        Ok(GroupWake::Delivered | GroupWake::Skipped) => {
                            let _ = d.store.mark_direct_delivered(
                                &message.event.event_id,
                                &message.recipient_kind,
                                &message.recipient_id,
                                &at,
                            );
                        }
                        Err(error) => {
                            eprintln!("[coordination] group wake not delivered: {error}");
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
                eprintln!("[coordination] could not list pending group wakes: {error}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn group_added_notice_names_members_and_says_no_reply() {
        let group = crate::coordination::GroupSummary {
            id: "group:1".into(),
            title: "jh".into(),
            members: vec![
                crate::coordination::GroupMember {
                    kind: "human".into(),
                    id: "human".into(),
                },
                crate::coordination::GroupMember {
                    kind: "agent".into(),
                    id: "x-scout".into(),
                },
                crate::coordination::GroupMember {
                    kind: "agent".into(),
                    id: "snippet".into(),
                },
            ],
            lead: Some("snippet".into()),
            created_at: String::new(),
            last_event_at: None,
            unread: 0,
            last_message: None,
        };
        let e = super::group_added_envelope(&group);
        assert!(e.starts_with("[group_added]\nthread_id: group:1\ntitle: jh\n"));
        assert!(e.contains("members: x-scout, snippet\nlead: snippet\n"));
        assert!(e.contains("do not post"));
        assert!(e.ends_with("[/group_added]"));
    }

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

    fn group() -> (crate::coordination::Store, String) {
        let db = crate::coordination::Store::open_in_memory().unwrap();
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

    /// The spec's scripted three-agent test with a fake deliverer: pending
    /// rows are the wake list, delivery marks them done, and a deferred wake
    /// is skipped once its post has a reply — the same check
    /// `deliver_group` makes.
    #[test]
    fn scripted_three_agent_turn_taking() {
        let (db, id) = group();
        let mut delivered: Vec<(String, String, String)> = Vec::new();
        fn deliver(
            db: &crate::coordination::Store,
            now: &str,
            delivered: &mut Vec<(String, String, String)>,
        ) {
            for pending in db.list_pending_group_deliveries(50, now).unwrap() {
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

        let green = db
            .post_group_message(
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
        deliver(&db, "2026-01-01T00:00:15+00:00", &mut delivered);
        assert!(
            !delivered.iter().any(|(e, _, _)| e == &green.event_id),
            "an agent's unmentioned reply wakes nobody: {delivered:?}"
        );

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
            "the lead — b, last addressed — takes the unmentioned post: {delivered:?}"
        );

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
        assert!(
            !delivered
                .iter()
                .any(|(e, k, _)| e == &p2.event_id && k == "group_deferred"),
            "deferred wakes for the follow-up skipped once b replied: {delivered:?}"
        );
    }

    #[test]
    fn envelope_carries_the_turn_fields() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e1".into(),
            thread_id: "group:x".into(),
            partition_key: "thread:group:x".into(),
            sequence: 1,
            event_type: "group_message".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({
                "body": "hi",
                "mentions": ["b"],
                "from_kind": "human",
                "from_id": "human",
                "in_reply_to": "p0",
            }),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: T0.into(),
        };
        let members = vec![
            ("a".to_string(), "Reviews Rust PRs.".to_string()),
            ("b".to_string(), String::new()),
        ];
        let expected = group_message_envelope(&event, "build", true, &[], &members);
        for field in [
            "thread_id: group:x",
            "title: build",
            "members: a (Reviews Rust PRs.); b",
            "from: human:human",
            "mentions: b",
            "in_reply_to: p0",
            "reply_expected: true",
            "say in one line what you are taking",
            "body: hi",
        ] {
            assert!(expected.contains(field), "missing {field} in {expected}");
        }
        let deferred = group_message_envelope(&event, "build", false, &[], &[]);
        assert!(!deferred.contains("members:"));
        assert!(deferred.contains("do not redo it"));
        assert!(deferred.contains("reply_expected: false"));
        assert!(
            deferred.contains(
                "join only if you add something: an answer, a relevant view, a correction."
            )
        );
    }

    /// The wake carries the thread's recent history like a DM wake does, with
    /// `body` still last so nothing runs into the sender's text.
    #[test]
    fn envelope_renders_the_history_digest_and_keeps_body_last() {
        let event = crate::coordination::types::CoordinationEvent {
            event_id: "e3".into(),
            thread_id: "group:x".into(),
            partition_key: "thread:group:x".into(),
            sequence: 3,
            event_type: "group_message".into(),
            actor_kind: "human".into(),
            actor_id: "human".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "wake body"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: T2.into(),
        };
        let prior = |sequence: u64, actor_kind: &str, actor_id: &str, body: &str, auto: bool| {
            let mut payload = serde_json::json!({"body": body});
            if auto {
                payload["auto"] = serde_json::json!(true);
            }
            crate::coordination::types::CoordinationEvent {
                event_id: format!("e{sequence}"),
                thread_id: "group:x".into(),
                partition_key: "thread:group:x".into(),
                sequence,
                event_type: "group_message".into(),
                actor_kind: actor_kind.into(),
                actor_id: actor_id.into(),
                payload_version: 1,
                payload,
                causation_id: None,
                correlation_id: None,
                idempotency_key: format!("k{sequence}"),
                created_at: T0.into(),
            }
        };
        let history = vec![
            prior(1, "agent", "a", "first post", false),
            prior(2, "human", "human", "second post", false),
            prior(3, "agent", "a", "auto ack", true),
        ];
        // The digest hides the `auto` event, so two messages render.
        let env = group_message_envelope(&event, "build", true, &history, &[]);
        assert!(env.contains("history: last 2 message(s), oldest first"));
        assert!(env.contains("  1 [agent] a: first post"));
        assert!(env.contains("  2 [human] human: second post"));
        assert!(!env.contains("auto ack"));
        assert!(env.contains("Recent history only; use read_chat for more."));
        let digest_at = env.find("history:").expect("digest present");
        let body_at = env.find("body: wake body").expect("body present");
        assert!(digest_at < body_at, "body must stay last: {env}");
        assert!(env.ends_with("[/group_message]"));

        let env = group_message_envelope(&event, "build", true, &[], &[]);
        assert!(env.contains("history: (first message in this conversation)"));
        assert!(env.find("history:").unwrap() < env.find("body: wake body").unwrap());
    }

    #[test]
    fn events_before_returns_prior_non_auto_events_even_after_many_newer_posts() {
        let (db, id) = group();
        let p1 = db
            .post_group_message(&id, ("human", "human"), "first", &[], None, "k1", T0)
            .unwrap();
        let p2 = db
            .post_group_message(&id, ("agent", "a"), "second", &[], None, "k2", T1)
            .unwrap();
        db.append_event(&crate::coordination::types::CoordinationEvent {
            event_id: "auto-1".into(),
            thread_id: id.clone(),
            partition_key: format!("thread:{id}"),
            sequence: 0,
            event_type: "group_message".into(),
            actor_kind: "agent".into(),
            actor_id: "a".into(),
            payload_version: 1,
            payload: serde_json::json!({"body": "ack", "auto": true}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k-auto".into(),
            created_at: T1.into(),
        })
        .unwrap();
        let target = db
            .post_group_message(
                &id,
                ("human", "human"),
                "wake post",
                &["b".into()],
                None,
                "k3",
                T2,
            )
            .unwrap();
        for i in 0..15 {
            db.post_group_message(
                &id,
                ("agent", "c"),
                &format!("later-{i}"),
                &[],
                None,
                &format!("kl-{i}"),
                T2,
            )
            .unwrap();
        }
        let history = db
            .events_before(
                &id,
                target.sequence,
                super::super::direct::DIRECT_HISTORY_ON_WAKE,
            )
            .unwrap();
        let seqs: Vec<u64> = history.iter().map(|e| e.sequence).collect();
        assert_eq!(seqs, vec![p1.sequence, p2.sequence]);
    }
}
