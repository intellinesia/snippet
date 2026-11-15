//! Agent tools for the theme store: list, read, schema, save and delete.
//!
//! There is deliberately no apply tool — only the user applies a theme, from
//! the app. Descriptions say so, and a successful save reminds the agent to
//! tell the user in chat that the theme is ready to try.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::themes;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

const APPLY_NOTE: &str = "Themes are applied by the user in the app; you cannot apply one. When a theme is saved, tell the user in chat that it is ready to try.";

/// The `agent:<id>` author string for this session's theme writes.
fn agent_author(ctx: &ToolContext) -> String {
    format!("agent:{}", ctx.agent_id().unwrap_or_else(|| ctx.owner()))
}

fn open_store(ctx: &ToolContext) -> Result<crate::store::Store, ToolError> {
    ctx.store()
        .map_err(|e| ToolError::msg(format!("open store: {e}")))
}

pub struct ThemeListTool;
#[async_trait]
impl Tool for ThemeListTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "theme_list".into(),
            description: format!(
                "List the user's themes: id, name, author, whether builtin, and which one is active. {APPLY_NOTE}"
            ),
            input_schema: schema(json!({}), &[]),
        }
    }
    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let store = open_store(ctx)?;
        let mut rows = store
            .list_themes()
            .map_err(|e| ToolError::msg(format!("list themes: {e}")))?;
        // Builtins not yet seeded still list: synthesize their rows.
        let have: std::collections::HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        for doc in themes::builtin_docs() {
            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !id.is_empty()
                && !have.contains(&id)
                && let Ok(Some(row)) = themes::get_theme(&store, &id)
            {
                rows.push(row);
            }
        }
        let active = themes::active_theme_id(&store)
            .map_err(|e| ToolError::msg(format!("active theme: {e}")))?;
        let list: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "id": row.id,
                    "name": row.name,
                    "author": row.author,
                    "builtin": row.builtin,
                    "active": row.id == active,
                })
            })
            .collect();
        Ok(ToolResult::success(json!({
            "active": active,
            "live_groups": themes::LIVE_GROUPS,
            "themes": list,
        })))
    }
}

#[derive(Deserialize)]
struct IdArgs {
    id: String,
}

pub struct ThemeGetTool;
#[async_trait]
impl Tool for ThemeGetTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "theme_get".into(),
            description: format!(
                "Get a theme's resolved document — every token filled from its base — so you can copy one and change a few tokens. {APPLY_NOTE}"
            ),
            input_schema: schema(
                json!({"id":{"type":"string","description":"theme id"}}),
                &["id"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: IdArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::msg(format!("theme_get arguments: {e}")))?;
        let store = open_store(ctx)?;
        let row = themes::get_theme(&store, args.id.trim())
            .map_err(|e| ToolError::msg(format!("get theme: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("no such theme `{}`", args.id)))?;
        let resolved = themes::resolve(&store, &row.doc)
            .map_err(|e| ToolError::msg(format!("resolve theme: {e}")))?;
        Ok(ToolResult::success(resolved))
    }
}

pub struct ThemeSchemaTool;
#[async_trait]
impl Tool for ThemeSchemaTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "theme_schema".into(),
            description: format!(
                "The theme token contract: every group's token names, types and ranges, the resolved-theme checks, and which groups are live on the client today. Read this before authoring a theme. {APPLY_NOTE}"
            ),
            input_schema: schema(json!({}), &[]),
        }
    }
    async fn execute(&self, _: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(themes::contract()))
    }
}

#[derive(Deserialize)]
struct SaveArgs {
    document: Value,
}

pub struct ThemeSaveTool;
#[async_trait]
impl Tool for ThemeSaveTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "theme_save".into(),
            description: format!(
                "Create or update a theme you authored. Takes the whole schema-1 document (see theme_schema); it may set any group in the contract and inherits the rest from `base`. Cannot overwrite builtins or other authors' themes. Returns validation problems to fix when the document is invalid. {APPLY_NOTE}"
            ),
            input_schema: schema(
                json!({"document":{"type":"object","description":"the full theme document"}}),
                &["document"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SaveArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::msg(format!("theme_save arguments: {e}")))?;
        let mut doc = args.document;
        let Some(obj) = doc.as_object() else {
            return Err(ToolError::msg("document must be a JSON object"));
        };
        let Some(id) = obj.get("id").and_then(Value::as_str).map(str::to_string) else {
            return Err(ToolError::msg("document.id is required"));
        };
        let store = open_store(ctx)?;
        let author = agent_author(ctx);
        if let Some(row) =
            themes::get_theme(&store, &id).map_err(|e| ToolError::msg(format!("get theme: {e}")))?
        {
            if row.builtin {
                return Err(ToolError::msg(format!("`{id}` is a builtin theme")));
            }
            if row.author != author {
                return Err(ToolError::msg(format!(
                    "`{id}` belongs to {} — you can only change your own themes",
                    row.author
                )));
            }
        }
        if let Some(o) = doc.as_object_mut() {
            o.insert("author".into(), json!(author));
        }
        let problems = themes::validate(&store, &doc, false);
        if !problems.is_empty() {
            return Ok(ToolResult::error(format!(
                "theme invalid:\n{}",
                problems.join("\n")
            )));
        }
        let name = doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id.as_str())
            .to_string();
        let now = chrono::Utc::now().timestamp();
        store
            .upsert_theme(&id, &name, &author, false, &doc, now)
            .map_err(|e| ToolError::msg(format!("save theme: {e}")))?;
        let active = themes::active_theme_id(&store).unwrap_or_else(|_| "black".into());
        themes::emit_changed(&active, &id);
        Ok(ToolResult::success(json!({
            "saved": id,
            "note": "Tell the user in chat that the theme is ready to try; they apply it in the app.",
        })))
    }
}

