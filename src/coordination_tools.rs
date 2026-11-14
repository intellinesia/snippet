use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::coordination::{
    NotificationMarker, Store, StoreError, TaskStatus, types::CoordinationEvent,
};
use crate::llm::NativeToolDefinition;
use crate::tools::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

/// Tools every coordination-aware session (Mission Control and workers) can use
/// to discover peers and post to the board.
pub fn add_coordination_tools(registry: &mut ToolRegistry) {
    registry.insert(ListCoordinationAgents);
    registry.insert(PostCoordinationMessage);
    registry.insert(ReadCoordinationThread);
    // Direct peer conversation: the human and any agent can address one named
    // agent, and every agent can always reach Mission Control.
    registry.insert(SendAgentMessage);
    registry.insert(ReadAgentThread);
    registry.insert(ReadAgentInbox);
    // Per-agent coordination memory: recall before acting, and record what is
    // worth keeping.
    registry.insert(ReadCoordinationBoard);
    registry.insert(RecordCoordinationNote);
    // Task-based coordination and session lease handoff
    registry.insert(MessageMissionControl);
    registry.insert(PostTaskCoordination);
    registry.insert(PassTurn);
    registry.insert(InspectTask);
    registry.insert(ClaimAndDispatchTask);
    registry.insert(crate::agent_wakes::WakeScheduleTool);
    registry.insert(crate::agent_context::ContextLimitTool);
    registry.insert(crate::keep_warm::KeepWarmTool);
    registry.insert(crate::mission_tools::ReachPolicyTool);
    registry.insert(crate::harness_map::HarnessMapTool);
    registry.insert(ShareArtifactTool);
    registry.insert(GroupThread);
}

/// Open the store the daemon owns: the path bound to this session when present,
/// otherwise the canonical general-purpose store.
fn db(ctx: &ToolContext) -> Result<Store, ToolError> {
    let path = ctx
        .store_path()
        .unwrap_or_else(crate::store::default_db_path);
    Store::open(path).map_err(|e| ToolError::msg(format!("open store: {e}")))
}

/// The board identity of the current session. The model never supplies this: a
/// session posts as itself, so it cannot impersonate another agent.
///
/// The kind separates the two cases that used to be conflated: an agent working
/// the session posts as `agent`, a plain session posts as `session`. Both are
/// addressable, but only the former is an identity the directory knows.
fn actor(ctx: &ToolContext) -> Result<(&'static str, String), ToolError> {
    // A lane speaks as the agent it was given, else as whoever its parent
    // session speaks for.
    if ctx.lane_origin().is_some()
        && let Some(agent_id) = ctx.agent_id()
    {
        return Ok(("agent", agent_id.to_string()));
    }
    let session = ctx
        .durable_session_id()
        .or(ctx.lane_origin().map(|(session, _)| session));
    if let Some(member) =
        session.and_then(|session| db(ctx).ok()?.member_by_session(session).ok().flatten())
    {
        return Ok(("agent", member.agent_id));
    }
    // The lease holder on work dispatched here comes first: a lease can move an
    // agent into a session that belongs to someone else, or to no one.
    if let Some(agent_id) = session.and_then(|session| lease_holder(ctx, session)) {
        return Ok(("agent", agent_id));
    }
    if let Some(agent_id) = ctx.agent_id() {
        return Ok(("agent", agent_id.to_string()));
    }
    if let Some(session_id) = session {
        return Ok(("session", session_id.to_string()));
    }
    Err(ToolError::msg(
        "posting to the coordination board requires a session identity",
    ))
}

/// Refuse a board thread this session has no place in. Allowed: the shared
/// room, a room it is a member of, the room of a task dispatched to this
/// session, and every room for Mission Control, which coordinates them all.
fn require_thread_access(
    db: &Store,
    ctx: &ToolContext,
    actor_id: &str,
    thread_id: &str,
) -> Result<(), ToolError> {
    if thread_id == crate::serve::COORDINATION_THREAD {
        return Ok(());
    }
    let lookup = |e: StoreError| ToolError::msg(format!("look up thread: {e}"));
    if !db.thread_exists(thread_id).map_err(lookup)? {
        return Err(ToolError::msg(format!("no thread `{thread_id}`")));
    }
    if actor_id == crate::mission_control::SESSION_ID
        || db
            .list_thread_participants(thread_id)
            .map_err(lookup)?
            .iter()
            .any(|(member, _)| member == actor_id)
    {
        return Ok(());
    }
    let own_task = db
        .get_task_by_thread(thread_id)
        .map_err(lookup)?
        .is_some_and(|task| Some(task.session_id.as_str()) == ctx.durable_session_id());
    if own_task {
        return Ok(());
    }
    Err(ToolError::msg(format!(
        "this session is not a member of `{thread_id}`"
    )))
}

/// The specialized agent holding the lease on a task this session is running.
/// The default worker is not one: it has no identity beyond the session's own.
pub(crate) fn speaks_as(ctx: &ToolContext) -> Option<(&'static str, String)> {
    actor(ctx).ok()
}

/// The task in progress in a session, if any: its id and title.
pub(crate) fn session_task(ctx: &ToolContext, session_id: &str) -> Option<(String, String)> {
    let db = db(ctx).ok()?;
    db.tasks_worked_in(
        session_id,
        Some(&crate::coordination::TaskStatus::InProgress),
    )
    .ok()?
    .into_iter()
    .find(|task| task.reporting_session.as_deref() == Some(session_id))
    .map(|task| (task.id, task.title))
}

pub(crate) fn task_awareness(ctx: &ToolContext) -> String {
    use crate::coordination::TaskStatus;
    if ctx.agent_id() == Some(crate::mission_control::SESSION_ID) {
        return String::new();
    }
    let path = ctx
        .store_path()
        .unwrap_or_else(crate::store::default_db_path);
    let Ok(db) = Store::open_cached(path) else {
        return String::new();
    };
    let session = ctx
        .durable_session_id()
        .or(ctx.lane_origin().map(|(session, _)| session));
    let label = |status: &TaskStatus| status.as_str().replace('_', " ");
    let mut out: Vec<String> = Vec::new();
    if let (Some(agent), Some(session)) = (ctx.agent_id(), session)
        && crate::session::is_inbox_session_id(session)
    {
        let digest = crate::progress::inbox_digest(&db, agent);
        if !digest.is_empty() {
            out.push(digest);
        }
    }

    let current: Vec<crate::coordination::Task> = session
        .and_then(|session| db.tasks_worked_in(session, None).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|task| !task.status.is_terminal() && task.status != TaskStatus::Todo)
        .collect();
    let mut agent = ctx.agent_id().map(str::to_string);
    for task in &current {
        let roster: Vec<_> = db
            .list_task_agents(&task.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| m.removed_at.is_none())
            .collect();
        if agent.is_none() {
            agent = roster
                .iter()
                .find(|m| m.status == "active")
                .map(|m| m.agent_id.clone());
        }
        let mut text = format!(
            "Task you're on: \"{}\" (task {}, {}), from {}.\n{}",
            sanitize_line(&task.title),
            task.id,
            label(&task.status),
            task.created_by_id,
            crate::task_state_tools::ledger_block(&db, task, false)
        );
        if !roster.is_empty() {
            let team: Vec<String> = roster
                .iter()
                .map(|m| {
                    if m.status == "active" {
                        format!("{} ({}, holds the turn)", m.agent_id, m.role)
                    } else {
                        format!("{} ({})", m.agent_id, m.role)
                    }
                })
                .collect();
            text.push_str(&format!("\nTeam: {}", team.join("; ")));
        }
        if let Some(me) = session
            .and_then(|s| {
                roster
                    .iter()
                    .find(|m| m.work_session_id.as_deref() == Some(s))
            })
            .filter(|m| m.status != "active")
        {
            let holder = roster
                .iter()
                .find(|m| m.status == "active")
                .map_or("nobody", |m| m.agent_id.as_str());
            text.push_str(&format!(
                "\nYou are seated here as {} and {holder} holds the turn: read anything and talk in the room, but only the holder changes files.",
                me.agent_id
            ));
        }
        let waits: Vec<String> = db
            .task_links(&task.id)
            .unwrap_or_default()
            .into_iter()
            .filter(|l| {
                l.kind == crate::coordination::TaskLinkKind::Blocks && l.to_task_id == task.id
            })
            .filter_map(|l| db.get_task(&l.from_task_id).ok().flatten())
            .filter(|t| !t.status.is_terminal())
            .map(|t| {
                format!(
                    "\"{}\" (task {}, {})",
                    sanitize_line(&t.title),
                    t.id,
                    label(&t.status)
                )
            })
            .collect();
        if !waits.is_empty() {
            text.push_str(&format!("\nWaiting on: {}", waits.join("; ")));
        }
        out.push(text);
    }
    if !current.is_empty() {
        out.push(
            "Share milestones and blockers with everyone on the task via post_task_coordination; report with report_mission_task when done.".to_string(),
        );
    }

    if let Some(agent) = agent.as_deref() {
        let others: Vec<String> = db
            .list_tasks_page(
                &crate::coordination::TaskFilter {
                    status: None,
                    agent_id: Some(agent),
                },
                None,
                50,
            )
            .unwrap_or_default()
            .into_iter()
            .filter(|t| !t.status.is_terminal() && !current.iter().any(|c| c.id == t.id))
            .take(8)
            .map(|t| {
                let place = if t.session_id.is_empty() {
                    "not dispatched yet".to_string()
                } else {
                    format!("session {}", t.session_id)
                };
                format!(
                    "- \"{}\" (task {}, {}, {place})",
                    sanitize_line(&t.title),
                    t.id,
                    label(&t.status)
                )
            })
            .collect();
        if !others.is_empty() {
            out.push(format!(
                "Your other open tasks as {agent} (inspect_task for detail; send_agent_message to task:<id> reaches the session doing it):\n{}",
                others.join("\n")
            ));
        }
    }
    out.join("\n")
}

fn sanitize_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn lease_holder(ctx: &ToolContext, session_id: &str) -> Option<String> {
    let db = db(ctx).ok()?;
    let tasks = db
        .list_tasks(
            Some(session_id),
            Some(&crate::coordination::TaskStatus::InProgress),
        )
        .ok()?;
    tasks
        .iter()
        .filter(|task| task.reporting_session.as_deref() == Some(session_id))
        .find_map(|task| {
            db.list_task_agents(&task.id)
                .ok()?
                .into_iter()
                .find(|m| m.status == "active" && m.removed_at.is_none())
                .map(|m| m.agent_id)
                .filter(|agent| agent != crate::coordination::SNIPPET_AGENT_ID)
        })
}

pub struct ListCoordinationAgents;
#[async_trait]
impl Tool for ListCoordinationAgents {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "list_coordination_agents".into(),
            description: "The agent directory: every agent's id, status, capabilities, the inference profile it works on (when it has one) and a one-line summary of what it is for. Use it to choose whom to message, invite onto a task (invite_task_agent) or run a lane as (delegate_task agent). Only active agents take new work.".into(),
            input_schema: schema(json!({}), &[]),
        }
    }
    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let agents = db(ctx)?
            .list_agents()
            .map_err(|e| ToolError::msg(format!("list agents: {e}")))?;
        let root = crate::coordination::agents_root(
            &ctx.mission_control_root()
                .unwrap_or_else(|| crate::config::snippet_home().join("mission-control")),
        );
        let profiles = db(ctx)?.agent_profiles().unwrap_or_default();
        let agents: Vec<Value> = agents
            .into_iter()
            .map(|agent| {
                let summary = crate::coordination::AgentHome::new(root.clone(), &agent.id)
                    .ok()
                    .and_then(|home| home.read_identity().ok())
                    .map(|text| identity_summary(&text))
                    .unwrap_or_default();
                let mut row = serde_json::to_value(&agent).unwrap_or(Value::Null);
                if !summary.is_empty() {
                    row["summary"] = json!(summary);
                }
                if let Some(profile) = profiles.get(&agent.id) {
                    row["profile"] = json!(profile);
                }
                row
            })
            .collect();
        Ok(ToolResult::success(json!({"agents": agents})))
    }
}

