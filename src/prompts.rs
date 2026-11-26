//! System-prompt composition.
//!
//! A stable execution/conversation core is always present. Capability- and
//! environment-specific guidance (git worktree, skills, vault, memory)
//! is appended only when it applies, so an absent capability costs no tokens.

pub const HARNESS_CORE: &str = include_str!("../prompts/harness_core.md");
pub const CODING_AGENT_LAYER: &str = include_str!("../prompts/coding_agent_layer.md");
pub const WORK_SESSION_LAYER: &str = include_str!("../prompts/work_session_layer.md");
pub const LEAD_LAYER: &str = include_str!("../prompts/lead_layer.md");
pub const DESK_LAYER: &str = include_str!("../prompts/desk_layer.md");
pub const DESK_GUIDE: &str = include_str!("../prompts/desk_guide.md");
pub const LANE_LAYER: &str = include_str!("../prompts/lane_layer.md");
pub const GIT_WORKTREE_LAYER: &str = include_str!("../prompts/git_worktree_layer.md");
pub const MEMORY_GUIDANCE_LAYER: &str = include_str!("../prompts/memory_layer.md");
pub const MEMORY_WRITE_LAYER: &str = include_str!("../prompts/memory_write_layer.md");
pub const SKILLS_LAYER: &str = include_str!("../prompts/skills_layer.md");
pub const VAULT_LAYER: &str = include_str!("../prompts/vault_layer.md");
pub const VISION_LAYER: &str = include_str!("../prompts/vision_layer.md");
pub const NO_VISION_LAYER: &str = include_str!("../prompts/no_vision_layer.md");

pub fn desk_guide_topics() -> Vec<&'static str> {
    DESK_GUIDE
        .lines()
        .filter_map(|l| l.strip_prefix("## "))
        .map(str::trim)
        .collect()
}

pub fn desk_guide_section(topic: &str) -> Option<&'static str> {
    let topic = topic.trim().trim_start_matches('#').trim();
    let heading = format!("## {topic}");
    let start = DESK_GUIDE
        .match_indices(&heading)
        .map(|(i, _)| i)
        .find(|&i| {
            (i == 0 || DESK_GUIDE[..i].ends_with('\n'))
                && DESK_GUIDE[i + heading.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| c == '\n')
        })?;
    let rest = &DESK_GUIDE[start..];
    let end = rest[heading.len()..]
        .find("\n## ")
        .map(|i| i + heading.len() + 1)
        .unwrap_or(rest.len());
    Some(rest[..end].trim_end())
}

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
    /// The session's `managed_by` is `agent`: an agent leads it, not the user.
    pub agent_managed: bool,
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
            agent_managed: false,
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
        layers
    }
}

pub fn coding_prompt(context: &PromptContext) -> String {
    let mut parts = vec![CODING_AGENT_LAYER.trim(), HARNESS_CORE.trim()];
    parts.extend(context.conditional_layers());
    parts.join("\n\n")
}

const HUMAN_MANAGED_LINE: &str =
    "The user leads this session; your desk keeps track but does not drive it.";
const AGENT_MANAGED_LINE: &str = "This is work you took on at your desk, toward its objective. The user may drop in: take their word as direction; the work stays yours.";

fn work_session_layer(context: &PromptContext) -> String {
    let line = if context.agent_managed {
        AGENT_MANAGED_LINE
    } else {
        HUMAN_MANAGED_LINE
    };
    WORK_SESSION_LAYER.trim().replace("{managed_line}", line)
}

pub fn conversation_prompt(context: &PromptContext) -> String {
    let mut parts = vec![
        CODING_AGENT_LAYER.trim().to_string(),
        HARNESS_CORE.trim().to_string(),
    ];
    parts.extend(context.conditional_layers().into_iter().map(String::from));
    parts.push(work_session_layer(context));
    parts.join("\n\n")
}

/// Base execution prompt with no optional capabilities — used by tests and any
/// caller that has not computed a `PromptContext`.
pub fn coding_system_prompt() -> String {
    coding_prompt(&PromptContext::default())
}

/// The prompt for an agent's desk: the harness core and the desk layer only.
///
/// No [`CODING_AGENT_LAYER`] (it claims a full shell and filesystem the desk
/// does not have) and no workspace memory or vault layers.
pub fn coordination_prompt(_context: &PromptContext) -> String {
    [HARNESS_CORE.trim(), DESK_LAYER.trim()].join("\n\n")
}

