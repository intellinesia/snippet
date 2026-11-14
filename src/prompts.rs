//! System-prompt composition.
//!
//! A stable execution/conversation core is always present. Capability- and
//! environment-specific guidance (git worktree, skills, vault, memory)
//! is appended only when it applies, so an absent capability costs no tokens.

pub const HARNESS_CORE: &str = include_str!("../prompts/harness_core.md");
pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const CONVERSATION_AGENT_LAYER: &str = include_str!("../prompts/conversation_agent_layer.md");
pub const MISSION_CONTROL_LAYER: &str = include_str!("../prompts/mission_control_layer.md");
pub const COORDINATION_LAYER: &str = include_str!("../prompts/coordination_layer.md");
pub const DELEGATION_LAYER: &str = include_str!("../prompts/delegation_layer.md");
pub const TASK_LAYER: &str = include_str!("../prompts/task_layer.md");
pub const GIT_WORKTREE_LAYER: &str = include_str!("../prompts/git_worktree_layer.md");
pub const MEMORY_GUIDANCE_LAYER: &str = include_str!("../prompts/memory_layer.md");
pub const MEMORY_WRITE_LAYER: &str = include_str!("../prompts/memory_write_layer.md");
pub const SKILLS_LAYER: &str = include_str!("../prompts/skills_layer.md");
pub const VAULT_LAYER: &str = include_str!("../prompts/vault_layer.md");
pub const VISION_LAYER: &str = include_str!("../prompts/vision_layer.md");
pub const NO_VISION_LAYER: &str = include_str!("../prompts/no_vision_layer.md");

/// Capability/environment facts that decide which optional layers render.
/// These are a session-start snapshot, so the assembled prompt stays stable
/// across a session's turns (prompt-cache friendly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PromptContext {
    /// The session workspace is a linked git worktree (not the main checkout).
    pub worktree: bool,
    /// Per-workspace memory is enabled; the index/patterns/rules block is
    /// appended separately at session start.
    pub memory: bool,
    /// This session may write durable memory (main session, not read-only lanes).
    pub memory_writable: bool,
    /// At least one skill is installed.
    pub skills: bool,
    /// The vault holds at least one secret.
    pub vault: bool,
    /// This is an agent's COORDINATION session: it answers direct messages and
    /// dispatches work, and holds no workspace tools. The layer is what tells it
    /// how to behave, which is not inferable from the tool list alone.
    pub coordination: bool,
    /// The session's model accepts image input.
    pub images: bool,
}

impl PromptContext {
    /// Detect the capability/environment snapshot for `workspace`. Memory
    /// enablement is supplied by the caller (it owns that fact); skills and
    /// vault are probed here.
    pub fn detect(workspace: &std::path::Path, memory: bool, memory_writable: bool) -> Self {
        Self {
            worktree: crate::session::workspace_is_worktree(workspace),
            memory,
            memory_writable: memory && memory_writable,
            skills: !crate::skills::discover().is_empty(),
            vault: !crate::vault::Vault::load().is_empty(),
            // Not inferable from the environment; the role's own constructor sets it.
            coordination: false,
            images: false,
        }
    }

    fn conditional_layers(&self) -> Vec<&'static str> {
        let mut layers = Vec::new();
        if self.worktree {
            layers.push(GIT_WORKTREE_LAYER.trim());
        }
        if self.skills {
            layers.push(SKILLS_LAYER.trim());
        }
        if self.vault {
            layers.push(VAULT_LAYER.trim());
        }
        layers.push(if self.images {
            VISION_LAYER.trim()
        } else {
            NO_VISION_LAYER.trim()
        });
        if self.memory {
            layers.push(MEMORY_GUIDANCE_LAYER.trim());
        }
        if self.memory && self.memory_writable {
            layers.push(MEMORY_WRITE_LAYER.trim());
        }
        if self.coordination {
            layers.push(COORDINATION_LAYER.trim());
        }
        layers
    }
}

pub fn coding_prompt(context: &PromptContext) -> String {
    let mut parts = vec![CODING_AGENT_LAYER.trim(), HARNESS_CORE.trim()];
    parts.extend(context.conditional_layers());
    parts.join("\n\n")
}

