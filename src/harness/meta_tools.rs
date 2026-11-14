use super::*;

impl CodingHarness {
    pub(super) fn dispatch_meta(
        &self,
        state: &mut HarnessState,
        lanes: &mut LaneManager,
        watches: &mut WatchManager,
        tool_name: &str,
        arguments: &Value,
    ) -> (Value, MetaControl) {
        match tool_name {
            "cancel_delegated_task" => {
                let lane_id = arguments
                    .get("lane_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                let reason = arguments
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty());
                match (lane_id, reason) {
                    (Some(lane_id), Some(reason)) => match lanes.cancel(lane_id, reason) {
                        Ok(title) => {
                            state.events.push(HarnessEvent::LaneCancelled {
                                id: lane_id.to_string(),
                                title: title.clone(),
                                reason: reason.to_string(),
                            });
                            (
                                json!({"schema_version": 1, "status": "success", "data": {
                                    "cancelled": true,
                                    "lane_id": lane_id,
                                    "title": title,
                                    "note": "The delegated scope is now yours. Partial workspace changes were preserved; inspect and validate them before continuing."
                                }}),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    },
                    _ => (
                        tool_error(
                            "cancel_delegated_task requires non-empty `lane_id` and `reason`.",
                        ),
                        MetaControl::Continue,
                    ),
                }
            }
            "update_plan" => match parse_plan(arguments) {
                Ok(steps) => {
                    let done = steps
                        .iter()
                        .filter(|s| s.status == PlanStatus::Done)
                        .count();
                    let total = steps.len();
                    let explanation = arguments
                        .get("explanation")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    state.plan = steps.clone();
                    self.mirror_plan_to_task(&steps);
                    state
                        .events
                        .push(HarnessEvent::PlanUpdated { steps, explanation });
                    (
                        json!({"schema_version": 1, "status": "success", "data": {"done": done, "total": total}}),
                        MetaControl::Continue,
                    )
                }
                Err(message) => (tool_error(&message), MetaControl::Continue),
            },
            "set_session_title" => {
                let title = arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim);
                let Some(title) = title else {
                    return (
                        tool_error("set_session_title requires a `title` string."),
                        MetaControl::Continue,
                    );
                };
                state.title = (!title.is_empty()).then(|| title.to_string());
                (
                    json!({"schema_version": 1, "status": "success", "data": {
                        "renamed": true,
                        "title": state.title,
                    }}),
                    MetaControl::Continue,
                )
            }
            "present_file" => {
                let text = |key: &str| {
                    arguments
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                };
                let Some(path) = text("path") else {
                    return (
                        tool_error("present_file requires a `path`."),
                        MetaControl::Continue,
                    );
                };
                let resolved = if std::path::Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    self.context.workspace_root().join(path)
                };
                if !resolved.exists() {
                    return (
                        tool_error(format!(
                            "present_file: `{path}` does not exist — write it before presenting it."
                        )),
                        MetaControl::Continue,
                    );
                }
                let caption = text("caption").map(str::to_string);
                let display = text("display")
                    .map(str::to_ascii_lowercase)
                    .filter(|d| d == "inline" || d == "card");
                let title = text("title");
                let artifact_id = text("artifact_id");
                let shown = resolved.display().to_string();
                if title.is_some() || artifact_id.is_some() || resolved.is_dir() {
                    let Some(session) = self.session_id() else {
                        return (
                            tool_error("present_file: artifacts need a saved session."),
                            MetaControl::Continue,
                        );
                    };
                    let title = title.or_else(|| {
                        resolved
                            .is_dir()
                            .then(|| resolved.file_name().and_then(|n| n.to_str()))
                            .flatten()
                    });
                    return match crate::artifacts::present(
                        &session,
                        &resolved,
                        title,
                        artifact_id,
                        caption.clone(),
                    ) {
                        Ok(presented) if presented.unchanged => (
                            json!({"schema_version": 1, "status": "success", "data": {
                                "artifact_id": presented.artifact.id,
                                "version": presented.version,
                                "link": crate::artifacts::markdown_link(&session, &presented.artifact.title, &presented.artifact.id, presented.version),
                                "note": "unchanged since the version the user already has, so nothing new was shown",
                                "broken_references": presented.broken,
                            }}),
                            MetaControl::Continue,
                        ),
                        Ok(presented) => {
                            state.events.push(HarnessEvent::ArtifactPresented {
                                id: presented.artifact.id.clone(),
                                version: presented.version,
                                title: presented.artifact.title.clone(),
                                artifact_kind: presented.artifact.kind.clone(),
                                caption,
                                path: shown,
                                display: display.clone(),
                            });
                            (
                                json!({"schema_version": 1, "status": "success", "data": {
                                    "artifact_id": presented.artifact.id,
                                    "version": presented.version,
                                    "link": crate::artifacts::markdown_link(&session, &presented.artifact.title, &presented.artifact.id, presented.version),
                                    "bundled": presented.bundled,
                                    "broken_references": presented.broken,
                                    "check": if presented.broken.is_empty() { serde_json::Value::Null } else {
                                        json!("Some files this page uses can't load from the artifact. Fix each broken reference and present it again with this artifact_id.")
                                    },
                                    "note": match display.as_deref() {
                                        Some("inline") => "rendered inline in the conversation; present again with this artifact_id to add a version",
                                        _ => "shown to the user as an artifact; present again with this artifact_id to add a version",
                                    },
                                }}),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (
                            tool_error(format!("present_file: {error}")),
                            MetaControl::Continue,
                        ),
                    };
                }
                let turn_start = state
                    .events
                    .iter()
                    .rposition(|e| matches!(e, HarnessEvent::UserInput { .. }))
                    .unwrap_or(0);
                let turn = &state.events[turn_start..];
                if turn.iter().any(
                    |e| matches!(e, HarnessEvent::FilePresented { path, .. } if *path == shown),
                ) {
                    return (
                        json!({"schema_version": 1, "status": "success", "data": {
                            "presented": shown,
                            "note": "already shown to the user this turn",
                        }}),
                        MetaControl::Continue,
                    );
                }
                let shared = turn
                    .iter()
                    .filter(|e| matches!(e, HarnessEvent::FilePresented { .. }))
                    .count()
                    + 1;
                state.events.push(HarnessEvent::FilePresented {
                    path: shown.clone(),
                    caption,
                });
                let note = if shared >= 10 {
                    format!(
                        "shown to the user; that's {shared} files this turn. For a batch, write them into one folder and present the folder: it becomes one collection with a download-all"
                    )
                } else {
                    "shown to the user as a file they can open or download; continue your turn as usual".to_string()
                };
                (
                    json!({"schema_version": 1, "status": "success", "data": {
                        "presented": shown,
                        "note": note,
                    }}),
                    MetaControl::Continue,
                )
            }
            "complete_goal" => {
                let summary = arguments
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                match state.goal.as_mut() {
                    Some(goal) if goal.status == GoalStatus::Active => {
                        goal.status = GoalStatus::Complete;
                        let text = goal.text.clone();
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "goal_completed".to_string(),
                            reasoning: if summary.is_empty() {
                                text
                            } else {
                                summary.clone()
                            },
                        });
                        (
                            json!({"schema_version": 1, "status": "success", "data": {"completed": true}}),
                            MetaControl::EndTurn {
                                kind: TurnEndKind::Complete,
                                final_text: Some(if summary.is_empty() {
                                    "Goal complete.".to_string()
                                } else {
                                    summary
                                }),
                            },
                        )
                    }
                    _ => (
                        tool_error("complete_goal: there is no active goal to complete."),
                        MetaControl::Continue,
                    ),
                }
            }
            "ask_user" => match parse_ask_user(arguments) {
                Ok(rendered) => {
                    let prompt_text = first_question_text(&rendered)
                        .or_else(|| {
                            rendered
                                .get("context")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "Waiting for your input.".to_string());
                    state.events.push(HarnessEvent::UserQuestion {
                        questions: rendered.clone(),
                    });
                    state.pending_question = Some(rendered);
                    (
                        json!({"schema_version": 1, "status": "success", "data": {"asked": true}}),
                        MetaControl::EndTurn {
                            kind: TurnEndKind::Ask,
                            final_text: Some(prompt_text),
                        },
                    )
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "delegate_task" => match parse_delegate_brief(arguments) {
                Ok(brief) => {
                    // Follow-up to an existing lane: resume it with the new brief,
                    // context intact.
                    if let Some(lane_id) = brief.lane_id.as_deref() {
                        if lanes
                            .records()
                            .iter()
                            .any(|r| r.id == lane_id && r.status == LaneStatus::Running)
                        {
                            return match lanes.message(lane_id, &brief.description) {
                                Ok(title) => (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "messaged": true,
                                            "lane_id": lane_id,
                                            "title": title,
                                            "note": "The lane is still working; it gets this between steps, or straight away if it's waiting on you. Its report still arrives as a [lane_report].",
                                        }
                                    }),
                                    MetaControl::Continue,
                                ),
                                Err(error) => (tool_error(error), MetaControl::Continue),
                            };
                        }
                        return match lanes.follow_up(
                            lane_id,
                            &brief.description,
                            brief.profile.clone(),
                        ) {
                            Ok(title) => {
                                state.events.push(HarnessEvent::LaneSpawned {
                                    id: lane_id.to_string(),
                                    title: title.clone(),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "continued": true,
                                            "lane_id": lane_id,
                                            "title": title,
                                            "note": "Lane resumed with its prior context; its report will arrive as a [lane_report] message.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        };
                    }
                    let (description, read_only) = if let Some(ref target_id) = brief.review_of {
                        let finished_list = || {
                            let list: Vec<String> = lanes
                                .records()
                                .iter()
                                .filter(|r| r.status == LaneStatus::Completed)
                                .map(|r| format!("\"{}\" ({})", r.title, r.id))
                                .collect();
                            if list.is_empty() {
                                "none".to_string()
                            } else {
                                list.join(", ")
                            }
                        };
                        let Some(target) = lanes.records().iter().find(|r| r.id == *target_id)
                        else {
                            return (
                                tool_error(format!(
                                    "review_of lane `{target_id}` is not known. Completed lanes: [{}].",
                                    finished_list()
                                )),
                                MetaControl::Continue,
                            );
                        };
                        if target.status != LaneStatus::Completed {
                            return (
                                tool_error(format!(
                                    "review_of lane `{target_id}` is not completed (status: {:?}). Completed lanes: [{}].",
                                    target.status,
                                    finished_list()
                                )),
                                MetaControl::Continue,
                            );
                        }
                        let st = target.structured.as_ref();
                        let summary = st
                            .map(|s| s.summary.trim())
                            .filter(|s| !s.is_empty())
                            .or_else(|| {
                                target
                                    .summary
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                            })
                            .or_else(|| {
                                target
                                    .report
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                            })
                            .unwrap_or("none reported");
                        let changed = st
                            .and_then(|s| s.changed.as_deref())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("not measured");
                        let tests = st
                            .and_then(|s| s.tests.as_deref())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("none reported");
                        let open = st
                            .and_then(|s| s.open.as_deref())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("none");
                        let prefixed = format!(
                            "Review of lane {} \"{}\". Judge the work, not the account of it; check claims against the files.\n\
                             Its report: {summary}\n\
                             Changed: {changed}\n\
                             Tests: {tests}\n\
                             Open: {open}\n\
                             Read the diff yourself (`git diff -- <files>` or `git diff HEAD`) before forming a view.\n\n\
                             {}",
                            target.id, target.title, brief.description
                        );
                        (prefixed, true)
                    } else {
                        (brief.description.clone(), brief.read_only)
                    };
                    match lanes.spawn_scoped(
                        &brief.title,
                        &description,
                        read_only,
                        brief.agent.clone(),
                        brief.profile.clone(),
                        brief.paths.clone(),
                    ) {
                        Ok(id) => {
                            state.events.push(HarnessEvent::LaneSpawned {
                                id: id.clone(),
                                title: brief.title.clone(),
                            });
                            let mut data = json!({
                                "delegated": true,
                                "lane_id": id,
                                "title": brief.title,
                                "access": if read_only { "read_only" } else { "full" },
                                "note": "Lane runs in the background; its report will arrive as a [lane_report] message. Follow up later by re-calling delegate_task with this lane_id.",
                            });
                            if let Some(ref agent) = brief.agent {
                                data["agent"] = json!(agent);
                            }
                            if let Some(ref profile) = brief.profile {
                                data["profile"] = json!(profile);
                            }
                            (
                                json!({
                                    "schema_version": 1,
                                    "status": "success",
                                    "data": data,
                                }),
                                MetaControl::Continue,
                            )
                        }
                        Err(error) => (tool_error(error), MetaControl::Continue),
                    }
                }
                Err(error) => (tool_error(error), MetaControl::Continue),
            },
            "monitor" => {
                let action = arguments
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("add");
                match action {
                    "add" => {
                        let Some(path) = arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        else {
                            return (
                                tool_error("monitor add requires a `path`."),
                                MetaControl::Continue,
                            );
                        };
                        let label = arguments
                            .get("label")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(path);
                        let filter = arguments
                            .get("filter")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        match watches.add(path, label, filter) {
                            Ok(record) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_added".to_string(),
                                    reasoning: format!(
                                        "watching \"{}\" ({})",
                                        record.label, record.path
                                    ),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": {
                                            "watching": true,
                                            "watch_id": record.id,
                                            "label": record.label,
                                            "path": record.path,
                                            "note": "Tailing from the current end of file. Appended text arrives as a [file_watch] message (debounced per burst); ending your turn is how you wait for it.",
                                        }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "remove" => {
                        let key = arguments
                            .get("watch_id")
                            .or_else(|| arguments.get("path"))
                            .or_else(|| arguments.get("label"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let Some(key) = key else {
                            return (
                                tool_error(
                                    "monitor remove needs a `watch_id`, `path`, or `label`.",
                                ),
                                MetaControl::Continue,
                            );
                        };
                        match watches.remove(key) {
                            Ok(label) => {
                                state.watches = watches.records().to_vec();
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "watch_removed".to_string(),
                                    reasoning: format!("stopped watching \"{label}\""),
                                });
                                (
                                    json!({
                                        "schema_version": 1,
                                        "status": "success",
                                        "data": { "removed": true, "label": label }
                                    }),
                                    MetaControl::Continue,
                                )
                            }
                            Err(error) => (tool_error(error), MetaControl::Continue),
                        }
                    }
                    "list" => (
                        json!({
                            "schema_version": 1,
                            "status": "success",
                            "data": {
                                "watches": watches.records().iter().map(|r| json!({
                                    "watch_id": r.id,
                                    "label": r.label,
                                    "path": r.path,
                                    "filter": r.filter,
                                })).collect::<Vec<_>>(),
                            }
                        }),
                        MetaControl::Continue,
                    ),
                    other => (
                        tool_error(format!(
                            "monitor action must be add | remove | list, got `{other}`."
                        )),
                        MetaControl::Continue,
                    ),
                }
            }
            other => (
                tool_error(format!("`{other}` is not a recognized meta tool.")),
                MetaControl::Continue,
            ),
        }
    }

    pub(super) fn definitions_for(
        &self,
        conversation_mode: bool,
        goal_active: bool,
    ) -> Vec<crate::llm::NativeToolDefinition> {
        let mut definitions = self.tools.definitions();
        if conversation_mode {
            // User-facing: meta tools (note/ask_user/delegate); no terminate tool —
            // a plain reply ends the turn. `complete_goal` is added only while a goal runs.
            definitions.extend(
                meta::conversation_meta_definitions_for(
                    goal_active,
                    self.config.allow_lane_control,
                )
                .into_iter()
                .filter(|d| !self.config.hidden_meta.contains(&d.name.as_str())),
            );
        } else {
            // Headless (lanes / one-shot run): an explicit terminate_loop carries a
            // structured summary back to the caller.
            definitions.push(meta::terminate_loop_tool());
        }
        definitions
    }

    /// Copy this session's plan onto the task it is working, so the board and
    /// Mission Control see how far along it is. Lanes keep their own plans.
    fn mirror_plan_to_task(&self, steps: &[PlanStep]) {
        if self.context.lane_origin().is_some() {
            return;
        }
        let Some(session) = self.context.durable_session_id() else {
            return;
        };
        let Some((task_id, _)) = crate::coordination_tools::session_task(&self.context, session)
        else {
            return;
        };
        let path = self
            .context
            .store_path()
            .unwrap_or_else(crate::store::default_db_path);
        let Ok(store) = crate::store::Store::open_cached(path) else {
            return;
        };
        let work: Vec<crate::coordination::WorkStep> = steps
            .iter()
            .map(|s| crate::coordination::WorkStep {
                step: s.step.clone(),
                status: serde_json::to_value(&s.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            })
            .collect();
        let _ = store.set_task_work_plan(&task_id, &work);
    }
}

/// Validate an `update_plan` call: 1–12 non-empty steps, at most one in progress.
pub(super) fn parse_plan(arguments: &Value) -> Result<Vec<PlanStep>, String> {
    let steps: Vec<PlanStep> = arguments
        .get("steps")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("update_plan `steps` must be a list of {{step, status}}: {e}"))?
        .unwrap_or_default();
    if steps.is_empty() || steps.len() > 12 {
        return Err("update_plan needs between 1 and 12 steps.".to_string());
    }
    if steps.iter().any(|s| s.step.trim().is_empty()) {
        return Err("every plan step needs text.".to_string());
    }
    if steps
        .iter()
        .filter(|s| s.status == PlanStatus::InProgress)
        .count()
        > 1
    {
        return Err("mark at most one step `in_progress`.".to_string());
    }
    Ok(steps
        .into_iter()
        .map(|s| PlanStep {
            step: s.step.trim().to_string(),
            status: s.status,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::LaneStructuredReport;
    use crate::llm::{AgentModel, ModelOutput, NativeToolDefinition, StreamHandle};
    use crate::tools::ToolError;
    use std::sync::Arc;

    struct IdleModel;

    #[async_trait::async_trait]
    impl AgentModel for IdleModel {
        async fn generate(
            &mut self,
            _messages: &[HarnessMessage],
            _tools: &[NativeToolDefinition],
            _force_tool: bool,
            _sink: Option<StreamHandle>,
        ) -> Result<ModelOutput, ToolError> {
            Ok(ModelOutput {
                content_text: Some("done".to_string()),
                ..ModelOutput::default()
            })
        }
    }

    #[tokio::test]
    async fn delegate_task_review_of_requires_completed_lane_and_prefixes_read_only_brief() {
        let dir = tempfile::tempdir().unwrap();
        let harness = CodingHarness::new(
            HarnessConfig {
                memory_enabled: false,
                ..HarnessConfig::default()
            },
            ToolRegistry::new(),
            ToolContext::new(dir.path()).unwrap(),
        );
        let mut state = HarnessState::blank(&dir.path().display().to_string(), None);
        let (result_tx, _result_rx) = mpsc::unbounded_channel();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        let (watch_tx, _watch_rx) = mpsc::unbounded_channel();
        let mut watches = WatchManager::new(dir.path().to_path_buf(), watch_tx);

        let running_record = LaneRecord {
            id: "lane-1".to_string(),
            title: "implement feature".to_string(),
            status: LaneStatus::Running,
            handoff: Some("do work".to_string()),
            summary: None,
            report: None,
            structured: None,
            error: None,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            finished_at: None,
            activity: None,
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
            read_only: false,
            agent: None,
            profile: None,
            transcript: None,
            paths: vec!["src/foo.rs".to_string()],
        };
        let completed_record = LaneRecord {
            id: "lane-2".to_string(),
            title: "implement parser".to_string(),
            status: LaneStatus::Completed,
            handoff: Some("build parser".to_string()),
            summary: Some("Built parser.".to_string()),
            report: Some("Full parser report.".to_string()),
            structured: Some(LaneStructuredReport {
                summary: "Implemented parser in src/parser.rs:10.".to_string(),
                findings: Vec::new(),
                changed: Some("src/parser.rs (+40 -2)".to_string()),
                tests: Some("cargo test parser: 3 passed".to_string()),
                open: None,
                unverified_citations: None,
                clipped: false,
            }),
            error: None,
            started_at: Utc::now().to_rfc3339(),
            finished_at: Some(Utc::now().to_rfc3339()),
            activity: None,
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
            read_only: false,
            agent: None,
            profile: None,
            transcript: None,
            paths: vec!["src/parser.rs".to_string()],
        };

        let factory: ModelFactory = Arc::new(|_profile, _session| Ok(Box::new(IdleModel)));
        let mut lanes = LaneManager::new(
            Some(factory),
            dir.path().to_path_buf(),
            dir.path().join("lanes"),
            result_tx,
            progress_tx,
            None,
        )
        .with_records(vec![running_record, completed_record]);

        // Unknown lane rejected.
        let (unknown_out, _) = harness.dispatch_meta(
            &mut state,
            &mut lanes,
            &mut watches,
            "delegate_task",
            &json!({
                "title": "review unknown",
                "description": "Check all edge cases and error handling in the parser implementation.",
                "review_of": "lane-99"
            }),
        );
        assert_eq!(unknown_out["status"], "error");
        let err_msg = unknown_out["error"]["message"].as_str().unwrap_or_default();
        assert!(err_msg.contains("lane-99"), "got: {err_msg}");
        assert!(
            err_msg.contains("lane-2"),
            "should list known finished lanes: {err_msg}"
        );

        // Running lane rejected.
        let (running_out, _) = harness.dispatch_meta(
            &mut state,
            &mut lanes,
            &mut watches,
            "delegate_task",
            &json!({
                "title": "review running",
                "description": "Check all edge cases and error handling in the feature implementation.",
                "review_of": "lane-1"
            }),
        );
        assert_eq!(running_out["status"], "error");
        let err_msg = running_out["error"]["message"].as_str().unwrap_or_default();
        assert!(err_msg.contains("lane-1"), "got: {err_msg}");
        assert!(
            err_msg.contains("lane-2"),
            "should list known finished lanes: {err_msg}"
        );

        // Completed lane spawns a read_only review lane with prefixed brief.
        let (ok_out, _) = harness.dispatch_meta(
            &mut state,
            &mut lanes,
            &mut watches,
            "delegate_task",
            &json!({
                "title": "review parser",
                "description": "Focus on UTF-8 boundary handling and error recovery.",
                "access": "full",
                "review_of": "lane-2"
            }),
        );
        assert_eq!(ok_out["status"], "success", "got: {ok_out}");
        assert_eq!(ok_out["data"]["access"], "read_only");
        let spawned_id = ok_out["data"]["lane_id"].as_str().unwrap();
        let spawned = lanes
            .records()
            .iter()
            .find(|r| r.id == spawned_id)
            .expect("spawned lane record");
        assert!(spawned.read_only, "review_of must force read_only");
        let handoff = spawned.handoff.as_deref().unwrap_or_default();
        assert!(
            handoff.starts_with(
                "Review of lane lane-2 \"implement parser\". Judge the work, not the account of it; check claims against the files.\n\
                 Its report: Implemented parser in src/parser.rs:10.\n\
                 Changed: src/parser.rs (+40 -2)\n\
                 Tests: cargo test parser: 3 passed\n\
                 Open: none\n\
                 Read the diff yourself (`git diff -- <files>` or `git diff HEAD`) before forming a view.\n\n\
                 Focus on UTF-8 boundary handling and error recovery."
            ),
            "unexpected handoff: {handoff}"
        );
    }
}