fn identity_summary(identity: &str) -> String {
    let para = identity
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty() && !p.starts_with('#'))
        .unwrap_or("");
    let line = para.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 240 {
        format!("{}…", line.chars().take(239).collect::<String>())
    } else {
        line
    }
}

#[derive(Deserialize)]
struct PostArgs {
    thread_id: String,
    body: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}

pub struct PostCoordinationMessage;
#[async_trait]
impl Tool for PostCoordinationMessage {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "post_coordination_message".into(),
            description: "Post a message or update to a coordination board thread. This talks to other permitted agents directly; it does not transfer ownership or acquire a session lease. The post is attributed to this session automatically.".into(),
            input_schema: schema(
                json!({
                    "thread_id":{"type":"string"},
                    "body":{"type":"string"},
                    "idempotency_key":{"type":"string"}
                }),
                &["thread_id", "body"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PostArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        if args.thread_id.trim().is_empty() {
            return Err(ToolError::msg("thread_id must not be empty"));
        }
        if args.body.trim().is_empty() {
            return Err(ToolError::msg("body must not be empty"));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        let db = db(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &args.thread_id)?;
        let key = args
            .idempotency_key
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: args.thread_id.clone(),
            partition_key: format!("thread:{}", args.thread_id),
            sequence: 0,
            event_type: "message.posted".into(),
            actor_kind: actor_kind.to_string(),
            actor_id,
            payload_version: 1,
            payload: stamp_origin(ctx, json!({"body": args.body})),
            causation_id: None,
            correlation_id: None,
            idempotency_key: key,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(&db, saved.clone());
        Ok(ToolResult::success(json!({"event": saved})))
    }
}

/// Default slice of room history handed over when a participant is woken. Small
/// enough to keep the wake cheap, large enough to hold the current exchange.
pub const BOARD_HISTORY_ON_WAKE: u32 = 10;

#[derive(Deserialize)]
struct ReadThreadArgs {
    #[serde(default)]
    thread_id: Option<String>,
    /// Page forward from just after this sequence. Omit to get the most recent
    /// messages instead (the usual first call).
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// One board message, flattened for the model: who said what, when.
fn flatten_event(event: &CoordinationEvent) -> Value {
    let mut out = json!({
        "event_id": event.event_id,
        "sequence": event.sequence,
        "from": event.actor_id,
        "kind": event.actor_kind,
        "at": event.created_at,
        "body": event.payload.get("body").and_then(|v| v.as_str()).unwrap_or_default(),
    });
    if let Some(reply_to) = event.payload.get("reply_to").and_then(|v| v.as_str()) {
        out["reply_to"] = json!(reply_to);
    }
    if let Some(reaction) = event.payload.get("reaction").and_then(|v| v.as_str()) {
        out["reaction"] = json!(reaction);
    }
    out
}

pub struct ReadCoordinationThread;
#[async_trait]
impl Tool for ReadCoordinationThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_coordination_thread".into(),
            description: "Read the coordination room's messages, oldest first. Call with no arguments for the most recent messages; pass after_sequence to page further back (use the returned oldest_sequence) or forward. The wake message already includes the recent history, so use this only to see more.".into(),
            input_schema: schema(
                json!({
                    "thread_id":{"type":"string","description":"defaults to the shared room"},
                    "after_sequence":{"type":"integer","minimum":0,"description":"omit for the most recent messages"},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadThreadArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let thread_id = args
            .thread_id
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| crate::serve::COORDINATION_THREAD.to_string());
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let db = db(ctx)?;
        let (_, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &thread_id)?;

        // No cursor → the most recent messages. Cursor → the next page forward.
        // `after_sequence: 0` deliberately means "from the beginning".
        let (events, page_forward) = match args.after_sequence {
            Some(after) => (
                db.events_for_thread(&thread_id, after, limit)
                    .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
                true,
            ),
            None => (
                db.recent_events_for_thread(&thread_id, limit)
                    .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
                false,
            ),
        };

        let oldest = events.first().map(|e| e.sequence).unwrap_or(0);
        let newest = events.last().map(|e| e.sequence).unwrap_or(0);
        // A full page is a hint there may be more; the caller pages with the
        // oldest sequence to go back, or the newest to move forward.
        let full_page = events.len() as u32 >= limit;
        Ok(ToolResult::success(json!({
            "thread_id": thread_id,
            "messages": events.iter().map(flatten_event).collect::<Vec<_>>(),
            "oldest_sequence": oldest,
            "newest_sequence": newest,
            "may_have_more": full_page,
            "direction": if page_forward { "forward" } else { "recent" },
        })))
    }
}

#[derive(Deserialize)]
struct SendAgentMessageArgs {
    recipient_agent_id: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    urgency: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    reaction: Option<String>,
    #[serde(default)]
    forward: Option<String>,
}

/// The session id named by the most recent human message in a direct thread.
///
/// Pure selection, split from resolution so the rule is testable on its own: the
/// newest message from the human is the one being answered, so its origin wins.
fn latest_human_origin(events: &[CoordinationEvent]) -> Option<&str> {
    events
        .iter()
        .rev()
        .find(|event| event.actor_kind == "human" && event.payload.get("reaction").is_none())
        .and_then(|event| event.payload.get("origin_session"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|session| !session.is_empty())
}

/// The session an agent's reply to `human` should actually land in.
///
/// An agent answers where it was asked. When the last message it received from
/// the human came from a session, that session is where the human is reading —
/// so the reply belongs there, not in the human's own thread. The destination is
/// derived from the thread rather than taken from the model's argument, because a
/// model that names the wrong address loses the answer somewhere the asker is not
/// looking.
fn reply_session_for_agent(db: &Store, agent_id: &str, human_id: &str) -> Option<String> {
    let thread = crate::coordination::direct_thread_id(("agent", agent_id), ("human", human_id));
    let events = db.recent_direct_thread_events(&thread, 10).ok()?;
    let origin = latest_human_origin(&events)?;
    // Only redirect to a session the store actually has: a stale origin would
    // create a delivery that can never land — worse than the human thread.
    db.has_conversation(origin)
        .ok()?
        .then(|| origin.to_string())
}

/// Send a direct message to a peer agent by id.
///
/// The recipient is an AGENT ID from the directory, never a raw thread id: the
/// thread is derived from the pair, so two agents exchanging messages cannot
/// each invent a different conversation. The message is written durably and the
/// daemon's delivery loop wakes the recipient — a tool call never delivers
/// inline, or a message sent to a stopped session would be lost.
pub struct SendAgentMessage;
#[async_trait]
impl Tool for SendAgentMessage {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "send_agent_message".into(),
            description: "Reply to a message or contact an agent. For session-origin messages, copy the exact `reply_to` value (`session:<id>`); use `human` only if no session was named, otherwise an agent id or `task:<id>`. This is conversation only: it cannot create work, transfer ownership or authorise workspace changes. Use Mission Control for tasks and `post_task_coordination` for task updates. `urgent` interrupts; reserve it for blockers or wrong direction. reply_to quotes the message you answer; reaction acknowledges one without words (check: done or agreed, eyes: seen and on it, thumbs_up, question: unclear, thinking).".into(),
            input_schema: schema(
                json!({
                    "recipient_agent_id":{"type":"string","description":"agent id, `task:<id>`, `session:<id>` or `human`"},
                    "body":{"type":"string"},
                    "idempotency_key":{"type":"string"},
                    "kind":{"type":"string","enum":["note","question","status","handoff_note","blocker","result","interrupt"]},
                    "urgency":{"type":"string","enum":["normal","urgent"],"description":"default normal; `interrupt` is always urgent"},
                    "reply_to":{"type":"string"},
                    "reaction":{"type":"string","enum":["check","eyes","thumbs_up","question","thinking"]},
                    "forward":{"type":"string","description":"event id of a chat message to quote under your body as context"}
                }),
                &["recipient_agent_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SendAgentMessageArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let recipient = args.recipient_agent_id.trim();
        if recipient.is_empty() {
            return Err(ToolError::msg("recipient_agent_id must not be empty"));
        }
        let (reply_to, reaction) = crate::coordination::typed::parse_reply_and_reaction(
            args.reply_to.as_deref(),
            args.reaction.as_deref(),
        )
        .map_err(ToolError::msg)?;
        let forward_id = args
            .forward
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        if reaction.is_none() && forward_id.is_none() && args.body.trim().is_empty() {
            return Err(ToolError::msg("body must not be empty"));
        }
        // `human` (or `local`) addresses the operator who is talking to this
        // agent. `session:<id>` answers a message in the session it was asked
        // from, which is where the asker is reading. Without these an agent could
        // receive a message and have no way to answer where it matters.
        let (requested_kind, requested_id) = match recipient {
            "human" | "local" => ("human", crate::coordination::LOCAL_HUMAN_ID),
            other => match other.strip_prefix("session:") {
                Some(session) if !session.trim().is_empty() => ("session", session.trim()),
                _ => ("agent", other),
            },
        };
        let db = db(ctx)?;
        let mut body = if reaction.is_some() {
            String::new()
        } else {
            args.body.trim().to_string()
        };
        let task_session;
        let mut about_task: Option<String> = None;
        let (requested_kind, requested_id) = match recipient.strip_prefix("task:") {
            Some(task_id) => {
                let task = db
                    .get_task(task_id.trim())
                    .map_err(|e| ToolError::msg(format!("look up task: {e}")))?
                    .ok_or_else(|| ToolError::msg(format!("no task `{}`", task_id.trim())))?;
                if task.status.is_terminal() {
                    return Err(ToolError::msg(format!(
                        "task `{}` is {}; nobody is working on it. Message the agent instead.",
                        task.id, task.status
                    )));
                }
                if task.session_id.is_empty() {
                    return Err(ToolError::msg(format!(
                        "task `{}` has not been dispatched to a session yet. Ask mission-control about it.",
                        task.id
                    )));
                }
                if reaction.is_none()
                    && task.status == TaskStatus::InReview
                    && ctx.agent_id() == Some(crate::mission_control::SESSION_ID)
                {
                    let now = now_rfc3339();
                    let back = crate::members::working_session(&db, &task);
                    let _ = db.update_task_in(&task.id, &now, |t| {
                        t.status = TaskStatus::InProgress;
                        t.reporting_session = Some(back);
                    });
                    let _ = db.modify_task_state(&task.id, &now, |s| {
                        s.pending_report = None;
                        s.review_started_at.clear();
                    });
                }
                if reaction.is_none() && forward_id.is_none() {
                    body = format!("[about task {}: {}]\n{body}", task.id, task.title);
                }
                about_task = Some(task.id.clone());
                task_session = crate::members::working_session(&db, &task);
                ("session", task_session.as_str())
            }
            None => (requested_kind, requested_id),
        };
        // An agent always answers where it was asked. When the message it is
        // answering came from a SESSION, that session is where the human is
        // reading — so a reply addressed to the person is routed there instead of
        // to the human's own thread. Decided here rather than left to the
        // argument: a wrong address loses the answer in a thread nobody watches,
        // which is exactly what the guidance in the description failed to prevent.
        let redirect = if requested_kind == "human" {
            ctx.agent_id()
                .and_then(|agent_id| reply_session_for_agent(&db, agent_id, requested_id))
        } else {
            None
        };
        let (to_kind, to_id): (&str, &str) = match redirect.as_deref() {
            Some(session) => ("session", session),
            None => (requested_kind, requested_id),
        };
        if to_kind == "agent" {
            // A recipient that is not in the directory would sit undelivered
            // forever, so an unknown id is refused rather than accepted and dropped.
            // A paused agent's mail waits; a disabled agent takes none.
            match db
                .get_agent(to_id)
                .map_err(|e| ToolError::msg(format!("look up agent: {e}")))?
            {
                None => {
                    return Err(ToolError::msg(format!(
                        "unknown agent `{to_id}` — list_coordination_agents shows the directory. Use `human` to reply to the person."
                    )));
                }
                Some(agent)
                    if agent.status == crate::coordination::types::AgentStatus::Disabled =>
                {
                    return Err(ToolError::msg(format!("agent `{to_id}` is disabled")));
                }
                Some(_) => {}
            }
        } else if to_kind == "session" {
            // A session id that does not resolve would create a delivery row that
            // can never be delivered, so it is refused at send time rather than
            // retried forever.
            //
            // The message must NOT suggest an alternative address. The previous
            // wording ended with "Use `human` to reply to the person instead",
            // and a model read that as guidance: every later reply went to
            // `human`, so the asker's session never saw one. Point back at the
            // value it was given — that is the correct target — and nothing else.
            let resolves = db.has_conversation(to_id).unwrap_or(false)
                || crate::session::state_path_for_id(to_id)
                    .map(|path| crate::session::read_session_meta(&path).is_some())
                    .unwrap_or(false);
            if !resolves {
                return Err(ToolError::msg(format!(
                    "unknown session `{to_id}`. Copy the `reply_to` value from the envelope as written, in full: it names the session the sender is reading."
                )));
            }
            if reaction.is_none() && ctx.agent_id() == Some(crate::mission_control::SESSION_ID) {
                let target_canon = crate::conversations::canonical_session_id(to_id);
                if let Ok(in_review) = db.list_tasks(None, Some(&TaskStatus::InReview)) {
                    let now = now_rfc3339();
                    for t in in_review {
                        let back = crate::members::working_session(&db, &t);
                        if crate::conversations::canonical_session_id(&back) == target_canon {
                            let _ = db.update_task_in(&t.id, &now, |task| {
                                task.status = TaskStatus::InProgress;
                                task.reporting_session = Some(back);
                            });
                            let _ = db.modify_task_state(&t.id, &now, |s| {
                                s.pending_report = None;
                                s.review_started_at.clear();
                            });
                        }
                    }
                }
            }
        }
        let (sender_kind, sender_id) = actor(ctx)?;
        let (kind, urgency) = if reaction.is_some() {
            (
                Some("reaction"),
                crate::coordination::typed::Urgency::Normal,
            )
        } else {
            crate::coordination::typed::parse(args.kind.as_deref(), args.urgency.as_deref())
                .map_err(ToolError::msg)?
        };
        let mut extra =
            crate::coordination::typed::payload_fields(kind, urgency, about_task.as_deref());
        crate::coordination::typed::apply_reply_and_reaction(
            &mut extra,
            reply_to.as_deref(),
            reaction.as_deref(),
        );
        if let Some(forward_id) = forward_id {
            let orig = db
                .event_by_id(forward_id)
                .map_err(|e| ToolError::msg(format!("look up event: {e}")))?
                .ok_or_else(|| ToolError::msg(format!("no event `{forward_id}`")))?;
            require_thread_access(&db, ctx, &sender_id, &orig.thread_id)?;
            if reaction.is_none() {
                let orig_body = orig
                    .payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let clipped: String = orig_body
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(1500)
                    .collect();
                let quoted = format!(
                    "[forwarded from {}:{} in {}, {}]\n{clipped}\n[/forwarded]",
                    orig.actor_kind, orig.actor_id, orig.thread_id, orig.created_at
                );
                body = if body.is_empty() {
                    quoted
                } else {
                    format!("{quoted}\n{body}")
                };
            }
            extra.insert(
                "forwarded_from".into(),
                json!({
                    "event_id": orig.event_id,
                    "thread_id": orig.thread_id,
                    "actor_kind": orig.actor_kind,
                    "actor_id": orig.actor_id,
                }),
            );
            if orig.actor_kind == "human"
                && orig.event_type == "direct_message.sent"
                && orig.thread_id.starts_with("direct:agent:")
                && requested_kind == "session"
            {
                extra.insert("about_thread".into(), json!(orig.thread_id));
            }
        }
        if to_kind == "human" {
            extra.insert("channel".into(), json!("chat"));
            if sender_kind == "agent"
                && reaction.is_none()
                && sender_id != crate::mission_control::SESSION_ID
            {
                crate::coordination::reach::apply_outbound_human(
                    &db, &sender_id, &body, &mut extra,
                );
            }
        }
        let key = args
            .idempotency_key
            .filter(|k| !k.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        // The session this message is asked FROM, taken from the context rather
        // than the model's arguments: the model must not be able to claim a
        // different origin any more than it can claim a different sender.
        let origin = ctx
            .durable_session_id()
            .or(ctx.lane_origin().map(|(session, _)| session))
            .map(str::to_string);
        let saved = db
            .send_direct_message_typed(
                (sender_kind, &sender_id),
                (to_kind, to_id),
                &body,
                &key,
                &chrono::Utc::now().to_rfc3339(),
                origin.as_deref(),
                ctx.lane_origin().map(|(_, lane)| lane),
                about_task.as_deref(),
                extra,
            )
            .map_err(|e| ToolError::msg(format!("send message: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved,
        }));
        Ok(ToolResult::success(json!({
            "event_id": saved.event_id,
            "thread_id": saved.thread_id,
            "sequence": saved.sequence,
            "to": format!("{to_kind}:{to_id}"),
            "note": "Accepted and queued for delivery. This is conversation only — it does not start work.",
        })))
    }
}

#[derive(Deserialize)]
struct ReadAgentThreadArgs {
    peer_agent_id: String,
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// Read the direct conversation between this session's actor and one peer.
pub struct ReadAgentThread;
#[async_trait]
impl Tool for ReadAgentThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_agent_thread".into(),
            description: "Read your direct conversation with one agent, oldest first. Omit after_sequence for the most recent messages; pass it to page forward. The wake message already includes the recent history, so use this only to see more.".into(),
            input_schema: schema(
                json!({
                    "peer_agent_id":{"type":"string"},
                    "after_sequence":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &["peer_agent_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadAgentThreadArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let peer = args.peer_agent_id.trim();
        if peer.is_empty() {
            return Err(ToolError::msg("peer_agent_id must not be empty"));
        }
        let (kind, id) = actor(ctx)?;
        let thread_id = crate::coordination::direct_thread_id((kind, &id), ("agent", peer));
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let db = db(ctx)?;
        let events = match args.after_sequence {
            Some(after) => db
                .direct_thread_events(&thread_id, after, limit)
                .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
            None => db
                .recent_direct_thread_events(&thread_id, limit)
                .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
        };
        Ok(ToolResult::success(json!({
            "thread_id": thread_id,
            "peer": peer,
            "messages": events.iter().map(flatten_event).collect::<Vec<_>>(),
            "oldest_sequence": events.first().map(|e| e.sequence).unwrap_or(0),
            "newest_sequence": events.last().map(|e| e.sequence).unwrap_or(0),
        })))
    }
}

/// The conversations this actor is part of, with unread counts — the inbox.
pub struct ReadAgentInbox;
#[async_trait]
impl Tool for ReadAgentInbox {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_agent_inbox".into(),
            description: "List your direct conversations with unread counts, most recent first. Use it to find messages you have not answered; read one with read_agent_thread.".into(),
            input_schema: schema(json!({}), &[]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let (kind, id) = actor(ctx)?;
        let threads = db(ctx)?
            .list_direct_threads(kind, &id)
            .map_err(|e| ToolError::msg(format!("list inbox: {e}")))?;
        Ok(ToolResult::success(json!({
            "actor": format!("{kind}:{id}"),
            "threads": threads,
        })))
    }
}

/// Read back this agent's own coordination memory.
///
/// The point of the board is recall: before dispatching, an agent asks "have I
/// sent work about this workspace before, and how did it go". Filtering by
/// workspace and searching the summaries are what make that a query instead of
/// a re-read of every transcript.
pub struct ReadCoordinationBoard;
#[async_trait]
impl Tool for ReadCoordinationBoard {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_coordination_board".into(),
            description: "Read your or another agent's board, newest first. Search with `contains`; filter by task_id, workspace or kind, or group by task/folder. Use `before_id` with the returned `next_before_id` to page back. `task_id` alone reads that task's log across all agents.".into(),
            input_schema: schema(
                json!({
                    "agent_id":{"type":"string","description":"whose board to read, or all for every agent's; defaults to your own"},
                    "task_id":{"type":"string","description":"only rows linked to this task; on its own, that task's log across everyone on it"},
                    "group_by":{"type":"string","enum":["task","folder"],"description":"one row per task or folder: its newest row and how many it holds"},
                    "workspace":{"type":"string","description":"only rows about this folder"},
                    "contains":{"type":"string","description":"substring to find in summaries"},
                    "kind":{"type":"string","enum":["dispatched","reported","noted","progress"]},
                    "outstanding_only":{"type":"boolean","description":"only dispatches with no report yet"},
                    "before_id":{"type":"integer","description":"only rows older than this id; pass next_before_id to read further back"},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        #[derive(Deserialize)]
        struct Args {
            #[serde(default)]
            agent_id: Option<String>,
            #[serde(default)]
            task_id: Option<String>,
            #[serde(default)]
            group_by: Option<String>,
            #[serde(default)]
            workspace: Option<String>,
            #[serde(default)]
            contains: Option<String>,
            #[serde(default)]
            kind: Option<String>,
            #[serde(default)]
            outstanding_only: bool,
            #[serde(default)]
            before_id: Option<i64>,
            #[serde(default)]
            limit: Option<u32>,
        }
        let args: Args =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let named = args
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty());
        let db = db(ctx)?;
        let task_alone = args
            .task_id
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .filter(|_| {
                named.is_none()
                    && args.group_by.is_none()
                    && args.workspace.is_none()
                    && args.contains.is_none()
                    && args.kind.is_none()
                    && !args.outstanding_only
            });
        if let Some(task) = task_alone {
            let limit = args.limit.unwrap_or(30).clamp(1, 100);
            let rows = db
                .task_log(task, args.before_id, limit)
                .map_err(|e| ToolError::msg(format!("read task log: {e}")))?;
            let next = rows
                .last()
                .map(|r| r.id)
                .filter(|_| rows.len() as u32 == limit);
            return Ok(ToolResult::success(json!({
                "task_id": task,
                "count": rows.len(),
                "entries": rows,
                "next_before_id": next,
            })));
        }
        let agent_id = match named {
            Some("all") => {
                if args.outstanding_only {
                    return Err(ToolError::msg(
                        "outstanding_only reads one agent's board; name that agent",
                    ));
                }
                crate::coordination::ALL_AGENTS.to_string()
            }
            Some(agent) => agent.to_string(),
            None => board_agent_id(ctx)?,
        };
        if args.outstanding_only {
            let outstanding = db
                .board_awaiting_report(&agent_id)
                .map_err(|e| ToolError::msg(format!("read board: {e}")))?;
            return Ok(ToolResult::success(json!({
                "agent_id": agent_id,
                "outstanding": outstanding,
                "note": "Dispatches with no report yet. Read a specific one by searching its correlation id.",
            })));
        }
        let kind = match args
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            None => None,
            Some(raw) => Some(crate::coordination::BoardEntryKind::parse(raw).ok_or_else(
                || {
                    ToolError::msg(format!(
                        "unknown kind `{raw}` — use dispatched, reported, noted, or progress"
                    ))
                },
            )?),
        };
        let query = crate::coordination::BoardQuery {
            workspace: args.workspace.as_deref().filter(|w| !w.trim().is_empty()),
            contains: args.contains.as_deref().filter(|c| !c.trim().is_empty()),
            kind,
            before_id: args.before_id,
            outstanding: false,
            correlation: args.task_id.as_deref().filter(|t| !t.trim().is_empty()),
        };
        let limit = args.limit.unwrap_or(30).clamp(1, 100);
        let (entries, last_id) = match args
            .group_by
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
        {
            Some(raw) => {
                let grouping = crate::coordination::BoardGrouping::parse(raw).ok_or_else(|| {
                    ToolError::msg(format!("unknown group_by `{raw}`: use task or folder"))
                })?;
                let groups = db
                    .read_board_groups(&agent_id, grouping, &query, limit)
                    .map_err(|e| ToolError::msg(format!("read board: {e}")))?;
                let last = groups.last().map(|g| g.latest.id);
                (json!(groups), last.filter(|_| groups.len() as u32 == limit))
            }
            None => {
                let rows = db
                    .read_board(&agent_id, &query, limit)
                    .map_err(|e| ToolError::msg(format!("read board: {e}")))?;
                let last = rows.last().map(|r| r.id);
                (json!(rows), last.filter(|_| rows.len() as u32 == limit))
            }
        };
        Ok(ToolResult::success(json!({
            "agent_id": agent_id,
            "count": entries.as_array().map_or(0, Vec::len),
            "entries": entries,
            "next_before_id": last_id,
        })))
    }
}

#[derive(Deserialize)]
struct NoteArgs {
    summary: String,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    correlation_id: Option<String>,
}

/// Record a note on this agent's board.
///
/// For conclusions the mechanical record cannot capture — which agent suited a
/// kind of work, what a workspace's conventions are, why an approach failed.
/// Dispatches and reports are recorded automatically; this is the agent's own
/// judgment, which is exactly the part worth keeping.
pub struct RecordCoordinationNote;
#[async_trait]
impl Tool for RecordCoordinationNote {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "record_coordination_note".into(),
            description: "Write a note to your own coordination memory, so a later turn can recall it. Use it for conclusions worth keeping: what a workspace needs, which agent fits a kind of work, why an approach failed. Keep it to one or two sentences — it is a memory row, not a report. Dispatches and their reports are recorded for you; do not duplicate those.".into(),
            input_schema: schema(
                json!({
                    "summary":{"type":"string","description":"one or two sentences"},
                    "workspace":{"type":"string","description":"the folder it concerns, when it concerns one"},
                    "session_id":{"type":"string","description":"the session it concerns, when it concerns one"},
                    "correlation_id":{"type":"string","description":"an assignment or goal id to link it to"}
                }),
                &["summary"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: NoteArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let summary = args.summary.trim();
        if summary.is_empty() {
            return Err(ToolError::msg("summary must not be empty"));
        }
        let agent_id = board_agent_id(ctx)?;
        let session = ctx
            .durable_session_id()
            .filter(|id| !crate::session::is_inbox_session_id(id));
        let here = ctx.workspace_root().display().to_string();
        let on_task = session.and_then(|s| crate::progress::working_as(ctx, s).1);
        let id = db(ctx)?
            .record_board_entry(
                &agent_id,
                crate::coordination::BoardEntryKind::Noted,
                &crate::coordination::NewBoardEntry {
                    session_id: args
                        .session_id
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .or(session),
                    workspace: args
                        .workspace
                        .as_deref()
                        .filter(|w| !w.trim().is_empty())
                        .or(session.map(|_| here.as_str())),
                    summary,
                    correlation_id: args
                        .correlation_id
                        .as_deref()
                        .filter(|c| !c.trim().is_empty())
                        .or(on_task.as_deref()),
                    created_at: &now_rfc3339(),
                },
            )
            .map_err(|e| ToolError::msg(format!("record note: {e}")))?;
        Ok(ToolResult::success(json!({"id": id, "recorded": true})))
    }
}

/// The agent identity a board row belongs to.
///
/// The board is PER AGENT, so a session with no agent identity has no board —
/// this is the same rule the turn lease uses, and for the same reason: a session
/// is not an agent.
fn board_agent_id(ctx: &ToolContext) -> Result<String, ToolError> {
    if let Ok((kind, id)) = actor(ctx)
        && kind == "agent"
    {
        return Ok(id);
    }
    if let Some(agent_id) = ctx.agent_id() {
        return Ok(agent_id.to_string());
    }
    // A plain conversation runs as `snippet` and logs there, so it reads there too.
    ctx.durable_session_id()
        .map(|session| crate::progress::working_as(ctx, session).0)
        .ok_or_else(|| {
            ToolError::msg(
                "this session is not working as an agent, and coordination memory requires an agent identity",
            )
        })
}
#[derive(Deserialize)]
struct MessageMissionControlArgs {
    task_id: String,
    message: String,
}

pub struct MessageMissionControl;
#[async_trait]
impl Tool for MessageMissionControl {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "message_mission_control".into(),
            description: "Message Mission Control using the active task board item you are working on. This posts to the task thread, notifies Mission Control, and records the update on the task board so your progress or question is linked to the task.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID you are working on"},
                    "message": {"type": "string", "description": "Message, question, or status update for Mission Control"}
                }),
                &["task_id", "message"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: MessageMissionControlArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let message = args.message.trim();
        if task_id.is_empty() || message.is_empty() {
            return Err(ToolError::msg("task_id and message must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let (actor_kind, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &task.thread_id)?;
        let now = now_rfc3339();
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.message".into(),
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(
                ctx,
                json!({
                    "body": message,
                    "task_id": task_id,
                    "to": "mission_control",
                }),
            ),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now.clone(),
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        let _ = db.update_task_in(task_id, &now, |t| {
            t.notifications.push(NotificationMarker {
                target: "mission_control".into(),
                kind: "message".into(),
                message: format!("{actor_id}: {message}"),
                delivered: false,
            });
        });
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(&db, saved.clone());
        Ok(ToolResult::success(json!({
            "sent": true,
            "task_id": task_id,
            "event": saved,
        })))
    }
}

#[derive(Deserialize)]
struct PostTaskCoordinationArgs {
    task_id: String,
    body: String,
    #[serde(default)]
    to: Vec<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    urgency: Option<String>,
}

pub struct PostTaskCoordination;
#[async_trait]
impl Tool for PostTaskCoordination {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "post_task_coordination".into(),
            description: "Coordinate with fellow agents assigned to this task board item. Posts context, plan proposals, or status to the task thread. Everyone on the task and Mission Control see it; a post that needs nothing from them can simply be read. The task room is the task's one channel, in task order. Optional `kind` types the post; `urgency: urgent` stops the other sessions' runs and wakes them now (a blocker or wrong direction only), while normal posts wait for their next turn.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID on the task board"},
                    "body": {"type": "string", "description": "The coordination message or update"},
                    "to": {"type": "array", "items": {"type": "string"}, "description": "The agents this needs an answer or action from; only they are woken. Everyone on the task can still read it. Omit it to reach whoever holds the turn."},
                    "kind": {"type": "string", "enum": ["note","question","status","handoff_note","blocker","result","interrupt"], "description": "optional"},
                    "urgency": {"type": "string", "enum": ["normal","urgent"], "description": "optional, default normal"}
                }),
                &["task_id", "body"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PostTaskCoordinationArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let body = args.body.trim();
        if task_id.is_empty() || body.is_empty() {
            return Err(ToolError::msg("task_id and body must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let (actor_kind, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, &actor_id, &task.thread_id)?;
        let (kind, urgency) =
            crate::coordination::typed::parse(args.kind.as_deref(), args.urgency.as_deref())
                .map_err(ToolError::msg)?;
        let mut payload = json!({"body": body, "task_id": task_id});
        let to: Vec<String> = args
            .to
            .iter()
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect();
        if !to.is_empty() {
            let roster = db.list_task_agents(task_id).unwrap_or_default();
            if let Some(stranger) = to.iter().find(|a| {
                a.as_str() != crate::mission_control::SESSION_ID
                    && !roster
                        .iter()
                        .any(|m| m.agent_id == **a && m.removed_at.is_none())
            }) {
                return Err(ToolError::msg(format!(
                    "`{stranger}` isn't on this task. Use invite_task_agent to bring them in."
                )));
            }
            crate::members::seat_holder(&db, &task);
            for agent in to
                .iter()
                .filter(|a| a.as_str() != crate::mission_control::SESSION_ID)
            {
                crate::members::seat(&db, &task, agent, &now_rfc3339()).map_err(ToolError::msg)?;
            }
            payload["to"] = json!(to);
        }
        for (key, value) in crate::coordination::typed::payload_fields(kind, urgency, None) {
            payload[key] = value;
        }
        let now = now_rfc3339();
        if task.status == TaskStatus::InReview && actor_id == crate::mission_control::SESSION_ID {
            let back = crate::members::working_session(&db, &task);
            let _ = db.update_task_in(&task.id, &now, |t| {
                t.status = TaskStatus::InProgress;
                t.reporting_session = Some(back);
            });
            let _ = db.modify_task_state(&task.id, &now, |s| {
                s.pending_report = None;
                s.review_started_at.clear();
            });
        }
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "message.posted".into(),
            actor_kind: actor_kind.to_string(),
            actor_id,
            payload_version: 1,
            payload: stamp_origin(ctx, payload),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let saved = db
            .append_event(&event)
            .map_err(|e| ToolError::msg(format!("post message: {e}")))?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": saved.clone(),
        }));
        crate::serve::queue_coordination_wake(&db, saved.clone());
        let mut out = json!({"event": saved});
        if !task.status.is_terminal()
            && ctx.durable_session_id().is_some_and(|s| {
                crate::conversations::canonical_session_id(s)
                    == crate::conversations::canonical_session_id(&task.session_id)
            })
        {
            let step_line = body
                .lines()
                .map(|l| l.trim().trim_start_matches(['#', '-', '*', ' ']))
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>();
            let now_step = now_rfc3339();
            let _ = db.touch_task_progress(&task.id, &now_step);
            if !step_line.is_empty() {
                let _ = db.update_task_in(&task.id, &now_step, |t| {
                    if let Some(active) = t.work_plan.iter_mut().find(|s| s.status == "in_progress")
                    {
                        active.step = step_line.clone();
                    }
                });
            }
            out["note"] = json!(
                "Posted to the task room. Note: a room post does not complete the task; call report_mission_task when the work is finished."
            );
        }
        Ok(ToolResult::success(out))
    }
}

#[derive(Deserialize)]
struct PassTurnArgs {
    task_id: String,
    to_agent_id: String,
    done: String,
    next: String,
    #[serde(default)]
    risks: String,
    #[serde(default)]
    files: Vec<String>,
}

pub struct PassTurn;
#[async_trait]
impl Tool for PassTurn {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "pass_turn".into(),
            description: "Hand the task turn to another agent, waking them in their task conversation. Only the turn holder may change files or pass it. Include what they need to continue; end your turn after passing. You remain on the task and can be addressed later.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string"},
                    "to_agent_id": {"type": "string", "description": "Any working agent, on the task or not"},
                    "done": {"type": "string", "description": "What is finished"},
                    "next": {"type": "string", "description": "What the next agent should do"},
                    "risks": {"type": "string", "description": "What could go wrong; open doubts"},
                    "files": {"type": "array", "items": {"type": "string"}, "description": "Files to look at. What actually changed is measured and added for you."}
                }),
                &["task_id", "to_agent_id", "done", "next"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: PassTurnArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let to_agent = args.to_agent_id.trim();
        if task_id.is_empty() || to_agent.is_empty() {
            return Err(ToolError::msg("task_id and to_agent_id must not be empty"));
        }
        if args.done.trim().is_empty() || args.next.trim().is_empty() {
            return Err(ToolError::msg("passing the turn needs `done` and `next`"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        if task.status != TaskStatus::InProgress {
            return Err(ToolError::msg(format!(
                "task `{task_id}` is {}, so there is no turn to pass",
                task.status
            )));
        }
        let holder = db
            .list_task_agents(task_id)
            .unwrap_or_default()
            .into_iter()
            .find(|m| m.status == "active" && m.removed_at.is_none())
            .map(|m| m.agent_id)
            .ok_or_else(|| ToolError::msg("nobody holds this task's turn yet"))?;
        let session = ctx
            .durable_session_id()
            .or(ctx.lane_origin().map(|(s, _)| s))
            .unwrap_or_default();
        let bound = task
            .reporting_session
            .as_deref()
            .unwrap_or(&task.session_id);
        if crate::conversations::canonical_session_id(bound)
            != crate::conversations::canonical_session_id(session)
        {
            return Err(ToolError::msg(format!(
                "only the holder of the turn can pass it, and that is `{holder}`. Ask them in the room."
            )));
        }
        if to_agent == holder {
            return Err(ToolError::msg("you already hold the turn"));
        }
        crate::mission_tools::require_working_agent(&db, to_agent)?;
        let now = now_rfc3339();
        let handover = crate::coordination::handoffs::Handover {
            id: Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            from_agent: holder.clone(),
            to_agent: to_agent.to_string(),
            done: args.done.trim().to_string(),
            next: args.next.trim().to_string(),
            risks: args.risks.trim().to_string(),
            files: args.files.clone(),
            created_at: now.clone(),
            acked_at: None,
        };
        let passed = crate::members::pass_turn(&db, &task, to_agent, &handover, &now)
            .map_err(ToolError::msg)?;
        let body = crate::members::handover_body(&handover, &passed);
        post_handover_event(
            &db,
            ctx,
            &task.thread_id,
            "task.lease_transferred",
            &holder,
            &body,
            json!({
                "task_id": task_id, "handoff_id": handover.id, "from_agent_id": holder, "to_agent_id": to_agent,
            }),
        )?;
        Ok(ToolResult::success(json!({
            "passed": true,
            "holder": to_agent,
            "changed_files": passed.files,
            "commits": passed.commits,
            "note": "End your turn now. You stay on the task and hear when someone addresses you.",
        })))
    }
}

fn post_handover_event(
    db: &Store,
    ctx: &ToolContext,
    thread_id: &str,
    event_type: &str,
    actor_id: &str,
    body: &str,
    mut payload: Value,
) -> Result<CoordinationEvent, ToolError> {
    payload["body"] = json!(body);
    let event = CoordinationEvent {
        event_id: Uuid::new_v4().to_string(),
        thread_id: thread_id.to_string(),
        partition_key: format!("thread:{thread_id}"),
        sequence: 0,
        event_type: event_type.into(),
        actor_kind: "agent".into(),
        actor_id: actor_id.to_string(),
        payload_version: 1,
        payload: stamp_origin(ctx, payload),
        causation_id: None,
        correlation_id: None,
        idempotency_key: Uuid::new_v4().to_string(),
        created_at: now_rfc3339(),
    };
    let saved = db
        .append_event(&event)
        .map_err(|e| ToolError::msg(format!("record event: {e}")))?;
    crate::session::emit_device_event(
        json!({"kind": "coordination_event", "event": saved.clone()}),
    );
    crate::serve::queue_coordination_wake(db, saved.clone());
    Ok(saved)
}

#[derive(Deserialize)]
struct InspectTaskArgs {
    task_id: String,
}

pub struct InspectTask;
#[async_trait]
impl Tool for InspectTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "inspect_task".into(),
            description: "Inspect a task's brief and plan, step statuses, assigned agents and lease holder, blockers, owned paths and result.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID to inspect"}
                }),
                &["task_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: InspectTaskArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        if task_id.is_empty() {
            return Err(ToolError::msg("task_id must not be empty"));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        let roster = db.list_task_agents(task_id).unwrap_or_default();
        let blockers = db.blockers_of(task_id).unwrap_or_default();
        let links = db.task_links(task_id).unwrap_or_default();

        Ok(ToolResult::success(json!({
            "task_id": task.id,
            "title": task.title,
            "description": task.description,
            "status": task.status,
            "plan": task.plan,
            "work_plan": task.work_plan,
            "profile": task.profile,
            "session_id": task.session_id,
            "owned_paths": task.owned_paths,
            "roster": roster,
            "turns": db.list_turns(&task.id).unwrap_or_default(),
            "blockers": blockers,
            "links": links,
            "thread_id": task.thread_id,
            "result": task.result,
        })))
    }
}

#[derive(Deserialize)]
struct ClaimAndDispatchArgs {
    task_id: String,
    #[serde(default)]
    profile: Option<String>,
}

pub struct ClaimAndDispatchTask;
#[async_trait]
impl Tool for ClaimAndDispatchTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "claim_and_dispatch_task".into(),
            description: "Claim an offered task to start it in its target session under your identity. Pass `profile` only to move it to another model; otherwise the session keeps its model and prompt cache. Finished or running tasks are refused.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID to claim"},
                    "profile": {"type": "string", "description": "Optional inference profile name. Omit to preserve prompt cache affinity."}
                }),
                &["task_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ClaimAndDispatchArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        if task_id.is_empty() {
            return Err(ToolError::msg("task_id must not be empty"));
        }
        let profile = args
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        if let Some(name) = profile.as_deref() {
            crate::mission_tools::check_profile(name).await?;
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        if task.status.is_terminal() || task.status == crate::coordination::TaskStatus::InProgress {
            return Err(ToolError::msg(format!(
                "task `{task_id}` is {}; only queued or blocked work can be claimed",
                task.status
            )));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        if actor_kind != "agent" {
            return Err(ToolError::msg(
                "claiming a task needs an agent identity; this session has none",
            ));
        }
        if actor_id == crate::mission_control::SESSION_ID {
            return Err(ToolError::msg(
                "Mission Control routes work and never takes a task's lease. To send a blocked or failed task to its session again, use retry_mission_task (pass profile to switch the worker's model).",
            ));
        }
        crate::mission_tools::require_working_agent(&db, &actor_id)?;
        let now = now_rfc3339();
        db.add_task_agent_full(task_id, &actor_id, "implementer", None, "", "active", &now)
            .map_err(|e| ToolError::msg(format!("take the lease: {e}")))?;
        db.retry_task(task_id, &now)
            .map_err(|e| ToolError::msg(format!("queue task: {e}")))?;
        let queued = db
            .update_task_in(task_id, &now, |t| {
                if profile.is_some() {
                    t.profile = profile.clone();
                }
            })
            .map_err(|e| ToolError::msg(format!("queue task: {e}")))?;

        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.dispatched".into(),
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(
                ctx,
                json!({
                    "body": format!("{actor_id} claimed the task"),
                    "task_id": task_id,
                    "agent_id": actor_id,
                    "profile": queued.profile,
                }),
            ),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let _ = db.append_event(&event);
        let _ = db.record_board_entry(
            &actor_id,
            crate::coordination::BoardEntryKind::Dispatched,
            &crate::coordination::NewBoardEntry {
                session_id: Some(&queued.session_id),
                workspace: None,
                summary: &format!("{task_id} — claimed: {}", task.title),
                correlation_id: Some(task_id),
                created_at: &event.created_at,
            },
        );

        Ok(ToolResult::success(json!({
            "queued": true,
            "task_id": task_id,
            "session_id": queued.session_id,
            "profile": queued.profile,
            "active_agent": actor_id,
            "note": "Lease taken; the daemon dispatches the task to its session within seconds.",
        })))
    }
}