pub fn conversation_prompt(context: &PromptContext) -> String {
    let mut parts = vec![CODING_AGENT_LAYER.trim(), HARNESS_CORE.trim()];
    parts.extend(context.conditional_layers());
    parts.push(CONVERSATION_AGENT_LAYER.trim());
    parts.push(DELEGATION_LAYER.trim());
    parts.push(TASK_LAYER.trim());
    parts.join("\n\n")
}

/// Base execution prompt with no optional capabilities — used by tests and any
/// caller that has not computed a `PromptContext`.
pub fn coding_system_prompt() -> String {
    coding_prompt(&PromptContext::default())
}

/// The prompt for an agent's COORDINATION session.
///
/// Deliberately does NOT include [`CODING_AGENT_LAYER`]: that layer states the
/// session has real bash and full filesystem access, which is false here and
/// would instruct the model to reach for tools it was not given. The
/// coordination layer states the real capability set instead, and the
/// per-workspace memory layers are excluded for the same reason — this session
/// has no workspace to hold memory about.
pub fn coordination_prompt(_context: &PromptContext) -> String {
    // No vault layer: it is about commands this session has no
    // shell to run. COORDINATION_LAYER is last so its statements about what
    // this session can and cannot do are the final word.
    [
        CONVERSATION_AGENT_LAYER.trim(),
        HARNESS_CORE.trim(),
        COORDINATION_LAYER.trim(),
    ]
    .join("\n\n")
}

pub fn mission_control_system_prompt() -> String {
    // Orchestrator only. Do not stack CODING_AGENT_LAYER — its
    // identities ("full filesystem", "own the task end to end") made Mission
    // Control advertise as a general engineer and skip list_sessions.
    let brief = crate::mission_autonomy::read_brief();
    if brief.trim().is_empty() {
        [HARNESS_CORE.trim(), MISSION_CONTROL_LAYER.trim()].join("\n\n")
    } else {
        format!(
            "{}\n\n{}\n\n## Your brief (as of this session's start)\n\n{}",
            HARNESS_CORE.trim(),
            MISSION_CONTROL_LAYER.trim(),
            brief.trim()
        )
    }
}

/// The normal shared session contract plus a bounded researched identity.
/// The role overlay changes judgment and specialization, not the session's
/// execution, safety, or conversation rules.
pub struct SpecializedAgentPromptContext<'a> {
    pub agent_id: &'a str,
    pub identity: &'a str,
    pub context: &'a PromptContext,
}

/// The identity overlay appended to whichever base a session runs on.
///
/// One definition so a specialized coding session and a coordination session
/// cannot disagree about how the agent is introduced to itself.
fn identity_overlay(agent_id: &str, identity: &str) -> String {
    format!(
        "## Your identity: {agent_id}\n\nThe identity below sets your expertise, judgment and voice. Everything above still governs how you work, talk and finish.\n\n{identity}\n\n## Improving yourself\n\nAfter a task, judge the run against your playbook for that kind of work and update the playbook in your agent memory with what held up or failed. When a lesson changes how your role should work, send Mission Control a proposed identity change rather than editing your identity alone. Call `harness_map` whenever you are unsure what you are made of or how you reach the user.\n",
        identity = identity.trim(),
    )
}

/// A delegated lane's prompt: the execution contract, plus the identity of the
/// agent it was assigned, when there is one.
const LANE_NOTE: &str = "## Working as a lane\n\nYou've taken one piece of a colleague's work: the agent that handed it to you carries on with the rest, and you run until your piece is done, then report once with `terminate_loop`. The brief's footer says who handed it over, what it's part of and which other lanes are working beside you.\n\nThe user isn't here, and there's no `monitor` or `update_plan`, so where the guidance above mentions them, adapt: run a long command in the foreground with a generous timeout (or poll its log a few times) rather than ending your turn to wait. You do have colleagues. The agent that handed you this is a `message_parent` away: ask it what you can't settle yourself (wait for the answer when you can't sensibly go on without it), and tell it early about anything that changes its plan or another lane's piece. An agent's reply can take a few minutes: carry on with your piece meanwhile, and when only waiting is left, wait in the shell (`sleep 30`, a few times) rather than asking again. For everything else, apply \"Decide or ask\" with the agent that handed you this standing in for the user: decide, and say in your report what you assumed; a real fork or an irreversible step is a question to it, and if only the user can decide, stop and report it as the blocker.";

