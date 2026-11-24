use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::sandbox::{self, Globals, HostRequest, Limits, Outcome};
use crate::llm::NativeToolDefinition;

pub const CODE_TOOLS: &[&str] = &["code_run", "code_resume", "code_describe"];
pub const CALL_TOOLS: &[&str] = &["calls_list", "calls_retry"];
const DENIED: &[&str] = &[
    "code",
    "calls",
    "ask_user",
    "terminate_loop",
    "complete_goal",
    "speak",
    "view_image",
];
const RESULT_CHARS: usize = 20_000;
const MAX_FS_READ_BYTES: usize = 2 * 1024 * 1024;

pub type CallFuture = Pin<Box<dyn Future<Output = Value> + Send>>;
type InFlightCall = Pin<Box<dyn Future<Output = (usize, u64, Value)> + Send>>;

#[derive(Serialize, Deserialize, Clone)]
pub struct RecordedCall {
    pub name: String,
    pub args: Value,
    pub result: Value,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct RunRecord {
    pub run_id: String,
    pub code: String,
    pub calls: Vec<RecordedCall>,
    pub answers: Vec<Value>,
    pub status: String,
}

#[async_trait::async_trait]
pub trait InnerCalls: Send {
    async fn call(&mut self, name: &str, args: Value) -> Value;

    /// Optional concurrent dispatch so `Promise.all` runs inner calls in parallel.
    fn call_concurrent(&self, _name: &str, _args: Value) -> Option<CallFuture> {
        None
    }

    /// Working directory for relative `fs.*` paths inside scripts.
    fn workspace(&self) -> Option<PathBuf> {
        None
    }
}

pub fn is_code_tool(name: &str) -> bool {
    CODE_TOOLS.contains(&name) || CALL_TOOLS.contains(&name)
}

fn def(name: &str, description: &str, props: Value, required: &[&str]) -> NativeToolDefinition {
    NativeToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": props, "required": required}),
    }
}

fn code_definitions() -> Vec<NativeToolDefinition> {
    vec![
        def(
            "code_run",
            "Run JavaScript that calls your tools.",
            json!({"code": {"type": "string", "description": "Body of an async function. Every tool is a function: `await task.update({...})` for a grouped tool's action, `await bash({command})` for a plain tool, and `await Promise.all([...])` runs calls concurrently. Untruncated file helpers: `await fs.read(path)`, `fs.write(path, text)`, `fs.replace(path, find, replacement, all?)`, `fs.exists(path)`, `fs.list(dir?)`. `return` only the compact value you need (skip raw tool envelopes); console.log lines come back too. Tool calls return their normal JSON and errors include the call's TypeScript signature when arguments fail—check `status`. Call direct `bash`, `change_files` or `view_image` for single steps, edits with `${...}`, or standalone image views; pause with `await agent.ask(question, data)` or `await agent.look(path, question)` only when later script steps depend on your answer, then continue with code.resume."}}),
            &["code"],
        ),
        def(
            "code_resume",
            "Resume a paused run with your answer, or rerun a run with edited code.",
            json!({
                "run_id": {"type": "string"},
                "answer": {"description": "What `agent.ask` returns at the pause."},
                "code": {"type": "string", "description": "Edited code: calls that match the earlier run reuse their recorded results up to the first difference."}
            }),
            &["run_id"],
        ),
        def(
            "code_describe",
            "Show one tool's call signature as a TypeScript declaration.",
            json!({"name": {"type": "string", "description": "A tool (`bash`), a grouped tool (`task`) or one action (`task.update`)."}}),
            &["name"],
        ),
    ]
}

fn call_definitions() -> Vec<NativeToolDefinition> {
    vec![
        def(
            "calls_list",
            "List your recent tool calls with ids, newest first.",
            json!({"limit": {"type": "integer", "minimum": 1, "maximum": 50}}),
            &[],
        ),
        def(
            "calls_retry",
            "Run a past call again with some arguments changed.",
            json!({
                "call_id": {"type": "integer", "description": "From calls.list."},
                "changes": {"type": "object", "description": "Arguments to change; objects merge, other values replace, null removes."}
            }),
            &["call_id"],
        ),
    ]
}

/// Globals for a script: grouped tools with their actions, plain tools, and `fs.*` helpers.
pub fn globals(advertised: &[NativeToolDefinition]) -> Globals {
    let mut surfaces = Vec::new();
    let mut plain = Vec::new();
    let mut fs = false;
    for def in advertised {
        if matches!(def.name.as_str(), "bash" | "change_files") {
            fs = true;
        }
        if DENIED.contains(&def.name.as_str()) {
            continue;
        }
        match actions_of(def) {
            Some(actions) => surfaces.push((def.name.clone(), actions)),
            None => plain.push(def.name.clone()),
        }
    }
    Globals {
        surfaces,
        plain,
        fs,
    }
}

