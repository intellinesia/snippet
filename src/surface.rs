use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};

use crate::llm::NativeToolDefinition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatRole {
    Work,
    Desk,
    Lane,
    Lead,
}

pub struct Act {
    pub action: &'static str,
    pub tool: &'static str,
    pub fixed: Option<&'static str>,
    pub op: bool,
    pub about: &'static str,
}

pub struct Surface {
    pub name: &'static str,
    pub intro: &'static str,
    pub acts: &'static [Act],
}

const fn act(action: &'static str, tool: &'static str, about: &'static str) -> Act {
    Act {
        action,
        tool,
        fixed: None,
        op: false,
        about,
    }
}

const fn fixed(
    action: &'static str,
    tool: &'static str,
    value: &'static str,
    about: &'static str,
) -> Act {
    Act {
        action,
        tool,
        fixed: Some(value),
        op: false,
        about,
    }
}

const fn op(action: &'static str, tool: &'static str, about: &'static str) -> Act {
    Act {
        action,
        tool,
        fixed: None,
        op: true,
        about,
    }
}

const WEB: Surface = Surface {
    name: "web",
    intro: "Search the web or read a URL.",
    acts: &[
        act(
            "search",
            "web_search",
            "search the web for current facts, docs, errors and releases.",
        ),
        act(
            "read",
            "web_read",
            "fetch the full text of one URL, usually a search result.",
        ),
    ],
};

const PROCESS: Surface = Surface {
    name: "process",
    intro: "Manage background processes and file watches.",
    acts: &[
        fixed(
            "list",
            "manage_process",
            "list",
            "this session's running background processes (all: true for every one).",
        ),
        fixed(
            "log",
            "manage_process",
            "log",
            "trailing lines of a process's log (id, lines).",
        ),
        fixed(
            "wait",
            "manage_process",
            "wait",
            "block until a process exits or its log matches pattern, up to timeout_seconds; use this instead of sleeping.",
        ),
        fixed(
            "kill",
            "manage_process",
            "kill",
            "terminate a process (id).",
        ),
        fixed(
            "watch",
            "monitor",
            "add",
            "watch a file for appended text matching filter (path, label); matches wake you as [file_watch].",
        ),
        fixed(
            "unwatch",
            "monitor",
            "remove",
            "remove a file watch (watch_id, path or label).",
        ),
        fixed("watches", "monitor", "list", "list your file watches."),
    ],
};

const PRESENT: Surface = Surface {
    name: "present",
    intro: "Present files or artifacts to the user, or share artifacts.",
    acts: &[
        act(
            "show",
            "present_file",
            "show a file to the user; a title makes it a versioned artifact (artifact_id adds a version).",
        ),
        act(
            "share",
            "share_artifact",
            "send an artifact or file to the user, an agent, a group or a channel (to).",
        ),
    ],
};

const DELEGATE: Surface = Surface {
    name: "delegate",
    intro: "Delegate background lanes, cancel them, or list profiles.",
    acts: &[
        act(
            "run",
            "delegate_task",
            "start a background lane with a written brief, or message one by lane_id.",
        ),
        act(
            "cancel",
            "cancel_delegated_task",
            "stop a running lane and take its scope back, or withdraw a lane offer by its offer id (lane_id, reason).",
        ),
        act(
            "profiles",
            "list_profiles",
            "list inference profiles a lane can run on.",
        ),
    ],
};

const SKILL: Surface = Surface {
    name: "skill",
    intro: "Search or load Agent Skills.",
    acts: &[
        act(
            "search",
            "search_skills",
            "find an installed skill (playbook) for a procedure.",
        ),
        act("load", "skill", "load a skill's full instructions by name."),
    ],
};

const MESSAGE: Surface = Surface {
    name: "message",
    intro: "Send direct messages, chat posts, task-room updates, group messages, or push notifications.",
    acts: &[
        act(
            "send",
            "send_agent_message",
            "direct message to an agent, the user (human), session:<id> or task:<id>; reaction alone acknowledges.",
        ),
        act(
            "task",
            "post_task_coordination",
            "post to a task room; to names who must act, urgency urgent interrupts.",
        ),
        op(
            "group",
            "group_thread",
            "create or post in group threads, set the lead, add members.",
        ),
        act("push", "ping_user", "push-notify the user's phone."),
    ],
};