pub fn lane_prompt(context: &PromptContext, identity: Option<(&str, &str)>) -> String {
    let base = format!("{}\n\n{LANE_NOTE}", coding_prompt(context));
    match identity {
        Some((agent_id, body)) => format!("{base}\n\n{}", identity_overlay(agent_id, body)),
        None => base,
    }
}

pub fn specialized_agent_system_prompt(context: SpecializedAgentPromptContext<'_>) -> String {
    format!(
        "{base}\n\n{overlay}",
        base = conversation_prompt(context.context),
        overlay = identity_overlay(context.agent_id, context.identity),
    )
}

/// The prompt for an agent's coordination session: the restricted coordination
/// contract plus the agent's own identity.
///
/// Uses [`coordination_prompt`] rather than the conversation prompt so the
/// session is not told it has bash and full filesystem access it was never
/// given.
pub fn specialized_coordination_prompt(context: SpecializedAgentPromptContext<'_>) -> String {
    format!(
        "{base}\n\n{overlay}",
        base = coordination_prompt(context.context),
        overlay = identity_overlay(context.agent_id, context.identity),
    )
}

#[cfg(test)]
mod self_improvement_tests {
    use super::*;

    #[test]
    fn identity_overlay_includes_bounded_self_improvement_paragraph() {
        let ctx = PromptContext::default();
        let prompt = specialized_agent_system_prompt(SpecializedAgentPromptContext {
            agent_id: "researcher",
            identity: "# Researcher\n\nFinds primary sources.",
            context: &ctx,
        });
        let section = prompt
            .split("## Improving yourself\n\n")
            .nth(1)
            .expect("missing Improving yourself section")
            .trim();
        assert!(section.contains("playbook"));
        assert!(section.contains("Mission Control"));
        assert!(section.contains("harness_map"));
        assert!(!section.contains("\n- ") && !section.contains("\n* "));
        let sentences = section.split('.').filter(|s| !s.trim().is_empty()).count();
        assert!(
            sentences <= 4,
            "expected at most 4 sentences, got {sentences}: {section}"
        );
    }
}

