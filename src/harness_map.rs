use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_wakes::{ReportTo, Setter};
use crate::config::SnippetConfig;
use crate::coordination::reach::{Channel, ChannelState, Kind};
use crate::llm::NativeToolDefinition;
use crate::memory::{ENTRY_FILE, ENTRY_PROMPT_CHARS, MAX_ENTRY_CHARS, MemoryRepo, ScopeKind};
use crate::store::Store;
use crate::tools::{Tool, ToolContext, ToolError, ToolResult};

pub const SECTIONS: [&str; 9] = [
    "sessions", "desk", "board", "reach", "memory", "wakes", "tools", "profiles", "limits",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessMap {
    pub sessions: Vec<SessionEntry>,
    pub desk: DeskInfo,
    pub board: BoardInfo,
    pub reach: ReachInfo,
    pub memory: Vec<MemoryScopeInfo>,
    pub wakes: WakesInfo,
    pub tools: ToolsInfo,
    pub profiles: Vec<ProfileInfo>,
    pub limits: LimitsInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    pub folder: String,
    pub status: String,
    pub profile: Option<String>,
    pub cache: String,
    pub idle_secs: i64,
    pub settled: bool,
    pub keep_awake: bool,
    pub task_id: Option<String>,
    pub is_current: bool,
    pub is_desk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeskInfo {
    pub agent_id: String,
    pub session_id: String,
    pub exists: bool,
    pub identity_source: String,
    pub identity_path: Option<String>,
    pub fallback_identity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardInfo {
    pub agent_id: String,
    pub total_rows: u64,
    pub outstanding_dispatches: u64,
    pub latest_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReachInfo {
    pub agent_id: String,
    pub channels: BTreeMap<Channel, ChannelState>,
    pub policy: BTreeMap<Kind, Vec<Channel>>,
    pub in_quiet_hours: bool,
    pub on_call: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryScopeInfo {
    pub scope: ScopeKind,
    pub key: String,
    pub label: String,
    pub path: String,
    pub entry_file: String,
    pub exists: bool,
    pub entry_prompt_budget_chars: usize,
    pub max_entry_chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeScheduleInfo {
    pub job_id: String,
    pub title: String,
    pub schedule: String,
    pub enabled: bool,
    pub set_by: Setter,
    pub quiet_start: Option<String>,
    pub quiet_end: Option<String>,
    pub max_runs_per_day: u32,
    pub report_to: ReportTo,
    pub next_run_at: Option<u64>,
    pub last_run_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeepWarmInfo {
    pub enabled: bool,
    pub session_id: String,
    pub interval_minutes: u32,
    pub max_idle_hours: u32,
    pub set_by: Setter,
    pub last_warm_at: Option<i64>,
    pub warms: u64,
    pub skipped_reason: Option<String>,
    pub provider_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakesInfo {
    pub schedule: Option<WakeScheduleInfo>,
    pub keep_warm: Option<KeepWarmInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleToolBudget {
    pub cap_bytes: usize,
    pub measured_bytes: usize,
    pub tool_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsInfo {
    pub role: String,
    pub cap_bytes: usize,
    pub measured_bytes: usize,
    pub tool_count: usize,
    pub names: Vec<String>,
    pub role_budgets: BTreeMap<String, RoleToolBudget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub is_default: bool,
    pub rate_limited_until: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsInfo {
    pub max_context_tokens: Option<u64>,
    pub min_context_tokens: u64,
    pub min_wake_interval_secs: u64,
    pub default_max_wake_runs_per_day: u32,
    pub max_wake_runs_per_day_ceiling: u32,
    pub keep_warm_min_interval_minutes: u32,
    pub keep_warm_max_interval_minutes: u32,
    pub keep_warm_max_idle_hours_ceiling: u32,
}

pub fn build(agent_id: &str, session_id: &str, role: &str) -> HarnessMap {
    build_with(agent_id, session_id, role, None, None)
}

pub(crate) fn build_with(
    agent_id: &str,
    session_id: &str,
    role: &str,
    store_path: Option<&Path>,
    workspace: Option<&Path>,
) -> HarnessMap {
    let all_sessions = crate::session::all_device_sessions();
    let inferred_workspace = workspace.map(Path::to_path_buf).or_else(|| {
        all_sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| std::path::PathBuf::from(&s.folder))
    });

    let sessions: Vec<SessionEntry> = all_sessions
        .iter()
        .filter(|s| {
            s.id == session_id
                || s.agent_id.as_deref() == Some(agent_id)
                || s.worker_agent_id.as_deref() == Some(agent_id)
        })
        .map(|s| SessionEntry {
            id: s.id.clone(),
            title: s.title.clone(),
            folder: s.folder.clone(),
            status: s.status.clone(),
            profile: s.profile.clone(),
            cache: s.cache.to_string(),
            idle_secs: s.idle_secs,
            settled: s.settled,
            keep_awake: s.keep_awake,
            task_id: s.task_id.clone(),
            is_current: s.id == session_id,
            is_desk: crate::session::is_inbox_session_id(&s.id),
        })
        .collect();

    let desk_session_id = if agent_id == crate::mission_control::SESSION_ID {
        crate::mission_control::SESSION_ID.to_string()
    } else {
        crate::session::inbox_session_id(agent_id)
    };
    let desk_exists = all_sessions.iter().any(|s| s.id == desk_session_id);
    let mc_root = crate::config::snippet_home().join("mission-control");
    let home =
        crate::coordination::AgentHome::new(crate::coordination::agents_root(&mc_root), agent_id)
            .ok();
    let (identity_source, identity_path) = if agent_id == crate::mission_control::SESSION_ID {
        ("mission_control".to_string(), None)
    } else if let Some(ref h) = home
        && h.read_identity().is_ok_and(|t| !t.trim().is_empty())
    {
        (
            "agent_home".to_string(),
            Some(h.identity_path().display().to_string()),
        )
    } else {
        (
            "minimal_inbox".to_string(),
            home.as_ref()
                .map(|h| h.identity_path().display().to_string()),
        )
    };
    let desk = DeskInfo {
        agent_id: agent_id.to_string(),
        session_id: desk_session_id,
        exists: desk_exists,
        identity_source,
        identity_path,
        fallback_identity: crate::session::minimal_inbox_identity(agent_id),
    };

    let db_path = store_path
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::store::default_db_path);
    let store = Store::open_cached(&db_path).ok();

    let (total_rows, outstanding_dispatches, latest_at) = match store.as_ref() {
        Some(db) => {
            let total = db.count_board_entries(agent_id).unwrap_or(0);
            let outstanding = db
                .board_awaiting_report(agent_id)
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            let latest = db
                .read_board(agent_id, &crate::coordination::BoardQuery::default(), 1)
                .ok()
                .and_then(|rows| rows.into_iter().next().map(|r| r.created_at));
            (total, outstanding, latest)
        }
        None => (0, 0, None),
    };
    let board = BoardInfo {
        agent_id: agent_id.to_string(),
        total_rows,
        outstanding_dispatches,
        latest_at,
    };

    let reach = match store.as_ref() {
        Some(db) => {
            let cfg = crate::coordination::reach::load(db, agent_id);
            let (in_quiet_hours, on_call) = crate::mission_autonomy::quiet_and_call_in(db);
            ReachInfo {
                agent_id: agent_id.to_string(),
                channels: cfg.channels,
                policy: cfg.policy,
                in_quiet_hours,
                on_call,
            }
        }
        None => {
            let cfg = crate::coordination::reach::Reach::default_for(agent_id);
            ReachInfo {
                agent_id: agent_id.to_string(),
                channels: cfg.channels,
                policy: cfg.policy,
                in_quiet_hours: false,
                on_call: false,
            }
        }
    };

    let mut repos = vec![MemoryRepo::global()];
    if let Some(ref ws) = inferred_workspace {
        repos.push(MemoryRepo::project(ws));
    }
    if let Some(repo) = MemoryRepo::agent(agent_id) {
        repos.push(repo);
    }
    if !session_id.trim().is_empty() {
        repos.push(MemoryRepo::session(session_id));
    }
    let memory: Vec<MemoryScopeInfo> = repos
        .into_iter()
        .map(|repo| MemoryScopeInfo {
            scope: repo.kind,
            exists: repo.exists(),
            key: repo.key,
            label: repo.label,
            path: repo.root.display().to_string(),
            entry_file: ENTRY_FILE.to_string(),
            entry_prompt_budget_chars: ENTRY_PROMPT_CHARS,
            max_entry_chars: MAX_ENTRY_CHARS,
        })
        .collect();

    let wake_root = crate::recurring::default_root();
    let schedule = crate::agent_wakes::get(&wake_root, agent_id)
        .ok()
        .flatten()
        .or_else(|| {
            (!session_id.trim().is_empty())
                .then(|| {
                    crate::agent_wakes::get(&wake_root, session_id)
                        .ok()
                        .flatten()
                })
                .flatten()
        })
        .and_then(|job| {
            let wake = job.wake?;
            Some(WakeScheduleInfo {
                job_id: job.id,
                title: job.title,
                schedule: job.schedule.display(),
                enabled: job.enabled,
                set_by: wake.set_by,
                quiet_start: wake.quiet_start,
                quiet_end: wake.quiet_end,
                max_runs_per_day: wake.max_runs_per_day,
                report_to: wake.report.to,
                next_run_at: job.enabled.then_some(job.next_run_at),
                last_run_at: job.last_run_at,
            })
        });
    let keep_warm = crate::keep_warm::get(&wake_root, agent_id)
        .ok()
        .flatten()
        .or_else(|| {
            (!session_id.trim().is_empty())
                .then(|| crate::keep_warm::get(&wake_root, session_id).ok().flatten())
                .flatten()
        })
        .map(|kw| KeepWarmInfo {
            enabled: kw.enabled,
            session_id: kw.session_id,
            interval_minutes: kw.interval_minutes,
            max_idle_hours: kw.max_idle_hours,
            set_by: kw.set_by,
            last_warm_at: kw.last_warm_at,
            warms: kw.warms,
            skipped_reason: kw.skipped_reason,
            provider_reason: kw.provider_reason,
        });
    let wakes = WakesInfo {
        schedule,
        keep_warm,
    };

    let mut role_budgets = BTreeMap::new();
    for r in ["work", "mission_control", "desk", "lane"] {
        if let Some((defs, cap_bytes)) = crate::lanes::role_tool_definitions(r, None) {
            let measured_bytes = defs.iter().map(crate::lanes::definition_byte_size).sum();
            role_budgets.insert(
                r.to_string(),
                RoleToolBudget {
                    cap_bytes,
                    measured_bytes,
                    tool_count: defs.len(),
                },
            );
        }
    }
    let custom_dir = (role == "work")
        .then(|| crate::agent_tools::tools_dir(agent_id))
        .flatten();
    let (role_defs, cap_bytes) = crate::lanes::role_tool_definitions(role, custom_dir)
        .or_else(|| crate::lanes::role_tool_definitions("work", None))
        .unwrap_or_default();
    let measured_bytes = role_defs
        .iter()
        .map(crate::lanes::definition_byte_size)
        .sum();
    let mut names: Vec<String> = role_defs.iter().map(|d| d.name.clone()).collect();
    names.sort();
    let tools = ToolsInfo {
        role: role.to_string(),
        cap_bytes,
        measured_bytes,
        tool_count: names.len(),
        names,
        role_budgets,
    };

    let profiles = std::fs::read_to_string(crate::config::default_config_path())
        .ok()
        .and_then(|raw| toml::from_str::<SnippetConfig>(&raw).ok())
        .map(|cfg| profiles_from_config(&cfg))
        .unwrap_or_default();

    let max_context_tokens = store
        .as_ref()
        .and_then(|db| db.agent_context_limit(agent_id).ok().flatten());
    let limits = LimitsInfo {
        max_context_tokens,
        min_context_tokens: crate::agent_context::MIN_CONTEXT_TOKENS,
        min_wake_interval_secs: crate::agent_wakes::MIN_AGENT_INTERVAL_SECS,
        default_max_wake_runs_per_day: crate::agent_wakes::DEFAULT_MAX_RUNS_PER_DAY,
        max_wake_runs_per_day_ceiling: crate::agent_wakes::MAX_RUNS_PER_DAY_CEILING,
        keep_warm_min_interval_minutes: crate::keep_warm::MIN_INTERVAL_MINUTES,
        keep_warm_max_interval_minutes: crate::keep_warm::MAX_INTERVAL_MINUTES,
        keep_warm_max_idle_hours_ceiling: crate::keep_warm::MAX_IDLE_HOURS_CEILING,
    };

    HarnessMap {
        sessions,
        desk,
        board,
        reach,
        memory,
        wakes,
        tools,
        profiles,
        limits,
    }
}

pub(crate) fn profiles_from_config(config: &SnippetConfig) -> Vec<ProfileInfo> {
    config
        .setups
        .iter()
        .flatten()
        .map(|(name, cfg)| ProfileInfo {
            name: name.clone(),
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            is_default: config.active_setup.as_deref() == Some(name.as_str()),
            rate_limited_until: crate::routing::limited_until(name),
        })
        .collect()
}

pub fn schema() -> Value {
    let channel_enum = json!(["chat", "push", "voice", "task_room"]);
    let setter_enum = json!(["user", "mission_control", "self"]);
    let channel_state = json!({
        "type": "object",
        "required": ["enabled", "quiet_hours"],
        "additionalProperties": false,
        "properties": {
            "enabled": {"type": "boolean"},
            "quiet_hours": {"type": "boolean"}
        }
    });
    let role_budget = json!({
        "type": "object",
        "required": ["cap_bytes", "measured_bytes", "tool_count"],
        "additionalProperties": false,
        "properties": {
            "cap_bytes": {"type": "integer"},
            "measured_bytes": {"type": "integer"},
            "tool_count": {"type": "integer"}
        }
    });

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "HarnessMap",
        "type": "object",
        "required": SECTIONS,
        "additionalProperties": false,
        "properties": {
            "sessions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "id", "title", "folder", "status", "profile", "cache",
                        "idle_secs", "settled", "keep_awake", "task_id", "is_current", "is_desk"
                    ],
                    "additionalProperties": false,
                    "properties": {
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "folder": {"type": "string"},
                        "status": {"type": "string"},
                        "profile": {"type": ["string", "null"]},
                        "cache": {"type": "string"},
                        "idle_secs": {"type": "integer"},
                        "settled": {"type": "boolean"},
                        "keep_awake": {"type": "boolean"},
                        "task_id": {"type": ["string", "null"]},
                        "is_current": {"type": "boolean"},
                        "is_desk": {"type": "boolean"}
                    }
                }
            },
            "desk": {
                "type": "object",
                "required": ["agent_id", "session_id", "exists", "identity_source", "identity_path", "fallback_identity"],
                "additionalProperties": false,
                "properties": {
                    "agent_id": {"type": "string"},
                    "session_id": {"type": "string"},
                    "exists": {"type": "boolean"},
                    "identity_source": {"type": "string"},
                    "identity_path": {"type": ["string", "null"]},
                    "fallback_identity": {"type": "string"}
                }
            },
            "board": {
                "type": "object",
                "required": ["agent_id", "total_rows", "outstanding_dispatches", "latest_at"],
                "additionalProperties": false,
                "properties": {
                    "agent_id": {"type": "string"},
                    "total_rows": {"type": "integer"},
                    "outstanding_dispatches": {"type": "integer"},
                    "latest_at": {"type": ["string", "null"]}
                }
            },
            "reach": {
                "type": "object",
                "required": ["agent_id", "channels", "policy", "in_quiet_hours", "on_call"],
                "additionalProperties": false,
                "properties": {
                    "agent_id": {"type": "string"},
                    "channels": {
                        "type": "object",
                        "additionalProperties": channel_state
                    },
                    "policy": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "array",
                            "items": {"type": "string", "enum": channel_enum}
                        }
                    },
                    "in_quiet_hours": {"type": "boolean"},
                    "on_call": {"type": "boolean"}
                }
            },
            "memory": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "scope", "key", "label", "path", "entry_file",
                        "exists", "entry_prompt_budget_chars", "max_entry_chars"
                    ],
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["global", "project", "agent", "session", "task"]},
                        "key": {"type": "string"},
                        "label": {"type": "string"},
                        "path": {"type": "string"},
                        "entry_file": {"type": "string"},
                        "exists": {"type": "boolean"},
                        "entry_prompt_budget_chars": {"type": "integer"},
                        "max_entry_chars": {"type": "integer"}
                    }
                }
            },
            "wakes": {
                "type": "object",
                "required": ["schedule", "keep_warm"],
                "additionalProperties": false,
                "properties": {
                    "schedule": {
                        "anyOf": [
                            {"type": "null"},
                            {
                                "type": "object",
                                "required": [
                                    "job_id", "title", "schedule", "enabled", "set_by",
                                    "quiet_start", "quiet_end", "max_runs_per_day",
                                    "report_to", "next_run_at", "last_run_at"
                                ],
                                "additionalProperties": false,
                                "properties": {
                                    "job_id": {"type": "string"},
                                    "title": {"type": "string"},
                                    "schedule": {"type": "string"},
                                    "enabled": {"type": "boolean"},
                                    "set_by": {"type": "string", "enum": setter_enum},
                                    "quiet_start": {"type": ["string", "null"]},
                                    "quiet_end": {"type": ["string", "null"]},
                                    "max_runs_per_day": {"type": "integer"},
                                    "report_to": {"type": "string", "enum": ["mission_control", "user", "task_room", "none"]},
                                    "next_run_at": {"type": ["integer", "null"]},
                                    "last_run_at": {"type": ["integer", "null"]}
                                }
                            }
                        ]
                    },
                    "keep_warm": {
                        "anyOf": [
                            {"type": "null"},
                            {
                                "type": "object",
                                "required": [
                                    "enabled", "session_id", "interval_minutes", "max_idle_hours",
                                    "set_by", "last_warm_at", "warms", "skipped_reason", "provider_reason"
                                ],
                                "additionalProperties": false,
                                "properties": {
                                    "enabled": {"type": "boolean"},
                                    "session_id": {"type": "string"},
                                    "interval_minutes": {"type": "integer"},
                                    "max_idle_hours": {"type": "integer"},
                                    "set_by": {"type": "string", "enum": setter_enum},
                                    "last_warm_at": {"type": ["integer", "null"]},
                                    "warms": {"type": "integer"},
                                    "skipped_reason": {"type": ["string", "null"]},
                                    "provider_reason": {"type": ["string", "null"]}
                                }
                            }
                        ]
                    }
                }
            },
            "tools": {
                "type": "object",
                "required": ["role", "cap_bytes", "measured_bytes", "tool_count", "names", "role_budgets"],
                "additionalProperties": false,
                "properties": {
                    "role": {"type": "string"},
                    "cap_bytes": {"type": "integer"},
                    "measured_bytes": {"type": "integer"},
                    "tool_count": {"type": "integer"},
                    "names": {"type": "array", "items": {"type": "string"}},
                    "role_budgets": {
                        "type": "object",
                        "additionalProperties": role_budget
                    }
                }
            },
            "profiles": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "provider", "model", "is_default", "rate_limited_until"],
                    "additionalProperties": false,
                    "properties": {
                        "name": {"type": "string"},
                        "provider": {"type": "string"},
                        "model": {"type": "string"},
                        "is_default": {"type": "boolean"},
                        "rate_limited_until": {"type": ["integer", "null"]}
                    }
                }
            },
            "limits": {
                "type": "object",
                "required": [
                    "max_context_tokens", "min_context_tokens", "min_wake_interval_secs",
                    "default_max_wake_runs_per_day", "max_wake_runs_per_day_ceiling",
                    "keep_warm_min_interval_minutes", "keep_warm_max_interval_minutes",
                    "keep_warm_max_idle_hours_ceiling"
                ],
                "additionalProperties": false,
                "properties": {
                    "max_context_tokens": {"type": ["integer", "null"]},
                    "min_context_tokens": {"type": "integer"},
                    "min_wake_interval_secs": {"type": "integer"},
                    "default_max_wake_runs_per_day": {"type": "integer"},
                    "max_wake_runs_per_day_ceiling": {"type": "integer"},
                    "keep_warm_min_interval_minutes": {"type": "integer"},
                    "keep_warm_max_interval_minutes": {"type": "integer"},
                    "keep_warm_max_idle_hours_ceiling": {"type": "integer"}
                }
            }
        }
    })
}

