//! Marks a direct message read at the moment a harness consumes its envelope,
//! and tells attached devices so the app can move the bubble to "read".

use serde_json::json;

/// The envelope body when `text` is a `[direct_message]` envelope, else None.
///
/// A `note:` line may sit ahead of the tag — the steer/stop prefixes and the
/// routed-to-task form (`note: about task ...`) all keep the tag itself intact.
fn envelope_body(text: &str) -> Option<&str> {
    let mut body = text.trim_start();
    if let Some(rest) = body.strip_prefix("note:") {
        body = rest.split_once('\n').map(|(_, after)| after.trim_start()).unwrap_or("");
    }
    body.strip_prefix("[direct_message]")
}

/// Record that the agent's turn consumed the direct message `text` carries.
///
/// Cheap no-op — the prefix check runs before any store or socket work — for
/// anything that is not a `[direct_message]` envelope with an `event_id:` line.
/// Returns whether the message was marked read.
pub fn direct_consumed(text: &str) -> bool {
    let Some(body) = envelope_body(text) else {
        return false;
    };
    let Some(event_id) = body
        .lines()
        .find_map(|line| line.strip_prefix("event_id:").map(str::trim))
        .filter(|id| !id.is_empty())
        .map(str::to_string)
    else {
        return false;
    };
    let Some(store) = crate::session::store_for_sessions() else {
        return false;
    };
    let _ = store.mark_direct_read_event(&event_id, &chrono::Utc::now().to_rfc3339());
    crate::session::emit_device_event(json!({
        "kind": "coordination_event",
        "event": {"event_type": "direct_message.read", "event_id": event_id},
    }));
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
        format!(
            "[direct_message]\nevent_id: {event_id}\nthread_id: direct:agent:x|human:local\nfrom: human:local\nreply_to: human\nbody: hi\n[/direct_message]"
        )
    }

    #[tokio::test]
    async fn consumed_marks_the_event_read_and_pushes_the_frame() {
        let agent_id = "consumed-read-agent";
        store().create_agent(&agent(agent_id)).unwrap();
        let event = send(agent_id, "k-consumed-read");
        let mut rx = crate::session::subscribe_device_events();

        assert!(direct_consumed(&envelope(&event.event_id)));

        let found = store().direct_deliveries(&[event.event_id.clone()]).unwrap();
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
        assert!(!direct_consumed("[agent_reply]\nfrom: agent:a\n[/agent_reply]"));
        assert!(!direct_consumed(&envelope("").replace("event_id: \n", "")));
    }
}