fn actions_of(def: &NativeToolDefinition) -> Option<Vec<String>> {
    if !crate::surface::is_surface(&def.name) {
        return None;
    }
    def.input_schema
        .pointer("/properties/action/enum")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

/// The `code` tool's description, with the functions this seat can call.
fn annotate(advertised: &mut [NativeToolDefinition], all: &[NativeToolDefinition]) {
    let g = globals(all);
    let mut names: Vec<String> = g
        .surfaces
        .iter()
        .map(|(name, actions)| format!("{name}.{{{}}}", actions.join(",")))
        .collect();
    names.extend(g.plain.iter().cloned());
    if g.fs {
        names.push("fs.{read,write,replace,exists,list}".into());
    }
    if let Some(code) = advertised.iter_mut().find(|d| d.name == "code") {
        code.description
            .push_str(&format!("\nFunctions in scripts: {}.", names.join(", ")));
    }
}

fn runs_dir(session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    crate::config::snippet_home().join("code_runs").join(safe)
}

fn load(session: &str, run_id: &str) -> Option<RunRecord> {
    let path = runs_dir(session).join(format!("{run_id}.json"));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save(session: &str, record: &RunRecord) {
    let dir = runs_dir(session);
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(bytes) = serde_json::to_vec(record) {
        let _ = std::fs::write(dir.join(format!("{}.json", record.run_id)), bytes);
    }
}

fn success(data: Value) -> Value {
    json!({"schema_version": 1, "status": "success", "data": data})
}

fn failure(code: &str, message: String, extra: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let (Some(extra), Some(obj)) = (extra, error.as_object_mut()) {
        obj.insert("details".into(), extra);
    }
    json!({"schema_version": 1, "status": "error", "error": error})
}

fn capped(value: Value) -> Value {
    let text = value.to_string();
    if text.chars().count() <= RESULT_CHARS {
        return value;
    }
    let cut: String = text.chars().take(RESULT_CHARS).collect();
    json!({"truncated": true, "preview": cut, "note": "Return less: filter or summarise inside the script."})
}

/// Run a `code_*` tool. `advertised` is every tool the seat has, for globals and describe.
pub async fn execute(
    tool: &str,
    args: &Value,
    session: &str,
    advertised: &[NativeToolDefinition],
    exec: &mut dyn InnerCalls,
) -> Value {
    match tool {
        "code_describe" => describe(args, advertised),
        "code_run" => {
            let Some(code) = args
                .get("code")
                .and_then(Value::as_str)
                .filter(|c| !c.trim().is_empty())
            else {
                return failure(
                    "invalid_arguments",
                    "`code` must not be empty.".into(),
                    None,
                );
            };
            let record = RunRecord {
                run_id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                code: code.to_string(),
                ..Default::default()
            };
            drive(record, Vec::new(), session, advertised, exec).await
        }
        "code_resume" => {
            let Some(run_id) = args.get("run_id").and_then(Value::as_str) else {
                return failure("invalid_arguments", "`run_id` is required.".into(), None);
            };
            let Some(mut record) = load(session, run_id.trim()) else {
                return failure(
                    "unknown_run",
                    format!("No run `{run_id}` in this session."),
                    None,
                );
            };
            let earlier = std::mem::take(&mut record.calls);
            if let Some(code) = args
                .get("code")
                .and_then(Value::as_str)
                .filter(|c| !c.trim().is_empty())
            {
                record.code = code.to_string();
            }
            let answer = args.get("answer");
            if record.status == "paused" {
                record.answers.push(answer.cloned().unwrap_or(Value::Null));
            } else if answer.is_some() {
                if args.get("code").is_none() {
                    return failure(
                        "not_paused",
                        format!(
                            "Run `{run_id}` is {}, not paused; pass `code` to rerun it.",
                            record.status
                        ),
                        None,
                    );
                }
            }
            drive(record, earlier, session, advertised, exec).await
        }
        _ => failure(
            "unknown_tool",
            format!("`{tool}` is not a code tool."),
            None,
        ),
    }
}

fn is_fs_call(name: &str) -> bool {
    matches!(
        name,
        "__fs_read" | "__fs_write" | "__fs_replace" | "__fs_exists" | "__fs_list"
    )
}

fn resolve_fs_path(workspace: Option<&Path>, raw: &str) -> PathBuf {
    let expanded = if raw == "~" || raw.starts_with("~/") {
        match std::env::var_os("HOME") {
            Some(home) => format!("{}{}", home.to_string_lossy(), &raw[1..]),
            None => raw.to_string(),
        }
    } else {
        raw.to_string()
    };
    let candidate = Path::new(&expanded);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
            .join(candidate)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn execute_fs(name: &str, args: &Value, workspace: Option<PathBuf>) -> Value {
    let path_arg = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match name {
        "__fs_read" => {
            let Some(raw) = path_arg("path") else {
                return failure(
                    "invalid_arguments",
                    "fs.read: `path` is required.".into(),
                    None,
                );
            };
            let path = resolve_fs_path(workspace.as_deref(), raw);
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(e) => return failure("fs_error", format!("fs.read({raw}): {e}"), None),
            };
            if meta.len() as usize > MAX_FS_READ_BYTES {
                return failure(
                    "fs_error",
                    format!(
                        "fs.read({raw}): file is {} bytes (limit {} bytes); use bash to inspect slices.",
                        meta.len(),
                        MAX_FS_READ_BYTES
                    ),
                    None,
                );
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => success(Value::String(content)),
                Err(e) => failure("fs_error", format!("fs.read({raw}): {e}"), None),
            }
        }
        "__fs_write" => {
            let Some(raw) = path_arg("path") else {
                return failure(
                    "invalid_arguments",
                    "fs.write: `path` is required.".into(),
                    None,
                );
            };
            let Some(content) = args.get("content").and_then(Value::as_str) else {
                return failure(
                    "invalid_arguments",
                    "fs.write: `content` is required.".into(),
                    None,
                );
            };
            let path = resolve_fs_path(workspace.as_deref(), raw);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return failure("fs_error", format!("fs.write({raw}): {e}"), None);
            }
            match std::fs::write(&path, content) {
                Ok(()) => {
                    success(json!({"path": path.display().to_string(), "bytes": content.len()}))
                }
                Err(e) => failure("fs_error", format!("fs.write({raw}): {e}"), None),
            }
        }
        "__fs_replace" => {
            let Some(raw) = path_arg("path") else {
                return failure(
                    "invalid_arguments",
                    "fs.replace: `path` is required.".into(),
                    None,
                );
            };
            let Some(find) = args
                .get("find")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return failure(
                    "invalid_arguments",
                    "fs.replace: non-empty `find` is required.".into(),
                    None,
                );
            };
            let Some(replacement) = args
                .get("replacement")
                .or_else(|| args.get("with"))
                .and_then(Value::as_str)
            else {
                return failure(
                    "invalid_arguments",
                    "fs.replace: `replacement` (or `with`) is required.".into(),
                    None,
                );
            };
            let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
            let path = resolve_fs_path(workspace.as_deref(), raw);
            let current = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => return failure("fs_error", format!("fs.replace({raw}): {e}"), None),
            };
            let count = current.matches(find).count();
            if count == 0 {
                return failure(
                    "fs_error",
                    format!("fs.replace({raw}): `find` text not found."),
                    None,
                );
            }
            if count > 1 && !all {
                return failure(
                    "fs_error",
                    format!(
                        "fs.replace({raw}): `find` matched {count} times; pass all=true or a more specific snippet."
                    ),
                    None,
                );
            }
            let updated = if all {
                current.replace(find, replacement)
            } else {
                current.replacen(find, replacement, 1)
            };
            match std::fs::write(&path, updated) {
                Ok(()) => {
                    success(json!({"path": path.display().to_string(), "replacements": count}))
                }
                Err(e) => failure("fs_error", format!("fs.replace({raw}): {e}"), None),
            }
        }
        "__fs_exists" => {
            let Some(raw) = path_arg("path") else {
                return failure(
                    "invalid_arguments",
                    "fs.exists: `path` is required.".into(),
                    None,
                );
            };
            let path = resolve_fs_path(workspace.as_deref(), raw);
            success(Value::Bool(path.exists()))
        }
        "__fs_list" => {
            let raw = path_arg("dir").or_else(|| path_arg("path")).unwrap_or(".");
            let path = resolve_fs_path(workspace.as_deref(), raw);
            let entries = match std::fs::read_dir(&path) {
                Ok(rd) => rd,
                Err(e) => return failure("fs_error", format!("fs.list({raw}): {e}"), None),
            };
            let mut names = Vec::new();
            for entry in entries.flatten() {
                let mut name = entry.file_name().to_string_lossy().to_string();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    name.push('/');
                }
                names.push(name);
            }
            names.sort();
            success(json!(names))
        }
        _ => failure("unknown_tool", format!("Unknown fs helper `{name}`."), None),
    }
}

