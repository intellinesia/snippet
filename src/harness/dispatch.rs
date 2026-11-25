use super::*;

pub(super) fn tool_activity_text(name: &str, args: &Value) -> String {
    let lowered = crate::surface::lower(name, args).ok().flatten();
    let (name, args) = match lowered.as_ref() {
        Some((n, a)) => (n.as_str(), a),
        None => (name, args),
    };
    let arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let base = |p: &str| {
        p.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(p)
            .to_string()
    };
    let detail = match name {
        "change_files" => {
            let paths: Vec<String> = args
                .get("changes")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .filter_map(|c| c.get("path").and_then(Value::as_str))
                        .map(base)
                        .collect()
                })
                .unwrap_or_default();
            match paths.as_slice() {
                [] => None,
                [one] => Some(format!("Editing {one}")),
                [first, rest @ ..] if rest.iter().all(|p| p == first) => {
                    Some(format!("Editing {first}"))
                }
                many => Some(format!("Editing {} files", many.len())),
            }
        }
        "view_image" => arg("path").map(|p| format!("Viewing {}", base(p))),
        "web_search" => arg("query").map(|q| format!("Searching: {q}")),
        "web_read" => arg("url").map(|u| {
            let host = u
                .split("://")
                .nth(1)
                .unwrap_or(u)
                .split('/')
                .next()
                .unwrap_or(u);
            format!("Reading {host}")
        }),
        "skill" => arg("name").map(|n| format!("Loading skill {n}")),
        "delegate_task" => arg("title").map(|t| format!("Delegating: {t}")),
        "update_plan" | "terminate_loop" => None,
        "Bash" => arg("description").or(arg("command")).map(str::to_string),
        "Read" => arg("file_path").map(|p| format!("Reading {}", base(p))),
        "Edit" | "MultiEdit" | "Write" => arg("file_path").map(|p| format!("Editing {}", base(p))),
        "Grep" => arg("pattern").map(|q| format!("Searching: {q}")),
        "Glob" => arg("pattern").map(|q| format!("Finding {q}")),
        "WebSearch" => arg("query").map(|q| format!("Searching: {q}")),
        "WebFetch" => arg("url").map(|u| {
            let host = u
                .split("://")
                .nth(1)
                .unwrap_or(u)
                .split('/')
                .next()
                .unwrap_or(u);
            format!("Reading {host}")
        }),
        "Task" => arg("description").map(|t| format!("Delegating: {t}")),
        _ => arg("label").map(str::to_string),
    };
    if let Some(d) = detail {
        let mut d = d.replace('\n', " ");
        if d.chars().count() > 90 {
            d = d.chars().take(89).collect::<String>() + "…";
        }
        return d;
    }
    match name {
        "bash" | "Bash" => "Running command".into(),
        "change_files" => "Editing files".into(),
        "view_image" => "Viewing image".into(),
        "canvas_read" => "Reading canvas".into(),
        "canvas_apply" => "Updating canvas".into(),
        "canvas_thumbnail" => "Rendering canvas frame".into(),
        "canvas_pins" => "Managing canvas pins".into(),
        "canvas_create" => "Creating canvas".into(),
        "canvas_list" => "Listing canvases".into(),
        "web_search" => "Searching the web".into(),
        "web_read" => "Reading web page".into(),
        "search_skills" => "Finding skills".into(),
        "skill" => "Loading skill".into(),
        "manage_process" => "Managing background process".into(),
        "delegate_task" => "Delegating task".into(),
        "update_plan" => "Updating plan".into(),
        "terminate_loop" => "Finishing task".into(),
        _ => format!("Running {name}"),
    }
}