const READ: Surface = Surface {
    name: "read",
    intro: "Read chats, threads, boards, group threads, and sessions.",
    acts: &[
        act(
            "chats",
            "chats",
            "your chat threads, most recent first, with unread counts.",
        ),
        act(
            "thread",
            "read_chat",
            "read one thread (group, room or direct) by thread_id; read.agent reads a direct thread by peer.",
        ),
        act(
            "agent",
            "read_agent_thread",
            "your direct thread with one agent or human.",
        ),
        act(
            "room",
            "read_coordination_thread",
            "the coordination room's messages.",
        ),
        act("inbox", "read_agent_inbox", "an agent's inbox items."),
        act(
            "board",
            "read_coordination_board",
            "an agent's board (progress, reports, notes), newest first.",
        ),
        fixed("groups", "group_thread", "list", "list your group threads."),
        fixed(
            "group",
            "group_thread",
            "read",
            "read one group thread (thread_id).",
        ),
        act(
            "session",
            "inspect_session",
            "inspect one session's state and recent activity.",
        ),
        act("sessions", "list_sessions", "list sessions."),
    ],
};

const TASK: Surface = Surface {
    name: "task",
    intro: "Work on, inspect, claim, hand off, or report tasks.",
    acts: &[
        act(
            "report",
            "report_mission_task",
            "report your Lead task done, blocked or failed.",
        ),
        act(
            "ledger",
            "task_ledger",
            "read a task's shared ledger and its version.",
        ),
        act(
            "update",
            "update_task_state",
            "patch the task ledger (expected_version from ledger).",
        ),
        act(
            "pass",
            "pass_turn",
            "hand the task turn to another agent with done, next and risks.",
        ),
        act(
            "invite",
            "invite_task_agent",
            "bring a specialist onto your task with a self-contained ask.",
        ),
        act(
            "review",
            "submit_review",
            "give your review verdict: approve or changes with findings.",
        ),
        act(
            "answer",
            "answer_session",
            "answer a session's pending question for the user.",
        ),
        act(
            "session",
            "steer_session",
            "steer a managed session: model, hand-off.",
        ),
        act(
            "follow_up",
            "file_follow_up_task",
            "file a small follow-up task that starts when yours is done.",
        ),
        act(
            "hand_off",
            "hand_off_session",
            "pass this whole session to another agent.",
        ),
        act(
            "inspect",
            "inspect_task",
            "a task's brief, steps, agents, blockers and result.",
        ),
        act(
            "list",
            "view_task_board",
            "every open task and those finished in the last day.",
        ),
        act(
            "claim",
            "claim_and_dispatch_task",
            "claim an offered task and dispatch it to a work session.",
        ),
        act(
            "decline",
            "decline_task",
            "decline an offered task with a reason.",
        ),
        act("start", "start_task", "start a task that is ready."),
    ],
};

const CODE: Surface = Surface {
    name: "code",
    intro: "Run a script that calls your tools: many steps in one round trip, and only what it returns comes back. Use it for chains of three or more calls, loops over items, or large output you only need part of; call directly for one step, or when you want to see each result before the next.",
    acts: &[
        act("run", "code_run", "run JavaScript that calls your tools."),
        act(
            "resume",
            "code_resume",
            "continue a paused run with your answer, or rerun it with edited code.",
        ),
        act(
            "describe",
            "code_describe",
            "one tool's signature as a TypeScript declaration.",
        ),
    ],
};

const CALLS: Surface = Surface {
    name: "calls",
    intro: "Your tool calls: list them, run one again with changes, or choose how you call tools.",
    acts: &[
        act("list", "calls_list", "recent calls with ids, newest first."),
        act(
            "retry",
            "calls_retry",
            "run a past call again with some arguments changed.",
        ),
        act(
            "mode",
            "tool_mode",
            "show or choose direct calls or code scripts, from your next session.",
        ),
    ],
};

