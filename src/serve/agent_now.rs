use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use rusqlite::params;
use serde::Serialize;
use serde_json::{Value, json};

use crate::coordination::{BoardQuery, Task, TaskState, TaskStatus, VerifyMode};
use crate::review_tools::REVIEW_TIMEOUT_SECS;
use crate::store::Store;

use super::{Shared, api_error};

const DETAIL_MAX: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentPresence {
    pub state: String,
    pub detail: String,
    pub since: String,
    pub session: String,
}

fn trim_detail(raw: &str) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(DETAIL_MAX).collect()
}

fn rfc3339_from_secs(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn secs_from_rfc3339(raw: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn state_priority(state: &str) -> u8 {
    match state {
        "waiting" => 7,
        "blocked" => 6,
        "reviewing" => 5,
        "typing" => 4,
        "thinking" => 3,
        "working" => 2,
        "idle" => 1,
        _ => 0,
    }
}

fn extract_question_detail(state_json: &str) -> String {
    let Ok(val) = serde_json::from_str::<Value>(state_json) else {
        return String::new();
    };
    let Some(pq) = val.get("pending_question") else {
        return String::new();
    };
    if let Some(questions) = pq.get("questions").and_then(Value::as_array) {
        for q in questions {
            if let Some(text) = q.get("text").and_then(Value::as_str) {
                let t = text.trim();
                if !t.is_empty() {
                    return trim_detail(t);
                }
            }
        }
    }
    if let Some(context) = pq.get("context").and_then(Value::as_str) {
        let c = context.trim();
        if !c.is_empty() {
            return trim_detail(c);
        }
    }
    String::new()
}

fn is_awaiting_human_review(store: &Store, task: &Task, state: Option<&TaskState>) -> bool {
    if task.status != TaskStatus::InReview {
        return false;
    }
    let default_state;
    let st = match state {
        Some(s) => s,
        None => {
            default_state = store
                .task_state(&task.id)
                .ok()
                .flatten()
                .unwrap_or_default();
            &default_state
        }
    };
    matches!(
        crate::review_tools::review_route(store, task, st),
        VerifyMode::MissionControl | VerifyMode::User
    )
}

#[derive(Clone)]
struct Candidate {
    state: &'static str,
    detail: String,
    since_rfc: String,
    since_secs: i64,
    session: String,
}

impl Candidate {
    fn key(&self) -> (u8, i64) {
        (state_priority(self.state), self.since_secs)
    }

    fn into_presence(self) -> AgentPresence {
        AgentPresence {
            state: self.state.to_string(),
            detail: trim_detail(&self.detail),
            since: self.since_rfc,
            session: self.session,
        }
    }
}

fn push_candidate(best: &mut Option<Candidate>, cand: Candidate) {
    if cand.session.is_empty() || state_priority(cand.state) == 0 {
        return;
    }
    if best.as_ref().is_none_or(|b| cand.key() > b.key()) {
        *best = Some(cand);
    }
}

/// Compute `(presence, needs_you)` for `agent_id`.
pub fn agent_presence_and_needs(store: &Store, agent_id: &str) -> (Option<AgentPresence>, u64) {
    let mut best: Option<Candidate> = None;
    let mut question_sessions = std::collections::HashSet::<String>::new();
    let mut review_tasks = std::collections::HashSet::<String>::new();

    // 1. Live in-memory presence events for this agent's sessions.
    let live = crate::harness::presence::snapshot_presence();
    for (session_id, p) in &live {
        let belongs = p.agent.as_deref() == Some(agent_id)
            || session_id.strip_prefix("inbox-") == Some(agent_id)
            || (agent_id == crate::mission_control::SESSION_ID
                && session_id == crate::mission_control::SESSION_ID);
        if !belongs {
            continue;
        }
        let mapped = match p.state.as_str() {
            "waiting" => "waiting",
            "blocked" => "blocked",
            "reviewing" => "reviewing",
            "typing" => "typing",
            "thinking" => "thinking",
            "tool" | "working" | "running" => "working",
            "idle" => "idle",
            _ => continue,
        };
        push_candidate(
            &mut best,
            Candidate {
                state: mapped,
                detail: p.detail.clone(),
                since_rfc: rfc3339_from_secs(p.since),
                since_secs: p.since,
                session: session_id.clone(),
            },
        );
    }

    // 2. Persisted sessions in `sessions` table belonging to this agent (or where it holds an active seat).
    let rows: Vec<(String, String, String, String)> = store
        .with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT s.id, s.status, s.updated_at, s.state_json
                 FROM sessions s
                 WHERE s.settled_at IS NULL
                   AND (s.agent_id = ?1
                        OR s.id = ('inbox-' || ?1)
                        OR (?1 = 'mission-control' AND s.id = 'mission-control')
                        OR EXISTS (
                            SELECT 1 FROM task_agents ta
                            JOIN tasks t ON t.id = ta.task_id
                            WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL
                              AND ta.status = 'active'
                              AND COALESCE(NULLIF(ta.work_session_id, ''), t.session_id) = s.id
                        ))",
            )?;
            let mapped = stmt.query_map(params![agent_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            mapped.collect()
        })
        .unwrap_or_default();

    for (session_id, status, updated_at, state_json) in rows {
        let q_detail = extract_question_detail(&state_json);
        let has_question = status == "waiting_for_input" || !q_detail.is_empty();
        if has_question {
            question_sessions.insert(session_id.clone());
        }
        let since_secs = secs_from_rfc3339(&updated_at);
        if has_question {
            let detail = if !q_detail.is_empty() {
                q_detail
            } else {
                live.get(&session_id)
                    .map(|p| p.detail.clone())
                    .filter(|d| !d.is_empty() && d != "user")
                    .unwrap_or_default()
            };
            push_candidate(
                &mut best,
                Candidate {
                    state: "waiting",
                    detail,
                    since_rfc: updated_at,
                    since_secs,
                    session: session_id,
                },
            );
            continue;
        }
        // If live presence already reported a non-idle state for this session, let live win;
        // still record the persisted status so a cold start after restart sees it.
        let persisted_state = match status.as_str() {
            "running" => "working",
            "idle" | "completed" | "failed" => "idle",
            _ => continue,
        };
        let detail = live
            .get(&session_id)
            .filter(|p| p.state != "idle")
            .map(|p| p.detail.clone())
            .unwrap_or_default();
        push_candidate(
            &mut best,
            Candidate {
                state: persisted_state,
                detail,
                since_rfc: updated_at,
                since_secs,
                session: session_id,
            },
        );
    }

    // 3. Tasks where this agent has a seat (non-removed).
    let seat_rows: Vec<(String, String, String, Option<String>)> = store
        .with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT task_id, role, status, work_session_id
                 FROM task_agents
                 WHERE agent_id = ?1 AND removed_at IS NULL",
            )?;
            let mapped = stmt.query_map(params![agent_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            mapped.collect()
        })
        .unwrap_or_default();

    for (task_id, role, seat_status, work_session) in seat_rows {
        let Some(task) = store.get_task(&task_id).ok().flatten() else {
            continue;
        };
        if task.status.is_terminal()
            || matches!(seat_status.as_str(), "done" | "withdrawn" | "released")
        {
            continue;
        }
        let session = work_session
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| task.session_id.clone());
        let session = if session.trim().is_empty() {
            format!("inbox-{agent_id}")
        } else {
            session
        };
        let since_rfc = task
            .last_progress_at
            .clone()
            .unwrap_or_else(|| task.updated_at.clone());
        let since_secs = secs_from_rfc3339(&since_rfc);
        let t_state = store.task_state(&task.id).ok().flatten();

        if task.status == TaskStatus::InReview {
            if role == "reviewer" && seat_status == "active" {
                push_candidate(
                    &mut best,
                    Candidate {
                        state: "reviewing",
                        detail: task.title.clone(),
                        since_rfc,
                        since_secs,
                        session,
                    },
                );
            } else if is_awaiting_human_review(store, &task, t_state.as_ref()) {
                review_tasks.insert(task.id.clone());
                let detail = t_state
                    .as_ref()
                    .and_then(|s| s.pending_report.as_ref())
                    .map(|r| r.summary.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(task.title.as_str())
                    .to_string();
                push_candidate(
                    &mut best,
                    Candidate {
                        state: "waiting",
                        detail,
                        since_rfc,
                        since_secs,
                        session,
                    },
                );
            }
        } else if task.status == TaskStatus::Blocked && seat_status == "active" {
            let detail = task
                .notifications
                .iter()
                .rev()
                .find(|n| n.kind == "blocked")
                .map(|n| n.message.as_str())
                .unwrap_or(task.title.as_str())
                .to_string();
            push_candidate(
                &mut best,
                Candidate {
                    state: "blocked",
                    detail,
                    since_rfc,
                    since_secs,
                    session,
                },
            );
        } else if task.status == TaskStatus::InProgress && seat_status == "active" {
            if role == "reviewer" {
                push_candidate(
                    &mut best,
                    Candidate {
                        state: "reviewing",
                        detail: task.title.clone(),
                        since_rfc,
                        since_secs,
                        session,
                    },
                );
            }
        }
    }

    let needs_you = (question_sessions.len() + review_tasks.len()) as u64;
    (best.map(Candidate::into_presence), needs_you)
}