#[derive(Default, Deserialize)]
struct HarnessMapArgs {
    #[serde(default)]
    section: Option<String>,
}

pub struct HarnessMapTool;

#[async_trait]
impl Tool for HarnessMapTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "harness_map".into(),
            description: "Read your own structure as data: sessions, desk, board, reach, memory, wakes, tools, profiles, and limits. Pass `section` to return a single key.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "section": {
                        "type": "string",
                        "enum": SECTIONS
                    }
                },
                "required": [],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        if ctx.lane_origin().is_some() {
            return Err(ToolError::msg("harness_map is not available in lanes"));
        }
        let args: HarnessMapArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let section = args
            .section
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(sec) = section
            && !SECTIONS.contains(&sec)
        {
            return Err(ToolError::msg(format!(
                "unknown section `{sec}`; use one of: {}",
                SECTIONS.join(", ")
            )));
        }
        let agent_id = if ctx.is_mission_control() {
            crate::mission_control::SESSION_ID.to_string()
        } else if let Some(("agent", id)) = crate::coordination_tools::speaks_as(ctx) {
            id
        } else {
            ctx.agent_id()
                .unwrap_or(crate::coordination::SNIPPET_AGENT_ID)
                .to_string()
        };
        let session_id = if ctx.is_mission_control() {
            ctx.durable_session_id()
                .unwrap_or(crate::mission_control::SESSION_ID)
        } else {
            ctx.durable_session_id().unwrap_or("")
        };
        let role = if ctx.is_mission_control() || session_id == crate::mission_control::SESSION_ID {
            "mission_control"
        } else if crate::session::is_inbox_session_id(session_id) {
            "desk"
        } else {
            "work"
        };
        let map = build_with(
            &agent_id,
            session_id,
            role,
            ctx.store_path().as_deref(),
            Some(ctx.workspace_root()),
        );
        let full = serde_json::to_value(&map).map_err(ToolError::from)?;
        match section {
            Some(sec) => {
                let part = full.get(sec).cloned().unwrap_or(Value::Null);
                Ok(ToolResult::success(json!({ sec: part })))
            }
            None => Ok(ToolResult::success(full)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InferenceProfileConfig;

    fn matches_type(val: &Value, ty: &str) -> bool {
        match ty {
            "object" => val.is_object(),
            "array" => val.is_array(),
            "string" => val.is_string(),
            "integer" => val.is_i64() || val.is_u64(),
            "boolean" => val.is_boolean(),
            "null" => val.is_null(),
            _ => false,
        }
    }

    fn validate_node(val: &Value, schema: &Value, path: &str) {
        if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
            assert!(
                any_of.iter().any(|branch| {
                    branch
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|t| matches_type(val, t))
                }),
                "{path} did not match anyOf branches: {val}"
            );
            for branch in any_of {
                if branch.get("type").and_then(Value::as_str) == Some("object") && val.is_object() {
                    validate_node(val, branch, path);
                }
            }
            return;
        }

        if let Some(ty) = schema.get("type") {
            let ok = if let Some(s) = ty.as_str() {
                matches_type(val, s)
            } else if let Some(arr) = ty.as_array() {
                arr.iter()
                    .filter_map(Value::as_str)
                    .any(|s| matches_type(val, s))
            } else {
                false
            };
            assert!(ok, "{path}: expected {ty}, got {val}");
        }

        if let Some(obj) = val.as_object() {
            if let Some(req) = schema.get("required").and_then(Value::as_array) {
                for key in req.iter().filter_map(Value::as_str) {
                    assert!(obj.contains_key(key), "{path} missing required key `{key}`");
                }
            }
            let props = schema.get("properties").and_then(Value::as_object);
            let addl = schema.get("additionalProperties");
            for (k, v) in obj {
                let child = format!("{path}.{k}");
                if let Some(prop_schema) = props.and_then(|p| p.get(k)) {
                    validate_node(v, prop_schema, &child);
                } else if let Some(addl_schema) = addl {
                    if addl_schema.as_bool() == Some(false) {
                        panic!("{path} has unexpected key `{k}`");
                    } else if addl_schema.is_object() {
                        validate_node(v, addl_schema, &child);
                    }
                }
            }
        }

        if let Some(arr) = val.as_array()
            && let Some(item_schema) = schema.get("items")
        {
            for (idx, item) in arr.iter().enumerate() {
                validate_node(item, item_schema, &format!("{path}[{idx}]"));
            }
        }
    }

    #[test]
    fn build_output_validates_against_schema() {
        let map = build("researcher", "inbox-researcher", "desk");
        let value = serde_json::to_value(&map).unwrap();
        let schema = schema();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), SECTIONS.len());
        for key in SECTIONS {
            assert!(obj.contains_key(key), "missing top-level key `{key}`");
        }
        validate_node(&value, &schema, "$");
    }

    #[test]
    fn profiles_never_expose_api_keys() {
        let mut config = SnippetConfig::default();
        config.model.api_key = "sk-live-super-secret-root-key".into();
        let mut setups = BTreeMap::new();
        let mut prof = InferenceProfileConfig::default();
        prof.provider = "anthropic".into();
        prof.model = "claude-sonnet-4-5".into();
        prof.api_key = "sk-ant-super-secret-profile-key".into();
        setups.insert("fast".into(), prof);
        config.setups = Some(setups);
        config.active_setup = Some("fast".into());

        let profiles = profiles_from_config(&config);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "fast");
        assert_eq!(profiles[0].provider, "anthropic");
        assert_eq!(profiles[0].model, "claude-sonnet-4-5");
        assert!(profiles[0].is_default);

        let dumped = serde_json::to_string(&profiles).unwrap();
        assert!(!dumped.contains("api_key"));
        assert!(!dumped.contains("sk-ant-super-secret"));
        assert!(!dumped.contains("sk-live-super-secret"));
    }

    #[tokio::test]
    async fn tool_supports_section_filter_and_refuses_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("snippet.db")).unwrap();
        drop(store);
        let ctx = ToolContext::new(dir.path())
            .unwrap()
            .with_durable_session_id("sess-work")
            .with_store_path(dir.path().join("snippet.db"))
            .with_agent_id("worker-1");

        let full = HarnessMapTool.execute(&ctx, json!({})).await.unwrap();
        let data = &full.value["data"];
        for key in SECTIONS {
            assert!(data.get(key).is_some(), "missing `{key}`");
        }

        let single = HarnessMapTool
            .execute(&ctx, json!({"section": "tools"}))
            .await
            .unwrap();
        let single_obj = single.value["data"].as_object().unwrap();
        assert_eq!(single_obj.len(), 1);
        assert_eq!(single_obj["tools"]["role"], "work");

        assert!(
            HarnessMapTool
                .execute(&ctx, json!({"section": "nope"}))
                .await
                .is_err()
        );

        let lane_ctx = ctx.with_lane_origin("sess-work", "lane-1");
        assert!(HarnessMapTool.execute(&lane_ctx, json!({})).await.is_err());
    }
}