fn should_attach_signature(result: &Value) -> bool {
    if result.get("status").and_then(Value::as_str) != Some("error") {
        return false;
    }
    let code = result
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(code, "invalid_arguments" | "unknown_action") {
        return true;
    }
    if code != "tool_error" {
        return false;
    }
    let msg = result
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    [
        "required",
        "missing",
        "unknown",
        "invalid",
        "must be",
        "expected",
        "no action",
        "unrecognized",
    ]
    .iter()
    .any(|kw| msg.contains(kw))
}

fn attach_signature(
    name: &str,
    args: &Value,
    result: &mut Value,
    advertised: &[NativeToolDefinition],
) {
    if !should_attach_signature(result) || is_fs_call(name) {
        return;
    }
    let action = args.get("action").and_then(Value::as_str);
    let sig =
        describe_text(name, action, advertised).or_else(|| describe_text(name, None, advertised));
    if let (Some(decl), Some(err)) = (sig, result.get_mut("error").and_then(Value::as_object_mut)) {
        err.insert("signature".into(), Value::String(decl));
    }
}

fn finish_in_flight(
    record: &mut RunRecord,
    slot: usize,
    id: u64,
    mut result: Value,
    advertised: &[NativeToolDefinition],
    reply_tx: &std::sync::mpsc::Sender<(u64, Value)>,
) {
    if let Some(entry) = record.calls.get_mut(slot) {
        attach_signature(&entry.name, &entry.args, &mut result, advertised);
        entry.result = result.clone();
    }
    let _ = reply_tx.send((id, result));
}