#[derive(Deserialize)]
struct DeclineTaskArgs {
    task_id: String,
    reason: String,
}

pub struct DeclineTask;

#[async_trait]
impl Tool for DeclineTask {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "decline_task".into(),
            description: "Turn down a task you were offered, with your reason: it isn't your kind of work, the target session is wrong, the briefing can't be done as written. You leave the roster, the task waits as blocked, and Mission Control is told why so it can reassign or reshape it. Ask Mission Control first instead when a question would settle it.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The full task id from the offer."},
                    "reason": {"type": "string", "description": "Why, and what would make it work (another agent, session or scope)."}
                }),
                &["task_id", "reason"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: DeclineTaskArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let task_id = args.task_id.trim();
        let reason = args.reason.trim();
        if task_id.is_empty() || reason.is_empty() {
            return Err(ToolError::msg("task_id and reason must not be empty"));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        if actor_kind != "agent" {
            return Err(ToolError::msg(
                "declining a task needs an agent identity; this session has none",
            ));
        }
        let db = db(ctx)?;
        let task = db
            .get_task(task_id)
            .map_err(|e| ToolError::msg(format!("lookup task: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("task `{task_id}` not found")))?;
        if task.status.is_terminal() || task.status == crate::coordination::TaskStatus::InProgress {
            return Err(ToolError::msg(format!(
                "task `{task_id}` is {}; only work that hasn't started can be declined",
                task.status
            )));
        }
        let now = now_rfc3339();
        db.remove_task_agent(task_id, &actor_id, &now)
            .map_err(|e| ToolError::msg(format!("leave the roster: {e}")))?;
        let message = format!("{actor_id} declined the task: {reason}");
        db.move_task(
            task_id,
            crate::coordination::TaskStatus::Blocked,
            &message,
            &now,
        )
        .map_err(|e| ToolError::msg(format!("park task: {e}")))?;
        let marker = crate::coordination::offer_marker(&actor_id);
        db.update_task_in(task_id, &now, |t| {
            t.notifications.retain(|n| n.target != marker);
            t.notify_once("declined", &message);
        })
        .map_err(|e| ToolError::msg(format!("notify Mission Control: {e}")))?;
        let event = CoordinationEvent {
            event_id: Uuid::new_v4().to_string(),
            thread_id: task.thread_id.clone(),
            partition_key: format!("thread:{}", task.thread_id),
            sequence: 0,
            event_type: "task.declined".into(),
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.clone(),
            payload_version: 1,
            payload: stamp_origin(
                ctx,
                json!({"body": message, "task_id": task_id, "agent_id": actor_id}),
            ),
            causation_id: None,
            correlation_id: Some(task_id.to_string()),
            idempotency_key: Uuid::new_v4().to_string(),
            created_at: now,
        };
        let _ = db.append_event(&event);
        Ok(ToolResult::success(json!({
            "declined": true,
            "task_id": task_id,
            "note": "Mission Control has been told why; the task waits as blocked until it is reassigned or reshaped.",
        })))
    }
}

/// Group threads: rooms with several agents and the human where a post fans
/// out and agents take turns like colleagues in a chat.
pub struct GroupThread;

/// The board identity a group call speaks as. Group threads belong to agents
/// and Mission Control; a plain session has no place in one.
fn group_agent(ctx: &ToolContext) -> Result<String, ToolError> {
    if ctx.is_mission_control() {
        return Ok(crate::mission_control::SESSION_ID.to_string());
    }
    match actor(ctx)? {
        ("agent", id) => Ok(id),
        ("session", id) if id == crate::mission_control::SESSION_ID => Ok(id),
        _ => Err(ToolError::msg(
            "group threads are for agents and Mission Control",
        )),
    }
}

fn known_group_agent(db: &Store, id: &str) -> bool {
    id == crate::mission_control::SESSION_ID || db.get_agent(id).ok().flatten().is_some()
}

fn require_group(
    db: &Store,
    group_id: &str,
) -> Result<crate::coordination::GroupSummary, ToolError> {
    db.get_group(group_id)
        .map_err(|e| ToolError::msg(format!("look up group: {e}")))?
        .ok_or_else(|| ToolError::msg(format!("no group `{group_id}`")))
}

fn require_group_member(
    group: &crate::coordination::GroupSummary,
    agent_id: &str,
) -> Result<(), ToolError> {
    if agent_id == crate::mission_control::SESSION_ID
        || group
            .members
            .iter()
            .any(|m| m.kind == "agent" && m.id == agent_id)
    {
        return Ok(());
    }
    Err(ToolError::msg(format!(
        "this agent is not a member of `{}`",
        group.id
    )))
}

fn string_list(arguments: &Value, key: &str) -> Vec<String> {
    arguments
        .get(key)
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::trim).map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

#[async_trait]
impl Tool for GroupThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "group_thread".into(),
            description: "Group threads: a thread with several agents and the human where posting fans out and agents take turns like colleagues in a chat — a post that names agents wakes just them, and a post that names nobody goes to the lead while everyone else only speaks if it is a question in their field. An agent's post wakes only the agents it mentions. Actions: create {title, members, lead?}, list, read {thread_id, after_sequence?, limit?}, post {thread_id, body, mentions?, in_reply_to?}, set_lead {thread_id, agent}, add_member {thread_id, agent}. An aside to the human is a normal DM whose body starts 'From the <title> group: '.".into(),
            input_schema: schema(
                json!({
                    "action": {"type": "string", "enum": ["create", "list", "read", "post", "set_lead", "add_member"]},
                    "title": {"type": "string"},
                    "members": {"type": "array", "items": {"type": "string"}},
                    "lead": {"type": "string"},
                    "agent": {"type": "string"},
                    "thread_id": {"type": "string"},
                    "body": {"type": "string"},
                    "mentions": {"type": "array", "items": {"type": "string"}},
                    "in_reply_to": {"type": "string"},
                    "after_sequence": {"type": "integer"},
                    "limit": {"type": "integer"}
                }),
                &["action"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let action = arguments
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("missing `action`"))?;
        let db = db(ctx)?;
        let agent_id = group_agent(ctx)?;
        match action {
            "create" => {
                let title = arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| ToolError::msg("`title` is required"))?;
                let members = string_list(&arguments, "members");
                if members.is_empty() {
                    return Err(ToolError::msg("`members` must name at least one agent"));
                }
                let mut deduped: Vec<String> = Vec::new();
                for member in &members {
                    if !known_group_agent(&db, member) {
                        return Err(ToolError::msg(format!("unknown agent `{member}`")));
                    }
                    if !deduped.contains(member) {
                        deduped.push(member.clone());
                    }
                }
                let lead = arguments
                    .get("lead")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|l| !l.is_empty());
                if let Some(lead) = lead
                    && !deduped.iter().any(|m| m == lead)
                {
                    return Err(ToolError::msg(format!(
                        "lead `{lead}` is not one of the group's members"
                    )));
                }
                let group = db
                    .create_group(
                        title,
                        ("agent", agent_id.as_str()),
                        &deduped,
                        lead,
                        &now_rfc3339(),
                    )
                    .map_err(|e| ToolError::msg(format!("create group: {e}")))?;
                Ok(ToolResult::success(json!(group)))
            }
            "list" => {
                let groups = db
                    .list_groups()
                    .map_err(|e| ToolError::msg(format!("list groups: {e}")))?;
                let groups: Vec<_> = if agent_id == crate::mission_control::SESSION_ID {
                    groups
                } else {
                    groups
                        .into_iter()
                        .filter(|g| {
                            g.members
                                .iter()
                                .any(|m| m.kind == "agent" && m.id == agent_id)
                        })
                        .collect()
                };
                Ok(ToolResult::success(json!(groups)))
            }
            "read" => {
                let thread_id = arguments
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::msg("`thread_id` is required"))?;
                let group = require_group(&db, thread_id)?;
                require_group_member(&group, &agent_id)?;
                let after = arguments
                    .get("after_sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(50)
                    .clamp(1, 200) as u32;
                let events = db
                    .events_for_thread(thread_id, after, limit)
                    .map_err(|e| ToolError::msg(format!("read group: {e}")))?;
                Ok(ToolResult::success(json!(events)))
            }
            "post" => {
                let thread_id = arguments
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::msg("`thread_id` is required"))?;
                let body = arguments
                    .get("body")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .ok_or_else(|| ToolError::msg("`body` is required"))?;
                let group = require_group(&db, thread_id)?;
                require_group_member(&group, &agent_id)?;
                let mentions = string_list(&arguments, "mentions");
                let in_reply_to = arguments
                    .get("in_reply_to")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                let event = db
                    .post_group_message(
                        thread_id,
                        ("agent", agent_id.as_str()),
                        body,
                        &mentions,
                        in_reply_to,
                        &uuid::Uuid::new_v4().to_string(),
                        &now_rfc3339(),
                    )
                    .map_err(|e| ToolError::msg(format!("post to group: {e}")))?;
                Ok(ToolResult::success(json!(event)))
            }
            "set_lead" => {
                let thread_id = arguments
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::msg("`thread_id` is required"))?;
                let agent = arguments
                    .get("agent")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| ToolError::msg("`agent` is required"))?;
                let group = require_group(&db, thread_id)?;
                require_group_member(&group, &agent_id)?;
                db.set_group_lead(thread_id, agent)
                    .map_err(|e| ToolError::msg(format!("set lead: {e}")))?;
                Ok(ToolResult::success(
                    json!({"thread_id": thread_id, "lead": agent}),
                ))
            }
            "add_member" => {
                let thread_id = arguments
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::msg("`thread_id` is required"))?;
                let agent = arguments
                    .get("agent")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| ToolError::msg("`agent` is required"))?;
                let group = require_group(&db, thread_id)?;
                require_group_member(&group, &agent_id)?;
                if !known_group_agent(&db, agent) {
                    return Err(ToolError::msg(format!("unknown agent `{agent}`")));
                }
                db.add_group_member(thread_id, agent)
                    .map_err(|e| ToolError::msg(format!("add member: {e}")))?;
                Ok(ToolResult::success(
                    json!({"thread_id": thread_id, "member": agent}),
                ))
            }
            other => Err(ToolError::msg(format!(
                "unknown action `{other}`; use create, list, read, post, set_lead or add_member"
            ))),
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Record the session an event was posted from, so the room's wake-up skips it.
const SHARE_ARTIFACT_TTL_SECS: i64 = 30 * 24 * 60 * 60;

