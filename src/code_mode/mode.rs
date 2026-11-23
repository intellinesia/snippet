use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::ToolMode;
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

/// The agent's chosen tool mode; direct until it picks code.
pub fn mode_for(agent_id: Option<&str>) -> ToolMode {
    let agent = agent_id.unwrap_or(crate::coordination::SNIPPET_AGENT_ID);
    let stored = crate::store::Store::open_cached(crate::store::default_db_path())
        .ok()
        .and_then(|store| store.agent_tool_mode(agent).ok().flatten());
    match stored.as_deref() {
        Some("code") => ToolMode::Code,
        _ => ToolMode::Direct,
    }
}

fn name(mode: ToolMode) -> &'static str {
    match mode {
        ToolMode::Direct => "direct",
        ToolMode::Code => "code",
    }
}

pub struct ToolModeTool;

#[async_trait]
impl Tool for ToolModeTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "tool_mode".into(),
            description: "Show or choose how you call tools. direct: every tool is its own call, and you see each result. code: a `code` tool runs scripts that chain your tools, so several steps take one round trip and only what the script returns comes back. Pick the one you make fewer mistakes with. Applies from your next session.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["direct", "code"], "description": "Omit to show the current choice."}
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let agent = if ctx.is_lead() {
            crate::lead::SESSION_ID.to_string()
        } else {
            ctx.agent_id()
                .unwrap_or(crate::coordination::SNIPPET_AGENT_ID)
                .to_string()
        };
        let requested = arguments.get("mode").and_then(Value::as_str);
        if let Some(mode) = requested {
            if !matches!(mode, "direct" | "code") {
                return Err(ToolError::msg("`mode` must be `direct` or `code`"));
            }
            let store = crate::store::Store::open_cached(crate::store::default_db_path())
                .map_err(|e| ToolError::msg(format!("open store: {e}")))?;
            if !store
                .set_agent_tool_mode(&agent, mode)
                .map_err(|e| ToolError::msg(e.to_string()))?
            {
                return Err(ToolError::msg(format!("no agent `{agent}`")));
            }
        }
        let chosen = name(mode_for(Some(&agent)));
        let mut data = json!({"agent_id": agent, "mode": chosen});
        if requested.is_some() {
            data["note"] =
                json!("Applies from your next session; this one keeps its current tools.");
        }
        Ok(ToolResult::success(data))
    }
}
