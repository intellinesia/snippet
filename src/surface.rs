use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value, json};

use crate::llm::NativeToolDefinition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatRole {
    Work,
    Desk,
    Lane,
    MissionControl,
}

pub struct Act {
    pub action: &'static str,
    pub tool: &'static str,
    pub fixed: Option<&'static str>,
    pub op: bool,
}

pub struct Surface {
    pub name: &'static str,
    pub intro: &'static str,
    pub acts: &'static [Act],
}

const fn act(action: &'static str, tool: &'static str) -> Act {
    Act {
        action,
        tool,
        fixed: None,
        op: false,
    }
}

const fn fixed(action: &'static str, tool: &'static str, value: &'static str) -> Act {
    Act {
        action,
        tool,
        fixed: Some(value),
        op: false,
    }
}

const fn op(action: &'static str, tool: &'static str) -> Act {
    Act {
        action,
        tool,
        fixed: None,
        op: true,
    }
}

const WEB: Surface = Surface {
    name: "web",
    intro: "Search the web or read a URL.",
    acts: &[act("search", "web_search"), act("read", "web_read")],
};

const PROCESS: Surface = Surface {
    name: "process",
    intro: "Manage background processes and file watches.",
    acts: &[
        fixed("list", "manage_process", "list"),
        fixed("log", "manage_process", "log"),
        fixed("wait", "manage_process", "wait"),
        fixed("kill", "manage_process", "kill"),
        fixed("watch", "monitor", "add"),
        fixed("unwatch", "monitor", "remove"),
        fixed("watches", "monitor", "list"),
    ],
};

const PRESENT: Surface = Surface {
    name: "present",
    intro: "Present files or artifacts to the user, or share artifacts.",
    acts: &[act("show", "present_file"), act("share", "share_artifact")],
};

const DELEGATE: Surface = Surface {
    name: "delegate",
    intro: "Delegate background lanes, cancel them, or list profiles.",
    acts: &[
        act("run", "delegate_task"),
        act("cancel", "cancel_delegated_task"),
        act("profiles", "list_profiles"),
    ],
};

const SKILL: Surface = Surface {
    name: "skill",
    intro: "Search or load Agent Skills.",
    acts: &[act("search", "search_skills"), act("load", "skill")],
};

const MESSAGE: Surface = Surface {
    name: "message",
    intro: "Send direct messages, chat posts, task-room updates, group messages, or push notifications.",
    acts: &[
        act("send", "send_agent_message"),
        act("post", "post_chat"),
        act("task", "post_task_coordination"),
        op("group", "group_thread"),
        act("push", "ping_user"),
    ],
};

const READ: Surface = Surface {
    name: "read",
    intro: "Read chats, threads, boards, group threads, and sessions.",
    acts: &[
        act("chats", "chats"),
        act("thread", "read_chat"),
        act("agent", "read_agent_thread"),
        act("room", "read_coordination_thread"),
        act("inbox", "read_agent_inbox"),
        act("board", "read_coordination_board"),
        fixed("groups", "group_thread", "list"),
        fixed("group", "group_thread", "read"),
        act("session", "inspect_session"),
        act("sessions", "list_sessions"),
    ],
};

const TASK: Surface = Surface {
    name: "task",
    intro: "Work on, inspect, claim, hand off, or report tasks.",
    acts: &[
        act("report", "report_mission_task"),
        act("ledger", "task_ledger"),
        act("update", "update_task_state"),
        act("pass", "pass_turn"),
        act("invite", "invite_task_agent"),
        act("review", "submit_review"),
        act("follow_up", "file_follow_up_task"),
        act("hand_off", "hand_off_session"),
        act("inspect", "inspect_task"),
        act("board", "view_task_board"),
        act("claim", "claim_and_dispatch_task"),
        act("decline", "decline_task"),
        act("start", "start_task"),
    ],
};

const WAKE: Surface = Surface {
    name: "wake",
    intro: "Schedule wakes and manage background watches.",
    acts: &[
        act("once", "schedule_wake"),
        op("every", "wake_schedule"),
        op("watch", "watch"),
    ],
};