async fn drive(
    mut record: RunRecord,
    earlier: Vec<RecordedCall>,
    session: &str,
    advertised: &[NativeToolDefinition],
    exec: &mut dyn InnerCalls,
) -> Value {
    let limits = Limits {
        js_timeout: Duration::from_secs(30),
        call_timeout: Duration::from_secs(180),
        memory_bytes: 64 * 1024 * 1024,
        max_log_chars: 8_000,
    };
    let g = globals(advertised);
    let fs_enabled = g.fs;
    let workspace = exec.workspace();
    let (mut rx, reply_tx, handle) = sandbox::spawn(record.code.clone(), g, limits);
    let mut replayed = 0usize;
    let mut diverged = false;
    let mut asked = 0usize;
    let mut in_flight: FuturesUnordered<InFlightCall> = FuturesUnordered::new();

    loop {
        tokio::select! {
            biased;
            Some((slot, id, result)) = in_flight.next(), if !in_flight.is_empty() => {
                finish_in_flight(&mut record, slot, id, result, advertised, &reply_tx);
            }
            req = rx.recv() => {
                let Some(request) = req else {
                    break;
                };
                match request {
                    HostRequest::Call { id, name, args } => {
                        let slot = record.calls.len();
                        record.calls.push(RecordedCall {
                            name: name.clone(),
                            args: args.clone(),
                            result: Value::Null,
                        });
                        if DENIED.contains(&name.as_str()) {
                            let res = failure(
                                "not_in_code",
                                format!("`{name}` cannot run inside code; call it directly."),
                                None,
                            );
                            finish_in_flight(&mut record, slot, id, res, advertised, &reply_tx);
                        } else if is_fs_call(&name) && !fs_enabled {
                            let res = failure(
                                "not_in_code",
                                "Filesystem helpers are not enabled for this seat.".into(),
                                None,
                            );
                            finish_in_flight(&mut record, slot, id, res, advertised, &reply_tx);
                        } else if !diverged
                            && earlier
                                .get(slot)
                                .is_some_and(|c| c.name == name && c.args == args)
                        {
                            replayed += 1;
                            let cached = earlier[slot].result.clone();
                            record.calls[slot].result = cached.clone();
                            let _ = reply_tx.send((id, cached));
                        } else {
                            diverged = true;
                            if is_fs_call(&name) {
                                let ws = workspace.clone();
                                in_flight.push(Box::pin(async move {
                                    let res = tokio::task::spawn_blocking(move || {
                                        execute_fs(&name, &args, ws)
                                    })
                                    .await
                                    .unwrap_or_else(|e| {
                                        failure("fs_error", format!("fs task failed: {e}"), None)
                                    });
                                    (slot, id, res)
                                }));
                            } else if let Some(fut) = exec.call_concurrent(&name, args.clone()) {
                                in_flight.push(Box::pin(async move {
                                    let res = fut.await;
                                    (slot, id, res)
                                }));
                            } else {
                                let res = exec.call(&name, args).await;
                                finish_in_flight(&mut record, slot, id, res, advertised, &reply_tx);
                            }
                        }
                    }
                    HostRequest::Ask { reply, .. } => {
                        let answer = record.answers.get(asked).cloned();
                        asked += 1;
                        if answer.is_none() {
                            // Drain any already-launched concurrent calls so their results are recorded before pausing.
                            while let Some((slot, id, result)) = in_flight.next().await {
                                finish_in_flight(&mut record, slot, id, result, advertised, &reply_tx);
                            }
                        }
                        let _ = reply.send(answer);
                    }
                }
            }
        }
    }
    while let Some((slot, id, result)) = in_flight.next().await {
        finish_in_flight(&mut record, slot, id, result, advertised, &reply_tx);
    }
    drop(reply_tx);

    let outcome = match handle.await {
        Ok(outcome) => outcome,
        Err(error) => Outcome::Failed {
            message: format!("the script runner stopped: {error}"),
            logs: Vec::new(),
        },
    };
    let calls = record.calls.len();
    let result = match outcome {
        Outcome::Completed { value, logs } => {
            record.status = "completed".into();
            let mut data = Map::new();
            data.insert("status".into(), json!("completed"));
            data.insert("run_id".into(), json!(record.run_id));
            data.insert("result".into(), capped(value));
            data.insert("calls".into(), json!(calls));
            if !logs.is_empty() {
                data.insert("logs".into(), json!(logs));
            }
            if replayed > 0 {
                data.insert("replayed".into(), json!(replayed));
            }
            success(Value::Object(data))
        }
        Outcome::Paused {
            question,
            data: pause_data,
            logs,
        } if pause_data.get("__look").is_some_and(Value::is_string) => {
            record.status = "paused".into();
            let mut data = Map::new();
            data.insert("status".into(), json!("paused"));
            data.insert("run_id".into(), json!(record.run_id));
            data.insert("question".into(), json!(question));
            data.insert("path".into(), pause_data["__look"].clone());
            data.insert("calls".into(), json!(calls));
            if !logs.is_empty() {
                data.insert("logs".into(), json!(logs));
            }
            if replayed > 0 {
                data.insert("replayed".into(), json!(replayed));
            }
            data.insert(
                "next".into(),
                json!("Look at the image, then code.resume with this run_id and your answer."),
            );
            success(Value::Object(data))
        }
        Outcome::Paused {
            question,
            data: pause_data,
            logs,
        } => {
            record.status = "paused".into();
            let mut data = Map::new();
            data.insert("status".into(), json!("paused"));
            data.insert("run_id".into(), json!(record.run_id));
            data.insert("question".into(), json!(question));
            data.insert("data".into(), capped(pause_data));
            data.insert("calls".into(), json!(calls));
            if !logs.is_empty() {
                data.insert("logs".into(), json!(logs));
            }
            if replayed > 0 {
                data.insert("replayed".into(), json!(replayed));
            }
            data.insert(
                "next".into(),
                json!("Decide, then code.resume with this run_id and your answer."),
            );
            success(Value::Object(data))
        }
        Outcome::Failed { message, logs } => {
            record.status = "failed".into();
            let mut details = Map::new();
            details.insert("run_id".into(), json!(record.run_id));
            details.insert("calls".into(), json!(calls));
            if !logs.is_empty() {
                details.insert("logs".into(), json!(logs));
            }
            details.insert(
                "next".into(),
                json!("Fix the code and code.resume with this run_id and `code`; calls that match reuse their results."),
            );
            failure("script_error", message, Some(Value::Object(details)))
        }
    };
    save(session, &record);
    result
}

