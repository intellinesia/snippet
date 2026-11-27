//! Progress reports from a working session to its agent's inbox.
//!
//! The daemon only decides when one is due. The working session writes it,
//! because it already knows what it did and its cache is warm. The nudge rides
//! on a tool result rather than a reminder because CLI-backed sessions take
//! reminders only at the start of a message, and a result sits at the tail of
//! the transcript, so neither path disturbs the cached prefix.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::coordination::{
    BoardEntry, BoardEntryKind, BoardQuery, NewBoardEntry, SNIPPET_AGENT_ID, TaskStatus,
};
use crate::store::Store;
use crate::tools::ToolContext;
const REPORT_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MIN_TOOL_CALLS: u32 = 20;
const RECENT_LABELS: usize = 6;
const LABEL_CHARS: usize = 80;
const INBOX_DIGEST_ROWS: usize = 6;
const INBOX_DIGEST_SCAN: u32 = 30;
const DIGEST_SUMMARY_CHARS: usize = 240;
const LEDGER_LOG_ROWS: u32 = 12;
const LEDGER_LOG_LINES: usize = 6;
const AUTOMATIC_PREFIX: &str = "Automatic, the session wrote no report:";

struct Clock {
    since: Instant,
    tool_calls: u32,
    nudged_at: Option<Instant>,
    recent: VecDeque<String>,
}

impl Clock {
    fn new(now: Instant) -> Self {
        Self {
            since: now,
            tool_calls: 0,
            nudged_at: None,
            recent: VecDeque::new(),
        }
    }

    fn note_call(&mut self, now: Instant) -> bool {
        self.tool_calls += 1;
        let due = self.tool_calls >= MIN_TOOL_CALLS
            && self
                .nudged_at
                .is_none_or(|at| now.duration_since(at) >= REPORT_INTERVAL);
        if due {
            self.nudged_at = Some(now);
        }
        due
    }
}

/// What a call says it's doing, for a report the session didn't write itself.
pub fn call_label(arguments: &Value) -> String {
    ["label", "toolSummary"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .chars()
        .take(LABEL_CHARS)
        .collect()
}

static CLOCKS: LazyLock<Mutex<HashMap<String, Clock>>> = LazyLock::new(Default::default);

fn reporting_session(ctx: &ToolContext) -> Option<&str> {
    if ctx.lane_origin().is_some() || ctx.is_lead() {
        return None;
    }
    ctx.durable_session_id()
        .filter(|id| !crate::session::is_inbox_session_id(id))
}

/// Counts one finished tool call; returns the note to attach when a report is due.
pub fn note_tool_call(ctx: &ToolContext, tool: &str, label: String) -> Option<String> {
    let session = reporting_session(ctx)?;
    let now = Instant::now();
    let mut clocks = CLOCKS.lock().unwrap_or_else(|e| e.into_inner());
    if tool == "update_plan" {
        clocks.insert(session.to_string(), Clock::new(now));
        return None;
    }
    let clock = clocks
        .entry(session.to_string())
        .or_insert_with(|| Clock::new(now));
    if clock.recent.len() == RECENT_LABELS {
        clock.recent.pop_front();
    }
    clock.recent.push_back(if label.is_empty() {
        tool.to_string()
    } else {
        label
    });
    let ignored = clock.nudged_at.is_some();
    if !clock.note_call(now) {
        return None;
    }
    // Some models pass over the note, and the inbox must still learn what happened.
    if ignored {
        let summary = format!(
            "{AUTOMATIC_PREFIX} {} tool calls over {} minutes. Recent: {}.",
            clock.tool_calls,
            now.duration_since(clock.since).as_secs() / 60,
            clock.recent.iter().cloned().collect::<Vec<_>>().join("; "),
        );
        clocks.insert(session.to_string(), Clock::new(now));
        drop(clocks);
        if let Err(error) = record_progress(ctx, session, &summary) {
            eprintln!("progress {session}: {error}");
        }
        return None;
    }
    Some(format!(
        "Progress report due: {} tool calls over {} minutes since your last one. Update your plan with `update_plan` (marking done steps with a `note`) so your board stays current.",
        clock.tool_calls,
        now.duration_since(clock.since).as_secs() / 60,
    ))
}



fn open_store(ctx: &ToolContext) -> Option<Store> {
    Store::open_cached(
        ctx.store_path()
            .unwrap_or_else(crate::store::default_db_path),
    )
    .ok()
}

/// The agent doing the work here and the task it counts towards. A plain
/// conversation has no bound agent and runs as `snippet`.
pub(crate) fn working_as(ctx: &ToolContext, session: &str) -> (String, Option<String>) {
    if let Some(member) =
        open_store(ctx).and_then(|db| db.member_by_session(session).ok().flatten())
    {
        return (member.agent_id, Some(member.task_id));
    }
    let task = open_store(ctx).and_then(|db| {
        let task = db
            .list_tasks(Some(session), Some(&TaskStatus::InProgress))
            .ok()?
            .into_iter()
            .find(|t| t.reporting_session.as_deref() == Some(session))?;
        let holder = db
            .list_task_agents(&task.id)
            .ok()?
            .into_iter()
            .find(|m| m.status == "active" && m.removed_at.is_none())
            .map(|m| m.agent_id);
        Some((holder, task.id))
    });
    let (holder, task_id) = task.map_or((None, None), |(holder, id)| (holder, Some(id)));
    let agent = holder
        .or_else(|| ctx.agent_id().map(str::to_string))
        .unwrap_or_else(|| SNIPPET_AGENT_ID.to_string());
    (agent, task_id)
}

fn record_progress(ctx: &ToolContext, session: &str, summary: &str) -> Result<String, String> {
    let (agent, task) = working_as(ctx, session);
    let workspace = ctx.workspace_root().display().to_string();
    let db = open_store(ctx).ok_or("open store failed")?;
    let now = chrono::Utc::now().to_rfc3339();
    db.record_board_entry(
        &agent,
        BoardEntryKind::Progress,
        &NewBoardEntry {
            session_id: Some(session),
            workspace: Some(&workspace),
            summary,
            correlation_id: task.as_deref(),
            created_at: &now,
        },
    )
    .map_err(|e| format!("record progress: {e}"))?;
    Ok(agent)
}

/// The written log lines a ledger reader gets, oldest first. Empty when none.
pub fn ledger_log(db: &Store, task_id: &str) -> String {
    let rows = db
        .task_log(task_id, None, LEDGER_LOG_ROWS)
        .unwrap_or_default();
    let mut lines: Vec<String> = rows
        .iter()
        .filter(|row| {
            row.kind == BoardEntryKind::Progress.as_str()
                && !row.summary.starts_with(AUTOMATIC_PREFIX)
        })
        .take(LEDGER_LOG_LINES)
        .map(|row| {
            format!(
                "- {} {}: {}",
                row.created_at.get(..16).unwrap_or(&row.created_at),
                row.agent_id,
                clip_summary(&row.summary),
            )
        })
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    lines.reverse();
    format!(
        "\nprogress log so far (read.board with task_id for more):\n{}",
        lines.join("\n")
    )
}

fn clip_summary(summary: &str) -> String {
    let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    match summary.char_indices().nth(DIGEST_SUMMARY_CHARS) {
        Some((cut, _)) => format!("{}…", &summary[..cut]),
        None => summary,
    }
}

/// What an inbox reads about its agent's sessions, in its own tail reminder.
pub fn inbox_digest(db: &Store, agent: &str) -> String {
    let rows = db
        .read_board(agent, &BoardQuery::default(), INBOX_DIGEST_SCAN)
        .unwrap_or_default();
    let lines: Vec<String> = rows
        .iter()
        .filter(|row| {
            row.kind == BoardEntryKind::Progress.as_str()
                || row.kind == BoardEntryKind::Reported.as_str()
        })
        .take(INBOX_DIGEST_ROWS)
        .map(digest_line)
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "What your sessions reported most recently (read.board for more):\n{}",
        lines.join("\n")
    )
}