fn artifact_signing_secret() -> String {
    let home = crate::config::snippet_home();
    if let Ok(bytes) = std::fs::read(home.join("serve.json"))
        && let Ok(val) = serde_json::from_slice::<Value>(&bytes)
        && let Some(tok) = val.get("token").and_then(Value::as_str).map(str::trim)
        && !tok.is_empty()
    {
        return tok.to_string();
    }
    if let Ok(tok) = std::fs::read_to_string(home.join("serve.token")) {
        let tok = tok.trim();
        if tok.len() >= 16 {
            return tok.to_string();
        }
    }
    crate::serve::resolve_token(None)
}

#[derive(Deserialize)]
struct ShareArtifactArgs {
    artifact_id: String,
    to: String,
    #[serde(default)]
    note: Option<String>,
}

pub struct ShareArtifactTool;

#[async_trait]
impl Tool for ShareArtifactTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "share_artifact".into(),
            description: "Share an artifact from this session (latest version, 30-day signed link) with `human`, an agent id, or `channel:<id>` (`channel:chat` or `channel:push`).".into(),
            input_schema: schema(
                json!({
                    "artifact_id": {"type": "string"},
                    "to": {"type": "string", "description": "`human`, an agent id, or `channel:<id>`"},
                    "note": {"type": "string"}
                }),
                &["artifact_id", "to"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ShareArtifactArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let artifact_id = args.artifact_id.trim();
        let target = args.to.trim();
        if artifact_id.is_empty() {
            return Err(ToolError::msg("artifact_id must not be empty"));
        }
        if target.is_empty() {
            return Err(ToolError::msg("to must not be empty"));
        }
        let session = ctx
            .durable_session_id()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::msg("share_artifact requires a durable session"))?;
        let artifact = crate::artifacts::get(session, artifact_id).ok_or_else(|| {
            ToolError::msg(format!(
                "unknown artifact `{artifact_id}` in session `{session}`"
            ))
        })?;
        let version = artifact
            .latest()
            .ok_or_else(|| ToolError::msg(format!("artifact `{artifact_id}` has no versions")))?;

        let secret = artifact_signing_secret();
        let url = crate::artifacts::signed_path(
            &secret,
            session,
            &artifact.id,
            version,
            SHARE_ARTIFACT_TTL_SECS,
        );
        let body = args
            .note
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&artifact.title)
            .to_string();
        let artifact_payload = json!({
            "session": session,
            "id": artifact.id,
            "title": artifact.title,
            "version": version.n,
            "url": url,
            "kind": artifact.kind,
        });

        let db = db(ctx)?;
        let (sender_kind, sender_id) = actor(ctx)?;
        let about_task = session_task(ctx, session).map(|(id, _)| id);
        let mut extra = crate::coordination::typed::payload_fields(
            Some("result"),
            crate::coordination::typed::Urgency::Normal,
            about_task.as_deref(),
        );
        extra.insert("artifact".into(), artifact_payload.clone());

        let saved = if let Some(raw_ch) = target.strip_prefix("channel:") {
            crate::coordination::reach::deliver_channel(
                &db,
                (sender_kind, &sender_id),
                raw_ch.trim(),
                &body,
                Some(session),
                ctx.lane_origin().map(|(_, lane)| lane),
                about_task.as_deref(),
                extra,
            )
            .map_err(ToolError::msg)?
        } else if target == "human" {
            extra.insert("channel".into(), json!("chat"));
            if sender_kind == "agent" && sender_id != crate::mission_control::SESSION_ID {
                crate::coordination::reach::apply_outbound_human(
                    &db, &sender_id, &body, &mut extra,
                );
            }
            let key = Uuid::new_v4().to_string();
            let ev = db
                .send_direct_message_typed(
                    (sender_kind, &sender_id),
                    ("human", crate::coordination::LOCAL_HUMAN_ID),
                    &body,
                    &key,
                    &chrono::Utc::now().to_rfc3339(),
                    Some(session),
                    ctx.lane_origin().map(|(_, lane)| lane),
                    about_task.as_deref(),
                    extra,
                )
                .map_err(|e| ToolError::msg(format!("share artifact: {e}")))?;
            crate::session::emit_device_event(json!({
                "kind": "coordination_event",
                "event": ev.clone(),
            }));
            ev
        } else {
            match db
                .get_agent(target)
                .map_err(|e| ToolError::msg(format!("look up agent: {e}")))?
            {
                None => {
                    return Err(ToolError::msg(format!(
                        "unknown target `{target}`; supported targets are `human`, a directory agent id, `channel:chat`, or `channel:push`"
                    )));
                }
                Some(agent)
                    if agent.status == crate::coordination::types::AgentStatus::Disabled =>
                {
                    return Err(ToolError::msg(format!("agent `{target}` is disabled")));
                }
                Some(_) => {}
            }
            let key = Uuid::new_v4().to_string();
            let ev = db
                .send_direct_message_typed(
                    (sender_kind, &sender_id),
                    ("agent", target),
                    &body,
                    &key,
                    &chrono::Utc::now().to_rfc3339(),
                    Some(session),
                    ctx.lane_origin().map(|(_, lane)| lane),
                    about_task.as_deref(),
                    extra,
                )
                .map_err(|e| ToolError::msg(format!("share artifact: {e}")))?;
            crate::session::emit_device_event(json!({
                "kind": "coordination_event",
                "event": ev.clone(),
            }));
            ev
        };

        Ok(ToolResult::success(json!({
            "event_id": saved.event_id,
            "thread_id": saved.thread_id,
            "to": target,
            "artifact": artifact_payload,
        })))
    }
}

