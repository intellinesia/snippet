// forkpty lives in libutil on glibc.
#[cfg(unix)]
#[link(name = "util")]
unsafe extern "C" {}

pub mod accounts;
pub mod agent_context;
pub mod agent_tools;
pub mod agent_wakes;
pub mod ambient;
pub mod ambient_tuning;
pub mod anthropic;
pub mod antigravity;
pub mod app_schema;
pub mod app_store;
pub mod artifacts;
pub mod bg;
pub mod builtins;
pub mod catalog;
pub mod chatgpt;
pub mod chatgpt_auth;
pub mod checkpoint;
pub mod claude_usage;
pub mod config;
pub mod conversations;
pub mod coordination;
pub mod coordination_tools;
pub mod devin;
pub mod devin_usage;
pub mod doctor;
pub mod gemini;
pub mod goal_plan;
pub mod goal_tools;
pub mod harness;
pub mod harness_map;
pub mod history_archive;
pub mod inline;
pub mod keep_warm;
mod lane_log;
pub mod lanes;
pub mod llm;
pub mod members;
pub mod memory;
pub mod meta;
pub mod mission_autonomy;
pub mod mission_control;
pub mod mission_tools;
pub mod openai;
pub mod ownership;
pub mod profiles;
pub mod progress;
pub mod prompts;
pub mod reasoning;
pub mod recording_policy;
pub mod recordings;
pub mod recurring;
pub mod review_tools;
pub mod routing;
pub mod sanitize;
pub mod serve;
pub mod session;
pub mod session_resolve;
pub mod session_stats;
pub mod settle;
pub mod shell_guard;
pub mod signals;
pub mod skills;
pub mod speech;
pub mod sse;
pub mod store;
pub mod stt;
pub mod task_state_tools;
pub mod term;
pub mod tools;
pub mod tts;
pub mod tts_stream;
pub mod tui;
pub mod update;
pub mod usage_ledger;
pub mod vault;
pub mod voiceprint;
pub mod watches;
pub mod xai;
pub mod xai_auth;

pub use harness::{
    CodingHarness, GoalStatus, HarnessConfig, HarnessEvent, HarnessOutcome, HarnessState,
    HarnessStatus, LoopInput,
};
pub use lanes::{LaneManager, LaneRecord, LaneResult, LaneStatus, ModelFactory};
pub use llm::{AgentModel, GeneratedToolCall, ModelOutput, NativeToolDefinition};
pub use tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