const WAKE: Surface = Surface {
    name: "wake",
    intro: "Wakes: once, on a schedule, or from a watched script.",
    acts: &[
        act(
            "once",
            "schedule_wake",
            "wake this seat once, in_seconds or in_minutes from now, with a note.",
        ),
        op(
            "every",
            "wake_schedule",
            "recurring wake with a brief (show, set, clear).",
        ),
        op(
            "watch",
            "watch",
            "background script that wakes you on new output (poll or stream).",
        ),
        op(
            "automation",
            "automated_wakes",
            "Lead's own heartbeat interval and quiet hours.",
        ),
    ],
};

const TASKS: Surface = Surface {
    name: "tasks",
    intro: "Create, assign, update, inspect, review, or report mission tasks.",
    acts: &[
        act(
            "create",
            "create_mission_task",
            "file a task: what, where, why, and what done looks like.",
        ),
        act(
            "update",
            "update_mission_task",
            "change a task's brief, status, plan or fields.",
        ),
        act("list", "list_mission_tasks", "list mission tasks."),
        act("cancel", "cancel_mission_task", "cancel a task."),
        act(
            "retry",
            "retry_mission_task",
            "retry a failed or stuck task.",
        ),
        act(
            "assign",
            "assign_task_agent",
            "add or change an agent on a task.",
        ),
        act(
            "transfer",
            "transfer_mission_task_lease",
            "move a task's lease to another session.",
        ),
        act(
            "inspect",
            "inspect_task",
            "a task's brief, steps, agents, blockers and result.",
        ),
        act("ledger", "task_ledger", "read a task's shared ledger."),
        act(
            "review",
            "submit_review",
            "approve a task in review or send it back with findings.",
        ),
        act(
            "report",
            "report_mission_task",
            "report a task done, blocked or failed.",
        ),
    ],
};

const GOALS: Surface = Surface {
    name: "goals",
    intro: "Create, plan, update, list, or close goals.",
    acts: &[
        act("create", "create_goal", "open a goal."),
        act("plan", "plan_goal", "plan a goal into tasks."),
        act(
            "replan",
            "replan_goal",
            "replan a goal after new information.",
        ),
        act("update", "update_goal", "change a goal."),
        act("list", "list_goals", "list goals."),
        act("close", "close_goal", "close a goal."),
    ],
};

const SESSIONS: Surface = Surface {
    name: "sessions",
    intro: "Create, message, stop, inspect, clean up, or answer worker sessions.",
    acts: &[
        act(
            "create",
            "create_mission_session",
            "open a new work session.",
        ),
        act(
            "deliver",
            "deliver_to_session",
            "deliver a message into a session.",
        ),
        act("stop", "stop_session", "stop a session's run."),
        act("state", "session_state", "a session's live state."),
        op(
            "clean_up",
            "clean_up_sessions",
            "list or delete finished sessions.",
        ),
        act(
            "answer",
            "answer_worker",
            "answer a worker's pending question.",
        ),
    ],
};

const AMBIENT: Surface = Surface {
    name: "ambient",
    intro: "Control ambient listening, room state, expectations, tuning, and recordings.",
    acts: &[
        act("mode", "ambient_mode", "switch ambient listening mode."),
        op("room", "ambient_room", "room state."),
        act("search", "ambient_search", "search what was heard."),
        act(
            "expect",
            "ambient_expect",
            "tell the listener what to expect next.",
        ),
        act(
            "feedback",
            "ambient_feedback",
            "record feedback on an ambient reply.",
        ),
        op("tune", "ambient_tune", "tune listener settings."),
        act("on_call", "user_on_call", "whether the user is on a call."),
        act("wait", "wait_for_user", "wait for the user to speak."),
        op("recordings", "recordings", "list or manage recordings."),
        op("policy", "recording_policy", "recording policy."),
    ],
};

pub const SURFACES: &[&Surface] = &[
    &WEB, &PROCESS, &PRESENT, &DELEGATE, &SKILL, &MESSAGE, &READ, &TASK, &WAKE, &TASKS, &GOALS,
    &SESSIONS, &AMBIENT, &CODE, &CALLS,
];

fn keep_for(role: SeatRole) -> &'static [&'static str] {
    match role {
        SeatRole::Work => &[
            "bash",
            "change_files",
            "view_image",
            "update_plan",
            "ask_user",
            "complete_goal",
            "custom_tool",
        ],
        SeatRole::Desk => &["bash", "change_files", "view_image", "complete_goal"],
        SeatRole::Lane => &[
            "bash",
            "change_files",
            "view_image",
            "message_parent",
            "terminate_loop",
            "submit_review",
            "custom_tool",
        ],
        SeatRole::Lead => &[
            "bash",
            "view_image",
            "ask_user",
            "speak",
            "update_brief",
            "complete_goal",
        ],
    }
}