pub(crate) fn stamp_origin(ctx: &ToolContext, mut payload: Value) -> Value {
    if let Some(session) = ctx.durable_session_id() {
        payload["origin_session"] = json!(session);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    fn context(root: &std::path::Path, session: &str) -> ToolContext {
        ToolContext::new(root)
            .unwrap()
            .with_durable_session_id(session)
            .with_store_path(root.join("snippet.db"))
    }

    fn migrate(root: &std::path::Path) -> Store {
        Store::open(root.join("snippet.db")).unwrap()
    }

    /// A session cannot read or post in a room it has no place in, such as
    /// another task's room or a direct thread between two others.
    #[tokio::test]
    async fn rooms_are_closed_to_outsiders() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_task(&crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Private".into(),
            "work".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        ))
        .unwrap();
        let room = crate::coordination::Task::thread_for("t1");

        let outsider = context(dir.path(), "work-2");
        assert!(
            PostCoordinationMessage
                .execute(&outsider, json!({"thread_id": room, "body": "hi"}))
                .await
                .is_err()
        );
        assert!(
            ReadCoordinationThread
                .execute(&outsider, json!({"thread_id": room}))
                .await
                .is_err()
        );
        assert!(
            PostCoordinationMessage
                .execute(&outsider, json!({"thread_id": "made-up", "body": "hi"}))
                .await
                .is_err()
        );

        // The task's own session and Mission Control are let in.
        let worker = context(dir.path(), "work-1");
        PostCoordinationMessage
            .execute(&worker, json!({"thread_id": room, "body": "progress"}))
            .await
            .unwrap();
        let mc = context(dir.path(), "mc").with_agent_id("mission-control");
        ReadCoordinationThread
            .execute(&mc, json!({"thread_id": room}))
            .await
            .unwrap();
    }

    /// Claiming takes the lease (demoting the holder) and queues the task for
    /// the dispatcher; it used to mark the task running and deliver nothing.
    #[tokio::test]
    async fn claiming_takes_the_lease_and_queues_for_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["snippet", "reviewer"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        }
        let mut task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Review".into(),
            "look".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        task.status = crate::coordination::TaskStatus::Blocked;
        db.create_task(&task).unwrap();
        db.add_task_agent_full(
            "t1",
            "snippet",
            "implementer",
            None,
            "",
            "active",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let ctx = context(dir.path(), "inbox-reviewer").with_agent_id("reviewer");
        ClaimAndDispatchTask
            .execute(&ctx, json!({"task_id": "t1"}))
            .await
            .unwrap();

        let task = db.get_task("t1").unwrap().unwrap();
        assert_eq!(task.status, crate::coordination::TaskStatus::Todo);
        let roster = db.list_task_agents("t1").unwrap();
        let status = |id: &str| {
            roster
                .iter()
                .find(|m| m.agent_id == id)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(status("reviewer"), "active");
        assert_eq!(status("snippet"), "waiting", "one lease holder at a time");

        db.claim_task_for_dispatch("t1", "2026-01-01T00:00:01Z")
            .unwrap();
        assert!(
            ClaimAndDispatchTask
                .execute(&ctx, json!({"task_id": "t1"}))
                .await
                .is_err()
        );
    }

    /// A lease moves an agent into a session it does not own; posts from that
    /// session are the lease holder's, not the session owner's.
    #[tokio::test]
    async fn posts_are_attributed_to_the_lease_holder() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "reviewer".into(),
            display_name: "Reviewer".into(),
            handle: "reviewer".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        })
        .unwrap();
        let mut task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Review".into(),
            "look".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        task.status = crate::coordination::TaskStatus::InProgress;
        task.reporting_session = Some("s1".into());
        db.create_task(&task).unwrap();
        db.add_task_agent_full(
            "t1",
            "reviewer",
            "reviewer",
            None,
            "",
            "active",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let ctx = context(dir.path(), "s1").with_agent_id("owner");
        let posted = PostTaskCoordination
            .execute(&ctx, json!({"task_id": "t1", "body": "looks good"}))
            .await
            .unwrap();
        assert_eq!(posted.value["data"]["event"]["actor_id"], "reviewer");
        assert_eq!(
            posted.value["data"]["event"]["payload"]["origin_session"],
            "s1"
        );
    }

    #[tokio::test]
    async fn post_message_is_attributed_to_the_session_not_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");

        let result = PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"system","body":"starting"}))
            .await
            .unwrap();

        let event_id = result.value["data"]["event"]["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        let events = db.events_for_thread("system", 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, event_id);
        // Identity came from the session, not from caller arguments. This session
        // has no agent bound, so it posts as the session — not as an "agent",
        // which is exactly the conflation this split removes.
        assert_eq!(events[0].actor_id, "mission-control");
        assert_eq!(events[0].actor_kind, "session");
    }

    #[tokio::test]
    async fn post_message_posts_as_the_bound_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "s1").with_agent_id("rust-pr-reviewer");

        PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"system","body":"reviewing"}))
            .await
            .unwrap();

        let events = db.events_for_thread("system", 0, 10).unwrap();
        // An agent-bound session posts as the AGENT, so the board names the
        // identity rather than the address it happens to run in.
        assert_eq!(events[0].actor_id, "rust-pr-reviewer");
        assert_eq!(events[0].actor_kind, "agent");
    }

    #[tokio::test]
    async fn post_message_requires_a_session_identity() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = ToolContext::new(dir.path()).unwrap();
        let error = PostCoordinationMessage
            .execute(&ctx, json!({"thread_id":"t1","body":"hi"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("session identity"));
    }

    #[tokio::test]
    async fn post_message_is_idempotent_for_a_repeated_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        let args = json!({"thread_id":"system","body":"once","idempotency_key":"k1"});

        PostCoordinationMessage
            .execute(&ctx, args.clone())
            .await
            .unwrap();
        PostCoordinationMessage.execute(&ctx, args).await.unwrap();

        assert_eq!(db.events_for_thread("system", 0, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_agents_reports_registered_agents() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "web-researcher".into(),
            display_name: "Web Research Specialist".into(),
            handle: "web_researcher".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec!["web_search".into()],
        })
        .unwrap();
        let ctx = context(dir.path(), "mission-control");

        let result = ListCoordinationAgents
            .execute(&ctx, json!({}))
            .await
            .unwrap();
        let agents = result.value["data"]["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["id"], "web-researcher");
    }

    /// The separation itself: a session is not an agent, so a session with no
    /// agent bound cannot take a turn. Before this split the session id stood in
    /// as the identity, which made every session an agent.

    #[tokio::test]
    async fn read_thread_defaults_to_the_shared_room_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        for body in ["one", "two", "three"] {
            PostCoordinationMessage
                .execute(
                    &ctx,
                    json!({"thread_id": crate::serve::COORDINATION_THREAD, "body": body}),
                )
                .await
                .unwrap();
        }

        // No arguments: the most recent messages, oldest first, defaults to the
        // shared room.
        let read = ReadCoordinationThread
            .execute(&ctx, json!({}))
            .await
            .unwrap();
        let data = &read.value["data"];
        assert_eq!(data["thread_id"], crate::serve::COORDINATION_THREAD);
        let bodies: Vec<&str> = data["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, ["one", "two", "three"]);
        assert_eq!(data["oldest_sequence"], 1);
        assert_eq!(data["newest_sequence"], 3);
    }

    #[tokio::test]
    async fn read_thread_pages_forward_and_back_by_cursor() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        for body in ["m1", "m2", "m3", "m4"] {
            PostCoordinationMessage
                .execute(&ctx, json!({"thread_id": "system", "body": body}))
                .await
                .unwrap();
        }

        // Recent window, then page further back using the returned oldest.
        let recent = ReadCoordinationThread
            .execute(&ctx, json!({"thread_id": "system", "limit": 2}))
            .await
            .unwrap();
        let data = &recent.value["data"];
        assert_eq!(
            data["messages"].as_array().unwrap().len(),
            2,
            "limit bounds the window"
        );
        assert_eq!(data["newest_sequence"], 4);
        assert_eq!(data["oldest_sequence"], 3);
        assert_eq!(data["may_have_more"], true);

        // after_sequence pages forward from a cursor.
        let forward = ReadCoordinationThread
            .execute(&ctx, json!({"thread_id": "system", "after_sequence": 2}))
            .await
            .unwrap();
        let fwd: Vec<&str> = forward.value["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap())
            .collect();
        assert_eq!(fwd, ["m3", "m4"]);
        assert_eq!(forward.value["data"]["direction"], "forward");
    }

    #[tokio::test]
    async fn read_thread_reports_an_empty_room_without_error() {
        let dir = tempfile::tempdir().unwrap();
        migrate(dir.path());
        let ctx = context(dir.path(), "mission-control");
        let read = ReadCoordinationThread
            .execute(&ctx, json!({}))
            .await
            .unwrap();
        let data = &read.value["data"];
        assert!(data["messages"].as_array().unwrap().is_empty());
        assert_eq!(data["may_have_more"], false);
        assert_eq!(data["newest_sequence"], 0);
    }

    fn direct_event(actor_kind: &str, origin: Option<&str>) -> CoordinationEvent {
        let mut payload = json!({"body": "x", "recipient": "agent:snippet"});
        if let Some(origin) = origin {
            payload["origin_session"] = json!(origin);
        }
        CoordinationEvent {
            event_id: format!("e-{actor_kind}-{origin:?}"),
            thread_id: "direct:agent:snippet|human:local".into(),
            partition_key: "thread:x".into(),
            sequence: 0,
            event_type: "direct_message.sent".into(),
            actor_kind: actor_kind.into(),
            actor_id: if actor_kind == "human" {
                "local".into()
            } else {
                "snippet".into()
            },
            payload_version: 1,
            payload,
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn the_newest_human_message_decides_where_a_reply_goes() {
        // The agent's own earlier reply sits between the two questions; it must
        // not be mistaken for the question being answered now.
        let events = vec![
            direct_event("human", Some("ws-a")),
            direct_event("agent", None),
            direct_event("human", Some("ws-b")),
        ];
        assert_eq!(latest_human_origin(&events), Some("ws-b"));
    }

    #[test]
    fn a_human_message_without_a_session_does_not_redirect() {
        let events = vec![direct_event("human", None)];
        assert_eq!(latest_human_origin(&events), None);
    }

    #[tokio::test]
    async fn a_reply_to_the_human_is_routed_to_the_session_that_asked() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        let state = crate::harness::HarnessState::blank("/tmp/ws", Some("asking".into()));
        db.import_session(
            "ws-a",
            "key",
            &state,
            &crate::conversations::SessionExtras::default(),
        )
        .unwrap();
        db.send_direct_message_from(
            ("human", "local"),
            ("agent", "snippet"),
            "did the dispatch land?",
            "k1",
            "2026-01-01T00:00:00Z",
            Some("ws-a"),
        )
        .unwrap();

        assert_eq!(
            reply_session_for_agent(&db, "snippet", "local").as_deref(),
            Some("ws-a"),
            "a reply addressed to the person belongs in the session that asked"
        );
    }

    #[tokio::test]
    async fn a_reply_is_not_routed_to_a_session_the_store_does_not_have() {
        // A stale origin must fall back to the human thread: redirecting into a
        // session that no longer exists is a delivery that can never land.
        let db = Store::open_in_memory().unwrap();
        db.send_direct_message_from(
            ("human", "local"),
            ("agent", "snippet"),
            "hello",
            "k1",
            "2026-01-01T00:00:00Z",
            Some("gone"),
        )
        .unwrap();

        assert_eq!(reply_session_for_agent(&db, "snippet", "local"), None);
    }

    #[tokio::test]
    async fn task_coordination_and_lease_handoff_flow() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "agent-a".into(),
            display_name: "Agent A".into(),
            handle: "agent_a".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        })
        .unwrap();
        db.create_agent(&Agent {
            id: "agent-b".into(),
            display_name: "Agent B".into(),
            handle: "agent_b".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        })
        .unwrap();

        let task = crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Title".into(),
            "Desc".into(),
            "sess-1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        db.create_task(&task).unwrap();
        db.add_task_agent_full(
            "t1",
            "agent-a",
            "implementer",
            None,
            "",
            "active",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        db.add_task_agent_full(
            "t1",
            "agent-b",
            "reviewer",
            None,
            "",
            "waiting",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();

        let ctx_a = ToolContext::new(dir.path())
            .unwrap()
            .with_store_path(dir.path().join("snippet.db"))
            .with_durable_session_id("sess-1")
            .with_agent_id("agent-a");

        let res = MessageMissionControl
            .execute(
                &ctx_a,
                json!({"task_id": "t1", "message": "Need clarification on auth"}),
            )
            .await
            .unwrap();
        assert_eq!(res.value["status"], "success");

        let t = db.get_task("t1").unwrap().unwrap();
        assert_eq!(t.notifications.len(), 1);
        assert_eq!(t.notifications[0].target, "mission_control");
        assert!(
            t.notifications[0]
                .message
                .contains("Need clarification on auth")
        );

        let res = PostTaskCoordination
            .execute(
                &ctx_a,
                json!({"task_id": "t1", "body": "Auth implementation ready for review"}),
            )
            .await
            .unwrap();
        assert_eq!(res.value["status"], "success");

        db.transfer_task_session_lease("t1", "agent-a", "agent-b")
            .unwrap();

        // Verify InspectTask provides maximum visibility
        let ctx_b = ToolContext::new(dir.path())
            .unwrap()
            .with_store_path(dir.path().join("snippet.db"))
            .with_durable_session_id("sess-1")
            .with_agent_id("agent-b");

        let inspect_res = InspectTask
            .execute(&ctx_b, json!({"task_id": "t1"}))
            .await
            .unwrap();
        assert_eq!(inspect_res.value["status"], "success");
        let val = &inspect_res.value["data"];
        assert_eq!(val["task_id"], "t1");
        assert_eq!(val["title"], "Title");
        assert_eq!(val["roster"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn send_agent_message_schema_and_reply_reaction_behavior() {
        let def = SendAgentMessage.definition();
        assert!(def.description.contains(
            "reply_to quotes the message you answer; reaction acknowledges one without words (check: done or agreed, eyes: seen and on it, thumbs_up, question: unclear, thinking)."
        ));
        let props = &def.input_schema["properties"];
        assert_eq!(props["reply_to"]["type"], "string");
        assert_eq!(
            props["reaction"]["enum"],
            json!(["check", "eyes", "thumbs_up", "question", "thinking"])
        );

        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["sender", "peer"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        }
        let ctx = context(dir.path(), "s-sender").with_agent_id("sender");

        let first = SendAgentMessage
            .execute(
                &ctx,
                json!({"recipient_agent_id": "peer", "body": "can you verify?"}),
            )
            .await
            .unwrap();
        let target_id = first.value["data"]["event_id"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(
            SendAgentMessage
                .execute(
                    &ctx,
                    json!({"recipient_agent_id": "peer", "reaction": "eyes"})
                )
                .await
                .is_err()
        );
        assert!(
            SendAgentMessage
                .execute(&ctx, json!({"recipient_agent_id": "peer", "body": ""}))
                .await
                .is_err()
        );

        SendAgentMessage
            .execute(
                &ctx,
                json!({
                    "recipient_agent_id": "peer",
                    "reply_to": target_id,
                    "reaction": "eyes",
                    "body": "ignored text",
                }),
            )
            .await
            .unwrap();

        SendAgentMessage
            .execute(
                &ctx,
                json!({
                    "recipient_agent_id": "peer",
                    "reply_to": target_id,
                    "body": "all set",
                }),
            )
            .await
            .unwrap();

        let thread = crate::coordination::direct_thread_id(("agent", "sender"), ("agent", "peer"));
        let events = db.direct_thread_events(&thread, 0, 10).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].payload["body"], "");
        assert_eq!(events[1].payload["kind"], "reaction");
        assert_eq!(events[1].payload["reaction"], "eyes");
        assert_eq!(events[1].payload["reply_to"], target_id);
        assert_eq!(events[2].payload["body"], "all set");
        assert_eq!(events[2].payload["reply_to"], target_id);
    }

    #[tokio::test]
    async fn send_agent_message_forward_quotes_body_sets_payload_and_checks_access() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["a", "b", "outsider"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        }
        let long_tail = "x".repeat(1600);
        let raw_body = format!("first   line\n\nsecond\tline {long_tail}");
        let orig = db
            .send_direct_message_typed(
                ("human", crate::coordination::LOCAL_HUMAN_ID),
                ("agent", "a"),
                &raw_body,
                "k-orig",
                "2026-01-01T00:00:00Z",
                None,
                None,
                None,
                Default::default(),
            )
            .unwrap();

        // An agent not on the thread cannot forward the event.
        let outsider_ctx = context(dir.path(), "s-outsider").with_agent_id("outsider");
        let denied = SendAgentMessage
            .execute(
                &outsider_ctx,
                json!({
                    "recipient_agent_id": "b",
                    "forward": orig.event_id,
                    "body": "leak",
                }),
            )
            .await;
        assert!(denied.is_err());

        // Agent `a` can forward to peer `b`.
        let a_ctx = context(dir.path(), "inbox-a").with_agent_id("a");
        SendAgentMessage
            .execute(
                &a_ctx,
                json!({
                    "recipient_agent_id": "b",
                    "forward": orig.event_id,
                    "body": "please check",
                }),
            )
            .await
            .unwrap();

        let ab_thread = crate::coordination::direct_thread_id(("agent", "a"), ("agent", "b"));
        let ab_events = db.direct_thread_events(&ab_thread, 0, 10).unwrap();
        assert_eq!(ab_events.len(), 1);
        let fwd = &ab_events[0];
        let expected_clipped: String = raw_body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(1500)
            .collect();
        assert_eq!(expected_clipped.chars().count(), 1500);
        let expected_body = format!(
            "[forwarded from human:local in {}, 2026-01-01T00:00:00Z]\n{expected_clipped}\n[/forwarded]\nplease check",
            orig.thread_id
        );
        assert_eq!(fwd.payload["body"], expected_body);
        assert_eq!(
            fwd.payload["forwarded_from"],
            json!({
                "event_id": orig.event_id,
                "thread_id": orig.thread_id,
                "actor_kind": "human",
                "actor_id": "local",
            })
        );
        assert!(fwd.payload.get("about_thread").is_none());

        // Forwarding a human message to a task sets about_thread to the original thread.
        let mut task = crate::coordination::Task::filed_by_human(
            "t-fwd".into(),
            "Forward task".into(),
            "desc".into(),
            "s1".into(),
            0,
            "2026-01-01T00:00:00Z".into(),
        );
        task.status = TaskStatus::InProgress;
        task.session_id = "task-sess-fwd".into();
        db.create_task(&task).unwrap();
        db.import_session(
            "task-sess-fwd",
            "wk",
            &crate::harness::HarnessState::blank("/tmp", None),
            &crate::conversations::SessionExtras::default(),
        )
        .unwrap();
        SendAgentMessage
            .execute(
                &a_ctx,
                json!({
                    "recipient_agent_id": "task:t-fwd",
                    "forward": orig.event_id,
                    "body": "on your task",
                }),
            )
            .await
            .unwrap();
        let task_thread =
            crate::coordination::direct_thread_id(("agent", "a"), ("session", "task-sess-fwd"));
        let task_events = db.direct_thread_events(&task_thread, 0, 10).unwrap();
        assert_eq!(task_events.len(), 1);
        assert_eq!(task_events[0].payload["about_thread"], orig.thread_id);
        assert_eq!(
            task_events[0].payload["forwarded_from"]["event_id"],
            orig.event_id
        );

        // Forwarding a human post from a task room sets no about_thread.
        db.add_task_agent_full(
            "t-fwd",
            "a",
            "implementer",
            Some("task-sess-fwd"),
            "",
            "active",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let room_post = db
            .append_event(&CoordinationEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                thread_id: task.thread_id.clone(),
                partition_key: format!("thread:{}", task.thread_id),
                sequence: 0,
                event_type: "message.posted".into(),
                actor_kind: "human".into(),
                actor_id: crate::coordination::LOCAL_HUMAN_ID.into(),
                payload_version: 1,
                payload: json!({"body": "note in task room"}),
                causation_id: None,
                correlation_id: None,
                idempotency_key: "k-room-post".into(),
                created_at: "2026-01-01T00:00:01Z".into(),
            })
            .unwrap();
        SendAgentMessage
            .execute(
                &a_ctx,
                json!({
                    "recipient_agent_id": "task:t-fwd",
                    "forward": room_post.event_id,
                    "body": "from room",
                }),
            )
            .await
            .unwrap();
        let task_events = db.direct_thread_events(&task_thread, 0, 10).unwrap();
        assert_eq!(task_events.len(), 2);
        assert!(task_events[1].payload.get("about_thread").is_none());
        assert_eq!(
            task_events[1].payload["forwarded_from"]["event_id"],
            room_post.event_id
        );
    }

    #[tokio::test]
    async fn share_artifact_payload_shape_channels_and_unknown_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["sender", "peer"] {
            db.create_agent(&Agent {
                id: id.into(),
                display_name: id.into(),
                handle: id.into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        }
        let session = format!("test-share-{}", Uuid::new_v4());
        let ctx = context(dir.path(), &session).with_agent_id("sender");

        let err = ShareArtifactTool
            .execute(&ctx, json!({"artifact_id": "missing", "to": "human"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown artifact"), "{err}");

        let page = dir.path().join("report.html");
        std::fs::write(
            &page,
            "<!doctype html><title>Weekly Report</title><h1>Hi</h1>",
        )
        .unwrap();
        let presented =
            crate::artifacts::present(&session, &page, Some("Weekly Report"), None, None).unwrap();
        assert_eq!(presented.version, 1);
        let aid = presented.artifact.id.clone();

        let shared_human = ShareArtifactTool
            .execute(&ctx, json!({"artifact_id": aid, "to": "human"}))
            .await
            .unwrap();
        let art = &shared_human.value["data"]["artifact"];
        assert_eq!(art["session"], session);
        assert_eq!(art["id"], aid);
        assert_eq!(art["title"], "Weekly Report");
        assert_eq!(art["version"], 1);
        assert_eq!(art["kind"], "html");
        let url = art["url"].as_str().unwrap();
        assert!(url.starts_with("/a/"), "{url}");

        let parts: Vec<&str> = url.trim_start_matches("/a/").splitn(3, '/').collect();
        let exp: i64 = parts[0].parse().unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!(
            exp >= now + 29 * 86_400 && exp <= now + 31 * 86_400,
            "exp={exp}, now={now}"
        );
        let resolved =
            crate::artifacts::resolve_signed(&artifact_signing_secret(), exp, parts[1], parts[2]);
        assert!(resolved.is_some_and(|p| p.is_file()));

        let human_thread = crate::coordination::direct_thread_id(
            ("agent", "sender"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        let human_events = db.direct_thread_events(&human_thread, 0, 10).unwrap();
        assert_eq!(human_events.len(), 1);
        assert_eq!(human_events[0].payload["body"], "Weekly Report");
        assert_eq!(human_events[0].payload["artifact"], *art);

        ShareArtifactTool
            .execute(
                &ctx,
                json!({
                    "artifact_id": aid,
                    "to": "peer",
                    "note": "Check this chart"
                }),
            )
            .await
            .unwrap();
        let peer_thread =
            crate::coordination::direct_thread_id(("agent", "sender"), ("agent", "peer"));
        let peer_events = db.direct_thread_events(&peer_thread, 0, 10).unwrap();
        assert_eq!(peer_events.len(), 1);
        assert_eq!(peer_events[0].payload["body"], "Check this chart");
        assert_eq!(peer_events[0].payload["artifact"]["id"], aid);

        ShareArtifactTool
            .execute(
                &ctx,
                json!({
                    "artifact_id": aid,
                    "to": "channel:chat",
                    "note": "Posted on chat"
                }),
            )
            .await
            .unwrap();
        let human_events = db.direct_thread_events(&human_thread, 0, 10).unwrap();
        assert_eq!(human_events.len(), 2);
        assert_eq!(human_events[1].payload["body"], "Posted on chat");
        assert_eq!(human_events[1].payload["channel"], "chat");
        assert_eq!(human_events[1].payload["artifact"]["id"], aid);

        let bad_chan = ShareArtifactTool
            .execute(&ctx, json!({"artifact_id": aid, "to": "channel:voice"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(bad_chan.contains("supported targets"), "{bad_chan}");

        crate::artifacts::delete_session(&session);
    }
}
