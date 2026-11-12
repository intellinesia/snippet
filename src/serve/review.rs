//! Starting the reviewer for a task that reported done and is waiting in review.

use crate::coordination::{Task, TaskState, TaskStatus, VerifyMode};
use crate::harness::{LaneSpawn, LoopInput};

use super::{Daemon, Shared};

/// With nobody seated to review, the agent doing the work checks it from a fresh
/// read-only lane, which hasn't seen its reasoning.
fn pick_reviewer(d: &Daemon, task: &Task) -> Option<String> {
    let roster = d.store.list_task_agents(&task.id).unwrap_or_default();
    let live = || roster.iter().filter(|m| m.removed_at.is_none());
    live()
        .find(|m| m.status == "active")
        .or_else(|| live().next())
        .map(|m| m.agent_id.clone())
}

fn git(workspace: &std::path::Path, args: &[&str]) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

/// The brief a reviewer starts from: the ledger, what the worker claims, which
/// commits and files changed, where to read the details and what to check.
pub(super) fn review_brief(task: &Task, state: &TaskState, workspace: &std::path::Path) -> String {
    let base = if state.dispatch_rev.is_empty() {
        "HEAD"
    } else {
        state.dispatch_rev.as_str()
    };
    let stat = git(workspace, &["diff", "--stat", base]);
    let commits = git(workspace, &["log", "--oneline", &format!("{base}..HEAD")]);
    let untracked = git(workspace, &["ls-files", "--others", "--exclude-standard"]);
    let report = state.pending_report.clone().unwrap_or_default();
    format!(
        "Review task \"{title}\" (task {id}) before it is accepted. You are read-only: do not change files. The worker says it is done; your job is to find out whether that is true.\n\n\
         {ledger}\n\n\
         worker's report:\n{summary}\n\n\
         workspace: {workspace}\ncommits since dispatch ({base}):\n{commits}\nfiles changed:\n{stat}\nnew untracked files:\n{untracked}\n\n\
         Read the work yourself: `git -C {workspace} diff {base} -- <path>` for the changes, and read_coordination_thread with thread_id `task:{id}` for the conversation behind them, including decisions and clarifications made along the way. Don't rely on the report alone.\n\n\
         Checklist:\n\
         1. Is the report true? Re-run what it cites and compare, open any artifact, look at any screenshot with view_image.\n\
         2. Is the scope delivered, and only that? A scope change or something left out needs a good reason in the report or the room. Flag unrelated files, leftovers and anything half-done.\n\
         3. Are there tests for new behaviour, and do they pass?\n\
         4. Anything unsafe, hard-coded or fragile; any claim in the report the diff does not support.\n\
         5. Content work: does it fit the audience, length, tone and definition of good in the ledger? Would its real reader accept it? For ideas and proposals, is each one specific and worth paying for?\n\n\
         Finish by calling submit_review (task_id {id}): verdict approve only if the scope is delivered and the report holds up, and say what you ran; otherwise changes with one finding per problem (file:line, what is wrong, what to do).",
        title = task.title,
        id = task.id,
        ledger = state.render(true),
        summary = report.summary,
        workspace = workspace.display(),
    )
}

/// review_due envelopes Mission Control gets before the task goes to the user.
const MAX_REVIEW_NUDGES: u32 = 3;

