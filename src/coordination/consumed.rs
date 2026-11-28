//! Marks a direct message read at the moment a harness consumes its envelope,
//! and tells attached devices so the app can move the bubble to "read".

use serde_json::json;

/// The `<direct_message>` envelope text when `text` is one, else None.
///
/// A `note:` line may sit ahead of the tag by older steer/stop prefixes; the
/// tag itself stays intact.
fn envelope_body(text: &str) -> Option<&str> {
    let mut body = text.trim_start();
    if let Some(rest) = body.strip_prefix("note:") {
        body = rest
            .split_once('\n')
            .map(|(_, after)| after.trim_start())
            .unwrap_or("");
    }
    crate::envelope::is(body, "direct_message").then_some(body)
}

/// Extract `[msg:<event_id>]` references from a `<desk_events>` batch (which may
/// be preceded by a `now:` status block or `note:` line).
fn desk_event_msg_ids(text: &str) -> Vec<String> {
    let Some(start) = text.find("<desk_events") else {
        return Vec::new();
    };
    let slice = &text[start..];
    let mut ids = Vec::new();
    for line in slice.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("<event ") {
            continue;
        }
        let mut rest = trimmed;
        while let Some(pos) = rest.find("[msg:") {
            let after = &rest[pos + 5..];
            if let Some(end) = after.find(']') {
                let id = after[..end].trim();
                if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                    ids.push(id.to_string());
                }
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
    }
    ids
}

/// Record that the agent's turn consumed the direct message(s) `text` carries.
///
/// Handles both `<direct_message event_id="...">` envelopes (Lead/session DMs)
/// and `<desk_events>` batches (desk-inbox DMs carrying `[msg:<event_id>]`).
/// Returns whether at least one message was marked read.
pub fn direct_consumed(text: &str) -> bool {
    let mut event_ids: Vec<String> = Vec::new();
    if let Some(body) = envelope_body(text)
        && let Some(id) = crate::envelope::attr(body, "event_id").filter(|id| !id.is_empty())
    {
        event_ids.push(id);
    } else if text.contains("<desk_events") {
        event_ids = desk_event_msg_ids(text);
    }
    if event_ids.is_empty() {
        return false;
    }
    let Some(store) = crate::session::store_for_sessions() else {
        return false;
    };
    let now = chrono::Utc::now().to_rfc3339();
    for event_id in event_ids {
        let _ = store.mark_direct_read_event(&event_id, &now);
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": {"event_type": "direct_message.read", "event_id": event_id},
        }));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    fn agent(id: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    fn store() -> crate::store::Store {
        crate::session::store_for_sessions().expect("session store")
    }

    fn send(agent_id: &str, key: &str) -> crate::coordination::types::CoordinationEvent {
        store()
            .send_direct_message_typed(
                ("human", "local"),
                ("agent", agent_id),
                "hello",
                key,
                "2026-01-01T00:00:00Z",
                None,
                None,
                None,
                Default::default(),
            )
            .unwrap()
    }

    fn envelope(event_id: &str) -> String {
        crate::envelope::Envelope::new("direct_message")
            .attr("event_id", event_id)
            .attr("thread_id", "direct:agent:x|human:local")
            .attr("from", "human:local")
            .attr("reply_to", "human")
            .child("body", "hi")
            .render()
    }

    #[tokio::test]
    async fn consumed_marks_the_event_read_and_pushes_the_frame() {
        let agent_id = "consumed-read-agent";
        store().create_agent(&agent(agent_id)).unwrap();
        let event = send(agent_id, "k-consumed-read");
        let mut rx = crate::session::subscribe_device_events();

        assert!(direct_consumed(&envelope(&event.event_id)));

        let found = store()
            .direct_deliveries(&[event.event_id.clone()])
            .unwrap();
        assert!(found[&event.event_id].1.is_some(), "read_at set");
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Ok(frame) = rx.recv().await {
                if frame["event"]["event_type"] == "direct_message.read"
                    && frame["event"]["event_id"] == event.event_id
                {
                    return frame;
                }
            }
            panic!("device event channel closed");
        })
        .await
        .expect("direct_message.read frame pushed");
        assert_eq!(frame["kind"], "coordination_event");
    }

    #[test]
    fn consumed_parses_plain_and_task_routed_forms_and_ignores_other_text() {
        let agent_id = "consumed-forms-agent";
        store().create_agent(&agent(agent_id)).unwrap();
        let plain = send(agent_id, "k-consumed-plain");
        let routed = send(agent_id, "k-consumed-routed");

        assert!(direct_consumed(&envelope(&plain.event_id)));
        // The routed-to-task form can carry a `note:` line ahead of the tag.
        let routed_text = format!(
            "note: about task t1 (Task t1); routed to the session doing it.\n{}",
            envelope(&routed.event_id)
        );
        assert!(direct_consumed(&routed_text));
        let found = store()
            .direct_deliveries(&[plain.event_id.clone(), routed.event_id.clone()])
            .unwrap();
        assert!(found[&plain.event_id].1.is_some());
        assert!(found[&routed.event_id].1.is_some());

        assert!(!direct_consumed("just a message"));
        assert!(!direct_consumed("<agent_reply from=\"agent:a\"/>"));
        assert!(!direct_consumed(
            &envelope("").replace(" event_id=\"\"", "")
        ));
    }
}
