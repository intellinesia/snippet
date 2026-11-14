//! The `watch` tool: an agent registers a shell script the daemon runs in the
//! background; when the script reports something new the owner's desk is woken
//! with a `[watch]` envelope. The runner lives in `crate::serve::watches`, the
//! rows in `crate::coordination::watches`.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::coordination::watches as cw;
use crate::llm::NativeToolDefinition;
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub struct WatchTool;

#[derive(Deserialize)]
struct WatchArgs {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    interval_minutes: Option<i64>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    max_wakes_per_day: Option<i32>,
    #[serde(default)]
    quiet_start: Option<String>,
    #[serde(default)]
    quiet_end: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
}

/// Owner resolution mirrors `WakeScheduleTool`: Mission Control may name any
/// agent, everyone else owns the watch as their own agent id.
fn resolve_owner(ctx: &ToolContext, agent_id: Option<&str>) -> Result<String, ToolError> {
    let named = agent_id.map(str::trim).filter(|a| !a.is_empty());
    let owner = match (ctx.is_mission_control(), named) {
        (true, Some(other)) => other.to_string(),
        (true, None) => crate::mission_control::SESSION_ID.to_string(),
        (false, Some(_)) => {
            return Err(ToolError::msg(
                "only Mission Control can manage another agent's watches",
            ));
        }
        (false, None) => ctx
            .agent_id()
            .filter(|a| *a != crate::coordination::SNIPPET_AGENT_ID)
            .ok_or_else(|| {
                ToolError::msg(
                    "a watch wakes an agent's desk, and this session runs as no named agent; ask that agent or Mission Control (agent_id) to add it",
                )
            })?
            .to_string(),
    };
    Ok(owner)
}

fn mc_root(ctx: &ToolContext) -> PathBuf {
    ctx.mission_control_root()
        .unwrap_or_else(|| crate::mission_control::MissionControlStore::default_root(None))
}

fn check_owner(store: &Store, owner: &str) -> Result<(), ToolError> {
    if owner == crate::mission_control::SESSION_ID {
        return Ok(());
    }
    match store.get_agent(owner) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(ToolError::msg(format!("unknown agent `{owner}`"))),
        Err(error) => Err(ToolError::msg(error.to_string())),
    }
}

fn name_arg(args: &WatchArgs) -> Result<&str, ToolError> {
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| ToolError::msg("name is required"))?;
    if !cw::valid_watch_name(name) {
        return Err(ToolError::msg("name must match [a-z0-9_-]{1,40}"));
    }
    Ok(name)
}

fn mode_arg(args: &WatchArgs) -> Result<String, ToolError> {
    let mode = args.mode.clone().unwrap_or_else(|| "poll".into());
    if !cw::valid_mode(&mode) {
        return Err(ToolError::msg("mode must be poll or stream"));
    }
    Ok(mode)
}

fn interval_secs(args: &WatchArgs) -> Result<i64, ToolError> {
    let minutes = args.interval_minutes.unwrap_or(5);
    if minutes < 1 {
        return Err(ToolError::msg("interval_minutes must be at least 1"));
    }
    Ok(minutes * 60)
}

fn filter_arg(filter: Option<&str>) -> Result<Option<String>, ToolError> {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(raw) => {
            Regex::new(raw).map_err(|e| ToolError::msg(format!("bad filter regex: {e}")))?;
            Ok(Some(raw.to_string()))
        }
        None => Ok(None),
    }
}

/// Empty clears; a value must be HH:MM.
fn clock_arg(raw: Option<&str>, field: &str) -> Result<Option<String>, ToolError> {
    match raw.map(str::trim).filter(|c| !c.is_empty()) {
        Some(clock) if crate::mission_autonomy::valid_clock(clock) => Ok(Some(clock.to_string())),
        Some(clock) => Err(ToolError::msg(format!(
            "{field} needs HH:MM, got `{clock}`"
        ))),
        None => Ok(None),
    }
}

fn cap_arg(cap: Option<i32>) -> Result<i32, ToolError> {
    let cap = cap.unwrap_or(cw::DEFAULT_MAX_WAKES_PER_DAY);
    if !(1..=cw::MAX_WAKES_PER_DAY_CEILING).contains(&cap) {
        return Err(ToolError::msg(format!(
            "max_wakes_per_day must be 1 to {}",
            cw::MAX_WAKES_PER_DAY_CEILING
        )));
    }
    Ok(cap)
}