fn surfaces_for(role: SeatRole) -> &'static [&'static Surface] {
    match role {
        SeatRole::Work => &[
            &PROCESS, &WEB, &PRESENT, &DELEGATE, &MESSAGE, &READ, &SKILL, &TASK, &WAKE, &CODE,
            &CALLS,
        ],
        SeatRole::Desk => &[&WEB, &MESSAGE, &READ, &TASK, &WAKE, &PRESENT, &CODE, &CALLS],
        SeatRole::Lane => &[&PROCESS, &WEB, &SKILL],
        SeatRole::Lead => &[
            &WEB, &PRESENT, &MESSAGE, &READ, &AMBIENT, &TASKS, &GOALS, &SESSIONS, &WAKE, &CODE,
            &CALLS,
        ],
    }
}

fn find_surface(name: &str) -> Option<&'static Surface> {
    SURFACES.iter().copied().find(|s| s.name == name)
}

pub fn is_surface(name: &str) -> bool {
    find_surface(name).is_some()
}

/// The action whose description line lists `action`'s fields (acts sharing a
/// tool list them once), and whether `action` takes `op`.
pub fn fields_listed_under(name: &str, action: &str) -> Option<(&'static str, bool)> {
    let surface = find_surface(name)?;
    let act = surface.acts.iter().find(|a| a.action == action)?;
    let first = surface.acts.iter().find(|a| a.tool == act.tool)?;
    Some((first.action, act.op))
}

pub fn uses_op(name: &str, args: &Value) -> bool {
    let Some(surface) = find_surface(name) else {
        return false;
    };
    let Some(action) = args.get("action").and_then(Value::as_str) else {
        return false;
    };
    surface
        .acts
        .iter()
        .find(|a| a.action == action)
        .is_some_and(|a| a.op)
}

pub fn lower(name: &str, args: &Value) -> Result<Option<(String, Value)>, String> {
    let Some(surface) = find_surface(name) else {
        return Ok(None);
    };
    let valid: Vec<&str> = surface.acts.iter().map(|a| a.action).collect();
    let Some(map) = args.as_object() else {
        return Err(format!(
            "`{name}` expects a JSON object with `action` (one of: {}).",
            valid.join(", ")
        ));
    };
    let Some(action) = map.get("action").and_then(Value::as_str) else {
        return Err(format!(
            "`{name}` requires `action` (one of: {}).",
            valid.join(", ")
        ));
    };
    let Some(act) = surface.acts.iter().find(|a| a.action == action) else {
        return Err(format!(
            "`{name}` action must be one of: {} (got `{action}`).",
            valid.join(", ")
        ));
    };
    let mut lowered = map.clone();
    lowered.remove("action");
    if let Some(fixed_action) = act.fixed {
        lowered.insert(
            "action".to_string(),
            Value::String(fixed_action.to_string()),
        );
    } else if act.op {
        if let Some(op_val) = lowered.remove("op") {
            lowered.insert("action".to_string(), op_val);
        }
    }
    Ok(Some((act.tool.to_string(), Value::Object(lowered))))
}

pub fn lower_for_advertised(
    name: &str,
    args: &Value,
    advertised: &NativeToolDefinition,
) -> Result<Option<(String, Value)>, String> {
    if !is_surface(name) {
        return Ok(None);
    }
    let allowed: Vec<&str> = advertised
        .input_schema
        .pointer("/properties/action/enum")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(map) = args.as_object() else {
        return Err(format!(
            "`{name}` expects a JSON object with `action` (one of: {}).",
            allowed.join(", ")
        ));
    };
    let Some(action) = map.get("action").and_then(Value::as_str) else {
        return Err(format!(
            "`{name}` requires `action` (one of: {}).",
            allowed.join(", ")
        ));
    };
    if !allowed.is_empty() && !allowed.contains(&action) {
        return Err(format!(
            "`{name}` action must be one of: {} (got `{action}`).",
            allowed.join(", ")
        ));
    }
    lower(name, args)
}

