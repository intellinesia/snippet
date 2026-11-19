use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{expect_object, object_schema};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

/// Under the 200s limit CLI sessions put on any tool call, so a wait is never detached.
const WAIT_CAP_SECS: u64 = 170;

pub struct ManageProcessTool;

#[derive(Debug, Deserialize)]
struct ManageProcessArgs {
    action: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    lines: Option<usize>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

fn age_text(started_at: &str) -> String {
    let secs = chrono::DateTime::parse_from_rfc3339(started_at)
        .map(|t| {
            (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0)
        })
        .unwrap_or(0);
    match secs {
        0..=119 => format!("{secs}s"),
        120..=7199 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

fn process_line(p: &crate::bg::BgStatus) -> String {
    let name = p.label.as_deref().unwrap_or(p.command.as_str());
    let name: String = name.chars().take(60).collect();
    let state = if p.running {
        "running".to_string()
    } else {
        format!("exited {}", p.status.as_deref().unwrap_or("?"))
    };
    format!("{} {} {} {}", p.id, name, age_text(&p.started_at), state)
}

#[async_trait]
impl Tool for ManageProcessTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "manage_process".to_string(),
            description: "Manage background processes (started via `bash {background: true}`, or a command that ran past the foreground limit). `list` shows this session's running processes, one line each (`all: true` for every process); `log` reads trailing lines; `wait` blocks until a process exits or `pattern` (a regex on its log) matches, up to `timeout_seconds` (default and max 170; on `timeout`, call it again), then returns the exit status and log tail. Use `wait` instead of sleeping and polling; `kill` terminates."
                .to_string(),
            input_schema: object_schema(
                json!({
                    "action": {
                        "type": "string",
                        "enum": ["list", "kill", "log", "wait"],
                        "description": "Management action."
                    },
                    "id": {
                        "type": "string",
                        "description": "Required for kill, log and wait; returned by bash or list."
                    },
                    "all": {
                        "type": "boolean",
                        "description": "list: include exited processes and other sessions' (default: this session's running only)."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "wait: regex on the process log; returns as soon as it matches."
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 170,
                        "description": "wait: longest to block (default and max 170)."
                    },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                        "default": 50,
                        "description": "Trailing log lines (default 50)."
                    }
                }),
                &["action"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ManageProcessArgs = expect_object("manage_process", arguments)?;
        let id_clean = args.id.as_deref().map(str::trim).filter(|s| !s.is_empty());
        match args.action.as_str() {
            "list" => {
                let mut processes = crate::bg::list(ctx.workspace_root());
                if !args.all {
                    let me = crate::bg::session_key(ctx);
                    processes.retain(|p| p.running && p.session.as_deref() == Some(me));
                }
                let running_count = processes.iter().filter(|p| p.running).count();
                let lines: Vec<String> = processes.iter().map(process_line).collect();
                Ok(ToolResult::success(json!({
                    "processes": lines,
                    "total": processes.len(),
                    "running": running_count,
                    "scope": if args.all { "all" } else { "this session, running" },
                })))
            }
            "wait" => {
                let Some(id) = id_clean else {
                    return Err(ToolError::msg(
                        "Missing required argument 'id' for action 'wait'",
                    ));
                };
                let pattern = match args
                    .pattern
                    .as_deref()
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                {
                    Some(p) => Some(
                        regex::Regex::new(p)
                            .map_err(|e| ToolError::msg(format!("invalid pattern: {e}")))?,
                    ),
                    None => None,
                };
                if crate::bg::find(ctx.workspace_root(), id).is_none() {
                    return Err(ToolError::msg(format!(
                        "Background process with ID '{id}' not found"
                    )));
                }
                let limit = std::time::Duration::from_secs(
                    args.timeout_seconds
                        .unwrap_or(WAIT_CAP_SECS)
                        .clamp(1, WAIT_CAP_SECS),
                );
                let mut cursor = crate::bg::LogCursor::new(ctx.workspace_root(), id);
                let lines_to_read = args.lines.unwrap_or(50).clamp(1, 1000);
                let started = std::time::Instant::now();
                let (outcome, status) = loop {
                    let current = crate::bg::find(ctx.workspace_root(), id);
                    let window = cursor.next_window();
                    if let Some(re) = pattern.as_ref()
                        && re.is_match(&window)
                    {
                        break ("pattern", current.and_then(|p| p.status));
                    }
                    match current {
                        Some(p) if p.running => {}
                        Some(p) => break ("exited", p.status),
                        None => break ("exited", None),
                    }
                    if started.elapsed() >= limit {
                        break ("timeout", None);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                };
                let (log, truncated) = crate::bg::tail_log(ctx.workspace_root(), id, lines_to_read)
                    .unwrap_or_default();
                Ok(ToolResult::success(json!({
                    "id": id,
                    "outcome": outcome,
                    "exit_status": status,
                    "waited_secs": started.elapsed().as_secs(),
                    "truncated": truncated,
                    "log": log,
                })))
            }
            "kill" => {
                let Some(id) = id_clean else {
                    return Err(ToolError::msg(
                        "Missing required argument 'id' for action 'kill'",
                    ));
                };
                match crate::bg::kill_by_id(ctx.workspace_root(), id) {
                    Ok(true) => Ok(ToolResult::success(json!({
                        "id": id,
                        "status": "terminated",
                        "note": format!("Background process '{id}' was sent SIGTERM"),
                    }))),
                    Ok(false) => Ok(ToolResult::success(json!({
                        "id": id,
                        "status": "already_stopped",
                        "note": format!("Background process '{id}' was already terminated or exited"),
                    }))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ToolError::msg(
                        format!("Background process with ID '{id}' not found"),
                    )),
                    Err(e) => Err(ToolError::msg(format!(
                        "Failed to kill background process '{id}': {e}"
                    ))),
                }
            }
            "log" => {
                let Some(id) = id_clean else {
                    return Err(ToolError::msg(
                        "Missing required argument 'id' for action 'log'",
                    ));
                };
                let lines_to_read = args.lines.unwrap_or(50).clamp(1, 1000);
                match crate::bg::tail_log(ctx.workspace_root(), id, lines_to_read) {
                    Ok((log_content, truncated)) => Ok(ToolResult::success(json!({
                        "id": id,
                        "requested_lines": lines_to_read,
                        "truncated": truncated,
                        "log": log_content,
                    }))),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ToolError::msg(
                        format!("Log for background process '{id}' not found"),
                    )),
                    Err(e) => Err(ToolError::msg(format!(
                        "Failed to read log for '{id}': {e}"
                    ))),
                }
            }
            other => Err(ToolError::msg(format!(
                "Invalid action '{other}'. Supported actions are 'list', 'kill', 'log' and 'wait'."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_manage_process_list_empty() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let res = tool.execute(&ctx, json!({"action": "list"})).await.unwrap();
        assert_eq!(res.value["data"]["total"], 0);
        assert_eq!(res.value["data"]["running"], 0);
    }

    #[tokio::test]
    async fn test_manage_process_kill_missing_id() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "kill"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Missing required argument 'id'"));
    }

    #[tokio::test]
    async fn test_manage_process_log_missing_id() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "log"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Missing required argument 'id'"));
    }

    #[tokio::test]
    async fn test_manage_process_kill_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "kill", "id": "unknown_proc"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_manage_process_log_success() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let id = "testlog1";
        crate::bg::record(
            temp.path(),
            id,
            "npm run dev",
            Some("Dev Server"),
            88888,
            None,
        )
        .unwrap();
        let log_file = crate::bg::log_path(temp.path(), id);
        std::fs::write(&log_file, "server starting...\nlistening on port 3000\n").unwrap();

        let res = tool
            .execute(&ctx, json!({"action": "log", "id": id, "lines": 10}))
            .await
            .unwrap();
        assert_eq!(res.value["data"]["id"], id);
        assert!(
            res.value["data"]["log"]
                .as_str()
                .unwrap()
                .contains("listening on port 3000")
        );
    }

    #[tokio::test]
    async fn test_manage_process_invalid_action() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let tool = ManageProcessTool;

        let err = tool
            .execute(&ctx, json!({"action": "restart", "id": "abc"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid action 'restart'"));
    }

    fn spawn_recorded(
        ws: &std::path::Path,
        id: &str,
        script: &str,
        session: &str,
    ) -> std::process::Child {
        std::fs::create_dir_all(crate::bg::bg_dir(ws)).unwrap();
        let log = std::fs::File::create(crate::bg::log_path(ws, id)).unwrap();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(log)
            .spawn()
            .unwrap();
        crate::bg::record(ws, id, script, Some(id), child.id(), Some(session)).unwrap();
        child
    }

    #[tokio::test]
    async fn wait_returns_on_exit_and_on_a_log_pattern() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let ws = temp.path().to_path_buf();
        let tool = ManageProcessTool;

        let mut quick = spawn_recorded(
            &ws,
            "quick001",
            "echo done-line; sleep 1",
            crate::bg::session_key(&ctx),
        );
        let status_path = crate::bg::status_path(&ws, "quick001");
        std::thread::spawn(move || {
            let code = quick.wait().unwrap().code().unwrap_or(-1);
            std::fs::write(status_path, code.to_string()).unwrap();
        });
        let res = tool
            .execute(
                &ctx,
                json!({"action": "wait", "id": "quick001", "timeout_seconds": 20}),
            )
            .await
            .unwrap();
        assert_eq!(res.value["data"]["outcome"], "exited", "{}", res.value);
        assert_eq!(res.value["data"]["exit_status"], "0");
        assert!(
            res.value["data"]["log"]
                .as_str()
                .unwrap()
                .contains("done-line")
        );

        let mut slow = spawn_recorded(
            &ws,
            "slow0001",
            "echo booting; echo ready now; sleep 30",
            crate::bg::session_key(&ctx),
        );
        let begun = std::time::Instant::now();
        let res = tool
            .execute(&ctx, json!({"action": "wait", "id": "slow0001", "pattern": "ready\\s+now", "timeout_seconds": 20}))
            .await
            .unwrap();
        assert_eq!(res.value["data"]["outcome"], "pattern", "{}", res.value);
        assert!(begun.elapsed().as_secs() < 10);
        let timed = tool
            .execute(
                &ctx,
                json!({"action": "wait", "id": "slow0001", "timeout_seconds": 1}),
            )
            .await
            .unwrap();
        assert_eq!(timed.value["data"]["outcome"], "timeout");
        let _ = slow.kill();
        let _ = slow.wait();
    }

    #[tokio::test]
    async fn wait_matches_a_line_that_scrolled_past_the_tail() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let ws = temp.path().to_path_buf();
        let mut chatty = spawn_recorded(
            &ws,
            "chatty01",
            "echo needle-found; i=0; while [ $i -lt 3000 ]; do echo noise $i; i=$((i+1)); done; sleep 30",
            crate::bg::session_key(&ctx),
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let res = ManageProcessTool
            .execute(&ctx, json!({"action": "wait", "id": "chatty01", "pattern": "needle-found", "timeout_seconds": 20}))
            .await
            .unwrap();
        assert_eq!(res.value["data"]["outcome"], "pattern", "{}", res.value);
        let _ = chatty.kill();
        let _ = chatty.wait();
    }

    #[tokio::test]
    async fn list_defaults_to_this_sessions_running_processes() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ToolContext::new(temp.path()).unwrap();
        let ws = temp.path().to_path_buf();
        let tool = ManageProcessTool;
        let mine = crate::bg::session_key(&ctx).to_string();

        let mut a = spawn_recorded(&ws, "mine0001", "sleep 30", &mine);
        let mut b = spawn_recorded(&ws, "other001", "sleep 30", "some-other-session");
        crate::bg::record(&ws, "old00001", "echo old", None, 999_999, Some(&mine)).unwrap();

        let res = tool.execute(&ctx, json!({"action": "list"})).await.unwrap();
        let lines = res.value["data"]["processes"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "{}", res.value);
        assert!(lines[0].as_str().unwrap().starts_with("mine0001 "));
        assert!(lines[0].as_str().unwrap().ends_with("running"));

        let all = tool
            .execute(&ctx, json!({"action": "list", "all": true}))
            .await
            .unwrap();
        assert_eq!(all.value["data"]["total"], 3);
        let _ = a.kill();
        let _ = b.kill();
        let _ = a.wait();
        let _ = b.wait();
    }
}
