use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::ToolMode;
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

fn store() -> Option<crate::store::Store> {
    crate::store::Store::open_cached(crate::store::default_db_path()).ok()
}

fn parse(mode: &str) -> Option<ToolMode> {
    match mode {
        "direct" => Some(ToolMode::Direct),
        "code" => Some(ToolMode::Code),
        _ => None,
    }
}

pub fn label(mode: ToolMode) -> &'static str {
    match mode {
        ToolMode::Direct => "direct",
        ToolMode::Code => "code",
    }
}

/// The mode in force and where it came from: the session's switch, else the
/// agent's choice, else the universal setting, else direct.
pub fn resolve(session: Option<&str>, agent: Option<&str>) -> (ToolMode, &'static str) {
    let agent = agent.unwrap_or(crate::coordination::SNIPPET_AGENT_ID);
    let rows = store()
        .and_then(|s| s.tool_modes_for(session, Some(agent)).ok())
        .unwrap_or_default();
    for scope in ["session", "agent", "all"] {
        if let Some(mode) = rows
            .iter()
            .find(|(s, _)| s == scope)
            .and_then(|(_, m)| parse(m))
        {
            return (mode, scope);
        }
    }
    (ToolMode::Direct, "default")
}

/// The universal setting, if one is stored.
pub fn universal() -> Option<&'static str> {
    let rows = store()?.tool_modes_for(None, None).ok()?;
    rows.iter()
        .find(|(s, _)| s == "all")
        .and_then(|(_, m)| parse(m))
        .map(label)
}

/// Set or clear one scope. `mode` of None or `inherit` clears it.
pub fn set(scope: &str, id: &str, mode: Option<&str>) -> Result<(), String> {
    let mode = mode.filter(|m| *m != "inherit");
    if let Some(m) = mode
        && parse(m).is_none()
    {
        return Err("mode must be `direct`, `code` or `inherit`".into());
    }
    store()
        .ok_or("store unavailable")?
        .set_tool_mode(scope, id, mode)
        .map_err(|e| e.to_string())
}

pub struct ToolModeTool;

#[async_trait]
impl Tool for ToolModeTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "tool_mode".into(),
            description: "Show or choose how you call tools. direct: every tool is its own call, and you see each result. code: a `code` tool runs scripts that chain your tools, so several steps take one round trip and only what the script returns comes back. Pick the one you make fewer mistakes with. Applies from your next turn.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "enum": ["direct", "code", "inherit"], "description": "Omit to show; inherit clears."},
                    "scope": {"type": "string", "enum": ["session", "agent"], "description": "This session, or all yours (default)."}
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
        let session = ctx.durable_session_id().map(str::to_string);
        let requested = arguments.get("mode").and_then(Value::as_str);
        if let Some(mode) = requested {
            let scope = arguments
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("agent");
            let id = match scope {
                "agent" => agent.clone(),
                "session" => session
                    .clone()
                    .ok_or_else(|| ToolError::msg("this run has no session id"))?,
                other => return Err(ToolError::msg(format!("unknown scope `{other}`"))),
            };
            set(scope, &id, Some(mode)).map_err(ToolError::msg)?;
        }
        let (mode, from) = resolve(session.as_deref(), Some(&agent));
        let mut data = json!({"mode": label(mode), "set_by": from});
        if requested.is_some() {
            data["note"] = json!("Applies from your next turn.");
        }
        Ok(ToolResult::success(data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_beats_agent_beats_universal() {
        let session = format!("s-{}", uuid::Uuid::new_v4().simple());
        let agent = format!("a-{}", uuid::Uuid::new_v4().simple());
        let before = resolve(Some(&session), Some(&agent));
        set("agent", &agent, Some("code")).unwrap();
        assert_eq!(
            resolve(Some(&session), Some(&agent)),
            (ToolMode::Code, "agent")
        );
        set("session", &session, Some("direct")).unwrap();
        assert_eq!(
            resolve(Some(&session), Some(&agent)),
            (ToolMode::Direct, "session")
        );
        set("session", &session, Some("inherit")).unwrap();
        set("agent", &agent, None).unwrap();
        assert_eq!(resolve(Some(&session), Some(&agent)), before);
        assert!(set("agent", &agent, Some("hybrid")).is_err());
    }
}