/// Write the script under the owner's watches dir, mode 0700.
fn write_script(
    root: &std::path::Path,
    owner: &str,
    name: &str,
    text: &str,
) -> Result<PathBuf, ToolError> {
    let dir = cw::watches_dir(root, owner);
    std::fs::create_dir_all(&dir).map_err(|e| ToolError::msg(format!("watches dir: {e}")))?;
    let path = dir.join(format!("{name}.sh"));
    std::fs::write(&path, text).map_err(|e| ToolError::msg(format!("write script: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ToolError::msg(format!("chmod script: {e}")))?;
    }
    Ok(path)
}

fn views(store: &Store, watches: &[cw::Watch]) -> Result<Vec<Value>, ToolError> {
    watches
        .iter()
        .map(|w| {
            store
                .watch_view(w)
                .map_err(|e| ToolError::msg(e.to_string()))
        })
        .collect()
}

async fn test_poll(
    root: &std::path::Path,
    owner: &str,
    name: &str,
    script_path: &str,
    script_text: &str,
    filter: Option<&str>,
) -> Result<Value, ToolError> {
    let state_dir = cw::state_dir(root, owner, name);
    std::fs::create_dir_all(&state_dir).ok();
    let cwd = cw::watches_dir(root, owner);
    let capture = crate::serve::watches::run_script_once(
        &PathBuf::from(script_path),
        &cwd,
        name,
        &state_dir,
        script_text,
        Duration::from_secs(60),
        8 * 1024,
    )
    .await;
    let re = filter.and_then(|f| Regex::new(f).ok());
    let lines: Vec<String> = crate::serve::watches::output_lines(&capture.stdout, None);
    let waking = crate::serve::watches::output_lines(&capture.stdout, re.as_ref());
    Ok(json!({
        "exit_code": capture.exit_code,
        "timed_out": capture.timed_out,
        "spawn_error": capture.spawn_error,
        "stdout": capture.stdout,
        "stderr_tail": capture.stderr_tail,
        "lines": lines,
        "would_wake_with": waking,
    }))
}

/// Stream test: run for 15 s, then kill the group and return what it printed.
async fn test_stream(
    root: &std::path::Path,
    owner: &str,
    name: &str,
    script_path: &str,
    script_text: &str,
    filter: Option<&str>,
) -> Result<Value, ToolError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let state_dir = cw::state_dir(root, owner, name);
    std::fs::create_dir_all(&state_dir).ok();
    let cwd = cw::watches_dir(root, owner);
    let mut cmd = crate::serve::watches::watch_command(
        &PathBuf::from(script_path),
        name,
        &state_dir,
        script_text,
        None,
    );
    cmd.current_dir(&cwd);
    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::msg(format!("spawn failed: {e}")))?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let re = filter.and_then(|f| Regex::new(f).ok());
    let mut lines_out = Vec::new();
    let mut raw = Vec::new();
    let mut reader = BufReader::new(stdout).lines();
    let deadline = tokio::time::sleep(Duration::from_secs(15));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            line = reader.next_line() => match line {
                Ok(Some(line)) => {
                    let trimmed = line.trim_end().to_string();
                    if !trimmed.is_empty() {
                        raw.push(trimmed.clone());
                        if re.as_ref().is_none_or(|r| r.is_match(&trimmed)) {
                            lines_out.push(trimmed);
                        }
                    }
                }
                _ => break,
            },
            _ = &mut deadline => break,
        }
    }
    if let Some(pid) = pid {
        crate::serve::watches::kill_group(pid);
    }
    let status = child.wait().await.ok();
    let stderr_tail = {
        let mut buf = String::new();
        use tokio::io::AsyncReadExt;
        let mut stderr = stderr;
        let _ = stderr.read_to_string(&mut buf).await;
        buf.lines().last().unwrap_or("").to_string()
    };
    let vault = crate::vault::Vault::load();
    Ok(json!({
        "exit_code": status.and_then(|s| s.code()),
        "timed_out": true,
        "lines": raw,
        "would_wake_with": lines_out,
        "stderr_tail": vault.scrub_str(&stderr_tail),
    }))
}

