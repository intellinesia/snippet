use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{expect_object, object_schema};
use crate::history_archive::{recall_turn_range, render_turn, search_history};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct HistoryTool;

#[derive(Deserialize)]
struct HistoryArgs {
    query: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
}

#[async_trait]
impl Tool for HistoryTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "history".to_string(),
            description: "This conversation's archived history (from compaction or a lean continuation). `query` searches it; `from` and `to` read messages by id in full.".to_string(),
            input_schema: object_schema(
                json!({
                    "query": {"type": "string"},
                    "from": {"type": "integer"},
                    "to": {"type": "integer"}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: HistoryArgs = expect_object("history", arguments)?;
        let session = ctx
            .durable_session_id()
            .ok_or_else(|| ToolError::msg("this session has no stored history"))?
            .to_string();
        let store = ctx.store().map_err(|e| ToolError::msg(e.to_string()))?;
        let text = match (args.query.as_deref().map(str::trim).filter(|q| !q.is_empty()), args.from, args.to) {
            (_, Some(from), Some(to)) => {
                let turns = recall_turn_range(&store, &session, from.min(to), from.max(to), 40)
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                if turns.is_empty() {
                    "No archived messages in that range.".to_string()
                } else {
                    turns.iter().map(render_turn).collect::<Vec<_>>().join("\n\n")
                }
            }
            (Some(query), _, _) => {
                let hits = search_history(&store, &session, query, 20).map_err(|e| ToolError::msg(e.to_string()))?;
                if hits.is_empty() {
                    format!("No archived messages match {query:?}.")
                } else {
                    hits.iter()
                        .map(|h| {
                            let who = h.tool_name.as_deref().unwrap_or(&h.role);
                            let snippet = h.snippet.split_whitespace().collect::<Vec<_>>().join(" ");
                            format!("#{} {who}: {} | {snippet}", h.archive_id, h.summary)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            _ => return Err(ToolError::msg("pass `query`, or both `from` and `to`")),
        };
        Ok(ToolResult::success(json!({"history": text})))
    }
}
