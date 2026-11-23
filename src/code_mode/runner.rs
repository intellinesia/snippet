use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
            json!({"code": {"type": "string", "description": "Body of an async function. Every other tool is a function: `await task.update({...})` for a grouped tool's action, `await bash({command})` for a plain tool. `return` the value you want back; console.log lines come back too. Each call returns the same JSON it does when called directly, and a failed call returns its error rather than throwing, so check `status`. Tools that end your turn are called directly, not from a script. `await agent.ask(question, data)` pauses the run and hands you the question and data to decide; `await view_image({path})` or `await agent.look(path, question)` pauses and shows you the image (a screenshot the script took, say). code.resume continues the script; your answer, if any, is what the call returns."}}),
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

/// Globals for a script: grouped tools with their actions, and plain tools.
pub fn globals(advertised: &[NativeToolDefinition]) -> Globals {
    let mut surfaces = Vec::new();
    let mut plain = Vec::new();
    for def in advertised {
        if DENIED.contains(&def.name.as_str()) {
            continue;
        }
        match actions_of(def) {
            Some(actions) => surfaces.push((def.name.clone(), actions)),
            None => plain.push(def.name.clone()),
        }
    }
    Globals { surfaces, plain }
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

async fn drive(
    mut record: RunRecord,
    earlier: Vec<RecordedCall>,
    session: &str,
    advertised: &[NativeToolDefinition],
    exec: &mut dyn InnerCalls,
) -> Value {
    let limits = Limits {
        timeout: Duration::from_secs(150),
        memory_bytes: 64 * 1024 * 1024,
        max_log_chars: 8_000,
    };
    let (mut rx, handle) = sandbox::spawn(record.code.clone(), globals(advertised), limits);
    let mut replayed = 0usize;
    let mut diverged = false;
    let mut asked = 0usize;
    while let Some(request) = rx.recv().await {
        match request {
            HostRequest::Call { name, args, reply } => {
                let index = record.calls.len();
                let result = if DENIED.contains(&name.as_str()) {
                    failure(
                        "not_in_code",
                        format!("`{name}` cannot run inside code; call it directly."),
                        None,
                    )
                } else if !diverged
                    && earlier
                        .get(index)
                        .is_some_and(|c| c.name == name && c.args == args)
                {
                    replayed += 1;
                    earlier[index].result.clone()
                } else {
                    diverged = true;
                    exec.call(&name, args.clone()).await
                };
                let _ = reply.send(result.clone());
                record.calls.push(RecordedCall { name, args, result });
            }
            HostRequest::Ask { reply, .. } => {
                let answer = record.answers.get(asked).cloned();
                asked += 1;
                let _ = reply.send(answer);
            }
        }
    }
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
            success(
                json!({"status": "completed", "run_id": record.run_id, "result": capped(value), "logs": logs, "calls": calls, "replayed": replayed}),
            )
        }
        Outcome::Paused {
            question,
            data,
            logs,
        } if data.get("__look").is_some_and(Value::is_string) => {
            record.status = "paused".into();
            success(json!({
                "status": "paused",
                "run_id": record.run_id,
                "question": question,
                "path": data["__look"],
                "logs": logs,
                "calls": calls,
                "replayed": replayed,
                "next": "Look at the image, then code.resume with this run_id and your answer."
            }))
        }
        Outcome::Paused {
            question,
            data,
            logs,
        } => {
            record.status = "paused".into();
            success(json!({
                "status": "paused",
                "run_id": record.run_id,
                "question": question,
                "data": capped(data),
                "logs": logs,
                "calls": calls,
                "replayed": replayed,
                "next": "Decide, then code.resume with this run_id and your answer."
            }))
        }
        Outcome::Failed { message, logs } => {
            record.status = "failed".into();
            failure(
                "script_error",
                message,
                Some(
                    json!({"run_id": record.run_id, "logs": logs, "calls": calls, "next": "Fix the code and code.resume with this run_id and `code`; calls that match reuse their results."}),
                ),
            )
        }
    };
    save(session, &record);
    result
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
    "view_image",
    "ask_user",
    "complete_goal",
    "update_plan",
    "speak",
    "custom_tool",
];

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
}