fn digest_line(row: &BoardEntry) -> String {
    format!(
        "- {} {} in {}: {}",
        row.created_at.get(..16).unwrap_or(&row.created_at),
        row.kind,
        row.session_id.as_deref().unwrap_or("no session"),
        clip_summary(&row.summary),
    )
}

const DESK_STATE_EVENTS: u32 = 8;
const DESK_STATE_SESSIONS: u32 = 6;
const DESK_STATE_SNOOZES: usize = 4;
const DESK_STATE_SUMMARY_CHARS: usize = 90;
pub const DESK_STATE_MAX_BYTES: usize = 1500;

/// The desk's live state for its reminder: open events, their deadlines and
/// the work sessions it is waiting on, capped so it fits every turn.
pub fn desk_state_render(db: &Store, agent_id: &str) -> String {
    desk_state_render_excluding(db, agent_id, &[])
}

/// Same as [`desk_state_render`], omitting events already listed in the
/// accompanying `<desk_events>` batch so a wake never prints an event twice.
pub fn desk_state_render_excluding(
    db: &Store,
    agent_id: &str,
    exclude_event_ids: &[&str],
) -> String {
    let events: Vec<_> = db
        .list_open_desk_events(agent_id, DESK_STATE_EVENTS)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| !exclude_event_ids.contains(&e.id.as_str()))
        .collect();
    let sessions = db
        .desk_work_sessions(agent_id, DESK_STATE_SESSIONS)
        .unwrap_or_default();
    let snoozes = db
        .list_active_group_snoozes(agent_id, &chrono::Utc::now().to_rfc3339())
        .unwrap_or_default();
    if events.is_empty() && sessions.is_empty() && snoozes.is_empty() {
        return String::new();
    }
    let mut lines = vec!["now:".to_string()];
    if let Ok((used, cap)) =
        crate::coordination_tools::slot_usage(db, agent_id, &chrono::Utc::now().to_rfc3339())
    {
        let hint = if used >= cap { " (at cap: slot.claim)" } else { "" };
        lines.push(format!("  slots: {used}/{cap}{hint}"));
    }
    if !snoozes.is_empty() {
        let mut parts: Vec<String> = snoozes
            .iter()
            .take(DESK_STATE_SNOOZES)
            .map(|s| match s.until_at.as_deref() {
                Some(until) => format!("{} until {until}", s.thread_id),
                None => format!("{} until pinged", s.thread_id),
            })
            .collect();
        if snoozes.len() > DESK_STATE_SNOOZES {
            parts.push(format!("+{} more", snoozes.len() - DESK_STATE_SNOOZES));
        }
        lines.push(format!("  snoozes (group.snooze off=true ends one): {}", parts.join(", ")));
    }
    if events.is_empty() {
        if exclude_event_ids.is_empty() {
            lines.push("  events: none open".to_string());
        }
    } else {
        let label = if exclude_event_ids.is_empty() {
            "  events (event.ack/noop/defer; re=<id> on a call acks it):"
        } else {
            "  other open events (event.ack/noop/defer; re=<id> on a call acks it):"
        };
        lines.push(label.to_string());
        for e in &events {
            let mut line = format!("  - {} {} p{}", e.id, e.kind, e.priority);
            if e.is_mention {
                line.push_str(" @");
            }
            if let Some(reply) = e.reply_thread.as_deref().or(e.thread_id.as_deref()) {
                line.push_str(&format!(" reply={reply}"));
            }
            if let Some(due) = e.deadline_at.as_deref() {
                line.push_str(&format!(" due={due}"));
            }
            let summary: String = e
                .summary
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let summary = match summary.char_indices().nth(DESK_STATE_SUMMARY_CHARS) {
                Some((cut, _)) => format!("{}…", &summary[..cut]),
                None => summary,
            };
            line.push_str(&format!(": {summary}"));
            lines.push(line);
        }
        let mut deadlines: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| e.deadline_at.as_deref().map(|d| (d, e.id.as_str())))
            .collect();
        deadlines.sort();
        if !deadlines.is_empty() {
            lines.push(format!(
                "  deadlines: {}",
                deadlines
                    .iter()
                    .map(|(d, id)| format!("{id}@{d}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if !sessions.is_empty() {
        let now_ts = chrono::Utc::now().timestamp();
        lines.push("  work:".to_string());
        for s in &sessions {
            let quiet = chrono::DateTime::parse_from_rfc3339(&s.updated_at)
                .ok()
                .map(|dt| now_ts.saturating_sub(dt.timestamp()))
                .filter(|&secs| secs >= 24 * 3600)
                .map(|secs| format!(" quiet={}d", secs / (24 * 3600)))
                .unwrap_or_default();
            lines.push(format!(
                "  - session={} task={} reply={}{quiet}",
                s.session_id,
                s.task_id,
                s.reply_thread.as_deref().unwrap_or("-")
            ));
        }
    }
    let mut out = String::new();
    for line in lines {
        if out.len() + line.len() + 1 > DESK_STATE_MAX_BYTES - 5 {
            out.push_str("\n  …");
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod desk_state_tests {
    use super::*;
    use crate::coordination::desk_inbox::NewDeskEvent;

    #[test]
    fn renders_open_events_and_stays_under_the_cap() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(desk_state_render(&store, "a"), "");
        let now = "2026-11-26T10:00:00Z";
        store
            .enqueue_desk_event(
                &NewDeskEvent {
                    agent_id: "a".into(),
                    kind: "dm".into(),
                    thread_id: Some("direct:a:human".into()),
                    reply_thread: Some("direct:a:human".into()),
                    is_mention: true,
                    summary: "can you check the deploy?".into(),
                    wake: true,
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        let reminder = store
            .create_self_desk_event("a", "follow up on CI", Some("2026-11-26T12:00:00Z"), None, 2, now)
            .unwrap();
        let out = desk_state_render(&store, "a");
        assert!(out.starts_with("now:"), "{out}");
        assert!(out.contains("  slots: 0/"), "{out}");
        assert!(out.contains("dm p0 @ reply=direct:a:human: can you check the deploy?"), "{out}");
        assert!(out.contains(&format!("{} self p2 due=2026-11-26T12:00:00Z", reminder.id)), "{out}");
        assert!(out.contains(&format!("deadlines: {}@2026-11-26T12:00:00Z", reminder.id)), "{out}");

        for i in 0..20 {
            store
                .enqueue_desk_event(
                    &NewDeskEvent {
                        agent_id: "a".into(),
                        kind: "progress".into(),
                        reply_thread: Some(format!("task-thread-with-a-long-name-{i}-{}", "x".repeat(60))),
                        deadline_at: Some("2026-11-26T13:00:00Z".into()),
                        summary: "y".repeat(400),
                        wake: true,
                        ..Default::default()
                    },
                    now,
                )
                .unwrap();
        }
        let out = desk_state_render(&store, "a");
        assert!(out.len() <= DESK_STATE_MAX_BYTES, "{}", out.len());
        assert!(out.ends_with('…'), "{out}");
    }
}