#[cfg(test)]
#[test]
fn tool_activity_uses_precise_labels_and_preserves_unknown_names() {
    let none = Value::Null;
    for (name, expected) in [
        ("bash", "Running command"),
        ("change_files", "Editing files"),
        ("view_image", "Viewing image"),
        ("web_search", "Searching the web"),
        ("web_read", "Reading web page"),
        ("search_skills", "Finding skills"),
        ("skill", "Loading skill"),
        ("manage_process", "Managing background process"),
        ("delegate_task", "Delegating task"),
        ("update_plan", "Updating plan"),
        ("terminate_loop", "Finishing task"),
        ("custom_tool", "Running custom_tool"),
    ] {
        assert_eq!(tool_activity_text(name, &none), expected);
    }
    let j = |v: &str| serde_json::from_str::<Value>(v).unwrap();
    assert_eq!(
        tool_activity_text("bash", &j(r#"{"label":"Read listener.rs"}"#)),
        "Read listener.rs"
    );
    assert_eq!(
        tool_activity_text("bash", &j(r#"{"label":"  "}"#)),
        "Running command"
    );
    assert_eq!(
        tool_activity_text(
            "change_files",
            &j(
                r#"{"changes":[{"path":"lib/a/transcript.dart"},{"path":"lib/a/transcript.dart"}]}"#
            )
        ),
        "Editing transcript.dart"
    );
    assert_eq!(
        tool_activity_text(
            "change_files",
            &j(r#"{"changes":[{"path":"a.rs"},{"path":"b.rs"}]}"#)
        ),
        "Editing 2 files"
    );
    assert_eq!(
        tool_activity_text("web_read", &j(r#"{"url":"https://docs.rs/tokio/latest"}"#)),
        "Reading docs.rs"
    );
    assert_eq!(
        tool_activity_text("update_plan", &j(r#"{"label":"x"}"#)),
        "Updating plan"
    );
    let long = "x".repeat(200);
    assert_eq!(
        tool_activity_text("bash", &serde_json::json!({ "label": long }))
            .chars()
            .count(),
        90
    );
}

/// Read-only tools with no side effects on the workspace or the session, so a
/// run of them can execute concurrently. Results are still recorded in call
/// order, one ToolCall/ToolResult pair at a time, which is what the clients pair
/// on.
const PARALLEL_SAFE_TOOLS: [&str; 3] = ["view_image", "web_search", "web_read"];

fn scrub_vault(result: &mut Value) {
    let vault = crate::vault::Vault::load();
    if !vault.is_empty() {
        vault.scrub_value(result);
    }
}

/// Rerunning a command that prints exactly what it printed earlier in this
/// request tells the model nothing new, so it gets a short notice instead of the
/// same output again. Short outputs are left alone: they cost little and a
/// notice there would read oddly.
fn collapse_repeated_output(vars: &mut LoopVars, command: String, result: &mut Value) {
    use std::hash::{Hash, Hasher};
    const MIN_CHARS: usize = 300;
    let Some(data) = result.get_mut("data").and_then(Value::as_object_mut) else {
        return;
    };
    let log_path = data
        .get("saved_output_path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let text = |key: &str| {
        let text = data.get(key).and_then(Value::as_str).unwrap_or("");
        if log_path.is_empty() {
            text.to_string()
        } else {
            text.replace(&log_path, "")
        }
    };
    let (stdout, stderr) = (text("stdout"), text("stderr"));
    if stdout.len() + stderr.len() < MIN_CHARS {
        return;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        &stdout,
        &stderr,
        data.get("exit_code").map(Value::to_string),
    )
        .hash(&mut hasher);
    let fingerprint = hasher.finish();
    if vars.bash_outputs.insert(command, fingerprint) == Some(fingerprint) {
        for key in [
            "stdout",
            "stderr",
            "hint",
            "truncated",
            "total_lines",
            "total_bytes",
        ] {
            data.remove(key);
        }
        data.insert(
            "note".to_string(),
            Value::String(
                "Identical to the output of this exact command earlier in this task; nothing changed. Use that result instead of rerunning it."
                    .to_string(),
            ),
        );
    }
}

fn is_error_result(result: &Value) -> bool {
    result.get("status").and_then(Value::as_str) == Some("error")
}

fn speak_asks(call: &GeneratedToolCall) -> bool {
    (call.tool_name == "speak" || call.tool_name.ends_with("__speak"))
        && crate::speech::asks_aloud(
            call.arguments
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or(""),
            call.arguments.get("expecting").and_then(Value::as_str),
        )
}

fn last_result_ok(state: &HarnessState) -> bool {
    matches!(state.messages.last(), Some(HarnessMessage::ToolResult { content, .. }) if !is_error_result(content))
}

fn execution_error(error: ToolError) -> Value {
    json!({
        "schema_version": 1,
        "status": "error",
        "error": {
            "code": "tool_execution_error",
            "message": error.to_string(),
        }
    })
}

pub(super) fn enrich_error_result(
    mut result: Value,
    surface_name: &str,
    internal_name: &str,
    uses_op: bool,
    internal_schema: Option<&Value>,
    force_arg_error: bool,
) -> Value {
    let Some(msg) = result
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return result;
    };
    if !force_arg_error && !crate::surface::looks_like_argument_error(&msg) {
        return result;
    }
    let enriched = crate::surface::format_argument_error(
        &msg,
        surface_name,
        internal_name,
        uses_op,
        internal_schema,
    );
    if let Some(err_obj) = result.get_mut("error").and_then(Value::as_object_mut) {
        err_obj.insert("message".to_string(), Value::String(enriched));
    }
    result
}

/// Answer a call in both logs: the event the clients render and the message the
/// model sees. Every call must be answered — an unanswered tool_call_id makes
/// strict providers reject the whole history.
fn answer_call(state: &mut HarnessState, tool_name: &str, call_id: &str, result: Value) {
    state.events.push(HarnessEvent::ToolResult {
        tool_name: tool_name.to_string(),
        result: result.clone(),
    });
    state.messages.push(HarnessMessage::ToolResult {
        tool_call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        content: result,
    });
}

pub(super) enum Executed {
    Answer(Value),
    Invalid { result: Value, error: String },
    EndTurn { result: Value, step: StepResult },
}

impl CodingHarness {
    /// Execute one turn's tool calls. `Err` ends the turn immediately
    /// (`terminate_loop`, `ask_user`); `Ok` carries the turn's productivity.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_tool_calls(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        sink: Option<&StreamHandle>,
        approval_rx: &mut mpsc::UnboundedReceiver<(ApprovalDecision, Option<DecisionSource>)>,
        calls: Vec<GeneratedToolCall>,
        definitions: &[NativeToolDefinition],
    ) -> Result<TurnStats, StepResult> {
        let mut stats = TurnStats::default();
        // Manual mode: count mutating calls up front so each approval prompt can
        // show "action N of M".
        let total_mutating = calls
            .iter()
            .filter(|c| {
                let resolved = crate::surface::resolve(&c.tool_name, &c.arguments);
                MUTATING_TOOLS.contains(&resolved.as_str())
            })
            .count();
        let mut approval_index = 0usize;

        let mut i = 0;
        while i < calls.len() {
            let end = self.parallel_run_end(&calls, i, conversation_mode, definitions);
            if end > i + 1 {
                self.run_parallel_reads(state, lanes, &calls[i..end], definitions, &mut stats)
                    .await;
                i = end;
                continue;
            }
            let call = calls[i].clone();
            i += 1;
            let asks_aloud = conversation_mode && i == calls.len() && speak_asks(&call);
            if let Some(end_turn) = self
                .run_call(
                    state,
                    lanes,
                    watches,
                    vars,
                    conversation_mode,
                    sink,
                    approval_rx,
                    call,
                    definitions,
                    &mut stats,
                    total_mutating,
                    &mut approval_index,
                )
                .await
            {
                return Err(end_turn);
            }
            if asks_aloud && last_result_ok(state) {
                return Err(StepResult::TurnEnded {
                    kind: TurnEndKind::Complete,
                    final_text: None,
                    closing: None,
                });
            }
        }
        Ok(stats)
    }

    fn is_parallel_safe(
        &self,
        call: &GeneratedToolCall,
        conversation_mode: bool,
        definitions: &[NativeToolDefinition],
    ) -> bool {
        let Some(advertised) = definitions.iter().find(|d| d.name == call.tool_name) else {
            return false;
        };
        let Ok(lowered) =
            crate::surface::lower_for_advertised(&call.tool_name, &call.arguments, advertised)
        else {
            return false;
        };
        let internal = lowered
            .as_ref()
            .map(|(n, _)| n.as_str())
            .unwrap_or(call.tool_name.as_str());
        PARALLEL_SAFE_TOOLS.contains(&internal)
            && !(conversation_mode && meta::is_meta_tool(internal))
            && self.tools.contains(internal)
    }

    /// End (exclusive) of the run of parallel-safe calls starting at `start`.
    fn parallel_run_end(
        &self,
        calls: &[GeneratedToolCall],
        start: usize,
        conversation_mode: bool,
        definitions: &[NativeToolDefinition],
    ) -> usize {
        let mut end = start;
        while end < calls.len()
            && self.is_parallel_safe(&calls[end], conversation_mode, definitions)
        {
            end += 1;
        }
        end
    }

    /// A run of read-only calls: they execute concurrently, and every pair is
    /// recorded in call order.
    async fn run_parallel_reads(
        &self,
        state: &mut HarnessState,
        lanes: &LaneManager,
        calls: &[GeneratedToolCall],
        definitions: &[NativeToolDefinition],
        stats: &mut TurnStats,
    ) {
        stats.only_delegations = false;
        let lowered_calls: Vec<(String, Value, bool)> = calls
            .iter()
            .map(|call| {
                let uses_op = crate::surface::uses_op(&call.tool_name, &call.arguments);
                let advertised = definitions.iter().find(|d| d.name == call.tool_name);
                let lowered = advertised.and_then(|adv| {
                    crate::surface::lower_for_advertised(&call.tool_name, &call.arguments, adv)
                        .ok()
                        .flatten()
                });
                match lowered {
                    Some((name, args)) => (name, args, uses_op),
                    None => (call.tool_name.clone(), call.arguments.clone(), false),
                }
            })
            .collect();
        for (call, (internal_name, internal_args, _)) in calls.iter().zip(&lowered_calls) {
            self.lane_progress(
                "tool_call",
                tool_activity_text(internal_name, internal_args),
            );
            self.presence(
                "tool",
                &super::presence::tool_detail(internal_name, internal_args),
            );
            let _ = call;
        }
        let results = futures_util::future::join_all(
            lowered_calls
                .iter()
                .map(|(name, args, _)| self.tools.execute(&self.context, name, args.clone())),
        )
        .await;
        for ((call, (internal_name, _, uses_op)), outcome) in
            calls.iter().zip(&lowered_calls).zip(results)
        {
            let call_id = call.id.clone().unwrap_or_default();
            state.events.push(HarnessEvent::ToolCall {
                tool_name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
            });
            stats.real_work += 1;
            let mut result = match outcome {
                Ok(result) => {
                    if is_error_result(&result.value) {
                        let schema = self.internal_tool_schema(internal_name);
                        enrich_error_result(
                            result.value,
                            &call.tool_name,
                            internal_name,
                            *uses_op,
                            schema.as_ref(),
                            false,
                        )
                    } else {
                        result.value
                    }
                }
                Err(error) => {
                    let force = matches!(
                        error,
                        ToolError::Json(_) | ToolError::InvalidArguments { .. }
                    );
                    let schema = self.internal_tool_schema(internal_name);
                    enrich_error_result(
                        execution_error(error),
                        &call.tool_name,
                        internal_name,
                        *uses_op,
                        schema.as_ref(),
                        force,
                    )
                }
            };
            scrub_vault(&mut result);
            if is_error_result(&result) {
                stats.failed += 1;
            }
            answer_call(state, &call.tool_name, &call_id, result);
        }
        let _ = self.persist(state, lanes).await;
    }

    /// One call through the full policy: headless completion, meta tools,
    /// unknown tools, shell discipline, vault gating, manual
    /// approval, execution. `Some` ends the turn.
    #[allow(clippy::too_many_arguments)]
    async fn run_call(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        sink: Option<&StreamHandle>,
        approval_rx: &mut mpsc::UnboundedReceiver<(ApprovalDecision, Option<DecisionSource>)>,
        call: GeneratedToolCall,
        definitions: &[NativeToolDefinition],
        stats: &mut TurnStats,
        total_mutating: usize,
        approval_index: &mut usize,
    ) -> Option<StepResult> {
        let surface_name = call.tool_name.clone();
        let surface_args = call.arguments.clone();
        let call_id = call.id.clone().unwrap_or_default();
        state.events.push(HarnessEvent::ToolCall {
            tool_name: surface_name.clone(),
            arguments: surface_args.clone(),
        });
        self.lane_progress(
            "tool_call",
            tool_activity_text(&surface_name, &surface_args),
        );
        self.presence(
            "tool",
            &super::presence::tool_detail(&surface_name, &surface_args),
        );

        // Headless explicit completion: a lane / one-shot run ends with a
        // structured `summary` (folded back into the caller). Not advertised to
        // the conversation agent, which finishes by replying with no tool calls.
        if surface_name == "terminate_loop" {
            stats.only_delegations = false;
            let summary = surface_args
                .get("summary")
                .or_else(|| surface_args.get("message"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let closing = summary.as_ref().map(|s| {
                let mut data = match surface_args.as_object() {
                    Some(map) => map.clone(),
                    None => serde_json::Map::new(),
                };
                data.insert("summary".to_string(), Value::String(s.clone()));
                Value::Object(data)
            });
            let result = match &closing {
                Some(data) => {
                    json!({"schema_version": 1, "status": "success", "data": data})
                }
                None => tool_error(
                    "`terminate_loop` requires a non-empty `summary` of what you did and found.",
                ),
            };
            answer_call(state, &surface_name, &call_id, result);
            if let Some(s) = summary {
                if conversation_mode && !replied_since_last_user(&state.events) {
                    record_assistant_text(state, s.clone(), None);
                    if let Some(sink) = sink {
                        StreamBuffer::clear(sink);
                    }
                }
                let _ = self.persist(state, lanes).await;
                return Some(StepResult::TurnEnded {
                    kind: TurnEndKind::Complete,
                    final_text: Some(s),
                    closing,
                });
            }
            let _ = self.persist(state, lanes).await;
            return None;
        }

        match self
            .execute_call(
                state,
                lanes,
                watches,
                vars,
                conversation_mode,
                approval_rx,
                &surface_name,
                &surface_args,
                definitions,
                stats,
                total_mutating,
                approval_index,
                false,
            )
            .await
        {
            Executed::Answer(result) => answer_call(state, &surface_name, &call_id, result),
            Executed::Invalid { result, error } => {
                state.events.push(HarnessEvent::InvalidToolCall {
                    tool_name: surface_name.clone(),
                    error,
                });
                state.messages.push(HarnessMessage::ToolResult {
                    tool_call_id: call_id,
                    tool_name: surface_name,
                    content: result,
                });
            }
            Executed::EndTurn { result, step } => {
                answer_call(state, &surface_name, &call_id, result);
                let _ = self.persist(state, lanes).await;
                return Some(step);
            }
        }
        // Flush after every tool result so a mid-batch kill still keeps completed
        // calls on disk.
        let _ = self.persist(state, lanes).await;
        None
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_call(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        approval_rx: &mut mpsc::UnboundedReceiver<(ApprovalDecision, Option<DecisionSource>)>,
        surface_name: &str,
        surface_args: &Value,
        definitions: &[NativeToolDefinition],
        stats: &mut TurnStats,
        total_mutating: usize,
        approval_index: &mut usize,
        in_code: bool,
    ) -> Executed {
        let surface_name = surface_name.to_string();
        let surface_args = surface_args.clone();
        let Some(advertised) = definitions.iter().find(|d| d.name == surface_name) else {
            stats.only_delegations = false;
            let available = definitions
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            vars.pending_signals.push(RuntimeSignal::UnknownTool {
                name: surface_name.clone(),
                available: available.clone(),
            });
            let error = format!("Unknown tool `{surface_name}`. Available tools: {available}");
            let result = json!({
                "schema_version": 1,
                "status": "error",
                "error": {"code": "unknown_tool", "message": error},
            });
            return Executed::Invalid { result, error };
        };

        let uses_op = crate::surface::uses_op(&surface_name, &surface_args);
        let (tool_name, args) =
            match crate::surface::lower_for_advertised(&surface_name, &surface_args, advertised) {
                Ok(Some((internal_name, internal_args))) => (internal_name, internal_args),
                Ok(None) => (surface_name.clone(), surface_args.clone()),
                Err(message) => {
                    stats.only_delegations = false;
                    stats.failed += 1;
                    return Executed::Answer(tool_error(message));
                }
            };

        if tool_name != "delegate_task" {
            stats.only_delegations = false;
        }

        if crate::code_mode::runner::is_code_tool(&tool_name) {
            if in_code {
                return Executed::Answer(tool_error(format!(
                    "`{surface_name}` cannot run inside code."
                )));
            }
            let result = self
                .run_code_tool(
                    state,
                    lanes,
                    watches,
                    vars,
                    conversation_mode,
                    approval_rx,
                    &tool_name,
                    &args,
                    stats,
                    total_mutating,
                    approval_index,
                )
                .await;
            if is_error_result(&result) {
                stats.failed += 1;
            }
            return Executed::Answer(result);
        }

        let is_meta = conversation_mode
            && meta::is_meta_tool(&tool_name)
            && (self.config.allow_lane_control
                || !matches!(
                    tool_name.as_str(),
                    "delegate_task" | "cancel_delegated_task"
                ));
        if is_meta {
            let (mut result, control) =
                self.dispatch_meta(state, lanes, watches, &tool_name, &args);
            if is_error_result(&result) {
                let schema = self.internal_tool_schema(&tool_name);
                result = enrich_error_result(
                    result,
                    &surface_name,
                    &tool_name,
                    uses_op,
                    schema.as_ref(),
                    false,
                );
            }
            if tool_name == "update_plan" && !is_error_result(&result) {
                stats.had_plan = true;
                vars.turns_since_plan = 0;
                vars.reminded.insert("plan", render_plan(&state.plan));
            }
            if tool_name == "delegate_task" {
                if result.get("status").and_then(Value::as_str) == Some("success") {
                    stats.delegations_ok += 1;
                } else {
                    stats.only_delegations = false;
                }
            }
            if let MetaControl::EndTurn { kind, final_text } = control {
                if in_code {
                    return Executed::Answer(tool_error(format!(
                        "`{surface_name}` ends the turn, so it cannot run inside code; call it directly."
                    )));
                }
                return Executed::EndTurn {
                    result,
                    step: StepResult::TurnEnded {
                        kind,
                        final_text,
                        closing: None,
                    },
                };
            }
            return Executed::Answer(result);
        }

        if !self.tools.contains(&tool_name) {
            let available = definitions
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            vars.pending_signals.push(RuntimeSignal::UnknownTool {
                name: surface_name.clone(),
                available: available.clone(),
            });
            let error = format!("Unknown tool `{surface_name}`. Available tools: {available}");
            let result = json!({
                "schema_version": 1,
                "status": "error",
                "error": {"code": "unknown_tool", "message": error},
            });
            return Executed::Invalid { result, error };
        }

        stats.real_work += 1;

        if tool_name == "bash"
            && let Some(command) = args.get("command").and_then(Value::as_str)
            && note_shell_command(vars, command)
        {
            stats.shell_nudged = true;
        }

        let custom = self
            .tools
            .custom_dir()
            .filter(|_| self.tools.is_custom(&tool_name))
            .and_then(|dir| crate::agent_tools::prepare(dir, &tool_name, &args))
            .and_then(Result::ok);
        let vault_secrets_used: Vec<String> = if tool_name == "bash" {
            args.get("command")
                .and_then(Value::as_str)
                .map(|c| crate::vault::Vault::load().referenced_names(c))
                .unwrap_or_default()
        } else if let Some(custom) = custom.as_ref() {
            custom.vault.clone()
        } else {
            Vec::new()
        };
        let first_run = custom.as_ref().is_some_and(|c| c.needs_first_approval);
        let mut lane_approved = false;
        if !conversation_mode
            && (first_run || !vault_secrets_used.is_empty())
            && let Some(route) = self.config.lane_approval.as_ref()
        {
            let summary = match custom.as_ref() {
                Some(custom) => crate::agent_tools::approval_summary(custom),
                None => format!(
                    "⚠ uses vault secret(s) [{}] — {}",
                    vault_secrets_used.join(", "),
                    approval_summary(&tool_name, &args)
                ),
            };
            if !route.ask(&tool_name, &summary).await {
                let result = json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {
                        "code": "user_denied",
                        "message": "The user denied this action. Retrying it as-is would just meet the same answer, so adjust your approach, or report what you needed."
                    }
                });
                return Executed::Answer(result);
            }
            if let (Some(custom), Some(dir)) = (custom.as_ref(), self.tools.custom_dir())
                && custom.needs_first_approval
            {
                crate::agent_tools::approve(dir, &custom.tool);
            }
            lane_approved = true;
        }
        if first_run && !conversation_mode && !lane_approved {
            let result = json!({
                "schema_version": 1,
                "status": "error",
                "error": {
                    "code": "custom_tool_needs_approval",
                    "message": format!("The custom tool `{tool_name}` is new and needs the user's approval before its first run, which isn't available in a delegated/headless run. Report that it needs approving, so it's run once on the main thread.")
                }
            });
            return Executed::Answer(result);
        }
        if !vault_secrets_used.is_empty() && !conversation_mode && !lane_approved {
            let result = json!({
                "schema_version": 1,
                "status": "error",
                "error": {
                    "code": "vault_needs_confirmation",
                    "message": format!(
                        "Using vault secret(s) [{}] requires user confirmation, which isn't available in a delegated/headless run. Report that this step needs the secret, so it's done on the main thread where the user can approve it.",
                        vault_secrets_used.join(", ")
                    )
                }
            });
            return Executed::Answer(result);
        }
        let force_vault_approval = !vault_secrets_used.is_empty() || first_run;
        let manual_gated = state.approval_mode == ApprovalMode::Manual
            && (MUTATING_TOOLS.contains(&tool_name.as_str()) || custom.is_some());
        let rule_eligible = !lane_approved && !force_vault_approval && manual_gated;

        if rule_eligible
            && let Some(label) = super::allow_rules::covering(
                &state.allow_rules,
                &tool_name,
                &args,
                custom.is_some(),
            )
        {
            let summary = match custom.as_ref() {
                Some(custom) => crate::agent_tools::approval_summary(custom),
                None => approval_summary(&tool_name, &args),
            };
            Self::record_rule_decision(state, &tool_name, summary, label);
        } else if !lane_approved && (force_vault_approval || manual_gated) {
            *approval_index += 1;
            while approval_rx.try_recv().is_ok() {}
            let summary = match custom.as_ref() {
                Some(custom) => crate::agent_tools::approval_summary(custom),
                None if force_vault_approval => format!(
                    "⚠ uses vault secret(s) [{}] — {}",
                    vault_secrets_used.join(", "),
                    approval_summary(&tool_name, &args)
                ),
                None => approval_summary(&tool_name, &args),
            };
            let rule = rule_eligible
                .then(|| super::allow_rules::offer(&tool_name, &args, custom.is_some()))
                .flatten();
            state.events.push(HarnessEvent::ApprovalRequest {
                tool_name: tool_name.clone(),
                summary,
                index: *approval_index,
                total: total_mutating.max(*approval_index),
                title: super::allow_rules::title(&tool_name, &args),
                reason: super::allow_rules::reason(&args),
                rule,
            });
            state.status = HarnessStatus::WaitingForInput;
            let _ = self.persist(state, lanes).await;
            let received = approval_rx.recv().await;
            state.status = HarnessStatus::Running;
            let (decision, source) = match received {
                Some((decision, source)) => (Some(decision), source),
                None => (None, None),
            };
            let (by, via) = match source {
                Some(src) => (src.by, src.via),
                None => ("human".to_string(), "app".to_string()),
            };
            let outcome = match decision {
                Some(ApprovalDecision::Approve | ApprovalDecision::ApproveRule) => "approved",
                Some(ApprovalDecision::ApproveAll) => "approved_all",
                Some(ApprovalDecision::Deny) => "denied",
                None => "outdated",
            };
            let granted = matches!(decision, Some(ApprovalDecision::ApproveRule))
                .then(|| Self::grant_pending_rule(state))
                .flatten();
            self.record_decision_with_rule(state, "approval", outcome, None, &by, &via, granted);
            if matches!(decision, Some(ApprovalDecision::ApproveAll)) && !force_vault_approval {
                state.approval_mode = ApprovalMode::Auto;
            }
            let approved = matches!(
                decision,
                Some(
                    ApprovalDecision::Approve
                        | ApprovalDecision::ApproveAll
                        | ApprovalDecision::ApproveRule
                )
            );
            if approved
                && let (Some(custom), Some(dir)) = (custom.as_ref(), self.tools.custom_dir())
                && custom.needs_first_approval
            {
                crate::agent_tools::approve(dir, &custom.tool);
            }
            if !approved {
                let result = json!({
                    "schema_version": 1,
                    "status": "error",
                    "error": {
                        "code": "user_denied",
                        "message": "The user denied this action. Retrying it as-is would just meet the same answer, so adjust your approach or ask what they'd prefer."
                    }
                });
                return Executed::Answer(result);
            }
        }

        let _ = self.persist(state, lanes).await;

        let edit_path = (tool_name == "change_files")
            .then(|| {
                args.pointer("/changes/0/path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let bash_command = (tool_name == "bash")
            .then(|| {
                args.get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .flatten();
        let background = args.get("background").and_then(Value::as_bool) == Some(true);
        let change_args = (tool_name == "change_files").then(|| args.clone());
        let cwd_before = self.context.current_dir();
        let mut result = match self.tools.execute(&self.context, &tool_name, args).await {
            Ok(result) => {
                if is_error_result(&result.value) {
                    let schema = self.internal_tool_schema(&tool_name);
                    enrich_error_result(
                        result.value,
                        &surface_name,
                        &tool_name,
                        uses_op,
                        schema.as_ref(),
                        false,
                    )
                } else {
                    result.value
                }
            }
            Err(error) => {
                let force = matches!(
                    error,
                    ToolError::Json(_) | ToolError::InvalidArguments { .. }
                );
                let schema = self.internal_tool_schema(&tool_name);
                enrich_error_result(
                    execution_error(error),
                    &surface_name,
                    &tool_name,
                    uses_op,
                    schema.as_ref(),
                    force,
                )
            }
        };
        scrub_vault(&mut result);
        let is_err = is_error_result(&result);
        if is_err {
            stats.failed += 1;
        }
        if tool_name == "change_files" {
            note_edit_result(vars, edit_path, is_err);
            if let Some(arguments) = change_args.as_ref().filter(|_| !is_err) {
                note_file_change(vars, arguments);
            }
        }
        if let Some(command) = bash_command
            && !is_err
            && !background
        {
            note_bash_command(vars, &command, &cwd_before);
            collapse_repeated_output(vars, command, &mut result);
        }
        Executed::Answer(result)
    }
}

struct NativeExec<'a> {
    harness: &'a CodingHarness,
    state: &'a mut HarnessState,
    lanes: &'a mut LaneManager,
    watches: &'a mut WatchManager,
    vars: &'a mut LoopVars,
    conversation_mode: bool,
    approval_rx: &'a mut mpsc::UnboundedReceiver<(ApprovalDecision, Option<DecisionSource>)>,
    definitions: &'a [NativeToolDefinition],
    stats: &'a mut TurnStats,
    total_mutating: usize,
    approval_index: &'a mut usize,
}

#[async_trait::async_trait]
impl crate::code_mode::runner::InnerCalls for NativeExec<'_> {
    async fn call(&mut self, name: &str, args: Value) -> Value {
        self.state.events.push(HarnessEvent::ToolCall {
            tool_name: name.to_string(),
            arguments: args.clone(),
        });
        self.harness
            .lane_progress("tool_call", tool_activity_text(name, &args));
        let executed = Box::pin(self.harness.execute_call(
            self.state,
            self.lanes,
            self.watches,
            self.vars,
            self.conversation_mode,
            self.approval_rx,
            name,
            &args,
            self.definitions,
            self.stats,
            self.total_mutating,
            self.approval_index,
            true,
        ))
        .await;
        let result = match executed {
            Executed::Answer(result)
            | Executed::Invalid { result, .. }
            | Executed::EndTurn { result, .. } => result,
        };
        self.state.events.push(HarnessEvent::ToolResult {
            tool_name: name.to_string(),
            result: result.clone(),
        });
        result
    }

    fn workspace(&self) -> Option<std::path::PathBuf> {
        Some(self.harness.context.current_dir())
    }
}

impl CodingHarness {
    #[allow(clippy::too_many_arguments)]
    async fn run_code_tool(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        vars: &mut LoopVars,
        conversation_mode: bool,
        approval_rx: &mut mpsc::UnboundedReceiver<(ApprovalDecision, Option<DecisionSource>)>,
        tool: &str,
        args: &Value,
        stats: &mut TurnStats,
        total_mutating: usize,
        approval_index: &mut usize,
    ) -> Value {
        use crate::code_mode::runner;
        if tool == "calls_list" {
            return runner::list_calls(args, &state.events);
        }
        let retry = (tool == "calls_retry").then(|| runner::retry_target(args, &state.events));
        let (_, all) = self.seat_definitions(conversation_mode, false);
        let session = self.session_id().unwrap_or_else(|| "main".to_string());
        let mut exec = NativeExec {
            harness: self,
            state,
            lanes,
            watches,
            vars,
            conversation_mode,
            approval_rx,
            definitions: &all,
            stats,
            total_mutating,
            approval_index,
        };
        match retry {
            Some(Ok((name, retried))) => {
                use runner::InnerCalls;
                exec.call(&name, retried).await
            }
            Some(Err(error)) => error,
            None => runner::execute(tool, args, &session, &all, &mut exec).await,
        }
    }
}