fn describe_fs(action: Option<&str>) -> Result<String, Value> {
    let entries = [
        (
            "read",
            "/** Read a UTF-8 file in full (no truncation, up to 2 MiB). */\nfs.read(path: string): Promise<string>",
        ),
        (
            "write",
            "/** Write UTF-8 text to a file, creating parent directories. */\nfs.write(path: string, content: string): Promise<{path: string, bytes: number}>",
        ),
        (
            "replace",
            "/** Replace `find` with `replacement` in a file (errors if missing or ambiguous unless all=true). */\nfs.replace(path: string, find: string, replacement: string, all?: boolean): Promise<{path: string, replacements: number}>",
        ),
        (
            "exists",
            "/** Return true if a path exists. */\nfs.exists(path: string): Promise<boolean>",
        ),
        (
            "list",
            "/** List sorted entries in a directory (trailing `/` on subdirectories). */\nfs.list(dir?: string): Promise<string[]>",
        ),
    ];
    match action {
        None => Ok(entries
            .iter()
            .map(|(_, d)| *d)
            .collect::<Vec<_>>()
            .join("\n\n")),
        Some(a) => entries
            .iter()
            .find(|(k, _)| *k == a)
            .map(|(_, d)| (*d).to_string())
            .ok_or_else(|| {
                failure(
                    "unknown_action",
                    format!("`fs` has no helper `{a}`; it has read, write, replace, exists, list."),
                    None,
                )
            }),
    }
}

