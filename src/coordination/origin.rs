//! A managed session's questions, approvals, files and results reach its desk,
//! which decides what the user sees. Nothing is posted to the user implicitly.

use super::Store;
use super::types::CoordinationEvent;

/// The desk an agent-managed session reports to.
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