pub fn resolve(name: &str, args: &Value) -> String {
    match lower(name, args) {
        Ok(Some((internal, _))) => internal,
        _ => name.to_string(),
    }
}

pub fn advertise(
    role: SeatRole,
    mut internal: Vec<NativeToolDefinition>,
) -> Vec<NativeToolDefinition> {
    if role != SeatRole::Desk
        && let Some(send) = internal.iter_mut().find(|d| d.name == "send_agent_message")
        && let Some(reply_to) = send.input_schema.pointer_mut("/properties/reply_to")
    {
        reply_to["description"] = json!(crate::coordination_tools::REACTION_TARGET);
    }
    let by_name: HashMap<&str, &NativeToolDefinition> =
        internal.iter().map(|d| (d.name.as_str(), d)).collect();
    let mut out = Vec::new();
    for keep in keep_for(role) {
        if let Some(def) = by_name.get(keep) {
            out.push((*def).clone());
        }
    }
    for surface in surfaces_for(role) {
        let active: Vec<(&Act, &NativeToolDefinition)> = surface
            .acts
            .iter()
            .filter_map(|act| by_name.get(act.tool).map(|def| (act, *def)))
            .collect();
        if active.is_empty() {
            continue;
        }
        out.push(build_surface_definition(surface, &active));
    }
    out
}

fn build_surface_definition(
    surface: &Surface,
    active: &[(&Act, &NativeToolDefinition)],
) -> NativeToolDefinition {
    let actions: Vec<Value> = active
        .iter()
        .map(|(a, _)| Value::String(a.action.to_string()))
        .collect();
    let mut props: BTreeMap<String, Value> = BTreeMap::new();
    let mut any_op = false;
    let mut listed: Vec<&str> = Vec::new();
    let mut lines = Vec::with_capacity(active.len() + 1);
    lines.push(surface.intro.to_string());

    for (act, def) in active {
        if act.op {
            any_op = true;
        }
        let required: Vec<&str> = def
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let internal_props = def
            .input_schema
            .get("properties")
            .and_then(Value::as_object);

        let mut field_names: Vec<String> = Vec::new();
        if let Some(map) = internal_props {
            for (k, v) in map {
                if k == "action" || k == "op" {
                    continue;
                }
                let mut prop_def = v.clone();
                match props.get_mut(k) {
                    None => {
                        props.insert(k.clone(), prop_def);
                    }
                    Some(existing) => {
                        merge_property_collision(existing, &mut prop_def);
                    }
                }
                if required.contains(&k.as_str()) {
                    field_names.push(format!("{k}*"));
                } else {
                    field_names.push(k.clone());
                }
            }
        }
        field_names.sort_by(|a, b| {
            let a_req = a.ends_with('*');
            let b_req = b.ends_with('*');
            b_req.cmp(&a_req).then_with(|| a.cmp(b))
        });

        let summary = act.about;
        let op_note = if act.op {
            let ops = op_values(surface.name, act.action, internal_props);
            if ops.is_empty() {
                String::new()
            } else {
                format!(" (op: {})", ops.join("|"))
            }
        } else {
            String::new()
        };
        let repeat = listed.contains(&act.tool);
        listed.push(act.tool);
        let fields_note = if field_names.is_empty() || repeat {
            String::new()
        } else {
            format!(" Fields: {}.", field_names.join(", "))
        };
        lines.push(format!("- {}: {summary}{op_note}{fields_note}", act.action));
    }

    let mut schema_props = Map::new();
    schema_props.insert(
        "action".to_string(),
        json!({ "type": "string", "enum": actions }),
    );
    if any_op {
        schema_props.insert("op".to_string(), json!({ "type": "string" }));
    }
    for (k, v) in props {
        schema_props.insert(k, v);
    }

    NativeToolDefinition {
        name: surface.name.to_string(),
        description: lines.join("\n"),
        input_schema: json!({
            "type": "object",
            "properties": schema_props,
            "required": ["action"],
        }),
    }
}