fn task_step_summary(task: &Task, state: Option<&TaskState>) -> String {
    let plan_step = task
        .work_plan
        .iter()
        .find(|s| s.status == "in_progress")
        .or_else(|| {
            task.work_plan
                .iter()
                .find(|s| s.status != "done" && s.status != "completed")
        })
        .map(|s| s.step.as_str());
    if let Some(step) = plan_step
        && !step.trim().is_empty()
    {
        return trim_detail(step);
    }
    if let Some(st) = state {
        if !st.current_step.trim().is_empty() {
            return trim_detail(&st.current_step);
        }
        if let Some(rep) = st.pending_report.as_ref()
            && !rep.summary.trim().is_empty()
        {
            return trim_detail(&rep.summary);
        }
    }
    if let Some(res) = task.result.as_ref()
        && !res.summary.trim().is_empty()
    {
        return trim_detail(&res.summary);
    }
    String::new()
}

fn is_seat_stale(
    store: &Store,
    task: &Task,
    state: Option<&TaskState>,
    session_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if task.status.is_terminal() {
        return true;
    }
    if task.status == TaskStatus::InReview {
        let started = state
            .map(|s| s.review_started_at.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(task.updated_at.as_str());
        if let Ok(at) = chrono::DateTime::parse_from_rfc3339(started)
            && (now - at.with_timezone(&chrono::Utc)).num_seconds() > REVIEW_TIMEOUT_SECS
        {
            return true;
        }
    }
    if !session_id.trim().is_empty() {
        let since = task
            .last_progress_at
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(task.updated_at.as_str());
        let canon = crate::conversations::canonical_session_id(session_id);
        let user_turns: i64 = store
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM session_messages
                     WHERE session_id = ?1 AND role = 'user' AND created_at > ?2",
                    params![canon, since],
                    |r| r.get(0),
                )
            })
            .unwrap_or(0);
        if user_turns > 0 {
            return true;
        }
    }
    false
}