const TASKS: Surface = Surface {
    name: "tasks",
    intro: "Create, assign, update, inspect, review, or report mission tasks.",
    acts: &[
        act("create", "create_mission_task"),
        act("update", "update_mission_task"),
        act("list", "list_mission_tasks"),
        act("cancel", "cancel_mission_task"),
        act("retry", "retry_mission_task"),
        act("assign", "assign_task_agent"),
        act("transfer", "transfer_mission_task_lease"),
        act("inspect", "inspect_task"),
        act("ledger", "task_ledger"),
        act("review", "submit_review"),
        act("report", "report_mission_task"),
    ],
};

const GOALS: Surface = Surface {
    name: "goals",
    intro: "Create, plan, update, list, or close goals.",
    acts: &[
        act("create", "create_goal"),
        act("plan", "plan_goal"),
        act("replan", "replan_goal"),
        act("update", "update_goal"),
        act("list", "list_goals"),
        act("close", "close_goal"),
    ],
};

const SESSIONS: Surface = Surface {
    name: "sessions",
    intro: "Create, message, stop, inspect, clean up, or answer worker sessions.",
    acts: &[
        act("create", "create_mission_session"),
        act("deliver", "deliver_to_session"),
        act("stop", "stop_session"),
        act("state", "session_state"),
        op("clean_up", "clean_up_sessions"),
        act("answer", "answer_worker"),
    ],
};

const SCHEDULE: Surface = Surface {
    name: "schedule",
    intro: "Schedule wakes, watches, and recurring jobs.",
    acts: &[
        act("once", "schedule_wake"),
        op("every", "wake_schedule"),
        op("watch", "watch"),
        act("job_create", "create_recurring_job"),
        act("jobs", "list_recurring_jobs"),
        act("job_cancel", "cancel_recurring_job"),
    ],
};

const AMBIENT: Surface = Surface {
    name: "ambient",
    intro: "Control ambient listening, room state, expectations, tuning, and recordings.",
    acts: &[
        act("mode", "ambient_mode"),
        op("room", "ambient_room"),
        act("search", "ambient_search"),
        act("expect", "ambient_expect"),
        act("feedback", "ambient_feedback"),
        op("tune", "ambient_tune"),
        act("on_call", "user_on_call"),
        act("wait", "wait_for_user"),
        op("recordings", "recordings"),
        op("policy", "recording_policy"),
    ],
};

pub const SURFACES: &[&Surface] = &[
    &WEB, &PROCESS, &PRESENT, &DELEGATE, &SKILL, &MESSAGE, &READ, &TASK, &WAKE, &TASKS, &GOALS,
    &SESSIONS, &SCHEDULE, &AMBIENT,
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
        SeatRole::Desk => &["bash", "change_files", "complete_goal"],
        SeatRole::Lane => &[
            "bash",
            "change_files",
            "view_image",
            "message_parent",
            "terminate_loop",
            "submit_review",
            "custom_tool",
        ],
        SeatRole::MissionControl => &[
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
            &PROCESS, &WEB, &PRESENT, &DELEGATE, &MESSAGE, &READ, &SKILL, &TASK,
        ],
        SeatRole::Desk => &[&WEB, &MESSAGE, &READ, &TASK, &WAKE],
        SeatRole::Lane => &[&PROCESS, &WEB, &SKILL],
        SeatRole::MissionControl => &[
            &WEB, &PRESENT, &MESSAGE, &READ, &AMBIENT, &TASKS, &GOALS, &SESSIONS, &SCHEDULE,
        ],
    }
}

fn find_surface(name: &str) -> Option<&'static Surface> {
    SURFACES.iter().copied().find(|s| s.name == name)
}

