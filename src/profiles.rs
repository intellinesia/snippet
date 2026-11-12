//! `list_profiles`: the configured inference profiles. Read on demand so the
//! tool definition stays static and cache-friendly.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub fn profile_line(name: &str, provider: &str, model: &str, limited_until: Option<i64>) -> String {
    let limited = limited_until
        .map(|at| format!(" | rate limited until {}", crate::routing::reset_label(at)))
        .unwrap_or_default();
    format!("{name} | {provider} | {model}{limited}")
}

pub struct ListProfiles;
#[async_trait]
impl Tool for ListProfiles {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "list_profiles".into(),
            description: "The configured inference profiles, one line each: exact name, provider, model, and a rate limit if one is active. A `profile` argument must be one of these names.".into(),
            input_schema: json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        }
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        _arguments: Value,
    ) -> Result<ToolResult, ToolError> {
        let config = crate::config::SnippetConfig::load(crate::config::default_config_path())
            .await
            .map_err(|e| ToolError::msg(format!("read config: {e}")))?;
        let lines: Vec<String> = config
            .setups
            .iter()
            .flatten()
            .map(|(name, cfg)| {
                let provider = serde_json::to_value(&cfg.provider)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                let mut line = profile_line(
                    name,
                    &provider,
                    &cfg.model,
                    crate::routing::limited_until(name),
                );
                if config.active_setup.as_deref() == Some(name.as_str()) {
                    line.push_str(" | default");
                }
                line
            })
            .collect();
        Ok(ToolResult::success(json!({"profiles": lines})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_compact_line() {
        assert_eq!(
            profile_line("fast", "anthropic", "haiku", None),
            "fast | anthropic | haiku"
        );
    }
}