#[async_trait]
impl Tool for WatchTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "watch".into(),
            description: "Run a shell script in the background, owned by the daemon, that wakes you when it reports something new: your own channel for email, chat apps, APIs, pages or feeds. poll runs it every interval and wakes you with output lines that were not there last time; stream keeps it running and wakes you with the lines it prints. Vault secrets work as $NAME. test runs it once without waking. Guide: harness_map section guide.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "add", "update", "remove", "pause", "resume", "test", "log"]},
                    "name": {"type": "string"},
                    "script": {"type": "string"},
                    "mode": {"type": "string", "enum": ["poll", "stream"]},
                    "interval_minutes": {"type": "integer"},
                    "filter": {"type": "string"},
                    "max_wakes_per_day": {"type": "integer"},
                    "quiet_start": {"type": "string"},
                    "quiet_end": {"type": "string"},
                    "agent_id": {"type": "string"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: WatchArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let owner = resolve_owner(ctx, args.agent_id.as_deref())?;
        let store = ctx
            .store()
            .map_err(|e| ToolError::msg(format!("open store: {e}")))?;
        check_owner(&store, &owner)?;
        let root = mc_root(ctx);
        match args.action.as_str() {
            "list" => {
                let watches = store
                    .list_watches(Some(&owner))
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                Ok(ToolResult::success(json!(views(&store, &watches)?)))
            }
            "add" => {
                let name = name_arg(&args)?.to_string();
                let script = args
                    .script
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| ToolError::msg("script is required for add"))?;
                let mode = mode_arg(&args)?;
                let interval_secs = interval_secs(&args)?;
                let filter = filter_arg(args.filter.as_deref())?;
                let cap = cap_arg(args.max_wakes_per_day)?;
                let quiet_start = clock_arg(args.quiet_start.as_deref(), "quiet_start")?;
                let quiet_end = clock_arg(args.quiet_end.as_deref(), "quiet_end")?;
                let path = write_script(&root, &owner, &name, script)?;
                let state_dir = cw::state_dir(&root, &owner, &name);
                let _ = std::fs::create_dir_all(&state_dir);
                let new = cw::NewWatch {
                    owner: owner.clone(),
                    name: name.clone(),
                    mode,
                    script_path: path.display().to_string(),
                    interval_secs,
                    filter,
                    max_wakes_per_day: cap,
                    quiet_start,
                    quiet_end,
                };
                match store.create_watch(&new) {
                    Ok(watch) => {
                        let view = store
                            .watch_view(&watch)
                            .map_err(|e| ToolError::msg(e.to_string()))?;
                        Ok(ToolResult::success(view))
                    }
                    Err(error) => {
                        let _ = std::fs::remove_file(&path);
                        Err(ToolError::msg(error.to_string()))
                    }
                }
            }
            "update" | "pause" | "resume" => {
                let name = name_arg(&args)?;
                let watch = store
                    .get_watch(&owner, name)
                    .map_err(|e| ToolError::msg(e.to_string()))?
                    .ok_or_else(|| ToolError::msg(format!("unknown watch `{name}`")))?;
                let mut patch = cw::WatchPatch::default();
                if args.action == "pause" {
                    patch.enabled = Some(false);
                } else if args.action == "resume" {
                    patch.enabled = Some(true);
                }
                if args.action == "update" {
                    if let Some(mode) = &args.mode {
                        if !cw::valid_mode(mode) {
                            return Err(ToolError::msg("mode must be poll or stream"));
                        }
                        patch.mode = Some(mode.clone());
                    }
                    if let Some(minutes) = args.interval_minutes {
                        if minutes < 1 {
                            return Err(ToolError::msg("interval_minutes must be at least 1"));
                        }
                        patch.interval_secs = Some(minutes * 60);
                    }
                    if let Some(filter) = &args.filter {
                        patch.filter = Some(filter_arg(Some(filter))?);
                    }
                    if let Some(cap) = args.max_wakes_per_day {
                        patch.max_wakes_per_day = Some(cap_arg(Some(cap))?);
                    }
                    if let Some(clock) = &args.quiet_start {
                        patch.quiet_start = Some(clock_arg(Some(clock), "quiet_start")?);
                    }
                    if let Some(clock) = &args.quiet_end {
                        patch.quiet_end = Some(clock_arg(Some(clock), "quiet_end")?);
                    }
                    if let Some(script) = args.script.as_deref().filter(|s| !s.trim().is_empty()) {
                        write_script(&root, &owner, name, script)?;
                    }
                }
                let updated = store
                    .update_watch(&watch.id, &patch)
                    .map_err(|e| ToolError::msg(e.to_string()))?
                    .ok_or_else(|| ToolError::msg(format!("unknown watch `{name}`")))?;
                let view = store
                    .watch_view(&updated)
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                Ok(ToolResult::success(view))
            }
            "remove" => {
                let name = name_arg(&args)?;
                let watch = store
                    .get_watch(&owner, name)
                    .map_err(|e| ToolError::msg(e.to_string()))?
                    .ok_or_else(|| ToolError::msg(format!("unknown watch `{name}`")))?;
                store
                    .delete_watch(&watch.id)
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                cw::remove_watch_files(&root, &owner, name);
                Ok(ToolResult::success(json!({"removed": true})))
            }
            "test" => {
                let name = name_arg(&args)?;
                let watch = store
                    .get_watch(&owner, name)
                    .map_err(|e| ToolError::msg(e.to_string()))?
                    .ok_or_else(|| ToolError::msg(format!("unknown watch `{name}`")))?;
                // An inline script wins; otherwise the stored file runs.
                let (script_path, script_text) =
                    match args.script.as_deref().filter(|s| !s.trim().is_empty()) {
                        Some(text) => {
                            let dir = cw::watches_dir(&root, &owner);
                            std::fs::create_dir_all(&dir)
                                .map_err(|e| ToolError::msg(format!("watches dir: {e}")))?;
                            let path =
                                dir.join(format!(".test-{}.sh", uuid::Uuid::new_v4().simple()));
                            std::fs::write(&path, text)
                                .map_err(|e| ToolError::msg(format!("write test script: {e}")))?;
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let _ = std::fs::set_permissions(
                                    &path,
                                    std::fs::Permissions::from_mode(0o700),
                                );
                            }
                            (path.display().to_string(), text.to_string())
                        }
                        None => {
                            let text = std::fs::read_to_string(&watch.script_path)
                                .map_err(|e| ToolError::msg(format!("read script: {e}")))?;
                            (watch.script_path.clone(), text)
                        }
                    };
                let result = if watch.mode == "stream" {
                    test_stream(
                        &root,
                        &owner,
                        name,
                        &script_path,
                        &script_text,
                        watch.filter.as_deref(),
                    )
                    .await?
                } else {
                    test_poll(
                        &root,
                        &owner,
                        name,
                        &script_path,
                        &script_text,
                        watch.filter.as_deref(),
                    )
                    .await?
                };
                if args.script.is_some() {
                    let _ = std::fs::remove_file(PathBuf::from(&script_path));
                }
                Ok(ToolResult::success(result))
            }
            "log" => {
                let name = name_arg(&args)?;
                let watch = store
                    .get_watch(&owner, name)
                    .map_err(|e| ToolError::msg(e.to_string()))?
                    .ok_or_else(|| ToolError::msg(format!("unknown watch `{name}`")))?;
                let events = store
                    .list_watch_events(&watch.id, 20)
                    .map_err(|e| ToolError::msg(e.to_string()))?;
                let events: Vec<Value> = events
                    .into_iter()
                    .map(|e| json!({"kind": e.kind, "at": e.at, "excerpt": e.excerpt}))
                    .collect();
                Ok(ToolResult::success(json!({"events": events})))
            }
            other => Err(ToolError::msg(format!(
                "unknown action `{other}`; use list, add, update, remove, pause, resume, test or log"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};
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

    fn ctx_for(dir: &std::path::Path, agent: &str) -> ToolContext {
        ToolContext::new(dir)
            .unwrap()
            .with_agent_id(agent)
            .with_store_path(dir.join("snippet.db"))
            .with_mission_control_root(dir.join("mc"))
    }

    fn ctx_mc(dir: &std::path::Path) -> ToolContext {
        ToolContext::mission_control(dir.join("mc"))
            .unwrap()
            .with_store_path(dir.join("snippet.db"))
    }

    #[tokio::test]
    async fn add_list_update_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("snippet.db")).unwrap();
        db.create_agent(&worker("a")).unwrap();
        drop(db);
        let ctx = ctx_for(dir.path(), "a");
        let tool = WatchTool;

        let added = tool
            .execute(
                &ctx,
                json!({"action":"add","name":"inbox","script":"echo hi","mode":"poll","interval_minutes":5}),
            )
            .await
            .unwrap();
        assert_eq!(added.value["data"]["name"], "inbox");
        assert_eq!(added.value["data"]["status"], "idle");
        assert!(
            added.value["data"]["script_path"]
                .as_str()
                .unwrap()
                .ends_with("inbox.sh")
        );

        let list = tool.execute(&ctx, json!({"action":"list"})).await.unwrap();
        assert_eq!(list.value["data"].as_array().unwrap().len(), 1);

        let updated = tool
            .execute(
                &ctx,
                json!({"action":"update","name":"inbox","filter":"error","max_wakes_per_day":10}),
            )
            .await
            .unwrap();
        assert_eq!(updated.value["data"]["filter"], "error");
        assert_eq!(updated.value["data"]["max_wakes_per_day"], 10);

        let paused = tool
            .execute(&ctx, json!({"action":"pause","name":"inbox"}))
            .await
            .unwrap();
        assert_eq!(paused.value["data"]["enabled"], false);
        let resumed = tool
            .execute(&ctx, json!({"action":"resume","name":"inbox"}))
            .await
            .unwrap();
        assert_eq!(resumed.value["data"]["enabled"], true);

        let removed = tool
            .execute(&ctx, json!({"action":"remove","name":"inbox"}))
            .await
            .unwrap();
        assert_eq!(removed.value["data"]["removed"], true);
    }

    #[tokio::test]
    async fn non_mc_cannot_name_another_agent_and_validation_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("snippet.db")).unwrap();
        db.create_agent(&worker("a")).unwrap();
        db.create_agent(&worker("b")).unwrap();
        drop(db);
        let ctx = ctx_for(dir.path(), "a");
        let tool = WatchTool;

        assert!(
            tool.execute(&ctx, json!({"action":"list","agent_id":"b"}))
                .await
                .is_err()
        );
        for bad in [
            json!({"action":"add","name":"Bad Name","script":"echo hi"}),
            json!({"action":"add","name":"ok","script":"echo hi","mode":"webhook"}),
            json!({"action":"add","name":"ok","script":"echo hi","interval_minutes":0}),
            json!({"action":"add","name":"ok","script":"echo hi","filter":"(["}),
            json!({"action":"add","name":"ok","script":"echo hi","max_wakes_per_day":999}),
            json!({"action":"add","name":"ok","script":"echo hi","quiet_start":"25:00"}),
            json!({"action":"update","name":"ghost","filter":"x"}),
            json!({"action":"frobnicate"}),
        ] {
            assert!(tool.execute(&ctx, bad.clone()).await.is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn mission_control_can_manage_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("snippet.db")).unwrap();
        db.create_agent(&worker("a")).unwrap();
        drop(db);
        let ctx = ctx_mc(dir.path());
        let tool = WatchTool;
        let added = tool
            .execute(
                &ctx,
                json!({"action":"add","agent_id":"a","name":"feed","script":"echo x"}),
            )
            .await
            .unwrap();
        assert_eq!(added.value["data"]["owner"], "a");
        // MC's own watches live under its own home.
        let own = tool
            .execute(&ctx, json!({"action":"add","name":"own","script":"echo y"}))
            .await
            .unwrap();
        assert_eq!(own.value["data"]["owner"], "mission-control");
        // Naming an unknown agent is refused.
        assert!(
            tool.execute(&ctx, json!({"action":"list","agent_id":"ghost"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_action_runs_script_once_without_waking() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("snippet.db")).unwrap();
        db.create_agent(&worker("a")).unwrap();
        drop(db);
        let ctx = ctx_for(dir.path(), "a");
        let tool = WatchTool;
        tool.execute(
            &ctx,
            json!({"action":"add","name":"t","script":"echo alpha\necho beta","filter":"alpha"}),
        )
        .await
        .unwrap();
        let result = tool
            .execute(&ctx, json!({"action":"test","name":"t"}))
            .await
            .unwrap();
        let data = &result.value["data"];
        assert_eq!(data["exit_code"], 0);
        assert!(data["stdout"].as_str().unwrap().contains("alpha"));
        assert_eq!(data["lines"].as_array().unwrap().len(), 2);
        assert_eq!(data["would_wake_with"].as_array().unwrap().len(), 1);

        let log = tool
            .execute(&ctx, json!({"action":"log","name":"t"}))
            .await
            .unwrap();
        assert!(log.value["data"]["events"].is_array());
    }
}