pub fn lead_system_prompt() -> String {
    // Orchestrator only. Do not stack CODING_AGENT_LAYER — its
    // identities ("full filesystem", "own the task end to end") made Mission
    // Control advertise as a general engineer and skip list_sessions.
    let brief = crate::mission_autonomy::read_brief();
    if brief.trim().is_empty() {
        [HARNESS_CORE.trim(), LEAD_LAYER.trim()].join("\n\n")
    } else {
        format!(
            "{}\n\n{}\n\n## Your brief (as of this session's start)\n\n{}",
            HARNESS_CORE.trim(),
            LEAD_LAYER.trim(),
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
        "## Your identity: {agent_id}\n\nThis section is injected from your agent identity: it sets your expertise, judgment and voice. Everything above still governs how you work, talk and finish.\n\n{identity}\n\n## Improving yourself\n\nAfter a task, judge the run against your playbook for that kind of work and update the playbook in your agent memory with what held up or failed. A lesson that changes how your role itself should work goes on your board: your desk weighs it when it reflects and may change your identity, and the user and Lead are told.\n",
        identity = identity.trim(),
    )
}

/// A delegated lane's prompt: the execution contract and the lane layer,
/// without the harness core, plus the assigned agent's identity.
pub fn lane_prompt(context: &PromptContext, identity: Option<(&str, &str)>) -> String {
    let mut parts = vec![CODING_AGENT_LAYER.trim()];
    parts.extend(context.conditional_layers());
    parts.push(LANE_LAYER.trim());
    let base = parts.join("\n\n");
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
        assert!(section.contains("Lead"));
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
    use std::collections::{BTreeMap, BTreeSet};

    const HARNESS_CORE_MAX_BYTES: usize = 5_100;
    const DESK_LAYER_MAX_BYTES: usize = 7_000;
    const WORK_SESSION_LAYER_MAX_BYTES: usize = 3_400;
    const LANE_LAYER_MAX_BYTES: usize = 1_600;

    #[test]
    fn every_seat_prompt_includes_harness_core_exactly_once() {
        let ctx = PromptContext::default();
        let heading = "## The harness you live in";
        for (name, prompt) in [
            ("coding_system_prompt", coding_system_prompt()),
            ("conversation_prompt", conversation_prompt(&ctx)),
            ("coordination_prompt", coordination_prompt(&ctx)),
            ("lead_system_prompt", lead_system_prompt()),
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
        for (name, body, limit) in [
            ("DESK_LAYER", DESK_LAYER, DESK_LAYER_MAX_BYTES),
            (
                "WORK_SESSION_LAYER",
                WORK_SESSION_LAYER,
                WORK_SESSION_LAYER_MAX_BYTES,
            ),
            ("LANE_LAYER", LANE_LAYER, LANE_LAYER_MAX_BYTES),
        ] {
            assert!(
                body.len() < limit,
                "{name} is {} bytes (limit {limit})",
                body.len()
            );
        }
    }

    const DESK_GUIDE_MAX_BYTES: usize = 9_000;

    #[test]
    fn desk_guide_fits_and_covers_every_topic_the_desk_layer_names() {
        assert!(
            DESK_GUIDE.len() <= DESK_GUIDE_MAX_BYTES,
            "desk_guide.md is {} bytes (limit {DESK_GUIDE_MAX_BYTES})",
            DESK_GUIDE.len()
        );
        let named: Vec<&str> = DESK_LAYER
            .split_once("topics: ")
            .expect("desk_layer.md names the desk.guide topics")
            .1
            .split_once('.')
            .unwrap()
            .0
            .split(',')
            .map(str::trim)
            .collect();
        let topics = desk_guide_topics();
        assert_eq!(
            topics,
            [
                "loop",
                "threads",
                "drafting",
                "orchestration",
                "parallelism"
            ]
        );
        for topic in &named {
            let section = desk_guide_section(topic)
                .unwrap_or_else(|| panic!("desk_guide.md has no `## {topic}`"));
            assert!(section.starts_with(&format!("## {topic}\n")), "{section}");
            assert!(
                !section[3..].contains("\n## "),
                "{topic} runs into the next topic"
            );
        }
        assert_eq!(named.len(), topics.len());
        assert!(
            desk_guide_section("parallelism")
                .unwrap()
                .contains("slot.claim")
        );
        assert!(desk_guide_section("nope").is_none());
    }

    #[test]
    fn only_harness_core_has_envelope_overview_or_list() {
        let non_core_layers = [
            ("CODING_AGENT_LAYER", CODING_AGENT_LAYER),
            ("WORK_SESSION_LAYER", WORK_SESSION_LAYER),
            ("LEAD_LAYER", LEAD_LAYER),
            ("DESK_LAYER", DESK_LAYER),
            ("LANE_LAYER", LANE_LAYER),
            ("GIT_WORKTREE_LAYER", GIT_WORKTREE_LAYER),
            ("MEMORY_GUIDANCE_LAYER", MEMORY_GUIDANCE_LAYER),
            ("MEMORY_WRITE_LAYER", MEMORY_WRITE_LAYER),
            ("SKILLS_LAYER", SKILLS_LAYER),
            ("VAULT_LAYER", VAULT_LAYER),
            ("VISION_LAYER", VISION_LAYER),
            ("NO_VISION_LAYER", NO_VISION_LAYER),
        ];
        for (name, body) in non_core_layers {
            assert!(
                !body.contains("What reaches you comes as typed envelopes"),
                "{name} must not repeat envelope overview"
            );
            assert!(
                !(body.contains("<direct_message>") && body.contains("<group_message>")),
                "{name} must not repeat the <direct_message> envelope list"
            );
        }
    }

    #[test]
    fn backticked_tool_names_in_prompts_are_known_or_allowlisted() {
        let mut role_tools: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        let mut all_known: BTreeSet<String> = BTreeSet::new();
        for role in ["work", "lead", "desk", "lane"] {
            let (defs, _) = crate::lanes::role_tool_definitions(role, None)
                .unwrap_or_else(|| panic!("missing role_tool_definitions for {role}"));
            let mut names: BTreeSet<String> = BTreeSet::new();
            for d in defs {
                if let Some(actions) = d
                    .input_schema
                    .pointer("/properties/action/enum")
                    .and_then(serde_json::Value::as_array)
                {
                    for a in actions.iter().filter_map(serde_json::Value::as_str) {
                        names.insert(format!("{}.{a}", d.name));
                    }
                }
                names.insert(d.name);
            }
            if role == "desk" {
                let cli_defs = crate::surface::desk_cli_definitions(
                    &crate::session::coordination_tool_registry(Some("k".into())).definitions(),
                );
                for d in cli_defs {
                    if let Some((surf, _)) = d.name.split_once('.') {
                        names.insert(surf.to_string());
                    }
                    names.insert(d.name);
                }
            }
            all_known.extend(names.iter().cloned());
            role_tools.insert(role, names);
        }
        for name in crate::meta::META_TOOL_NAMES {
            all_known.insert(name.to_string());
        }

        // Non-tool snake_case parameters, enums, fields, reactions, and agent ids.
        let arg_allow: BTreeSet<&str> = [
            "agent_id",
            "artifact_id",
            "blocked_by",
            "continuation_of",
            "handoff_mode",
            "heard_none",
            "in_minutes",
            "in_progress",
            "in_reply_to",
            "in_review",
            "include_hidden",
            "interrupted_reply",
            "keep_awake",
            "lane_id",
            "lead",
            "next_step",
            "other_party",
            "played_audio",
            "read_only",
            "reply_in",
            "reply_to",
            "review_escalated",
            "review_of",
            "room_limit",
            "set_rule",
            "snooze_days",
            "task_id",
            "thumbs_up",
            "updated_at",
            "verify_mode",
            "wake",
            "waiting_on",
            "wake_at",
        ]
        .into_iter()
        .collect();

        let shared_core = [
            "process.watch",
            "update_plan",
            "task.follow_up",
            "task.pass",
            "task.invite",
            "wake.once",
            "wake.every",
            "wake.watch",
        ];
        let exemptions_for = |role: &str| -> BTreeSet<&'static str> {
            let extra: &[&'static str] = match role {
                "work" => &["terminate_loop"],
                "desk" => &["ask_user", "present.show"],
                "lane" => &[
                    "present.show",
                    "read.chats",
                    "read.thread",
                    "read.board",
                    "message.post",
                    "message.send",
                ],
                _ => &[],
            };
            shared_core.iter().chain(extra).copied().collect()
        };

        let default_ctx = PromptContext::default();
        let has_skills = !crate::skills::discover().is_empty();
        let work_full_ctx = PromptContext {
            worktree: true,
            memory: true,
            memory_writable: true,
            skills: has_skills,
            vault: true,
            agent_managed: false,
            images: true,
        };
        let lane_full_ctx = PromptContext {
            memory_writable: false,
            ..work_full_ctx
        };

        let seats = [
            ("work", "coding_system_prompt", coding_system_prompt()),
            (
                "work",
                "conversation_prompt",
                conversation_prompt(&default_ctx),
            ),
            (
                "work",
                "conversation_prompt(all_caps)",
                conversation_prompt(&work_full_ctx),
            ),
            (
                "desk",
                "coordination_prompt",
                coordination_prompt(&default_ctx),
            ),
            ("lead", "lead_system_prompt", lead_system_prompt()),
            ("lane", "lane_prompt", lane_prompt(&default_ctx, None)),
            (
                "lane",
                "lane_prompt(all_caps)",
                lane_prompt(&lane_full_ctx, None),
            ),
        ];

        let mut unknown = Vec::new();
        for (role, label, text) in seats {
            let tools = &role_tools[role];
            let exempt = exemptions_for(role);
            let mut parts = text.split('`');
            let _ = parts.next();
            while let Some(span) = parts.next() {
                if parts.next().is_none() {
                    break;
                }
                let dotted = span
                    .split_once('.')
                    .is_some_and(|(t, _)| crate::surface::is_surface(t));
                let looks_like_tool = dotted
                    || (span.contains('_')
                        && !span.is_empty()
                        && span.bytes().all(|b| matches!(b, b'a'..=b'z' | b'_')))
                    || all_known.contains(span);
                if looks_like_tool
                    && !tools.contains(span)
                    && !arg_allow.contains(span)
                    && !exempt.contains(span)
                {
                    unknown.push(format!("{label} ({role}): `{span}`"));
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "seat prompts name tools absent from that seat's tool set: {unknown:?}"
        );
    }
}
