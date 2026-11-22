//! `hand_off_session` for standard sessions: pass ownership of this same
//! session to another active agent at the end of the current turn.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct HandOffSession;

#[derive(Deserialize)]
struct Args {
    to_agent: String,
    #[serde(default)]
    note: String,
}

#[async_trait]
impl Tool for HandOffSession {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "hand_off_session".into(),
            description: "Pass this standard session to another active agent when the work enters their field. Applies at turn end.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to_agent": { "type": "string" },
                    "note": { "type": "string" }
                },
                "required": ["to_agent", "note"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        if ctx.is_lead() || ctx.lane_origin().is_some() {
            return Err(ToolError::msg(
                "task.hand_off is only available in standard sessions",
            ));
        }
        let session = ctx
            .durable_session_id()
            .ok_or_else(|| ToolError::msg("task.hand_off requires a durable session"))?;
        let args: Args =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let store = ctx
            .store()
            .map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        let outcome = crate::serve::handoff::queue_pending_handoff_from_tool(
            &store,
            session,
            ctx.agent_id(),
            &args.to_agent,
            &args.note,
        )
        .map_err(|e| ToolError::msg(e.message()))?;
        Ok(ToolResult::success(json!({
            "session": outcome.session,
            "from": outcome.from,
            "to": outcome.to,
            "pending": true,
            "message": format!(
                "Session hand-off to `{}` queued; it takes effect when this turn ends. Finish with a short wrap-up.",
                outcome.to
            ),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::SessionRow;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};
    use crate::harness::HarnessEvent;
    use crate::store::Store;

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

    fn seed_session(store: &Store, id: &str, role: &str, kind: &str, agent_id: Option<&str>) {
        store
            .create_conversation(
                &SessionRow {
                    id: id.into(),
                    workspace_key: "wk".into(),
                    workspace: "/tmp/ws".into(),
                    title: Some("t".into()),
                    status: "running".into(),
                    created_at: "2026-01-01T00:00:00Z".into(),
                    updated_at: "2026-01-01T00:00:00Z".into(),
                    last_active: Some(1),
                    profile: None,
                    role: role.into(),
                    agent_id: agent_id.map(str::to_string),
                    kind: kind.into(),
                    settled_at: None,
                    keep_awake: false,
                    resume_mode: "ask".into(),
                    last_changes: None,
                    total_changes: None,
                    user_seen_at: None,
                    finished_at: None,
                    result_summary: None,
                    pending_handoff: None,
                    managed_by: "human".into(),
                },
                &[],
                "[]",
            )
            .unwrap();
        store.set_session_role(id, role, agent_id).unwrap();
    }

    #[test]
    fn tool_is_registered_only_in_work_sessions() {
        let work = crate::session::work_tool_registry(None, None);
        let mc = crate::session::lead_tool_registry(None);
        let desk = crate::session::coordination_tool_registry(None);
        assert!(work.contains("hand_off_session"));
        assert!(!mc.contains("hand_off_session"));
        assert!(!desk.contains("hand_off_session"));
    }

    #[tokio::test]
    async fn tool_queues_pending_handoff_and_refuses_non_standard_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.db");
        let store = Store::open(&db_path).unwrap();
        store.create_agent(&worker("snippet")).unwrap();
        store.create_agent(&worker("rook")).unwrap();
        seed_session(&store, "std-1", "standard", "conversation", None);
        seed_session(&store, "inbox-rook", "coordination", "inbox", Some("rook"));

        let ctx = ToolContext::new(dir.path())
            .unwrap()
            .with_durable_session_id("std-1")
            .with_store_path(&db_path);
        let res = HandOffSession
            .execute(&ctx, json!({ "to_agent": "rook", "note": "check TLS" }))
            .await
            .unwrap();
        assert_eq!(res.value["data"]["pending"], true);
        assert_eq!(res.value["data"]["from"], "snippet");
        assert_eq!(res.value["data"]["to"], "rook");
        assert!(
            res.value["data"]["message"]
                .as_str()
                .unwrap()
                .contains("wrap-up")
        );

        let row = store.get_session_row("std-1").unwrap().unwrap();
        assert!(row.pending_handoff.is_some());
        let events = store.load_conversation_events("std-1").unwrap();
        assert!(matches!(
            events.last(),
            Some(HarnessEvent::SessionHandoff {
                from,
                to,
                by,
                pending: true,
                ..
            }) if from == "snippet" && to == "rook" && by == "snippet"
        ));

        // Refuses Lead, desk, and lane contexts.
        let mc_ctx = ToolContext::lead(dir.path())
            .unwrap()
            .with_durable_session_id(crate::lead::SESSION_ID)
            .with_store_path(&db_path);
        assert!(
            HandOffSession
                .execute(&mc_ctx, json!({ "to_agent": "rook", "note": "x" }))
                .await
                .is_err()
        );

        let desk_ctx = ToolContext::new(dir.path())
            .unwrap()
            .with_durable_session_id("inbox-rook")
            .with_store_path(&db_path)
            .with_agent_id("rook");
        assert!(
            HandOffSession
                .execute(&desk_ctx, json!({ "to_agent": "snippet", "note": "x" }))
                .await
                .is_err()
        );

        let lane_ctx = ToolContext::new(dir.path())
            .unwrap()
            .with_durable_session_id("std-1")
            .with_store_path(&db_path)
            .with_lane_origin("std-1", "lane-1");
        assert!(
            HandOffSession
                .execute(&lane_ctx, json!({ "to_agent": "rook", "note": "x" }))
                .await
                .is_err()
        );
    }
}