pub struct ThemeDeleteTool;
#[async_trait]
impl Tool for ThemeDeleteTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "theme_delete".into(),
            description: format!(
                "Delete a theme you authored. Builtins and other authors' themes cannot be deleted. {APPLY_NOTE}"
            ),
            input_schema: schema(
                json!({"id":{"type":"string","description":"theme id"}}),
                &["id"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: IdArgs = serde_json::from_value(arguments)
            .map_err(|e| ToolError::msg(format!("theme_delete arguments: {e}")))?;
        let store = open_store(ctx)?;
        let author = agent_author(ctx);
        let row = themes::get_theme(&store, args.id.trim())
            .map_err(|e| ToolError::msg(format!("get theme: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("no such theme `{}`", args.id)))?;
        if row.builtin {
            return Err(ToolError::msg("builtin themes cannot be deleted"));
        }
        if row.author != author {
            return Err(ToolError::msg(format!(
                "`{}` belongs to {} — you can only delete your own themes",
                row.id, row.author
            )));
        }
        let active = themes::active_theme_id(&store).unwrap_or_else(|_| "black".into());
        store
            .delete_theme(&row.id)
            .map_err(|e| ToolError::msg(format!("delete theme: {e}")))?;
        if active == row.id {
            store
                .set_active_theme_id(themes::DEFAULT_THEME_ID)
                .map_err(|e| ToolError::msg(format!("reset active theme: {e}")))?;
        }
        let active = themes::active_theme_id(&store).unwrap_or_else(|_| "black".into());
        themes::emit_changed(&active, &row.id);
        Ok(ToolResult::success(
            json!({ "deleted": row.id, "active": active }),
        ))
    }
}

/// Register the theme tools alongside the other coordination tools.
pub fn add_theme_tools(registry: &mut ToolRegistry) {
    registry.insert(ThemeListTool);
    registry.insert(ThemeGetTool);
    registry.insert(ThemeSchemaTool);
    registry.insert(ThemeSaveTool);
    registry.insert(ThemeDeleteTool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;

    fn ctx(dir: &std::path::Path, agent: &str) -> ToolContext {
        let db = dir.join("snippet.db");
        let store = crate::store::Store::open_cached(&db).unwrap();
        crate::themes::seed_builtins(&store).unwrap();
        ToolContext::new(dir)
            .unwrap()
            .with_store_path(&db)
            .with_agent_id(agent)
    }

    fn doc(id: &str) -> Value {
        json!({
            "schema": 1,
            "id": id,
            "name": "Agent Theme",
            "base": "black",
            "color": {"fg4": "#FFEEEEEE"},
        })
    }

    #[tokio::test]
    async fn save_get_list_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), "agent-a");

        let saved = ThemeSaveTool
            .execute(&ctx, json!({"document": doc("agent-ocean")}))
            .await
            .unwrap();
        assert_eq!(saved.value["status"], "success", "{:?}", saved.value);

        let got = ThemeGetTool
            .execute(&ctx, json!({"id": "agent-ocean"}))
            .await
            .unwrap();
        assert_eq!(got.value["data"]["id"], "agent-ocean");
        assert_eq!(got.value["data"]["color"]["fg4"], "#FFEEEEEE");
        assert_eq!(got.value["data"]["color"]["bg"], "#000000");

        let list = ThemeListTool.execute(&ctx, json!({})).await.unwrap();
        let themes = list.value["data"]["themes"].as_array().unwrap();
        let row = themes.iter().find(|t| t["id"] == "agent-ocean").unwrap();
        assert_eq!(row["author"], "agent:agent-a");
        assert_eq!(row["active"], false);
        assert_eq!(list.value["data"]["active"], "black");

        let del = ThemeDeleteTool
            .execute(&ctx, json!({"id": "agent-ocean"}))
            .await
            .unwrap();
        assert_eq!(del.value["status"], "success");
        assert!(
            ThemeGetTool
                .execute(&ctx, json!({"id": "agent-ocean"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn agent_cannot_touch_user_or_builtin_themes() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), "agent-a");
        let store = ctx.store().unwrap();
        let user_doc = json!({
            "schema": 1, "id": "user-theme", "name": "User", "base": "black",
        });
        store
            .upsert_theme("user-theme", "User", "user", false, &user_doc, 0)
            .unwrap();

        // Save over a user theme: refused.
        let err = ThemeSaveTool
            .execute(&ctx, json!({"document": doc("user-theme")}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("user"), "{err}");

        // Delete a user theme: refused.
        let err = ThemeDeleteTool
            .execute(&ctx, json!({"id": "user-theme"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("user"), "{err}");

        // Builtin: refused both ways.
        assert!(
            ThemeSaveTool
                .execute(&ctx, json!({"document": doc("black")}))
                .await
                .is_err()
        );
        assert!(
            ThemeDeleteTool
                .execute(&ctx, json!({"id": "black"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn save_returns_validation_problems() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path(), "agent-a");
        let mut bad = doc("broken");
        bad["shape"] = json!({"radii": {"card": 60}});
        let result = ThemeSaveTool
            .execute(&ctx, json!({"document": bad}))
            .await
            .unwrap();
        assert_eq!(result.value["status"], "error");
        assert!(
            result.value["error"]["message"]
                .as_str()
                .unwrap()
                .contains("shape.radii.card")
        );
    }

    #[test]
    fn no_apply_tool_is_registered() {
        let mut registry = ToolRegistry::new();
        add_theme_tools(&mut registry);
        let names: Vec<String> = registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        for expected in [
            "theme_list",
            "theme_get",
            "theme_schema",
            "theme_save",
            "theme_delete",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        assert!(!names.iter().any(|n| n.contains("apply")), "{names:?}");
    }
}