#[cfg(test)]
mod seat_layer_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    const HARNESS_CORE_MAX_BYTES: usize = 7_000;
    const COORDINATION_LAYER_MAX_BYTES: usize = 3_000;
    const DELEGATION_LAYER_MAX_BYTES: usize = 3_500;
    const TASK_LAYER_MAX_BYTES: usize = 4_000;

    #[test]
    fn every_seat_prompt_includes_harness_core_exactly_once() {
        let ctx = PromptContext::default();
        let heading = "## The harness you live in";
        for (name, prompt) in [
            ("coding_system_prompt", coding_system_prompt()),
            ("conversation_prompt", conversation_prompt(&ctx)),
            ("coordination_prompt", coordination_prompt(&ctx)),
            (
                "mission_control_system_prompt",
                mission_control_system_prompt(),
            ),
        ] {
            let count = prompt.matches(heading).count();
            assert_eq!(
                count, 1,
                "{name} expected `{heading}` once, found {count} times"
            );
        }
    }

    #[test]
    fn layer_sizes_stay_within_budget() {
        assert!(
            HARNESS_CORE.len() < HARNESS_CORE_MAX_BYTES,
            "HARNESS_CORE is {} bytes (limit {HARNESS_CORE_MAX_BYTES})",
            HARNESS_CORE.len()
        );
        assert!(
            COORDINATION_LAYER.len() < COORDINATION_LAYER_MAX_BYTES,
            "COORDINATION_LAYER is {} bytes (limit {COORDINATION_LAYER_MAX_BYTES})",
            COORDINATION_LAYER.len()
        );
        assert!(
            DELEGATION_LAYER.len() < DELEGATION_LAYER_MAX_BYTES,
            "DELEGATION_LAYER is {} bytes (limit {DELEGATION_LAYER_MAX_BYTES})",
            DELEGATION_LAYER.len()
        );
        assert!(
            TASK_LAYER.len() < TASK_LAYER_MAX_BYTES,
            "TASK_LAYER is {} bytes (limit {TASK_LAYER_MAX_BYTES})",
            TASK_LAYER.len()
        );
    }

    #[test]
    fn only_harness_core_has_envelope_overview_or_list() {
        let non_core_layers = [
            ("CODING_AGENT_LAYER", CODING_AGENT_LAYER),
            ("CONVERSATION_AGENT_LAYER", CONVERSATION_AGENT_LAYER),
            ("MISSION_CONTROL_LAYER", MISSION_CONTROL_LAYER),
            ("COORDINATION_LAYER", COORDINATION_LAYER),
            ("DELEGATION_LAYER", DELEGATION_LAYER),
            ("TASK_LAYER", TASK_LAYER),
            ("GIT_WORKTREE_LAYER", GIT_WORKTREE_LAYER),
            ("MEMORY_GUIDANCE_LAYER", MEMORY_GUIDANCE_LAYER),
            ("MEMORY_WRITE_LAYER", MEMORY_WRITE_LAYER),
            ("SKILLS_LAYER", SKILLS_LAYER),
            ("VAULT_LAYER", VAULT_LAYER),
            ("VISION_LAYER", VISION_LAYER),
            ("NO_VISION_LAYER", NO_VISION_LAYER),
            ("LANE_NOTE", LANE_NOTE),
        ];
        for (name, body) in non_core_layers {
            assert!(
                !body.contains("What reaches you comes as typed envelopes"),
                "{name} must not repeat envelope overview"
            );
            assert!(
                !(body.contains("[direct_message]") && body.contains("[group_message]")),
                "{name} must not repeat the [direct_message] envelope list"
            );
        }
    }

    #[test]
    fn backticked_tool_names_in_prompts_are_known_or_allowlisted() {
        let mut known = BTreeSet::new();
        for role in ["work", "mission_control", "desk", "lane"] {
            if let Some((defs, _)) = crate::lanes::role_tool_definitions(role, None) {
                for def in defs {
                    known.insert(def.name);
                }
            }
        }
        for name in crate::meta::META_TOOL_NAMES {
            known.insert(name.to_string());
        }
        known.insert(crate::meta::terminate_loop_tool().name);

        let mut reg = crate::tools::ToolRegistry::new();
        crate::coordination_tools::add_coordination_tools(&mut reg);
        crate::mission_tools::add_mission_control_tools(&mut reg);
        crate::mission_tools::add_worker_report_tool(&mut reg);
        crate::mission_tools::add_coordination_session_tools(&mut reg);
        for def in reg.definitions() {
            known.insert(def.name);
        }

        // Sibling-branch chat tools plus non-tool snake_case parameters, enums and reactions.
        let allow: BTreeSet<&str> = [
            "chats",
            "read_chat",
            "post_chat",
            "agent_id",
            "artifact_id",
            "blocked_by",
            "continuation_of",
            "handoff_mode",
            "heard_none",
            "in_reply_to",
            "in_review",
            "include_hidden",
            "in_progress",
            "interrupted_reply",
            "keep_awake",
            "lane_id",
            "mission_control",
            "next_step",
            "other_party",
            "played_audio",
            "read_only",
            "reply_in",
            "reply_to",
            "review_escalated",
            "schedule_wake",
            "room_limit",
            "set_rule",
            "snooze_days",
            "task_id",
            "thumbs_up",
            "updated_at",
            "verify_mode",
            "waiting_on",
            "wake_at",
        ]
        .into_iter()
        .collect();

        let prompts_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("prompts");
        let mut unknown = Vec::new();
        for entry in std::fs::read_dir(&prompts_dir).expect("read prompts dir") {
            let path = entry.expect("prompt entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read prompt file");
            let mut parts = text.split('`');
            let _ = parts.next();
            while let Some(span) = parts.next() {
                if parts.next().is_none() {
                    break;
                }
                if span.contains('_')
                    && !span.is_empty()
                    && span.bytes().all(|b| matches!(b, b'a'..=b'z' | b'_'))
                    && !known.contains(span)
                    && !allow.contains(span)
                {
                    unknown.push(format!("{}: `{span}`", path.display()));
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "unknown backticked tool-like identifiers in prompts/*.md: {unknown:?}"
        );
    }
}
