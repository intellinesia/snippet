//! Lean continuation: once a session's prompt cache has lapsed, its older history is
//! archived (still searchable) and replaced by a deterministic handover, so the next
//! turn doesn't re-read everything at full price.

use super::*;

const REQUESTS: usize = 6;

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max).collect::<String>())
}

/// The handover a lean segment starts from. Built from state only; no model call.
pub(super) fn handover(
    messages: &[HarnessMessage],
    state: &HarnessState,
    branch: Option<&str>,
    pointers: &str,
) -> String {
    let requests: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            HarnessMessage::User { content } if !is_daemon_envelope(content) => {
                Some(format!("- {}", clip(content, 400)))
            }
            _ => None,
        })
        .collect();
    let requests = requests
        .iter()
        .skip(requests.len().saturating_sub(REQUESTS))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let running: Vec<String> = state
        .lanes
        .iter()
        .filter(|l| l.status == LaneStatus::Running)
        .map(|l| format!("\"{}\" ({})", l.title, l.id))
        .collect();
    let watches: Vec<String> = state
        .watches
        .iter()
        .map(|w| format!("\"{}\" ({})", w.label, w.id))
        .collect();
    let last = messages.iter().rev().find_map(|m| match m {
        HarnessMessage::Assistant { content, .. } if !content.trim().is_empty() => {
            Some(clip(content, 1500))
        }
        _ => None,
    });
    crate::envelope::Envelope::new("lean_continuation")
        .attr_opt("branch", branch)
        .child(
            "note",
            "This conversation continues from an earlier stretch. Its history is archived, not sent: \
             bring back what you need with `snippet history search <words>` and `snippet history show --from <id> --to <id>` rather than guessing.",
        )
        .child_with("requests", &[("order", "oldest first")], requests)

        .child("lanes_running", running.join(", "))
        .child("active_watches", watches.join(", "))
        .child_opt("last_reply", last)
        .child("pointers", pointers.trim())
        .render()
}

impl CodingHarness {
    /// Moves the non-system history into the searchable archive; returns it and its pointers.
    fn archive_history(&self, state: &mut HarnessState) -> Option<(Vec<HarnessMessage>, String)> {
        let keep = state
            .messages
            .iter()
            .take_while(|m| matches!(m, HarnessMessage::System { .. }))
            .count();
        if state.messages.len() <= keep + 1 {
            return None;
        }
        let (Ok(store), Some(session_id)) =
            (self.context.store(), self.context.durable_session_id())
        else {
            return None;
        };
        let now = chrono::Utc::now().to_rfc3339();
        // Drop the turns only once they are safely archived.
        let turns = match crate::history_archive::archive_messages(
            &store,
            session_id,
            &state.messages[keep..],
            &now,
        ) {
            Ok(turns) => turns,
            Err(err) => {
                state.events.push(HarnessEvent::SystemDecision {
                    step: "archive_failed".into(),
                    reasoning: format!("Kept the full history: archiving failed ({err})."),
                });
                return None;
            }
        };
        let pointers = crate::history_archive::render_micro_pointers("", &turns);
        let older: Vec<HarnessMessage> = state.messages.drain(keep..).collect();
        state.history_rewritten = true;
        Some((older, pointers))
    }

    /// A CLI compacted its own context: archive what it dropped so history search still finds it.
    pub(super) fn archive_cli_compaction(&self, state: &mut HarnessState) {
        let Some((older, pointers)) = self.archive_history(state) else {
            return;
        };
        let content = crate::envelope::Envelope::new("compacted")
            .attr("archived_messages", older.len().to_string())
            .child(
                "note",
                "earlier messages are archived; `snippet history search <words>` and `snippet history show --from <id> --to <id>` read them.",
            )
            .child("pointers", pointers.trim())
            .render();
        state.messages.push(HarnessMessage::Summary {
            kind: "cli_compaction".into(),
            content,
        });
    }

    /// Archive everything after the system prompt and continue from a handover.
    /// Returns the handover, or `None` when there was nothing to fold — including
    /// a repeat right after a lean handover, which an old app can still send.
    pub(super) fn continue_lean(&self, state: &mut HarnessState) -> Option<String> {
        let last_real = state
            .messages
            .iter()
            .rev()
            .find(|m| !matches!(m, HarnessMessage::System { .. }));
        if matches!(
            last_real,
            Some(HarnessMessage::Summary { kind, .. }) if kind == "lean_handover"
        ) {
            return None;
        }
        let (older, pointers) = self.archive_history(state)?;
        let branch = crate::session_stats::git_branch(&self.context.current_dir());
        let text = handover(&older, state, branch.as_deref(), &pointers);
        state.messages.push(HarnessMessage::Summary {
            kind: "lean_handover".into(),
            content: text.clone(),
        });
        state.last_prompt_tokens = 0;
        state.events.push(HarnessEvent::SystemDecision {
            step: "continued_lean".into(),
            reasoning: format!(
                "Continued lean: {} earlier messages archived, not sent.",
                older.len()
            ),
        });
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handover_carries_requests_plan_and_last_reply() {
        let mut state = HarnessState::default();
        state.plan = vec![PlanStep {
            step: "Ship it".into(),
            status: PlanStatus::InProgress,
        }];
        let messages = vec![
            HarnessMessage::User {
                content: "fix the header".into(),
            },
            HarnessMessage::User {
                content: "<file_watch subject=\"noise\"/>".into(),
            },
            HarnessMessage::Assistant {
                content: "Header fixed.".into(),
                tool_calls: vec![],
            },
        ];
        let text = handover(&messages, &state, Some("ui-polish"), "");
        assert!(text.contains("- fix the header"));
        assert!(!text.contains("noise"));
        assert!(crate::envelope::is(&text, "lean_continuation"));
        assert!(crate::envelope::child(&text, "plan").is_none());
        assert_eq!(
            crate::envelope::attr(&text, "branch").as_deref(),
            Some("ui-polish")
        );
        assert_eq!(
            crate::envelope::child(&text, "last_reply").as_deref(),
            Some("Header fixed.")
        );
    }
}