pub fn is_surface(name: &str) -> bool {
    find_surface(name).is_some()
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
        lowered.insert("action".to_string(), Value::String(fixed_action.to_string()));
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

pub fn advertise(role: SeatRole, internal: Vec<NativeToolDefinition>) -> Vec<NativeToolDefinition> {
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
                let mut prop_def = trim_prop_description(v, 120);
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

        let summary = first_sentence(&def.description, 140);
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
        let fields_note = if field_names.is_empty() {
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

fn trim_prop_description(prop: &Value, max_chars: usize) -> Value {
    let mut cloned = prop.clone();
    if let Some(obj) = cloned.as_object_mut()
        && let Some(desc) = obj.get("description").and_then(Value::as_str)
        && desc.chars().count() > max_chars
    {
        let cut: String = desc.chars().take(max_chars.saturating_sub(1)).collect();
        obj.insert("description".to_string(), Value::String(format!("{cut}…")));
    }
    cloned
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
        && !any_of.iter().any(|v| v.get("type").and_then(Value::as_str) == Some(t2))
    {
        any_of.push(json!({ "type": t2 }));
    }
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    let compact: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = compact.len();
    let bytes = compact.as_bytes();
    for (idx, b) in bytes.iter().enumerate() {
        if *b == b'.' {
            let next_is_sep = bytes.get(idx + 1).is_none_or(|n| *n == b' ');
            let is_abbrev = idx >= 3 && matches!(&compact[idx - 3..=idx], "e.g." | "i.e.");
            if next_is_sep && !is_abbrev {
                end = idx + 1;
                break;
            }
        }
    }
    let sentence = &compact[..end];
    if sentence.chars().count() <= max_chars {
        sentence.to_string()
    } else {
        let cut: String = sentence.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
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
    if surface_name != internal_name {
        let op_hint = if uses_op_field {
            " (pass internal `action` as `op`)"
        } else {
            ""
        };
        format!(
            "{error_text}\nCall `{surface_name}` again using the `{internal_name}` schema{op_hint}: {schema}"
        )
    } else {
        format!(
            "{error_text}\nCall `{surface_name}` again with arguments matching this schema: {schema}"
        )
    }
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
    fn advertise_handles_property_collisions_and_truncation() {
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
        let adv = advertise(SeatRole::MissionControl, defs);
        assert_eq!(adv.len(), 1);
        let amb = &adv[0];
        assert_eq!(amb.name, "ambient");
        let id_prop = &amb.input_schema["properties"]["id"];
        assert!(id_prop.get("type").is_none());
        assert_eq!(
            id_prop["anyOf"],
            json!([{"type": "integer"}, {"type": "string"}])
        );
        assert!(
            id_prop["description"].as_str().unwrap().chars().count() <= 120
        );
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
        println!("Work native ({}): {work_bytes} bytes -> {:?}", work_names.len(), work_names);

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
        println!("Work CLI bridge ({}): {cli_bytes} bytes -> {:?}", cli_names.len(), cli_names);

        let (desk_defs, _) = crate::lanes::role_tool_definitions("desk", None).unwrap();
        let (desk_names, desk_bytes) = sizes(&desk_defs);
        println!("Desk ({}): {desk_bytes} bytes -> {:?}", desk_names.len(), desk_names);

        let (lane_defs, _) = crate::lanes::role_tool_definitions("lane", None).unwrap();
        let (lane_names, lane_bytes) = sizes(&lane_defs);
        println!("Lane ({}): {lane_bytes} bytes -> {:?}", lane_names.len(), lane_names);

        let (mc_defs, _) = crate::lanes::role_tool_definitions("mission_control", None).unwrap();
        let (mc_names, mc_bytes) = sizes(&mc_defs);
        println!("MC ({}): {mc_bytes} bytes -> {:?}", mc_names.len(), mc_names);

        assert_eq!(desk_names.len(), 7);
        assert_eq!(mc_names.len(), 14);
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
            for names in [&work_names, &cli_names, &desk_names, &lane_names, &mc_names] {
                assert!(!names.iter().any(|n| n == hidden), "{hidden} leaked into {names:?}");
            }
        }
    }
}