fn describe_text(
    tool: &str,
    action: Option<&str>,
    advertised: &[NativeToolDefinition],
) -> Option<String> {
    let def = advertised
        .iter()
        .find(|d| d.name == tool && !DENIED.contains(&tool))?;
    match (actions_of(def), action) {
        (Some(actions), Some(action)) if actions.iter().any(|a| a == action) => Some(declaration(
            &format!("{tool}.{action}"),
            &action_fields(def, action),
            &def.description,
        )),
        (Some(_), Some(_)) => None,
        (Some(actions), None) => Some(
            actions
                .iter()
                .map(|a| {
                    declaration(
                        &format!("{tool}.{a}"),
                        &action_fields(def, a),
                        &def.description,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        (None, _) => Some(declaration(tool, &all_fields(def), &def.description)),
    }
}

fn describe(args: &Value, advertised: &[NativeToolDefinition]) -> Value {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let (tool, action) = match name.split_once('.') {
        Some((t, a)) => (t, Some(a)),
        None => (name, None),
    };
    if tool == "fs" && globals(advertised).fs {
        return match describe_fs(action) {
            Ok(text) => success(json!({"name": name, "declaration": text})),
            Err(err) => err,
        };
    }
    let Some(def) = advertised
        .iter()
        .find(|d| d.name == tool && !DENIED.contains(&tool))
    else {
        return failure(
            "unknown_tool",
            format!("No tool `{tool}` in scripts."),
            None,
        );
    };
    let text = match (actions_of(def), action) {
        (Some(actions), Some(action)) if actions.iter().any(|a| a == action) => declaration(
            &format!("{tool}.{action}"),
            &action_fields(def, action),
            &def.description,
        ),
        (Some(actions), Some(action)) => {
            return failure(
                "unknown_action",
                format!(
                    "`{tool}` has no action `{action}`; it has {}.",
                    actions.join(", ")
                ),
                None,
            );
        }
        (Some(actions), None) => actions
            .iter()
            .map(|a| {
                declaration(
                    &format!("{tool}.{a}"),
                    &action_fields(def, a),
                    &def.description,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        (None, _) => declaration(tool, &all_fields(def), &def.description),
    };
    success(json!({"name": name, "declaration": text}))
}

fn all_fields(def: &NativeToolDefinition) -> Vec<(String, Value, bool)> {
    let required: Vec<&str> = def
        .input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    def.input_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .filter(|(k, _)| k.as_str() != "action")
                .map(|(k, v)| (k.clone(), v.clone(), required.contains(&k.as_str())))
                .collect()
        })
        .unwrap_or_default()
}

/// A surface lists each action's fields in its description as `- action: ... Fields: a*, b.`
fn action_fields(def: &NativeToolDefinition, action: &str) -> Vec<(String, Value, bool)> {
    let props = def
        .input_schema
        .get("properties")
        .and_then(Value::as_object);
    let (listed, op) =
        crate::surface::fields_listed_under(&def.name, action).unwrap_or((action, false));
    let prefix = format!("- {listed}:");
    let line = def
        .description
        .lines()
        .find(|l| l.trim_start().starts_with(&prefix));
    let mut out: Vec<(String, Value, bool)> = line
        .and_then(|l| l.split("Fields: ").nth(1))
        .map(|fields| {
            fields
                .trim_end_matches('.')
                .split(", ")
                .filter(|f| !f.is_empty())
                .map(|f| {
                    let required = f.ends_with('*');
                    let key = f.trim_end_matches('*').to_string();
                    let schema = props
                        .and_then(|p| p.get(&key))
                        .cloned()
                        .unwrap_or(Value::Null);
                    (key, schema, required)
                })
                .collect()
        })
        .unwrap_or_default();
    if op {
        let ops = def
            .description
            .lines()
            .find(|l| l.trim_start().starts_with(&format!("- {action}:")))
            .and_then(|l| l.split("(op: ").nth(1))
            .and_then(|rest| rest.split(')').next())
            .map(|ops| json!({"enum": ops.split('|').collect::<Vec<_>>()}))
            .unwrap_or_else(|| json!({"type": "string"}));
        out.insert(0, ("op".into(), ops, true));
    }
    out
}

/// Tools for a session seat: what the model sees, and what its scripts can call.
pub fn seat_tools(
    role: crate::surface::SeatRole,
    mut internal: Vec<NativeToolDefinition>,
    mode: crate::config::ToolMode,
) -> (Vec<NativeToolDefinition>, Vec<NativeToolDefinition>) {
    use crate::config::ToolMode;
    let lane = role == crate::surface::SeatRole::Lane;
    if !lane {
        internal.extend(call_definitions());
    }
    let scripts = mode == ToolMode::Code && !lane;
    if scripts {
        internal.extend(code_definitions());
    }
    let all = crate::surface::advertise(role, internal);
    if !scripts {
        return (all.clone(), all);
    }
    let mut shown: Vec<NativeToolDefinition> = all
        .iter()
        .filter(|d| CODE_MODE_DIRECT.contains(&d.name.as_str()))
        .cloned()
        .collect();
    annotate(&mut shown, &all);
    (shown, all)
}

const CODE_MODE_DIRECT: &[&str] = &[
    "code",
    "calls",
    "bash",
    "change_files",
    "view_image",
    "ask_user",
    "complete_goal",
    "update_plan",
    "speak",
    "custom_tool",
];

/// Reminder injected on the next turn when a session switches tool mode mid-conversation.
pub fn switch_notice(
    mode: crate::config::ToolMode,
    direct_tools: &[NativeToolDefinition],
) -> String {
    match mode {
        crate::config::ToolMode::Code => {
            let names: Vec<&str> = direct_tools.iter().map(|d| d.name.as_str()).collect();
            format!(
                "Tool mode switched to code mode. Direct tools now available: {}. All other tools are functions inside `code.run` scripts (with `Promise.all` concurrency and untruncated `fs.{{read,write,replace,exists,list}}` helpers). Call `bash`, `change_files` and `view_image` directly for single steps, file edits or standalone images; use `code.run` to batch multi-step workflows, loops or filtered output.",
                names.join(", ")
            )
        }
        crate::config::ToolMode::Direct => {
            "Tool mode switched to direct mode. All tools are now called directly; `code` is no longer available.".into()
        }
    }
}

fn ts_type(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "string".into(),
        Some("integer" | "number") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("array") => format!(
            "Array<{}>",
            schema
                .get("items")
                .map(ts_type)
                .unwrap_or_else(|| "unknown".into())
        ),
        Some("object") => "Record<string, unknown>".into(),
        _ => "unknown".into(),
    }
}

fn declaration(path: &str, fields: &[(String, Value, bool)], about: &str) -> String {
    let mut out = String::new();
    let summary = about.lines().next().unwrap_or("").trim();
    if !summary.is_empty() {
        out.push_str(&format!("/** {summary} */\n"));
    }
    out.push_str(&format!("{path}(args: {{"));
    if fields.is_empty() {
        out.push_str("}): Promise<any>");
        return out;
    }
    out.push('\n');
    for (name, schema, required) in fields {
        if let Some(d) = schema.get("description").and_then(Value::as_str) {
            out.push_str(&format!("  /** {d} */\n"));
        }
        let q = if *required { "" } else { "?" };
        out.push_str(&format!("  {name}{q}: {};\n", ts_type(schema)));
    }
    out.push_str("}): Promise<any>");
    out
}

/// Past calls from the session's events, as (id, tool, arguments).
pub fn past_calls(events: &[crate::harness::HarnessEvent]) -> Vec<(usize, String, Value)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            crate::harness::HarnessEvent::ToolCall {
                tool_name,
                arguments,
            } => {
                let name = tool_name
                    .rsplit("__")
                    .next()
                    .unwrap_or(tool_name)
                    .to_string();
                Some((i, name, arguments.clone()))
            }
            _ => None,
        })
        .filter(|(_, name, _)| !matches!(name.as_str(), "calls"))
        .collect()
}

pub fn list_calls(args: &Value, events: &[crate::harness::HarnessEvent]) -> Value {
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(15)
        .clamp(1, 50) as usize;
    let rows: Vec<Value> = past_calls(events)
        .into_iter()
        .rev()
        .take(limit)
        .map(|(id, name, arguments)| {
            let mut preview = arguments.to_string();
            if preview.chars().count() > 200 {
                preview = preview.chars().take(200).collect::<String>() + "...";
            }
            json!({"call_id": id, "tool": name, "arguments": preview})
        })
        .collect();
    success(json!({"calls": rows}))
}

/// The call `calls_retry` should make: the past call with `changes` merged in.
pub fn retry_target(
    args: &Value,
    events: &[crate::harness::HarnessEvent],
) -> Result<(String, Value), Value> {
    let Some(id) = args.get("call_id").and_then(Value::as_u64) else {
        return Err(failure(
            "invalid_arguments",
            "`call_id` is required; calls.list shows ids.".into(),
            None,
        ));
    };
    let Some((_, name, mut arguments)) = past_calls(events)
        .into_iter()
        .find(|(i, _, _)| *i as u64 == id)
    else {
        return Err(failure(
            "unknown_call",
            format!("No call {id}; calls.list shows recent ids."),
            None,
        ));
    };
    if matches!(name.as_str(), "code" | "calls" | "ask_user") {
        return Err(failure(
            "not_retryable",
            format!("`{name}` calls are not retried here; code.resume reruns a script."),
            None,
        ));
    }
    if let Some(changes) = args.get("changes") {
        merge(&mut arguments, changes);
    }
    Ok((name, arguments))
}

fn merge(target: &mut Value, changes: &Value) {
    match (target.as_object_mut(), changes.as_object()) {
        (Some(t), Some(c)) => {
            for (k, v) in c {
                if v.is_null() {
                    t.remove(k);
                } else if v.is_object() && t.get(k).is_some_and(Value::is_object) {
                    merge(t.get_mut(k).unwrap(), v);
                } else {
                    t.insert(k.clone(), v.clone());
                }
            }
        }
        _ => *target = changes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<(String, Value)>);

    #[async_trait::async_trait]
    impl InnerCalls for Fake {
        async fn call(&mut self, name: &str, args: Value) -> Value {
            self.0.push((name.to_string(), args.clone()));
            success(json!({"echo": args, "n": self.0.len()}))
        }
    }

    fn defs() -> Vec<NativeToolDefinition> {
        vec![NativeToolDefinition {
            name: "bash".into(),
            description: "Run a command.".into(),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}),
        }]
    }

    #[tokio::test]
    async fn pause_then_resume_replays_recorded_calls() {
        let session = format!("test-{}", uuid::Uuid::new_v4().simple());
        let code = "const a = await bash({command: 'one'}); const pick = await agent.ask('which?', a); const b = await bash({command: pick}); return b.data.echo.command;";
        let mut exec = Fake(Vec::new());
        let paused = execute(
            "code_run",
            &json!({"code": code}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(paused["data"]["status"], "paused");
        assert_eq!(exec.0.len(), 1);
        let run_id = paused["data"]["run_id"].as_str().unwrap().to_string();
        let done = execute(
            "code_resume",
            &json!({"run_id": run_id, "answer": "two"}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(done["data"]["status"], "completed");
        assert_eq!(done["data"]["result"], "two");
        assert_eq!(done["data"]["replayed"], 1);
        assert_eq!(exec.0.len(), 2);
        let _ = std::fs::remove_dir_all(runs_dir(&session));
    }

    #[tokio::test]
    async fn view_image_in_a_script_pauses_and_resumes_without_an_answer() {
        let session = format!("test-{}", uuid::Uuid::new_v4().simple());
        let code =
            "const v = await view_image({path: 'a.png'}); return [v.data.path, v.data.answer];";
        let mut exec = Fake(Vec::new());
        let paused = execute(
            "code_run",
            &json!({"code": code}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(paused["data"]["status"], "paused");
        assert_eq!(paused["data"]["path"], "a.png");
        let run_id = paused["data"]["run_id"].as_str().unwrap().to_string();
        let done = execute(
            "code_resume",
            &json!({"run_id": run_id}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(done["data"]["result"], json!(["a.png", null]));
        assert!(exec.0.is_empty());
        let _ = std::fs::remove_dir_all(runs_dir(&session));
    }

    #[test]
    fn retry_merges_changes_into_the_past_call() {
        let events = vec![crate::harness::HarnessEvent::ToolCall {
            tool_name: "mcp__snippet__task".into(),
            arguments: json!({"action": "update", "patch": {"brief": "a", "current_step": "x"}, "task_id": "t"}),
        }];
        let (name, args) = retry_target(
            &json!({"call_id": 0, "changes": {"patch": {"brief": "b", "current_step": null}}}),
            &events,
        )
        .unwrap();
        assert_eq!(name, "task");
        assert_eq!(
            args,
            json!({"action": "update", "patch": {"brief": "b"}, "task_id": "t"})
        );
    }

    #[test]
    fn describe_reads_action_fields_from_the_surface() {
        let internal: Vec<NativeToolDefinition> =
            crate::meta::conversation_meta_definitions_for(true, true)
                .into_iter()
                .chain(crate::session::work_tool_registry(None, None).definitions())
                .collect();
        let (direct, _) = seat_tools(
            crate::surface::SeatRole::Work,
            internal.clone(),
            crate::config::ToolMode::Direct,
        );
        assert!(direct.iter().any(|d| d.name == "calls"));
        assert!(!direct.iter().any(|d| d.name == "code"));
        let (shown, all) = seat_tools(
            crate::surface::SeatRole::Work,
            internal,
            crate::config::ToolMode::Code,
        );
        assert!(
            shown
                .iter()
                .any(|d| d.name == "code" && d.description.contains("task.{"))
        );
        let out = describe(&json!({"name": "task.update"}), &all);
        let text = out["data"]["declaration"].as_str().unwrap();
        assert!(text.contains("task.update(args: {"), "{text}");
        assert!(text.contains("expected_version: number;"), "{text}");
        assert!(shown.iter().any(|d| d.name == "bash"));
        assert!(shown.iter().any(|d| d.name == "change_files"));
        assert!(shown.iter().any(|d| {
            d.name == "code"
                && d.description
                    .contains("fs.{read,write,replace,exists,list}")
        }));
        let fs_doc = describe(&json!({"name": "fs.replace"}), &all);
        assert!(
            fs_doc["data"]["declaration"]
                .as_str()
                .unwrap()
                .contains("fs.replace(")
        );
        let (code_only, _) = seat_tools(
            crate::surface::SeatRole::Work,
            Vec::new(),
            crate::config::ToolMode::Code,
        );
        assert!(
            code_only
                .iter()
                .all(|d| CODE_MODE_DIRECT.contains(&d.name.as_str()))
        );
    }

    #[tokio::test]
    async fn fs_helpers_read_write_replace_and_replay_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let big = format!(
            "head\n{}\nconst x = '${{idx}}';\ntail\n",
            "a".repeat(30_000)
        );
        std::fs::write(dir.path().join("sample.dart"), &big).unwrap();

        struct WsExec(PathBuf);
        #[async_trait::async_trait]
        impl InnerCalls for WsExec {
            async fn call(&mut self, _name: &str, _args: Value) -> Value {
                success(json!({}))
            }
            fn workspace(&self) -> Option<PathBuf> {
                Some(self.0.clone())
            }
        }

        let session = format!("test-{}", uuid::Uuid::new_v4().simple());
        let mut exec = WsExec(dir.path().to_path_buf());
        let code = r#"
            const before = await fs.read('sample.dart');
            await fs.replace('sample.dart', "const x = '${idx}';", "const x = '${idx + 1}';");
            await fs.write('sub/out.txt', 'ok');
            const after = await fs.read('sample.dart');
            const entries = await fs.list('.');
            const hasSub = await fs.exists('sub/out.txt');
            return { len: before.length, changed: after.includes('${idx + 1}'), hasSub, entries };
        "#;
        let out = execute(
            "code_run",
            &json!({"code": code}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(out["data"]["status"], "completed", "{out}");
        assert_eq!(out["data"]["result"]["len"], big.len());
        assert_eq!(out["data"]["result"]["changed"], true);
        assert_eq!(out["data"]["result"]["hasSub"], true);
        assert_eq!(
            out["data"]["result"]["entries"],
            json!(["sample.dart", "sub/"])
        );
        // Slim output: empty logs and zero replays are omitted.
        assert!(out["data"].get("logs").is_none(), "{out}");
        assert!(out["data"].get("replayed").is_none(), "{out}");
        let _ = std::fs::remove_dir_all(runs_dir(&session));
    }

    #[tokio::test]
    async fn promise_all_runs_concurrent_inner_calls_and_replays_deterministically() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ParallelExec {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl InnerCalls for ParallelExec {
            async fn call(&mut self, _name: &str, _args: Value) -> Value {
                unreachable!("concurrent path should be used")
            }
            fn call_concurrent(&self, _name: &str, args: Value) -> Option<CallFuture> {
                let calls = self.calls.clone();
                Some(Box::pin(async move {
                    let delay = args["delay"].as_u64().unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    calls.fetch_add(1, Ordering::SeqCst);
                    success(json!({"label": args["label"]}))
                }))
            }
        }

        let session = format!("test-{}", uuid::Uuid::new_v4().simple());
        let counter = Arc::new(AtomicUsize::new(0));
        let mut exec = ParallelExec {
            calls: counter.clone(),
        };
        let code = r#"
            const [a, b, c] = await Promise.all([
                bash({label: 'first', delay: 180}),
                bash({label: 'second', delay: 40}),
                bash({label: 'third', delay: 120}),
            ]);
            const ans = await agent.ask('ok?', [a.data.label, b.data.label, c.data.label]);
            return { labels: [a.data.label, b.data.label, c.data.label], ans };
        "#;
        let t0 = std::time::Instant::now();
        let paused = execute(
            "code_run",
            &json!({"code": code}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        let elapsed = t0.elapsed();
        assert_eq!(paused["data"]["status"], "paused", "{paused}");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        // Sequential would take >= 340ms; concurrent finishes near max(180, 40, 120) = 180ms.
        assert!(
            elapsed < Duration::from_millis(310),
            "expected concurrent execution, took {elapsed:?}"
        );

        let run_id = paused["data"]["run_id"].as_str().unwrap().to_string();
        let done = execute(
            "code_resume",
            &json!({"run_id": run_id, "answer": "yes"}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(done["data"]["status"], "completed", "{done}");
        assert_eq!(done["data"]["replayed"], 3);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert_eq!(
            done["data"]["result"],
            json!({"labels": ["first", "second", "third"], "ans": "yes"})
        );
        let _ = std::fs::remove_dir_all(runs_dir(&session));
    }

    #[tokio::test]
    async fn inner_argument_errors_attach_tool_signature() {
        struct ErrExec;
        #[async_trait::async_trait]
        impl InnerCalls for ErrExec {
            async fn call(&mut self, _name: &str, _args: Value) -> Value {
                failure("tool_error", "`command` is required".into(), None)
            }
        }

        let session = format!("test-{}", uuid::Uuid::new_v4().simple());
        let mut exec = ErrExec;
        let out = execute(
            "code_run",
            &json!({"code": "return await bash({});"}),
            &session,
            &defs(),
            &mut exec,
        )
        .await;
        assert_eq!(out["data"]["status"], "completed", "{out}");
        let sig = out["data"]["result"]["error"]["signature"]
            .as_str()
            .expect("signature attached on missing required arg");
        assert!(sig.contains("bash(args: {"), "{sig}");
        assert!(sig.contains("command: string;"), "{sig}");
        let _ = std::fs::remove_dir_all(runs_dir(&session));
    }

    #[test]
    fn switch_notice_lists_direct_tools_in_code_mode() {
        let notice = switch_notice(crate::config::ToolMode::Code, &defs());
        assert!(notice.contains("code mode"), "{notice}");
        assert!(notice.contains("bash"), "{notice}");
        let direct = switch_notice(crate::config::ToolMode::Direct, &[]);
        assert!(direct.contains("direct mode"), "{direct}");
    }
}
