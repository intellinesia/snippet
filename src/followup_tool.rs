//! `schedule_wake` for desks and work sessions: wake this same session once,
//! later, with a note. Mission Control keeps its own `schedule_wake`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct SessionWakeTool;

pub const MAX_MINUTES: i64 = 7 * 24 * 60;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    in_minutes: Option<i64>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    cancel: Option<String>,
}

fn due_at(minutes: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[async_trait]
impl Tool for SessionWakeTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "schedule_wake".into(),
            description: "Wake this session once, in_minutes from now, with your note as a [wake] envelope: to check back on something nobody will tell you about. Omit in_minutes to list pending wakes; cancel takes an id.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "in_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_MINUTES},
                    "note": {"type": "string"},
                    "cancel": {"type": "string"}
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: Args =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let session = ctx
            .durable_session_id()
            .ok_or_else(|| ToolError::msg("this session cannot be woken later"))?
            .to_string();
        let store = ctx
            .store()
            .map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        let err = |e: crate::store::StoreError| ToolError::msg(e.to_string());
        if let Some(id) = args.cancel.as_deref() {
            let removed = store.cancel_followup(&session, id).map_err(err)?;
            return Ok(ToolResult::success(json!({ "cancelled": removed })));
        }
        let Some(minutes) = args.in_minutes else {
            let pending = store.pending_followups(&session).map_err(err)?;
            let rows: Vec<Value> = pending
                .iter()
                .map(|f| json!({ "id": f.id, "due_at": f.due_at, "note": f.note }))
                .collect();
            return Ok(ToolResult::success(json!({ "pending": rows })));
        };
        if !(1..=MAX_MINUTES).contains(&minutes) {
            return Err(ToolError::msg(format!(
                "in_minutes must be 1 to {MAX_MINUTES}"
            )));
        }
        let note = args.note.unwrap_or_default();
        let now = crate::coordination::watches::now_rfc3339();
        let owner = ctx
            .agent_id()
            .filter(|_| crate::session::is_inbox_session_id(&session));
        let f = store
            .schedule_followup(&session, owner, &due_at(minutes), &note, &now)
            .map_err(err)?;
        Ok(ToolResult::success(
            json!({ "id": f.id, "due_at": f.due_at, "session": f.session_id }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_definition_stays_small() {
        let d = SessionWakeTool.definition();
        let bytes = d.description.len() + d.input_schema.to_string().len();
        assert!(bytes < 600, "{bytes}");
    }

    #[test]
    fn due_at_is_in_the_future_and_sortable() {
        let now = crate::coordination::watches::now_rfc3339();
        let later = due_at(5);
        assert!(later > now);
        assert!(later.ends_with('Z'));
    }
}