fn op_values(
    surface: &str,
    action: &str,
    internal_props: Option<&Map<String, Value>>,
) -> Vec<String> {
    if surface == "message" && action == "group" {
        return ["create", "post", "set_lead", "add_member"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
    }
    internal_props
        .and_then(|p| p.get("action"))
        .and_then(|a| a.get("enum"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn merge_property_collision(existing: &mut Value, incoming: &mut Value) {
    let (Some(ex), Some(inc)) = (existing.as_object_mut(), incoming.as_object()) else {
        return;
    };
    if ex.get("enum") != inc.get("enum") {
        ex.remove("enum");
    }
    // Preserve compatibility when two merged actions disagree on a field's scalar type (e.g. ambient.id).
    let ex_type = ex.get("type").and_then(Value::as_str).map(str::to_string);
    let inc_type = inc.get("type").and_then(Value::as_str).map(str::to_string);
    if let (Some(t1), Some(t2)) = (ex_type, inc_type)
        && t1 != t2
    {
        ex.remove("type");
        ex.insert("anyOf".to_string(), json!([{ "type": t1 }, { "type": t2 }]));
    } else if let Some(any_of) = ex.get_mut("anyOf").and_then(Value::as_array_mut)
        && let Some(t2) = inc.get("type").and_then(Value::as_str)
        && !any_of
            .iter()
            .any(|v| v.get("type").and_then(Value::as_str) == Some(t2))
    {
        any_of.push(json!({ "type": t2 }));
    }
}

pub fn looks_like_argument_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "missing field",
        "missing required",
        "unknown field",
        "invalid type",
        "unknown variant",
        "expected json object",
        "requires ",
        "is required",
        "must be ",
        "needs ",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub fn format_argument_error(
    error_text: &str,
    surface_name: &str,
    internal_name: &str,
    uses_op_field: bool,
    internal_schema: Option<&Value>,
) -> String {
    let Some(schema) = internal_schema else {
        return error_text.to_string();
    };
    let act = find_surface(surface_name)
        .filter(|_| surface_name != internal_name)
        .and_then(|s| s.acts.iter().find(|a| a.tool == internal_name));
    let call = match act {
        Some(act) => format!("Call `{surface_name}` again with action `{}`", act.action),
        None => format!("Call `{surface_name}` again"),
    };
    let fixed = act.is_some_and(|a| a.fixed.is_some());
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut lines = Vec::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (name, prop) in props {
            let shown = match name.as_str() {
                "action" if fixed => continue,
                "action" if uses_op_field => "op",
                other => other,
            };
            let kind = prop
                .get("enum")
                .and_then(Value::as_array)
                .map(|e| {
                    e.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .or_else(|| prop.get("type").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "any".to_string());
            let need = if required.contains(&name.as_str()) {
                ", required"
            } else {
                ""
            };
            let about = prop
                .get("description")
                .and_then(Value::as_str)
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            lines.push(format!("- `{shown}` ({kind}{need}){about}"));
        }
    }
    if lines.is_empty() {
        return format!("{error_text}\n{call}; it takes no fields.");
    }
    format!(
        "{error_text}\n{call} and these fields:\n{}",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_handles_plain_fixed_op_and_unknown_action() {
        assert_eq!(lower("bash", &json!({"command": "ls"})).unwrap(), None);

        let (tool, args) = lower("web", &json!({"action": "search", "query": "rust"}))
            .unwrap()
            .unwrap();
        assert_eq!(tool, "web_search");
        assert_eq!(args, json!({"query": "rust"}));

        let (tool, args) = lower("process", &json!({"action": "watch", "path": "/tmp/a"}))
            .unwrap()
            .unwrap();
        assert_eq!(tool, "monitor");
        assert_eq!(args, json!({"action": "add", "path": "/tmp/a"}));

        let (tool, args) = lower("wake", &json!({"action": "every", "op": "show"}))
            .unwrap()
            .unwrap();
        assert_eq!(tool, "wake_schedule");
        assert_eq!(args, json!({"action": "show"}));

        let err = lower("web", &json!({"action": "bogus"})).unwrap_err();
        assert!(err.contains("search"));
        assert!(err.contains("read"));

        assert_eq!(resolve("web", &json!({"action": "read"})), "web_read");
        assert_eq!(resolve("bash", &json!({})), "bash");
    }

    #[test]
    fn advertise_handles_property_collisions_and_keeps_descriptions() {
        let long_desc = "a".repeat(200);
        let defs = vec![
            NativeToolDefinition {
                name: "ambient_room".into(),
                description: "Room state. Second sentence.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["get", "drop"]},
                        "id": {"type": "integer", "description": long_desc},
                    },
                    "required": ["action"],
                }),
            },
            NativeToolDefinition {
                name: "recordings".into(),
                description: "Manage recordings.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list", "delete"]},
                        "id": {"type": "string"},
                    },
                    "required": ["action"],
                }),
            },
        ];
        let adv = advertise(SeatRole::Lead, defs);
        assert_eq!(adv.len(), 1);
        let amb = &adv[0];
        assert_eq!(amb.name, "ambient");
        let id_prop = &amb.input_schema["properties"]["id"];
        assert!(id_prop.get("type").is_none());
        assert_eq!(
            id_prop["anyOf"],
            json!([{"type": "integer"}, {"type": "string"}])
        );
        assert_eq!(id_prop["description"].as_str().unwrap(), long_desc);
        assert!(amb.description.contains("(op: get|drop)"));
        assert!(amb.description.contains("(op: list|delete)"));
    }

    #[test]
    fn advertised_sets_and_byte_totals_match_spec() {
        let key = Some("k".to_string());
        let sizes = |defs: &[NativeToolDefinition]| -> (Vec<String>, usize) {
            let names = defs.iter().map(|d| d.name.clone()).collect();
            let bytes = defs.iter().map(crate::lanes::definition_byte_size).sum();
            (names, bytes)
        };

        let (work_defs, _) = crate::lanes::role_tool_definitions("work", None).unwrap();
        let (work_names, work_bytes) = sizes(&work_defs);
        println!(
            "Work native ({}): {work_bytes} bytes -> {:?}",
            work_names.len(),
            work_names
        );

        let mut cli_internal = crate::session::work_tool_registry(key.clone(), None).definitions();
        cli_internal.extend(crate::meta::conversation_meta_definitions_for(true, true));
        cli_internal.push(NativeToolDefinition {
            name: "custom_tool".into(),
            description: "Run one of your custom tools by name, including one you created or changed during this conversation that isn't in your tool list yet.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The custom tool's name."},
                    "arguments": {"type": "object", "description": "Its arguments, as its parameters schema describes."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        });
        let cli_defs = advertise(SeatRole::Work, cli_internal);
        let (cli_names, cli_bytes) = sizes(&cli_defs);
        println!(
            "Work CLI bridge ({}): {cli_bytes} bytes -> {:?}",
            cli_names.len(),
            cli_names
        );

        let (desk_defs, _) = crate::lanes::role_tool_definitions("desk", None).unwrap();
        let (desk_names, desk_bytes) = sizes(&desk_defs);
        println!(
            "Desk ({}): {desk_bytes} bytes -> {:?}",
            desk_names.len(),
            desk_names
        );

        let (lane_defs, _) = crate::lanes::role_tool_definitions("lane", None).unwrap();
        let (lane_names, lane_bytes) = sizes(&lane_defs);
        println!(
            "Lane ({}): {lane_bytes} bytes -> {:?}",
            lane_names.len(),
            lane_names
        );

        let (lead_defs, _) = crate::lanes::role_tool_definitions("lead", None).unwrap();
        let (lead_names, lead_bytes) = sizes(&lead_defs);
        println!(
            "MC ({}): {lead_bytes} bytes -> {:?}",
            lead_names.len(),
            lead_names
        );

        assert_eq!(desk_names.len(), 10);
        assert_eq!(lead_names.len(), 15);
        assert!(cli_names.iter().any(|n| n == "custom_tool"));
        assert!(cli_names.iter().any(|n| n == "complete_goal"));
        for hidden in [
            "record_coordination_note",
            "list_coordination_agents",
            "register_agent",
            "keep_warm",
            "context_limit",
            "reach_policy",
            "set_session_title",
            "report_progress",
        ] {
            for names in [
                &work_names,
                &cli_names,
                &desk_names,
                &lane_names,
                &lead_names,
            ] {
                assert!(
                    !names.iter().any(|n| n == hidden),
                    "{hidden} leaked into {names:?}"
                );
            }
        }
    }
}