/// GET /agents/{agent_id}/now
pub(crate) async fn agent_now(State(d): State<Shared>, Path(agent_id): Path<String>) -> Response {
    match d.store.get_agent(&agent_id) {
        Ok(Some(_)) => {}
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "no such agent"),
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("get agent: {error}"),
            );
        }
    }

    let now = chrono::Utc::now();
    let (presence, _needs_you) = agent_presence_and_needs(&d.store, &agent_id);

    // Load all non-removed seats for this agent.
    let seat_rows: Vec<(String, String, String, Option<String>, String)> = d
        .store
        .with_connection(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ta.task_id, ta.role, ta.status, ta.work_session_id, ta.added_at
                 FROM task_agents ta
                 JOIN tasks t ON t.id = ta.task_id
                 WHERE ta.agent_id = ?1 AND ta.removed_at IS NULL
                 ORDER BY t.updated_at DESC, ta.added_at DESC",
            )?;
            let mapped = stmt.query_map(params![agent_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            mapped.collect()
        })
        .unwrap_or_default();

    let mut sessions_json = Vec::new();
    let mut open_tasks_count: u64 = 0;
    let mut in_review_count: u64 = 0;
    let mut current_task_candidate: Option<(u8, String, Task, Option<TaskState>)> = None;

    for (task_id, _role, seat_status, work_session, added_at) in seat_rows {
        let Some(task) = d.store.get_task(&task_id).ok().flatten() else {
            continue;
        };
        if !task.status.is_terminal() {
            open_tasks_count += 1;
            if task.status == TaskStatus::InReview {
                in_review_count += 1;
            }
        }
        let t_state = d.store.task_state(&task.id).ok().flatten();

        // Candidate for current_task: prefer non-terminal tasks where agent is active.
        if !task.status.is_terminal() {
            let rank = match (seat_status.as_str(), &task.status) {
                ("active", TaskStatus::InProgress) => 5,
                ("active", TaskStatus::InReview) => 4,
                ("active", TaskStatus::Blocked) => 3,
                (_, TaskStatus::InProgress | TaskStatus::InReview | TaskStatus::Blocked) => 2,
                _ => 1,
            };
            let stamp = task
                .last_progress_at
                .clone()
                .unwrap_or_else(|| task.updated_at.clone());
            if current_task_candidate
                .as_ref()
                .is_none_or(|(r, s, _, _)| (rank, stamp.as_str()) > (*r, s.as_str()))
            {
                current_task_candidate = Some((rank, stamp, task.clone(), t_state.clone()));
            }
        }

        // Active seats for `sessions` list.
        if seat_status == "active" {
            let session_id = work_session
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| task.session_id.clone());
            let lease = d.store.lease_of(&task.id).ok().flatten();
            let open_turn = d.store.open_turn_of(&task.id).ok().flatten();
            let holder = lease
                .as_ref()
                .map(|l| l.agent_id == agent_id)
                .unwrap_or(true);
            let since = open_turn
                .as_ref()
                .filter(|t| t.agent_id == agent_id)
                .map(|t| t.started_at.clone())
                .or_else(|| {
                    lease
                        .as_ref()
                        .filter(|l| l.agent_id == agent_id)
                        .map(|l| l.acquired_at.clone())
                })
                .unwrap_or(added_at);
            let last_progress_at = task
                .last_progress_at
                .clone()
                .unwrap_or_else(|| task.updated_at.clone());
            let stale = is_seat_stale(&d.store, &task, t_state.as_ref(), &session_id, now);
            sessions_json.push(json!({
                "session": session_id,
                "task_id": task.id,
                "task_title": task.title,
                "task_status": task.status.as_str(),
                "holder": holder,
                "since": since,
                "last_progress_at": last_progress_at,
                "stale": stale,
            }));
        }
    }

    let current_task_json = current_task_candidate.map(|(_, since, task, t_state)| {
        let verify_mode = t_state
            .as_ref()
            .map(|s| crate::review_tools::review_route(&d.store, &task, s))
            .unwrap_or(VerifyMode::SelfCheck);
        let step = task_step_summary(&task, t_state.as_ref());
        json!({
            "id": task.id,
            "title": task.title,
            "status": task.status.as_str(),
            "step": step,
            "since": since,
            "verify_mode": verify_mode.as_str(),
        })
    });

    let inbox_unread: u64 = d
        .store
        .with_connection(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM message_deliveries
                 WHERE recipient_kind = 'agent' AND recipient_id = ?1 AND read_at IS NULL",
                params![agent_id],
                |r| r.get(0),
            )?;
            Ok(count.max(0) as u64)
        })
        .unwrap_or(0);

    let board_rows = d
        .store
        .read_board(&agent_id, &BoardQuery::default(), 5)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            json!({
                "at": entry.created_at,
                "summary": entry.summary,
            })
        })
        .collect::<Vec<_>>();

    let sessions_count = sessions_json.len() as u64;

    Json(json!({
        "agent_id": agent_id,
        "presence": presence,
        "current_task": current_task_json,
        "sessions": sessions_json,
        "counts": {
            "open_tasks": open_tasks_count,
            "in_review": in_review_count,
            "inbox_unread": inbox_unread,
            "sessions": sessions_count,
        },
        "board": board_rows,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};
    use crate::coordination::{BoardEntryKind, NewBoardEntry, PendingReport, WorkStep};
    use crate::llm::HarnessMessage;

    fn test_daemon() -> Shared {
        std::sync::Arc::new(crate::serve::Daemon {
            config: std::sync::Mutex::new(crate::config::SnippetConfig::default()),
            config_path: std::path::PathBuf::new(),
            token: String::new(),
            hostname: "test".into(),
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            shells: crate::term::SessionTerms::new(std::env::temp_dir()),
            git_write: tokio::sync::Mutex::new(()),
            seen_nonces: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_hidden: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_revision: std::sync::atomic::AtomicU64::new(0),
            mission_control_root: tempfile::tempdir().expect("tempdir").keep(),
            store: Store::open_in_memory().unwrap(),
            coordination_events: tokio::sync::broadcast::channel(16).0,
            recurring_root: tempfile::tempdir().expect("tempdir").keep(),
        })
    }

    fn make_agent(store: &Store, id: &str) {
        store
            .create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn list_agents_includes_presence_priority_and_needs_you() {
        crate::harness::presence::clear_presence_for_test();
        let d = test_daemon();
        make_agent(&d.store, "ag-idle");
        make_agent(&d.store, "ag-busy");

        // ag-busy has an open question in session s-wait and a running tool in s-work;
        // also an in-review task awaiting human (mission_control).
        crate::harness::presence::record_session_presence(
            "s-work",
            Some("ag-busy".into()),
            "tool",
            "cargo test",
            100,
        );
        d.store
            .save_session_scalar(
                "s-wait",
                "wk",
                "/tmp",
                Some("Wait"),
                "waiting_for_input",
                &json!({
                    "pending_question": {
                        "questions": [{"text": "May I release the machine now?"}]
                    }
                })
                .to_string(),
                "2026-11-12T18:00:00Z",
                "2026-11-12T18:58:00Z",
            )
            .unwrap();
        d.store
            .set_session_role("s-wait", "standard", Some("ag-busy"))
            .unwrap();

        let mut task = Task::filed_by_human(
            "t-rev".into(),
            "Demo TTS".into(),
            "desc".into(),
            "s-rev".into(),
            0,
            "2026-11-12T17:00:00Z".into(),
        );
        task.status = TaskStatus::InReview;
        task.updated_at = "2026-11-12T18:30:00Z".into();
        d.store.create_task(&task).unwrap();
        d.store
            .add_task_agent_full(
                "t-rev",
                "ag-busy",
                "implementer",
                Some("s-rev"),
                "",
                "active",
                "2026-11-12T17:00:00Z",
            )
            .unwrap();
        d.store
            .modify_task_state("t-rev", "2026-11-12T18:30:00Z", |s| {
                s.verify_mode = Some(VerifyMode::MissionControl);
                s.pending_report = Some(PendingReport {
                    summary: "v5 ready".into(),
                    artifacts: vec![],
                });
            })
            .unwrap();

        let resp = super::super::agent_routes::list_agents(
            State(d.clone()),
            axum::extract::Query(super::super::agent_routes::AgentsQuery {
                after_name: None,
                after_id: None,
                limit: 50,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let list = body_json(resp).await;
        let arr = list.as_array().unwrap();
        let idle = arr.iter().find(|a| a["id"] == "ag-idle").unwrap();
        assert!(idle["presence"].is_null());
        assert_eq!(idle["needs_you"], 0);

        let busy = arr.iter().find(|a| a["id"] == "ag-busy").unwrap();
        assert_eq!(busy["needs_you"], 2);
        assert_eq!(busy["presence"]["state"], "waiting");
        assert_eq!(busy["presence"]["detail"], "May I release the machine now?");
        assert_eq!(busy["presence"]["session"], "s-wait");
    }

    #[tokio::test]
    async fn agent_now_returns_task_stale_seat_counts_and_board() {
        crate::harness::presence::clear_presence_for_test();
        let d = test_daemon();
        make_agent(&d.store, "snippet");

        let mut task = Task::filed_by_human(
            "eb1b9fce".into(),
            "XDR+ISE demo: fix TTS acronyms".into(),
            "desc".into(),
            "technoxi-xdr-bridge".into(),
            0,
            "2026-11-11T12:00:00Z".into(),
        );
        task.status = TaskStatus::InReview;
        task.updated_at = "2026-11-11T12:31:00Z".into();
        task.last_progress_at = Some("2026-11-11T12:31:00Z".into());
        task.work_plan = vec![WorkStep {
            step: "v5 delivered".into(),
            status: "in_progress".into(),
        }];
        d.store.create_task(&task).unwrap();
        d.store
            .add_task_agent_full(
                "eb1b9fce",
                "snippet",
                "implementer",
                Some("technoxi-xdr-bridge"),
                "",
                "active",
                "2026-11-11T12:00:00Z",
            )
            .unwrap();
        d.store
            .open_turn(
                "eb1b9fce",
                "snippet",
                "technoxi-xdr-bridge",
                "",
                "2026-11-11T12:00:00Z",
            )
            .unwrap();
        d.store
            .modify_task_state("eb1b9fce", "2026-11-11T12:31:00Z", |s| {
                s.verify_mode = Some(VerifyMode::MissionControl);
                s.current_step = "v5 delivered".into();
                s.review_started_at = "2026-11-11T12:31:00Z".into();
                s.pending_report = Some(PendingReport {
                    summary: "Built v5 with ISE spoken as Ice".into(),
                    artifacts: vec![],
                });
            })
            .unwrap();

        // User turn after last_progress_at also makes seat stale.
        d.store
            .save_session_scalar(
                "technoxi-xdr-bridge",
                "wk",
                "/tmp",
                Some("XDR"),
                "idle",
                "{}",
                "2026-11-11T12:00:00Z",
                "2026-11-12T19:17:00Z",
            )
            .unwrap();
        d.store
            .append_conversation_messages(
                "technoxi-xdr-bridge",
                &[HarnessMessage::User {
                    content: "user took session back".into(),
                }],
                "2026-11-12T19:17:00Z",
            )
            .unwrap();

        for i in 1..=6 {
            d.store
                .record_board_entry(
                    "snippet",
                    BoardEntryKind::Progress,
                    &NewBoardEntry {
                        summary: &format!("entry {i}"),
                        created_at: &format!("2026-11-12T18:0{i}:00Z"),
                        ..Default::default()
                    },
                )
                .unwrap();
        }

        let resp = agent_now(State(d.clone()), Path("snippet".into())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["agent_id"], "snippet");
        assert_eq!(body["presence"]["state"], "waiting");
        assert_eq!(body["current_task"]["id"], "eb1b9fce");
        assert_eq!(body["current_task"]["status"], "in_review");
        assert_eq!(body["current_task"]["step"], "v5 delivered");
        assert_eq!(body["current_task"]["verify_mode"], "mission_control");

        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session"], "technoxi-xdr-bridge");
        assert_eq!(sessions[0]["task_id"], "eb1b9fce");
        assert_eq!(sessions[0]["holder"], true);
        assert_eq!(sessions[0]["stale"], true);

        assert_eq!(body["counts"]["open_tasks"], 1);
        assert_eq!(body["counts"]["in_review"], 1);
        assert_eq!(body["counts"]["sessions"], 1);

        let board = body["board"].as_array().unwrap();
        assert_eq!(board.len(), 5);
        assert_eq!(board[0]["summary"], "entry 6");
        assert_eq!(board[4]["summary"], "entry 2");
    }
}