/// What the review clock owes a Mission Control or user verdict: one
/// `[review_due]` envelope per further timeout (at most three), then the user.
/// `None` while the current window is still open or the task was escalated.
pub(super) fn review_due_action(
    state: &crate::coordination::TaskState,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ReviewDue> {
    if state.review_escalated {
        return None;
    }
    // A report held before the clock existed has no start; treat it as due now.
    let elapsed = chrono::DateTime::parse_from_rfc3339(&state.review_started_at)
        .map(|at| (now - at.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(crate::review_tools::REVIEW_TIMEOUT_SECS + 1);
    let period = crate::review_tools::REVIEW_TIMEOUT_SECS;
    let due = elapsed > (state.review_nudges as i64 + 1) * period;
    if !due {
        return None;
    }
    Some(if state.review_nudges < MAX_REVIEW_NUDGES {
        ReviewDue::Nudge
    } else {
        ReviewDue::Escalate
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReviewDue {
    Nudge,
    Escalate,
}

/// Start a reviewer for every task waiting on one, drive the Mission Control
/// and user verdicts on their clock, and hand tasks whose reviewer went quiet
/// to Mission Control.
pub(super) async fn drive_reviews(d: &Shared) {
    let Ok(tasks) = d.store.list_tasks(None, Some(&TaskStatus::InReview)) else {
        return;
    };
    let now = chrono::Utc::now();
    let stamp = now.to_rfc3339();
    for task in tasks {
        let Ok(Some(state)) = d.store.task_state(&task.id) else {
            continue;
        };
        match crate::review_tools::review_route(&d.store, &task, &state) {
            VerifyMode::Reviewer => {
                if state.review_escalated {
                    continue;
                }
                if state.review_started_at.is_empty() {
                    start_review(d.as_ref(), &task, &state, &stamp).await;
                    continue;
                }
            }
            VerifyMode::MissionControl | VerifyMode::User => {
                review_due(d, &task, &state, now).await;
                continue;
            }
            VerifyMode::SelfCheck => continue,
        }
        let elapsed_secs = chrono::DateTime::parse_from_rfc3339(&state.review_started_at)
            .map(|at| (now - at.with_timezone(&chrono::Utc)).num_seconds())
            .unwrap_or(crate::review_tools::REVIEW_TIMEOUT_SECS + 1);
        let lane_alive = elapsed_secs < 20
            || crate::session::state_path_for_id(&task.session_id)
                .and_then(|p| crate::session::read_session_meta(&p))
                .is_some_and(|meta| {
                    meta.lanes
                        .iter()
                        .any(|l| l.status == crate::lanes::LaneStatus::Running)
                });
        let seated = crate::review_tools::seated_reviewer(&d.store, &task).is_some();
        if elapsed_secs > crate::review_tools::REVIEW_TIMEOUT_SECS || (!seated && !lane_alive) {
            crate::review_tools::escalate(
                &d.store,
                &task.id,
                "the reviewer gave no verdict in time; check the work and submit_review yourself",
                &stamp,
            );
        }
    }
}

/// One step of the Mission Control / user review clock: nag Mission Control
/// with the report, or after the last nudge put the question to the user.
async fn review_due(
    d: &Shared,
    task: &Task,
    state: &crate::coordination::TaskState,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(action) = review_due_action(state, now) else {
        return;
    };
    let stamp = now.to_rfc3339();
    let report = state
        .pending_report
        .as_ref()
        .map(|r| r.summary.as_str())
        .unwrap_or("");
    let age_minutes = chrono::DateTime::parse_from_rfc3339(&state.review_started_at)
        .map(|at| (now - at.with_timezone(&chrono::Utc)).num_minutes())
        .unwrap_or(0);
    match action {
        ReviewDue::Nudge => {
            let nudges = state.review_nudges + 1;
            let _ = d
                .store
                .modify_task_state(&task.id, &stamp, |s| s.review_nudges = nudges);
            let envelope = format!(
                "[review_due]\ntask_id: {}\ntitle: {}\nin review for: {age_minutes} minutes\nworker's report:\n{report}\nNo verdict has arrived. Call submit_review with verdict approve, or changes with findings; if neither fits, hand it to the user.\n[/review_due]",
                task.id, task.title
            );
            super::autonomy::deliver_to_mission_control(d, envelope).await;
        }
        ReviewDue::Escalate => {
            let _ = d
                .store
                .modify_task_state(&task.id, &stamp, |s| s.review_escalated = true);
            let body = format!(
                "\"{}\" has been in review for {age_minutes} minutes with no verdict from Mission Control.\n\nWorker's report:\n{report}\n\nApprove it or send it back?",
                task.title
            );
            crate::coordination::reach::reach(
                &d.store,
                crate::mission_control::SESSION_ID,
                crate::coordination::reach::Kind::Question,
                &task.title,
                &body,
                false,
                None,
                Some(&task.id),
            );
        }
    }
}

async fn start_review(d: &Daemon, task: &Task, state: &TaskState, now: &str) {
    let workspace = crate::session::state_path_for_id(&task.session_id)
        .and_then(|p| crate::session::read_session_meta(&p))
        .map(|s| std::path::PathBuf::from(s.workspace));
    let Some(workspace) = workspace else {
        crate::review_tools::escalate(
            &d.store,
            &task.id,
            "no reviewer could start: the work session's workspace is gone",
            now,
        );
        return;
    };
    if let Some(member) = crate::review_tools::seated_reviewer(&d.store, task) {
        crate::members::seat_holder(&d.store, task);
        match crate::members::seat(&d.store, task, &member.agent_id, now) {
            Ok(session) => {
                let _ = d
                    .store
                    .modify_task_state(&task.id, now, |s| s.review_started_at = now.to_string());
                let brief = review_brief(task, state, &workspace);
                let envelope = format!(
                    "[review_request]\ntask_id: {}\nround: {}\nYou are the reviewer seated on this task, in your own conversation. Only your approval finishes the work. Ask the worker anything you need in the room (post_task_coordination with `to`).\n\n{brief}\n[/review_request]",
                    task.id,
                    state.review_rounds + 1,
                );
                d.deliver(&session, LoopInput::UserMessage(envelope)).await;
            }
            Err(error) => crate::review_tools::escalate(
                &d.store,
                &task.id,
                &format!(
                    "the seated reviewer {} couldn't start: {error}; check the work and submit_review yourself.",
                    member.agent_id
                ),
                now,
            ),
        }
        return;
    }
    let Some(reviewer) = pick_reviewer(d, task) else {
        crate::review_tools::escalate(
            &d.store,
            &task.id,
            "this task has no agent to review it; check the work and submit_review yourself",
            now,
        );
        return;
    };
    let profile = d.store.agent_profile(&reviewer).ok().flatten();
    let spec = LaneSpawn {
        title: format!("Review: {}", task.title),
        brief: review_brief(task, state, &workspace),
        read_only: true,
        agent: Some(reviewer),
        profile,
    };
    let _ = d
        .store
        .modify_task_state(&task.id, now, |s| s.review_started_at = now.to_string());
    d.deliver(&task.session_id, LoopInput::SpawnLane(spec))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::PendingReport;

    #[test]
    fn the_review_brief_lists_files_and_points_at_the_diff_and_room() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            )
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        let base = crate::task_state_tools::head_rev(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let task = Task::filed_by_human(
            "r1".into(),
            "Fix a".into(),
            "d".into(),
            "s".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        let mut state = TaskState::seeded(&task);
        state.dispatch_rev = base;
        state.pending_report = Some(PendingReport {
            summary: "changed a".into(),
            artifacts: vec![],
        });
        let brief = review_brief(&task, &state, dir.path());
        for needle in [
            "[task_ledger v1]",
            "worker's report:\nchanged a",
            "a.txt",
            "thread_id `task:r1`",
            "submit_review (task_id r1)",
            "Checklist",
        ] {
            assert!(brief.contains(needle), "missing {needle}: {brief}");
        }
        assert!(
            !brief.contains("+two"),
            "the diff itself stays out of the brief: {brief}"
        );
    }

    #[test]
    fn review_due_nudges_once_per_timeout_then_goes_to_the_user() {
        let now = chrono::Utc::now();
        let t = crate::review_tools::REVIEW_TIMEOUT_SECS;
        let mut state = crate::coordination::TaskState::default();
        state.review_started_at = (now - chrono::Duration::seconds(t + 1)).to_rfc3339();
        assert_eq!(review_due_action(&state, now), Some(ReviewDue::Nudge));
        // A restart inside the next window must not re-fire: the nudge count
        // is what the ledger persisted.
        state.review_nudges = 1;
        state.review_started_at = (now - chrono::Duration::seconds(t + t / 2)).to_rfc3339();
        assert_eq!(review_due_action(&state, now), None);
        state.review_started_at = (now - chrono::Duration::seconds(2 * t + 1)).to_rfc3339();
        assert_eq!(review_due_action(&state, now), Some(ReviewDue::Nudge));
        state.review_nudges = 3;
        state.review_started_at = (now - chrono::Duration::seconds(4 * t + 1)).to_rfc3339();
        assert_eq!(review_due_action(&state, now), Some(ReviewDue::Escalate));
        state.review_escalated = true;
        assert_eq!(review_due_action(&state, now), None);
    }

    #[test]
    fn review_nudges_survive_a_store_round_trip() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let task = Task::filed_by_human(
            "rd1".into(),
            "T".into(),
            "d".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        store.create_task(&task).unwrap();
        store
            .modify_task_state("rd1", "t", |s| {
                s.review_nudges = 2;
                s.review_started_at =
                    (chrono::Utc::now() - chrono::Duration::seconds(2 * 60 * 60)).to_rfc3339();
            })
            .unwrap();
        let state = store.task_state("rd1").unwrap().unwrap();
        assert_eq!(state.review_nudges, 2);
        let now = chrono::Utc::now();
        // 2h in with 2 nudges recorded: inside the third window, nothing due.
        assert_eq!(review_due_action(&state, now), None);
    }
}
