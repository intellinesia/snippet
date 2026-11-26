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

/// Tools every coordination-aware session (Lead and workers) can use
/// to discover peers and post to the board.
pub fn add_coordination_tools(registry: &mut ToolRegistry) {
    registry.insert(ListCoordinationAgents);
    registry.insert(PostCoordinationMessage);
    registry.insert(ReadCoordinationThread);
    // Direct peer conversation: the human and any agent can address one named
    // agent, and every agent can always reach Lead.
    registry.insert(SendAgentMessage);
    registry.insert(ReadAgentThread);
    registry.insert(ReadAgentInbox);
    // Per-agent coordination memory: recall before acting, and record what is
    // worth keeping.
    registry.insert(ReadCoordinationBoard);
    registry.insert(RecordCoordinationNote);
    // Task-based coordination and session lease handoff
    registry.insert(MessageLead);
    registry.insert(PostTaskCoordination);
    registry.insert(PassTurn);
    registry.insert(InspectTask);
    registry.insert(ClaimAndDispatchTask);
    registry.insert(crate::agent_wakes::WakeScheduleTool);
    registry.insert(crate::watch_tool::WatchTool);
    registry.insert(crate::agent_context::ContextLimitTool);
    registry.insert(crate::code_mode::mode::ToolModeTool);
    registry.insert(crate::keep_warm::KeepWarmTool);
    registry.insert(crate::mission_tools::ReachPolicyTool);
    registry.insert(ShareArtifactTool);
    registry.insert(GroupThread);
    // One chat surface over every thread kind: list them, read any, post to any.
    registry.insert(Chats);
    registry.insert(ReadChat);
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
/// session, and every room for Lead, which coordinates them all.
/// On a direct thread the participant PAIR (kind and id) must match, so either
/// end of the conversation — agent or human — reads it and nobody else does.
fn require_thread_access(
    db: &Store,
    ctx: &ToolContext,
    actor: (&str, &str),
    thread_id: &str,
) -> Result<(), ToolError> {
    let (actor_kind, actor_id) = actor;
    if thread_id == crate::serve::COORDINATION_THREAD {
        return Ok(());
    }
    let lookup = |e: StoreError| ToolError::msg(format!("look up thread: {e}"));
    if !db.thread_exists(thread_id).map_err(lookup)? {
        return Err(ToolError::msg(format!("no thread `{thread_id}`")));
    }
    if actor_id == crate::lead::SESSION_ID
        || db
            .list_thread_participants(thread_id)
            .map_err(lookup)?
            .iter()
            .any(|(member, kind)| member.as_str() == actor_id && kind.as_str() == actor_kind)
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
    if ctx.agent_id() == Some(crate::lead::SESSION_ID) {
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
        let state = crate::progress::desk_state_render(&db, agent);
        if !state.is_empty() {
            out.push(state);
        }
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
            "Share milestones and blockers with everyone on the task via message.task; report with task.report when done.".to_string(),
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
                "Your other open tasks as {agent} (task.inspect for detail; message.send to task:<id> reaches the session doing it):\n{}",
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
            description: "The agent directory: every agent's id, status, capabilities, the inference profile it works on (when it has one) and a one-line summary of what it is for. Only active agents take new work.".into(),
            input_schema: schema(json!({}), &[]),
        }
    }
    async fn execute(&self, ctx: &ToolContext, _: Value) -> Result<ToolResult, ToolError> {
        let agents = db(ctx)?
            .list_agents()
            .map_err(|e| ToolError::msg(format!("list agents: {e}")))?;
        let root = crate::coordination::agents_root(
            &ctx.lead_root()
                .unwrap_or_else(|| crate::config::snippet_home().join("lead")),
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

pub(crate) fn identity_summary(identity: &str) -> String {
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
        require_thread_access(&db, ctx, (actor_kind, actor_id.as_str()), &args.thread_id)?;
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
        if crate::coordination::direct::is_removed_reaction(&event.payload) {
            out["removed"] = json!(true);
        }
    }
    out
}

pub struct ReadCoordinationThread;
#[async_trait]
impl Tool for ReadCoordinationThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_coordination_thread".into(),
            description: "Read the coordination room's messages, oldest first. Call with no arguments for the most recent messages; pass after_sequence to page further back (use the returned oldest_sequence) or forward. The wake message already includes the recent history. For direct, group and task threads addressed by peer or id, use read_chat.".into(),
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
        let (actor_kind, actor_id) = actor(ctx)?;
        require_thread_access(&db, ctx, (actor_kind, actor_id.as_str()), &thread_id)?;
        Ok(ToolResult::success(thread_page(
            &db,
            &thread_id,
            args.after_sequence,
            limit,
            false,
        )?))
    }
}

/// One page of a thread's events, flattened for the model — the paging and
/// result shape `read_coordination_thread` and `read_chat` share. `hide_auto`
/// drops the daemon's own acknowledgement events, the rule wake envelopes use.
/// The sequence cursors still cover the raw page, so paging stays honest.
fn thread_page(
    db: &Store,
    thread_id: &str,
    after_sequence: Option<u64>,
    limit: u32,
    hide_auto: bool,
) -> Result<Value, ToolError> {
    // No cursor → the most recent messages. Cursor → the next page forward.
    // `after_sequence: 0` deliberately means "from the beginning".
    let (events, page_forward) = match after_sequence {
        Some(after) => (
            db.events_for_thread(thread_id, after, limit)
                .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
            true,
        ),
        None => (
            db.recent_events_for_thread(thread_id, limit)
                .map_err(|e| ToolError::msg(format!("read thread: {e}")))?,
            false,
        ),
    };

    let oldest = events.first().map(|e| e.sequence).unwrap_or(0);
    let newest = events.last().map(|e| e.sequence).unwrap_or(0);
    // A full page is a hint there may be more; the caller pages with the
    // oldest sequence to go back, or the newest to move forward.
    let full_page = events.len() as u32 >= limit;
    let messages: Vec<Value> = events
        .iter()
        .filter(|event| !hide_auto || event.payload.get("auto").is_none())
        .map(flatten_event)
        .collect();
    Ok(json!({
        "thread_id": thread_id,
        "messages": messages,
        "oldest_sequence": oldest,
        "newest_sequence": newest,
        "may_have_more": full_page,
        "direction": if page_forward { "forward" } else { "recent" },
    }))
}

#[derive(Deserialize)]
struct SendAgentMessageArgs {
    to: String,
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

pub(crate) const REACTION_TARGET: &str = "event id a reaction acknowledges";
pub(crate) const DESK_QUOTE: &str =
    "event id a reaction acknowledges, or the user's message your reply to `human` quotes";
#[async_trait]
impl Tool for SendAgentMessage {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "send_agent_message".into(),
            description: "Reply to a message or contact an agent. For a session-origin message the recipient is its exact `reply_to` value (`session:<id>`); `human` is the user, otherwise an agent id or `task:<id>`. This is conversation only: it cannot create work, transfer ownership or authorise workspace changes (`post_task_coordination` posts task updates). `urgent` interrupts the recipient. reaction acknowledges the message in reply_to without words (check: done or agreed, eyes: seen and on it, thumbs_up, question: unclear, thinking); only a desk's reply to `human` quotes it.".into(),
            input_schema: schema(
                json!({
                    "to":{"type":"string","description":"agent id, `task:<id>`, `session:<id>` or `human`"},
                    "body":{"type":"string"},
                    "idempotency_key":{"type":"string"},
                    "kind":{"type":"string","enum":["note","question","status","handoff_note","blocker","result","interrupt"]},
                    "urgency":{"type":"string","enum":["normal","urgent"],"description":"default normal; `interrupt` is always urgent"},
                    "reply_to":{"type":"string","description":DESK_QUOTE},
                    "reaction":{"type":"string","enum":["check","eyes","thumbs_up","question","thinking"]},
                    "forward":{"type":"string","description":"event id of a chat message to quote under your body as context"}
                }),
                &["to"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: SendAgentMessageArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let recipient = bare_address(&args.to);
        if recipient.is_empty() {
            return Err(ToolError::msg("to must not be empty"));
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
        let body = if reaction.is_some() {
            String::new()
        } else {
            args.body.trim().to_string()
        };
        let task_session;
        let mut about_task: Option<String> = None;
        let mut about_task_title: Option<String> = None;
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
                        "task `{}` has not been dispatched to a session yet. Ask lead about it.",
                        task.id
                    )));
                }
                if reaction.is_none()
                    && task.status == TaskStatus::InReview
                    && ctx.agent_id() == Some(crate::lead::SESSION_ID)
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
                about_task = Some(task.id.clone());
                about_task_title = Some(task.title.clone());
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
        if requested_kind == "human"
            && reaction.is_none()
            && let Some(session) = ctx.durable_session_id()
            && crate::coordination::origin::managing_desk(&db, session).is_some()
        {
            let mut extra = serde_json::Map::new();
            extra.insert("kind".into(), json!(args.kind.as_deref().unwrap_or("note")));
            extra.insert("for_user".into(), json!(true));
            crate::coordination::origin::tell_managing_desk(
                &db,
                session,
                &Uuid::new_v4().to_string(),
                &body,
                extra,
            )
            .ok_or_else(|| ToolError::msg("could not reach your desk"))?;
            return Ok(ToolResult::success(json!({
                "to": "desk",
                "note": "your desk decides what reaches the user",
            })));
        }
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
                        "unknown agent `{to_id}` — snippet tool list_coordination_agents shows the directory. Use `human` to reply to the person."
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
            if reaction.is_none() && ctx.agent_id() == Some(crate::lead::SESSION_ID) {
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
        let desk_to_user = to_kind == "human"
            && ctx
                .durable_session_id()
                .is_some_and(crate::session::is_inbox_session_id);
        let reply_to = reply_to.filter(|id| {
            reaction.is_some()
                || (desk_to_user
                    && db.event_by_id(id).ok().flatten().is_some_and(|e| {
                        e.actor_kind == "human"
                            && e.thread_id
                                == crate::coordination::direct_thread_id(
                                    ("agent", ctx.agent_id().unwrap_or_default()),
                                    ("human", to_id),
                                )
                    }))
        });
        crate::coordination::typed::apply_reply_and_reaction(
            &mut extra,
            reply_to.as_deref(),
            reaction.as_deref(),
        );
        if let Some(title) = &about_task_title {
            extra.insert("about_task_title".into(), json!(title));
        }
        if let Some(forward_id) = forward_id {
            let orig = db
                .event_by_id(forward_id)
                .map_err(|e| ToolError::msg(format!("look up event: {e}")))?
                .ok_or_else(|| ToolError::msg(format!("no event `{forward_id}`")))?;
            require_thread_access(&db, ctx, (sender_kind, sender_id.as_str()), &orig.thread_id)?;
            let mut forwarded_text = String::new();
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
                forwarded_text = clipped;
            }
            extra.insert(
                "forwarded_from".into(),
                json!({
                    "event_id": orig.event_id,
                    "thread_id": orig.thread_id,
                    "actor_kind": orig.actor_kind,
                    "actor_id": orig.actor_id,
                    "created_at": orig.created_at,
                    "text": forwarded_text,
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
            if sender_kind == "agent" && reaction.is_none() && sender_id != crate::lead::SESSION_ID
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
    peer: String,
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
            description: "Read your direct conversation with one agent — or with `human`/`local` for the user — oldest first. Omit after_sequence for the most recent messages; pass it to page forward. The wake message already includes the recent history. For group threads, task rooms and any thread by id, use read_chat.".into(),
            input_schema: schema(
                json!({
                    "peer":{"type":"string","description":"peer agent id or `human`"},
                    "after_sequence":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1,"maximum":100}
                }),
                &["peer"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadAgentThreadArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let peer = args.peer.trim();
        if peer.is_empty() {
            return Err(ToolError::msg("peer must not be empty"));
        }
        let (kind, id) = actor(ctx)?;
        let (peer_kind, peer_id) = match peer {
            "human" | "local" => ("human", crate::coordination::LOCAL_HUMAN_ID),
            agent => ("agent", agent),
        };
        let thread_id = crate::coordination::direct_thread_id((kind, &id), (peer_kind, peer_id));
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
            "peer": if peer_kind == "human" { "human" } else { peer },
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
            description: "Write a note to your own coordination memory, so a later turn can recall it. One or two sentences per row. Dispatches and their reports are recorded automatically.".into(),
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
struct MessageLeadArgs {
    task_id: String,
    message: String,
}

pub struct MessageLead;
#[async_trait]
impl Tool for MessageLead {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "message_lead".into(),
            description: "Message Lead using the active task board item you are working on. This posts to the task thread, notifies Lead, and records the update on the task board so your progress or question is linked to the task.".into(),
            input_schema: schema(
                json!({
                    "task_id": {"type": "string", "description": "The task ID you are working on"},
                    "message": {"type": "string", "description": "Message, question, or status update for Lead"}
                }),
                &["task_id", "message"],
            ),
        }
    }
    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: MessageLeadArgs =
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
        require_thread_access(&db, ctx, (actor_kind, actor_id.as_str()), &task.thread_id)?;
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
                    "to": "lead",
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
            let target = t.notification_target();
            t.notifications.push(NotificationMarker {
                target,
                kind: "message".into(),
                message: format!("{actor_id}: {message}"),
                delivered: false,
                origin_reported: false,
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
            description: "Post context, a plan proposal or status to the task thread of a task board item. Everyone on the task and Lead see it. The task room is the task's one channel, in task order. Optional `kind` types the post; `urgency: urgent` stops the other sessions' runs and wakes them now, while normal posts wait for their next turn.".into(),
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
        require_thread_access(&db, ctx, (actor_kind, actor_id.as_str()), &task.thread_id)?;
        let (kind, urgency) =
            crate::coordination::typed::parse(args.kind.as_deref(), args.urgency.as_deref())
                .map_err(ToolError::msg)?;
        let mut payload = json!({"body": body, "task_id": task_id});
        let to: Vec<String> = args
            .to
            .iter()
            .map(|a| bare_address(a).to_string())
            .filter(|a| !a.is_empty())
            .collect();
        if !to.is_empty() {
            let roster = db.list_task_agents(task_id).unwrap_or_default();
            if let Some(stranger) = to.iter().find(|a| {
                a.as_str() != crate::lead::SESSION_ID
                    && !roster
                        .iter()
                        .any(|m| m.agent_id == **a && m.removed_at.is_none())
            }) {
                return Err(ToolError::msg(format!(
                    "`{stranger}` isn't on this task. Use task.invite to bring them in."
                )));
            }
            crate::members::seat_holder(&db, &task);
            for agent in to.iter().filter(|a| a.as_str() != crate::lead::SESSION_ID) {
                crate::members::seat(&db, &task, agent, &now_rfc3339()).map_err(ToolError::msg)?;
            }
            payload["to"] = json!(to);
        }
        for (key, value) in crate::coordination::typed::payload_fields(kind, urgency, None) {
            payload[key] = value;
        }
        let now = now_rfc3339();
        if task.status == TaskStatus::InReview && actor_id == crate::lead::SESSION_ID {
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
            let _ = step_line;
            out["note"] = json!(
                "Posted to the task room. Note: a room post does not complete the task; call task.report when the work is finished."
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
    #[serde(default)]
    context: Option<String>,
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
                    "profile": {"type": "string", "description": "Optional inference profile name. Omit to preserve prompt cache affinity."},
                    "context": {"type": "string", "description": "What the worker should know from your memory, skills or past work that the brief does not say; added to its brief."}
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
        if crate::lane_offers::is_offer_id(task_id) {
            let (_, actor_id) = actor(ctx)?;
            let db = db(ctx)?;
            let note = crate::lane_offers::claim_or_decline(
                &db,
                task_id,
                &actor_id,
                true,
                args.context.as_deref(),
            )
            .map_err(ToolError::msg)?;
            return Ok(ToolResult::success(
                json!({"claimed": true, "offer_id": task_id, "note": note}),
            ));
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
        if actor_id == crate::lead::SESSION_ID {
            return Err(ToolError::msg(
                "Lead routes work and never takes a task's lease. To send a blocked or failed task to its session again, use tasks.retry (pass profile to switch the worker's model).",
            ));
        }
        crate::mission_tools::require_working_agent(&db, &actor_id)?;
        let now = now_rfc3339();
        let desk_context = args
            .context
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        db.add_task_agent_full(task_id, &actor_id, "implementer", None, "", "active", &now)
            .map_err(|e| ToolError::msg(format!("take the lease: {e}")))?;
        db.retry_task(task_id, &now)
            .map_err(|e| ToolError::msg(format!("queue task: {e}")))?;
        let queued = db
            .update_task_in(task_id, &now, |t| {
                if profile.is_some() {
                    t.profile = profile.clone();
                }
                if let Some(context) = desk_context.as_deref() {
                    t.description = format!(
                        "{}\n\nFrom your desk ({actor_id}), when it took this on:\n{context}",
                        t.description
                    );
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
            description: "Turn down a task you were offered, with a reason. You leave the roster, the task waits as blocked, and Lead is told the reason.".into(),
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
        if crate::lane_offers::is_offer_id(task_id) {
            let note =
                crate::lane_offers::claim_or_decline(&db, task_id, &actor_id, false, Some(reason))
                    .map_err(ToolError::msg)?;
            return Ok(ToolResult::success(
                json!({"declined": true, "offer_id": task_id, "note": note}),
            ));
        }
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
        .map_err(|e| ToolError::msg(format!("notify Lead: {e}")))?;
        let _ = db.record_board_entry(
            &actor_id,
            crate::coordination::BoardEntryKind::Noted,
            &crate::coordination::NewBoardEntry {
                session_id: Some(&task.session_id),
                workspace: None,
                summary: &format!("{task_id} — declined: {reason}"),
                correlation_id: Some(task_id),
                created_at: &now,
            },
        );
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
            "note": "Lead has been told why; the task waits as blocked until it is reassigned or reshaped.",
        })))
    }
}

/// Group threads: rooms with several agents and the human where a post fans
/// out and agents take turns like colleagues in a chat.
pub struct GroupThread;

/// The board identity a group call speaks as. Group threads belong to agents
/// and Lead; a plain session has no place in one.
fn group_agent(ctx: &ToolContext) -> Result<String, ToolError> {
    if ctx.is_lead() {
        return Ok(crate::lead::SESSION_ID.to_string());
    }
    match actor(ctx)? {
        ("agent", id) => Ok(id),
        ("session", id) if id == crate::lead::SESSION_ID => Ok(id),
        _ => Err(ToolError::msg("group threads are for agents and Lead")),
    }
}

fn known_group_agent(db: &Store, id: &str) -> bool {
    id == crate::lead::SESSION_ID || db.get_agent(id).ok().flatten().is_some()
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
    if agent_id == crate::lead::SESSION_ID
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

/// An address as the envelopes write it (`agent:snippet`, `human:local`) in
/// the bare form the tools take (`snippet`, `human`).
pub(crate) fn bare_address(raw: &str) -> &str {
    let raw = raw.trim();
    if raw == "human" || raw.starts_with("human:") {
        return "human";
    }
    raw.strip_prefix("agent:").map(str::trim).unwrap_or(raw)
}

fn string_list(arguments: &Value, key: &str) -> Vec<String> {
    arguments
        .get(key)
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(bare_address).map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_group_choices(arguments: &Value) -> Result<Option<Vec<String>>, ToolError> {
    let Some(raw) = arguments.get("choices").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let items = raw
        .as_array()
        .ok_or_else(|| ToolError::msg("`choices` must be an array of strings"))?;
    let mut choices = Vec::with_capacity(items.len());
    for item in items {
        let s = item
            .as_str()
            .ok_or_else(|| ToolError::msg("each entry in `choices` must be a string"))?
            .trim();
        if s.is_empty() {
            return Err(ToolError::msg("each option in `choices` must be non-empty"));
        }
        if s.chars().count() > 40 {
            return Err(ToolError::msg(
                "each option in `choices` must be at most 40 characters",
            ));
        }
        choices.push(s.to_string());
    }
    if !(2..=5).contains(&choices.len()) {
        return Err(ToolError::msg(
            "`choices` must contain between 2 and 5 options",
        ));
    }
    Ok(Some(choices))
}

fn strip_leading_mentions(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        let Some(rest) = s.strip_prefix('@') else {
            return s;
        };
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
            .unwrap_or(rest.len());
        if end == 0 {
            return s;
        }
        s = rest[end..].trim_start_matches([',', ':']);
    }
}

const GROUP_POST_MAX_CHARS: usize = 2500;
const GROUP_POST_MAX_QUOTED_CHARS: usize = 6000;

fn check_group_post_chatter(body: &str) -> Result<(), &'static str> {
    const ACK_PREFIXES: &[&str] = &[
        "thank you",
        "sounds good",
        "standing by",
        "understood",
        "confirmed",
        "will do",
        "got it",
        "on it",
        "agreed",
        "copied",
        "acked",
        "noted",
        "roger",
        "thanks",
        "holding",
        "copy",
        "seen",
        "okay",
        "ack",
        "ok",
    ];
    let trimmed = body.trim();
    let unquoted: usize = trimmed
        .lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .map(|l| l.chars().count())
        .sum();
    if unquoted > GROUP_POST_MAX_CHARS || trimmed.chars().count() > GROUP_POST_MAX_QUOTED_CHARS {
        return Err(
            "Too long for one post. Put long material in a file shared with present.share (path, to: the group) and post the gist.",
        );
    }
    let stripped = strip_leading_mentions(trimmed).trim_end();
    if stripped.chars().count() <= 80 && !stripped.contains('?') {
        let lower = stripped.to_ascii_lowercase();
        for prefix in ACK_PREFIXES {
            if let Some(after) = lower.strip_prefix(prefix)
                && after
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
            {
                return Err(
                    "That only acknowledges. React to the post instead: reaction \"check\" with in_reply_to.",
                );
            }
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for GroupThread {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "group_thread".into(),
            description: "Group threads: several agents and the human taking turns like colleagues. A post wakes whom it names (mention or name in text); one naming nobody goes to the lead, then to all with any replies. An agent's first post after the human's reaches the other agents mid-turn; later ones reach only whom they name. Mentioning `human` pings the user's phone. Actions: create {title, members, lead?}, list, read {thread_id, after_sequence?, limit?}, post {thread_id, body, mentions?, in_reply_to?, choices?, reaction? (with in_reply_to, no body)}, set_lead {thread_id, agent}, add_member {thread_id, agent}. An aside to the human is a normal DM whose body starts 'From the <title> group: '.".into(),
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
                    "choices": {"type": "array", "items": {"type": "string"}},
                    "reaction": {"type": "string"},
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
                    .map(bare_address)
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
                let groups: Vec<_> = if agent_id == crate::lead::SESSION_ID {
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
                let group = require_group(&db, thread_id)?;
                require_group_member(&group, &agent_id)?;
                let in_reply_to = arguments
                    .get("in_reply_to")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                if let Some(reaction) = arguments
                    .get("reaction")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                {
                    if arguments.get("choices").is_some_and(|v| !v.is_null()) {
                        return Err(ToolError::msg("a reaction cannot include `choices`"));
                    }
                    let target = in_reply_to
                        .ok_or_else(|| ToolError::msg("a reaction needs `in_reply_to`"))?;
                    let event = db
                        .post_group_reaction(
                            thread_id,
                            ("agent", agent_id.as_str()),
                            target,
                            reaction,
                            &uuid::Uuid::new_v4().to_string(),
                            &now_rfc3339(),
                        )
                        .map_err(|e| ToolError::msg(format!("react in group: {e}")))?;
                    return Ok(ToolResult::success(json!(event)));
                }
                let body = arguments
                    .get("body")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .ok_or_else(|| ToolError::msg("`body` is required"))?;
                check_group_post_chatter(body).map_err(ToolError::msg)?;
                let choices = parse_group_choices(&arguments)?;
                let mentions = string_list(&arguments, "mentions");
                let extra = choices.map(|c| json!({ "choices": c }));
                let event = db
                    .post_group_message_ex(
                        thread_id,
                        ("agent", agent_id.as_str()),
                        body,
                        &mentions,
                        in_reply_to,
                        &uuid::Uuid::new_v4().to_string(),
                        &now_rfc3339(),
                        extra,
                    )
                    .map_err(|e| ToolError::msg(format!("post to group: {e}")))?;
                crate::session::emit_device_event(json!({
                    "kind": "coordination_event",
                    "event": event.clone(),
                }));
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
                    .map(bare_address)
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
                    .map(bare_address)
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

/// A freshly presented artifact is offered to the desk managing its session,
/// which decides whether and how the user sees it.
pub(crate) fn mirror_origin_artifact(
    ctx: &ToolContext,
    session: &str,
    artifact: &crate::artifacts::Artifact,
) {
    let Ok(db) = db(ctx) else { return };
    mirror_artifact_to_desk(&db, session, artifact);
}

/// An agent-managed session's artifact is offered to its desk, which shares it
/// with the user itself.
fn mirror_artifact_to_desk(db: &Store, session: &str, artifact: &crate::artifacts::Artifact) {
    let Some(card) = artifact_payload(session, artifact) else {
        return;
    };
    let version = artifact.latest().map(|v| v.n).unwrap_or_default();
    let label = crate::coordination::origin::session_label(db, session);
    let body = crate::envelope::Envelope::new("session_artifact")
        .attr("session", session)
        .attr("title", &label)
        .attr("artifact_id", &artifact.id)
        .attr("version", version.to_string())
        .child("artifact_title", &artifact.title)
        .child(
            "note",
            format!(
                "a session you manage presented this. present.share with artifact_id `{}`, session `{session}` and to `human` shares it with the user as a card.",
                artifact.id
            ),
        )
        .render();
    let mut extra = serde_json::Map::new();
    extra.insert("kind".into(), json!("status"));
    extra.insert("auto".into(), json!("session_artifact"));
    extra.insert("artifact".into(), card);
    crate::coordination::origin::tell_managing_desk(
        db,
        session,
        &format!("desk-artifact:{session}:{}:{version}", artifact.id),
        &body,
        extra,
    );
}

/// The session an artifact is read from: the caller's own, or one it manages.
fn artifact_source(ctx: &ToolContext, own: &str, named: Option<&str>) -> Result<String, ToolError> {
    let Some(named) = named.map(str::trim).filter(|s| !s.is_empty() && *s != own) else {
        return Ok(own.to_string());
    };
    let db = db(ctx)?;
    let manager = crate::coordination::origin::managing_desk(&db, named);
    match (manager, ctx.agent_id()) {
        (Some(desk), Some(me)) if desk == me => Ok(named.to_string()),
        _ => Err(ToolError::msg(format!(
            "session `{named}` is not one you manage; share only your own artifacts or a managed session's"
        ))),
    }
}

/// Record the session an event was posted from, so the room's wake-up skips it.
const SHARE_ARTIFACT_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// The `payload.artifact` object `share_artifact` builds and origin mirrors
/// post: the latest version's signed link, valid for
/// [`SHARE_ARTIFACT_TTL_SECS`].
pub(crate) fn artifact_payload(
    session: &str,
    artifact: &crate::artifacts::Artifact,
) -> Option<Value> {
    let version = artifact.latest()?;
    let secret = artifact_signing_secret();
    let url = crate::artifacts::signed_path(
        &secret,
        session,
        &artifact.id,
        version,
        SHARE_ARTIFACT_TTL_SECS,
    );
    Some(json!({
        "session": session,
        "id": artifact.id,
        "title": artifact.title,
        "version": version.n,
        "url": url,
        "kind": artifact.kind,
    }))
}

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
    #[serde(default)]
    artifact_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    title: Option<String>,
    to: String,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    session: Option<String>,
}

/// A `canvas:<id>` (optionally `/<frame>[/<el>][@rev]`) reference: its canvas id
/// and the link to paste.
fn canvas_share_ref(raw: &str) -> Result<(String, String), ToolError> {
    let raw = raw.trim();
    if let Ok(link) = crate::canvas::links::parse_canvas_ref(raw) {
        return Ok((link.canvas_id, raw.to_string()));
    }
    let id = raw.trim_start_matches("canvas:");
    if crate::canvas::links::is_safe_component(id) {
        return Ok((id.to_string(), format!("canvas:{id}")));
    }
    Err(ToolError::msg(format!(
        "`{raw}` is not a canvas link; use canvas:<id> or canvas:<id>/<frame>"
    )))
}

/// Share a canvas by link: the post carries the ready-to-paste `canvas:` link
/// and a `canvas` card instead of a file artifact.
fn share_canvas(
    ctx: &ToolContext,
    session: &str,
    target: &str,
    canvas_id: &str,
    link: &str,
    title: Option<&str>,
    note: Option<&str>,
) -> Result<ToolResult, ToolError> {
    let label = title.unwrap_or(canvas_id);
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    let body = match note {
        Some(n) if n.contains(link) => n.to_string(),
        Some(n) => format!("{n}\n{link}"),
        None => format!("{label}\n{link}"),
    };
    let card = json!({"id": canvas_id, "title": label, "link": link});
    let db = db(ctx)?;
    let (sender_kind, sender_id) = actor(ctx)?;
    let to_user = target == "human" || matches!(target, "channel:chat" | "channel:push");
    if to_user && let Some(desk) = crate::coordination::origin::managing_desk(&db, session) {
        let mut extra = serde_json::Map::new();
        extra.insert("kind".into(), json!("status"));
        extra.insert("auto".into(), json!("session_canvas"));
        extra.insert("canvas".into(), card.clone());
        let envelope = crate::envelope::Envelope::new("session_canvas")
            .attr("session", session)
            .attr("canvas", canvas_id)
            .child("link", link)
            .child("note", format!("a session you manage shared this canvas. present.share path={link} to=human posts it in your chat."))
            .render();
        crate::coordination::origin::tell_managing_desk(
            &db,
            session,
            &format!("desk-canvas:{session}:{link}"),
            &envelope,
            extra,
        );
        return Ok(ToolResult::success(json!({
            "to": "desk",
            "desk": desk,
            "canvas": card,
            "link": link,
            "note": "your desk shares it with the user in its chat",
        })));
    }
    let about_task = session_task(ctx, session).map(|(id, _)| id);
    let mut extra = crate::coordination::typed::payload_fields(
        Some("result"),
        crate::coordination::typed::Urgency::Normal,
        about_task.as_deref(),
    );
    extra.insert("canvas".into(), card.clone());
    let saved = deliver_share(
        ctx,
        &db,
        session,
        target,
        (sender_kind, sender_id.as_str()),
        &body,
        about_task.as_deref(),
        extra,
        "canvas",
        &card,
    )?;
    Ok(ToolResult::success(json!({
        "event_id": saved.event_id,
        "thread_id": saved.thread_id,
        "to": target,
        "canvas": card,
        "link": link,
    })))
}

/// Post a share to its target: a group, a channel, the user or an agent.
#[allow(clippy::too_many_arguments)]
fn deliver_share(
    ctx: &ToolContext,
    db: &Store,
    session: &str,
    target: &str,
    sender: (&str, &str),
    body: &str,
    about_task: Option<&str>,
    mut extra: serde_json::Map<String, Value>,
    card_key: &str,
    card: &Value,
) -> Result<CoordinationEvent, ToolError> {
    let (sender_kind, sender_id) = sender;
    let sender_id = sender_id.to_string();
    let body = body.to_string();
    let about_task = about_task.map(str::to_string);
    let mut group_payload = serde_json::Map::new();
    group_payload.insert(card_key.to_string(), card.clone());
    Ok(if target.starts_with("group:") {
        let now = chrono::Utc::now().to_rfc3339();
        let ev = db
            .post_group_message_ex(
                target,
                (sender_kind, sender_id.as_str()),
                &body,
                &[],
                None,
                &Uuid::new_v4().to_string(),
                &now,
                Some(Value::Object(group_payload)),
            )
            .map_err(|e| {
                ToolError::msg(format!(
                    "share artifact to group: {e}; `to` must name a group you belong to"
                ))
            })?;
        crate::session::emit_device_event(json!({
            "kind": "coordination_event",
            "event": ev.clone(),
        }));
        ev
    } else if let Some(raw_ch) = target.strip_prefix("channel:") {
        crate::coordination::reach::deliver_channel(
            db,
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
        if sender_kind == "agent" && sender_id != crate::lead::SESSION_ID {
            crate::coordination::reach::apply_outbound_human(
                db, &sender_id, &body, &mut extra,
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
    })
}

pub struct ShareArtifactTool;

#[async_trait]
impl Tool for ShareArtifactTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "share_artifact".into(),
            description: "Share an artifact (`artifact_id`), file (`path`, optional `title`) or canvas (`path` = `canvas:<id>`) with `human`, agent id, `group:<id>`, or `channel:<id>` (`channel:chat`/`channel:push`).".into(),
            input_schema: schema(
                json!({
                    "artifact_id": {"type": "string"},
                    "path": {"type": "string"},
                    "title": {"type": "string"},
                    "to": {"type": "string", "description": "`human`, agent id, `group:<id>`, or `channel:<id>`"},
                    "note": {"type": "string"},
                    "session": {"type": "string"}
                }),
                &["to"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ShareArtifactArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let artifact_id = args
            .artifact_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let path = args
            .path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let title = args
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let target = bare_address(&args.to);
        if target.is_empty() {
            return Err(ToolError::msg("to must not be empty"));
        }
        let session = ctx
            .durable_session_id()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::msg("present.share requires a durable session"))?;
        if let Some(raw) = path.or(artifact_id).filter(|r| r.starts_with("canvas:")) {
            if artifact_id.is_some() && path.is_some() {
                return Err(ToolError::msg(
                    "provide either `artifact_id` or `path`, not both",
                ));
            }
            let (canvas_id, link) = canvas_share_ref(raw)?;
            return share_canvas(ctx, session, target, &canvas_id, &link, title, args.note.as_deref());
        }
        let mut shared_from: Option<String> = None;
        let artifact = match (artifact_id, path) {
            (Some(_), Some(_)) => {
                return Err(ToolError::msg(
                    "provide either `artifact_id` or `path`, not both",
                ));
            }
            (None, None) => {
                return Err(ToolError::msg("provide `artifact_id` or `path`"));
            }
            (Some(id), None) => {
                let source = artifact_source(ctx, session, args.session.as_deref())?;
                let artifact = crate::artifacts::get(&source, id).ok_or_else(|| {
                    ToolError::msg(format!("unknown artifact `{id}` in session `{source}`"))
                })?;
                if source != session {
                    shared_from = Some(source);
                }
                artifact
            }
            (None, Some(raw_path)) => {
                let resolved = if std::path::Path::new(raw_path).is_absolute() {
                    std::path::PathBuf::from(raw_path)
                } else {
                    ctx.workspace_root().join(raw_path)
                };
                if !resolved.exists() {
                    return Err(ToolError::msg(format!("file `{raw_path}` does not exist")));
                }
                crate::artifacts::present(session, &resolved, title, None, None)
                    .map_err(ToolError::msg)?
                    .artifact
            }
        };
        let artifact_payload =
            artifact_payload(shared_from.as_deref().unwrap_or(session), &artifact).ok_or_else(
                || ToolError::msg(format!("artifact `{}` has no versions", artifact.id)),
            )?;
        let body = args
            .note
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&artifact.title)
            .to_string();

        let db = db(ctx)?;
        let (sender_kind, sender_id) = actor(ctx)?;
        let to_user = target == "human" || matches!(target, "channel:chat" | "channel:push");
        if to_user && crate::coordination::origin::managing_desk(&db, session).is_some() {
            mirror_artifact_to_desk(&db, session, &artifact);
            return Ok(ToolResult::success(json!({
                "to": "desk",
                "artifact": artifact_payload,
                "note": "your desk shares it with the user in its chat",
            })));
        }
        let about_task = session_task(ctx, session).map(|(id, _)| id);
        let mut extra = crate::coordination::typed::payload_fields(
            Some("result"),
            crate::coordination::typed::Urgency::Normal,
            about_task.as_deref(),
        );
        extra.insert("artifact".into(), artifact_payload.clone());

        let saved = deliver_share(
            ctx,
            &db,
            session,
            target,
            (sender_kind, sender_id.as_str()),
            &body,
            about_task.as_deref(),
            extra,
            "artifact",
            &artifact_payload,
        )?;

        Ok(ToolResult::success(json!({
            "event_id": saved.event_id,
            "thread_id": saved.thread_id,
            "to": target,
            "artifact": artifact_payload,
        })))
    }
}

#[derive(Deserialize)]
struct ChatsArgs {
    #[serde(default)]
    kind: Option<String>,
}

/// The kind of chat thread an id names: a `direct:` thread, a group thread or
/// a task room. `None` for anything else, the shared board room included.
fn chat_thread_kind(db: &Store, thread_id: &str) -> Option<&'static str> {
    if thread_id.starts_with("direct:") {
        return Some("direct");
    }
    if db.get_group(thread_id).ok().flatten().is_some() {
        return Some("group");
    }
    if db.get_task_by_thread(thread_id).ok().flatten().is_some() {
        return Some("task");
    }
    None
}

/// The `last` preview on a `chats` row: who wrote the newest event and the
/// start of its body, plus the event's sequence and time for sorting.
fn chat_preview(db: &Store, thread_id: &str) -> (u64, Option<String>, Option<Value>) {
    let event = db
        .events_before(thread_id, i64::MAX as u64, 1)
        .ok()
        .and_then(|mut events| events.pop());
    match event {
        Some(event) => {
            let body: String = event
                .payload
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .chars()
                .take(160)
                .collect();
            (
                event.sequence,
                Some(event.created_at.clone()),
                Some(json!({
                    "from": format!("{}:{}", event.actor_kind, event.actor_id),
                    "body": body,
                })),
            )
        }
        None => (0, None, None),
    }
}

/// Every chat thread the calling actor belongs to: direct conversations
/// (the human's included), group threads it is a member of, and the rooms of
/// open tasks it is seated on. Lead sees every group and open task.
pub struct Chats;
#[async_trait]
impl Tool for Chats {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "chats".into(),
            description: "List your chat threads, most recent first: direct conversations (the human included), your groups, and the rooms of open tasks you sit on. Rows carry thread_id, kind, title, last_sequence, last_at and a `last` preview; direct rows add `peer` and `unread`. Open one with read_chat.".into(),
            input_schema: schema(
                json!({"kind": {"type": "string", "enum": ["direct", "group", "task"]}}),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ChatsArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let only = match args
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            None => None,
            Some(kind @ ("direct" | "group" | "task")) => Some(kind),
            Some(other) => {
                return Err(ToolError::msg(format!(
                    "unknown kind `{other}`; use direct, group or task"
                )));
            }
        };
        let (actor_kind, actor_id) = actor(ctx)?;
        let db = db(ctx)?;
        let lead = ctx.is_lead() || actor_id == crate::lead::SESSION_ID;
        let mut rows: Vec<(String, Value)> = Vec::new();

        if only.is_none() || only == Some("direct") {
            for thread in db
                .list_direct_threads(actor_kind, &actor_id)
                .map_err(|e| ToolError::msg(format!("list direct threads: {e}")))?
            {
                let (sequence, at, last) = chat_preview(&db, &thread.thread_id);
                let last_at = at.unwrap_or_else(|| thread.created_at.clone());
                let last_sequence = if sequence > 0 {
                    sequence
                } else {
                    thread.last_sequence.max(0) as u64
                };
                rows.push((
                    last_at.clone(),
                    json!({
                        "thread_id": thread.thread_id,
                        "kind": "direct",
                        "title": thread.peer_id,
                        "peer": format!("{}:{}", thread.peer_kind, thread.peer_id),
                        "unread": thread.unread,
                        "last_sequence": last_sequence,
                        "last_at": last_at,
                        "last": last,
                    }),
                ));
            }
        }

        if only.is_none() || only == Some("group") {
            for group in db
                .list_groups()
                .map_err(|e| ToolError::msg(format!("list groups: {e}")))?
            {
                if !lead
                    && !group
                        .members
                        .iter()
                        .any(|m| m.kind == "agent" && m.id == actor_id)
                {
                    continue;
                }
                let (sequence, at, last) = chat_preview(&db, &group.id);
                let last_at = at.unwrap_or_else(|| group.created_at.clone());
                rows.push((
                    last_at.clone(),
                    json!({
                        "thread_id": group.id,
                        "kind": "group",
                        "title": group.title,
                        "last_sequence": sequence,
                        "last_at": last_at,
                        "last": last,
                    }),
                ));
            }
        }

        if only.is_none() || only == Some("task") {
            let tasks = db
                .list_open_tasks((!lead).then_some(actor_id.as_str()))
                .map_err(|e| ToolError::msg(format!("list tasks: {e}")))?;
            for task in tasks {
                let (sequence, at, last) = chat_preview(&db, &task.thread_id);
                let last_at = at.unwrap_or_else(|| task.created_at.clone());
                rows.push((
                    last_at.clone(),
                    json!({
                        "thread_id": task.thread_id,
                        "kind": "task",
                        "title": task.title,
                        "last_sequence": sequence,
                        "last_at": last_at,
                        "last": last,
                    }),
                ));
            }
        }

        rows.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(ToolResult::success(json!({
            "actor": format!("{actor_kind}:{actor_id}"),
            "chats": rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
        })))
    }
}

#[derive(Deserialize)]
struct ReadChatArgs {
    thread_id: String,
    #[serde(default)]
    after_sequence: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// Read any chat thread — the general form of `read_agent_thread` and
/// `group_thread read`.
pub struct ReadChat;
#[async_trait]
impl Tool for ReadChat {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "read_chat".into(),
            description: "Read one chat thread (group, task room or direct), oldest first, by `thread_id`; read_agent_thread reads a direct thread by peer. Omit after_sequence for the most recent messages; pass it to page forward.".into(),
            input_schema: schema(
                json!({
                    "thread_id": {"type": "string"},
                    "after_sequence": {"type": "integer", "minimum": 0},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100}
                }),
                &["thread_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, arguments: Value) -> Result<ToolResult, ToolError> {
        let args: ReadChatArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let thread_id = args.thread_id.trim().to_string();
        if thread_id.is_empty() {
            return Err(ToolError::msg("thread_id must not be empty"));
        }
        let (actor_kind, actor_id) = actor(ctx)?;
        let db = db(ctx)?;
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        require_thread_access(&db, ctx, (actor_kind, actor_id.as_str()), &thread_id)?;
        let mut page = thread_page(&db, &thread_id, args.after_sequence, limit, true)?;
        page["thread_kind"] = json!(chat_thread_kind(&db, &thread_id).unwrap_or("room"));
        Ok(ToolResult::success(page))
    }
}

pub(crate) fn stamp_origin(ctx: &ToolContext, mut payload: Value) -> Value {
    if let Some(session) = ctx.durable_session_id() {
        payload["origin_session"] = json!(session);
    }
    payload
}

pub fn add_desk_event_tools(registry: &mut ToolRegistry) {
    registry.insert(DeskEventList);
    registry.insert(DeskEventAck);
    registry.insert(DeskEventNoop);
    registry.insert(DeskEventDefer);
    registry.insert(DeskEventCreate);
    registry.insert(DeskEventHidden);
    registry.insert(DeskEventRelease);
    registry.insert(GroupSnooze);
    registry.insert(DeskParallelismTool);
    registry.insert(SlotClaimTool);
    registry.insert(DeskGuideTool);
}

/// Event ids from a comma-separated string or an array of strings.
pub(crate) fn parse_event_ids(value: &Value) -> Vec<String> {
    let mut ids: Vec<String> = match value {
        Value::String(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .flat_map(|s| s.split(','))
            .map(|p| p.trim().to_string())
            .collect(),
        _ => Vec::new(),
    };
    ids.retain(|id| !id.is_empty());
    ids.dedup();
    ids
}

fn desk_agent(ctx: &ToolContext) -> Result<String, ToolError> {
    let session = ctx
        .durable_session_id()
        .filter(|s| crate::session::is_inbox_session_id(s))
        .ok_or_else(|| ToolError::msg("desk events belong to a desk (inbox) session"))?;
    Ok(ctx
        .agent_id()
        .map(str::to_string)
        .unwrap_or_else(|| session.trim_start_matches("inbox-").to_string()))
}

fn event_json(e: &crate::coordination::desk_inbox::DeskEvent) -> Value {
    json!({
        "id": e.id,
        "kind": e.kind,
        "priority": e.priority,
        "mention": e.is_mention,
        "thread_id": e.thread_id,
        "reply_thread": e.reply_thread,
        "deadline_at": e.deadline_at,
        "summary": e.summary,
        "created_at": e.created_at,
        "updated_at": e.updated_at,
        "presented": e.presented_at.is_some(),
    })
}

fn deadline_from(args: &Value, required: bool) -> Result<Option<String>, ToolError> {
    if let Some(at) = args
        .get("deadline")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return crate::coordination::desk_inbox::normalize_ts(at)
            .map(Some)
            .map_err(|e| ToolError::msg(e.to_string()));
    }
    let seconds = match (
        args.get("in_seconds").and_then(Value::as_i64),
        args.get("in_minutes").and_then(Value::as_i64),
    ) {
        (Some(s), _) => Some(s),
        (None, Some(m)) => Some(m.saturating_mul(60)),
        (None, None) => None,
    };
    match seconds {
        Some(s) if s < 1 => Err(ToolError::msg("in_seconds / in_minutes must be positive")),
        Some(s) => Ok(Some(
            (chrono::Utc::now() + chrono::Duration::seconds(s.min(366 * 86_400)))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )),
        None if required => Err(ToolError::msg(
            "give deadline (RFC 3339), in_minutes or in_seconds",
        )),
        None => Ok(None),
    }
}

fn ids_schema() -> Value {
    json!({
        "type": "string",
        "description": "event ids, comma-separated (an array of ids also works)"
    })
}

fn deadline_props(mut props: Value) -> Value {
    props["deadline"] = json!({"type": "string", "description": "RFC 3339 time"});
    props["in_minutes"] = json!({"type": "integer", "minimum": 1});
    props["in_seconds"] = json!({"type": "integer", "minimum": 1});
    props
}

pub struct DeskEventList;
#[async_trait]
impl Tool for DeskEventList {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_list".into(),
            description: "Your open desk events, most pressing first: mentions, then priority, then age.".into(),
            input_schema: schema(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 100}}), &[]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let agent = desk_agent(ctx)?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(30).clamp(1, 100) as u32;
        let events = db(ctx)?
            .list_open_desk_events(&agent, limit)
            .map_err(|e| ToolError::msg(format!("list events: {e}")))?;
        Ok(ToolResult::success(json!({
            "events": events.iter().map(event_json).collect::<Vec<_>>(),
        })))
    }
}

async fn settle_events(ctx: &ToolContext, args: &Value, resolution: &str) -> Result<ToolResult, ToolError> {
    let agent = desk_agent(ctx)?;
    let ids = parse_event_ids(args.get("ids").or_else(|| args.get("id")).unwrap_or(&Value::Null));
    if ids.is_empty() {
        return Err(ToolError::msg("ids is required (comma-separated or an array)"));
    }
    let db = db(ctx)?;
    let now = now_rfc3339();
    let settled = if resolution == "ack" {
        db.ack_desk_events(&agent, &ids, &now)
    } else {
        db.resolve_desk_events(&agent, &ids, resolution, &now)
    }
    .map_err(|e| ToolError::msg(format!("{resolution} events: {e}")))?;
    let done: Vec<&str> = settled.iter().map(|e| e.id.as_str()).collect();
    let unknown: Vec<&String> = ids.iter().filter(|id| !done.contains(&id.as_str())).collect();
    Ok(ToolResult::success(json!({
        "resolution": resolution,
        "settled": done,
        "not_open": unknown,
    })))
}

pub struct DeskEventAck;
#[async_trait]
impl Tool for DeskEventAck {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_ack".into(),
            description: "Mark desk events handled. re=<id> on any other call does the same once that call succeeds.".into(),
            input_schema: schema(json!({"ids": ids_schema()}), &["ids"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        settle_events(ctx, &args, "ack").await
    }
}

pub struct DeskEventNoop;
#[async_trait]
impl Tool for DeskEventNoop {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_noop".into(),
            description: "Close desk events that need nothing from you. A noop on an approval or question still lets the daemon's fallback notice reach the user.".into(),
            input_schema: schema(json!({"ids": ids_schema()}), &["ids"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        settle_events(ctx, &args, "noop").await
    }
}

pub struct DeskEventDefer;
#[async_trait]
impl Tool for DeskEventDefer {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_defer".into(),
            description: "Push a desk event back: it stays open and is presented again at the deadline.".into(),
            input_schema: schema(deadline_props(json!({"id": {"type": "string"}})), &["id"]),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let agent = desk_agent(ctx)?;
        let id = args
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::msg("id is required"))?;
        let deadline = deadline_from(&args, true)?.unwrap_or_default();
        let event = db(ctx)?
            .defer_desk_event(&agent, id, &deadline, &now_rfc3339())
            .map_err(|e| ToolError::msg(format!("defer event: {e}")))?
            .ok_or_else(|| ToolError::msg(format!("no open event `{id}`")))?;
        Ok(ToolResult::success(json!({"event": event_json(&event)})))
    }
}

pub struct DeskEventCreate;
#[async_trait]
impl Tool for DeskEventCreate {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_create".into(),
            description: "Open a desk event for yourself: a commitment or follow-up that is presented back to you at its deadline and stays open until you ack it.".into(),
            input_schema: schema(
                deadline_props(json!({
                    "summary": {"type": "string"},
                    "thread_id": {"type": "string", "description": "the chat it concerns, where the answer goes"},
                    "priority": {"type": "integer", "minimum": 0, "maximum": 100}
                })),
                &["summary"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let agent = desk_agent(ctx)?;
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::msg("summary is required"))?;
        let deadline = deadline_from(&args, false)?;
        let thread = args
            .get("thread_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let priority = args.get("priority").and_then(Value::as_i64).unwrap_or(0).clamp(0, 100);
        let event = db(ctx)?
            .create_self_desk_event(&agent, summary, deadline.as_deref(), thread, priority, &now_rfc3339())
            .map_err(|e| ToolError::msg(format!("create event: {e}")))?;
        Ok(ToolResult::success(json!({"event": event_json(&event)})))
    }
}

const HIDDEN_EVENT_PRIORITY: i64 = 20;
const ROUND_WAKE_PRIORITY: i64 = 40;

/// A group the caller belongs to (Lead and the group's lead always do).
fn member_group(
    db: &Store,
    args: &Value,
    caller: &str,
) -> Result<crate::coordination::GroupSummary, ToolError> {
    let thread_id = args
        .get("thread_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::msg("thread_id is required"))?;
    let group = require_group(db, thread_id)?;
    if group.lead.as_deref() != Some(caller) {
        require_group_member(&group, caller)?;
    }
    Ok(group)
}

/// A group member that can take desk events: an agent with a desk, not Lead.
fn desk_member(
    group: &crate::coordination::GroupSummary,
    raw: &str,
) -> Result<String, ToolError> {
    let id = bare_address(raw).to_string();
    if id == crate::lead::SESSION_ID {
        return Err(ToolError::msg("Lead has no desk inbox; reach it with message.send"));
    }
    if !group.members.iter().any(|m| m.kind == "agent" && m.id == id) {
        return Err(ToolError::msg(format!("`{id}` is not an agent member of `{}`", group.id)));
    }
    Ok(id)
}

fn clipped_title(title: &str) -> String {
    let title: String = title.chars().take(40).collect();
    title
}

pub struct DeskEventHidden;
#[async_trait]
impl Tool for DeskEventHidden {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_hidden".into(),
            description: "Hand a group member a private desk event about that group: it reaches only their desk inbox, never the thread, and wakes them. Use it to steer a peer without posting.".into(),
            input_schema: schema(
                deadline_props(json!({
                    "thread_id": {"type": "string", "description": "the group it concerns"},
                    "to": {"type": "string", "description": "the agent member it is for"},
                    "summary": {"type": "string"},
                    "priority": {"type": "integer", "minimum": 0, "maximum": 100}
                })),
                &["thread_id", "to", "summary"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let caller = group_agent(ctx)?;
        let db = db(ctx)?;
        let group = member_group(&db, &args, &caller)?;
        let to = args
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::msg("to is required"))?;
        let to = desk_member(&group, to)?;
        if to == caller {
            return Err(ToolError::msg("that is you; event.create opens an event for yourself"));
        }
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::msg("summary is required"))?;
        let deadline = deadline_from(&args, false)?;
        let priority = args
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(HIDDEN_EVENT_PRIORITY)
            .clamp(0, 100);
        let event = db
            .enqueue_desk_event(
                &crate::coordination::desk_inbox::NewDeskEvent {
                    agent_id: to.clone(),
                    kind: "hidden".into(),
                    thread_id: Some(group.id.clone()),
                    reply_thread: Some(group.id.clone()),
                    priority,
                    is_mention: true,
                    summary: format!("from {caller} about \"{}\": {summary}", clipped_title(&group.title)),
                    payload: Some(json!({"from": caller})),
                    deadline_at: deadline,
                    wake: true,
                    ..Default::default()
                },
                &now_rfc3339(),
            )
            .map_err(|e| ToolError::msg(format!("hidden event: {e}")))?;
        Ok(ToolResult::success(json!({"to": to, "event": event_json(&event)})))
    }
}

pub struct DeskEventRelease;
#[async_trait]
impl Tool for DeskEventRelease {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_event_release".into(),
            description: "Release the group round you lead, with your decision. wake_agents are woken to engage; with a decision and nobody named, the other unsnoozed members are woken with it, else they get it as a quiet event. Stops the round's owed-reply nudge.".into(),
            input_schema: schema(
                json!({
                    "thread_id": {"type": "string"},
                    "wake_agents": {"type": "string", "description": "agent members to wake, comma-separated (an array also works)"},
                    "decision": {"type": "string", "description": "what you decided, passed to the members"}
                }),
                &["thread_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let caller = group_agent(ctx)?;
        let db = db(ctx)?;
        let group = member_group(&db, &args, &caller)?;
        let wake: Vec<String> = parse_event_ids(args.get("wake_agents").unwrap_or(&Value::Null))
            .iter()
            .map(|raw| desk_member(&group, raw))
            .collect::<Result<_, _>>()?;
        let decision = args
            .get("decision")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let now = now_rfc3339();
        let Some(round) = db
            .release_group_round(&group.id, &caller, decision, &now)
            .map_err(|e| ToolError::msg(format!("release round: {e}")))?
        else {
            let open = db.open_group_round(&group.id).ok().flatten();
            return Err(ToolError::msg(match open {
                Some(round) => format!("this round is led by {}; only its lead releases it", round.lead_agent),
                None => "there is no open round in this group to release".to_string(),
            }));
        };
        let title = clipped_title(&group.title);
        let said = decision.map(|d| format!(": {d}")).unwrap_or_default();
        let enqueue = |agent: &str, summary: String, priority: i64, mention: bool, wake: bool| {
            db.enqueue_desk_event(
                &crate::coordination::desk_inbox::NewDeskEvent {
                    agent_id: agent.to_string(),
                    kind: "round".into(),
                    thread_id: Some(group.id.clone()),
                    reply_thread: Some(group.id.clone()),
                    priority,
                    is_mention: mention,
                    summary,
                    payload: Some(json!({
                        "round_id": round.round_id,
                        "from": caller,
                        "decision": decision,
                    })),
                    wake,
                    ..Default::default()
                },
                &now,
            )
            .map(|e| e.id)
            .map_err(|e| ToolError::msg(format!("round event for {agent}: {e}")))
        };
        let mut woken = Vec::new();
        for agent in wake.iter().filter(|a| **a != caller) {
            enqueue(
                agent,
                format!("{caller} released the round in \"{title}\" and asks you to engage{said}"),
                ROUND_WAKE_PRIORITY,
                true,
                true,
            )?;
            woken.push(agent.clone());
        }
        let mut told = Vec::new();
        if decision.is_some() {
            for member in group.members.iter().filter(|m| m.kind == "agent") {
                let id = member.id.as_str();
                if id == caller
                    || id == crate::lead::SESSION_ID
                    || wake.iter().any(|w| w == id)
                    || db.is_group_snoozed(id, &group.id, &now).unwrap_or(false)
                {
                    continue;
                }
                enqueue(
                    id,
                    format!("{caller} released the round in \"{title}\"{said}. Engage only if you add something, else event.noop."),
                    0,
                    false,
                    wake.is_empty(),
                )?;
                told.push(id.to_string());
            }
        }
        Ok(ToolResult::success(json!({
            "round_id": round.round_id,
            "decision": round.lead_decision,
            "woken": woken,
            "told": told,
        })))
    }
}

pub struct GroupSnooze;
#[async_trait]
impl Tool for GroupSnooze {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "group_snooze".into(),
            description: "Stop optional wakes from a group: in_minutes, in_seconds or until (RFC 3339) for a timed snooze, none for until someone @mentions you, off: true to end it. An explicit mention always reaches you.".into(),
            input_schema: schema(
                json!({
                    "thread_id": {"type": "string"},
                    "in_minutes": {"type": "integer", "minimum": 1},
                    "in_seconds": {"type": "integer", "minimum": 1},
                    "until": {"type": "string", "description": "RFC 3339 time"},
                    "off": {"type": "boolean"}
                }),
                &["thread_id"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let caller = group_agent(ctx)?;
        let db = db(ctx)?;
        let group = member_group(&db, &args, &caller)?;
        if args.get("off").and_then(Value::as_bool).unwrap_or(false) {
            let cleared = db
                .clear_group_snooze(&caller, &group.id)
                .map_err(|e| ToolError::msg(format!("clear snooze: {e}")))?;
            return Ok(ToolResult::success(json!({
                "thread_id": group.id,
                "snoozed": false,
                "was_snoozed": cleared,
            })));
        }
        let until = match args
            .get("until")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            Some(at) => Some(
                crate::coordination::desk_inbox::normalize_ts(at)
                    .map_err(|e| ToolError::msg(e.to_string()))?,
            ),
            None => deadline_from(&args, false)?,
        };
        let now = now_rfc3339();
        if let Some(at) = until.as_deref()
            && at <= crate::coordination::desk_inbox::normalize_ts(&now)
                .map_err(|e| ToolError::msg(e.to_string()))?
                .as_str()
        {
            return Err(ToolError::msg("until must be in the future"));
        }
        let snooze = db
            .set_group_snooze(&caller, &group.id, until.as_deref(), &now)
            .map_err(|e| ToolError::msg(format!("snooze: {e}")))?;
        Ok(ToolResult::success(json!({
            "thread_id": group.id,
            "snoozed": true,
            "until": snooze.until_at.as_deref().unwrap_or("pinged"),
        })))
    }
}

/// Fallback when no config names the work model's window: the full 6 slots.
const SLOT_FALLBACK_WINDOW: u64 = 200_000;

/// The profile an agent's work sessions run on and its effective window.
pub(crate) fn agent_slot_window(db: &Store, agent_id: &str) -> (Option<String>, u64) {
    let config = std::fs::read_to_string(crate::config::default_config_path())
        .ok()
        .and_then(|raw| toml::from_str::<crate::config::SnippetConfig>(&raw).ok());
    let profile = db
        .agent_profile(agent_id)
        .ok()
        .flatten()
        .or_else(|| config.as_ref().and_then(|c| c.active_setup.clone()));
    let window = profile_slot_window(config.as_ref(), db, agent_id, profile.as_deref());
    (profile, window)
}

/// The effective window an agent gets on `profile`, after its context limit.
pub(crate) fn profile_slot_window(
    config: Option<&crate::config::SnippetConfig>,
    db: &Store,
    agent_id: &str,
    profile: Option<&str>,
) -> u64 {
    let window = config
        .map(|c| {
            profile
                .and_then(|p| c.setups.as_ref()?.get(p))
                .unwrap_or(&c.model)
                .context_window
        })
        .filter(|w| *w > 0)
        .unwrap_or(SLOT_FALLBACK_WINDOW);
    let limit = db.agent_context_limit(agent_id).ok().flatten();
    crate::agent_context::effective_context_window(window, limit)
}

/// `used/cap` for an agent: live work sessions plus held grants, against its cap.
pub(crate) fn slot_usage(db: &Store, agent_id: &str, now: &str) -> Result<(u32, u32), ToolError> {
    let (_, window) = agent_slot_window(db, agent_id);
    let cap = db
        .get_desk_parallelism(agent_id, window)
        .map_err(|e| ToolError::msg(format!("read parallelism: {e}")))?
        .max_parallel;
    let active = db
        .count_active_slots(agent_id)
        .map_err(|e| ToolError::msg(format!("count slots: {e}")))?;
    let reserved = db
        .reserved_slots(agent_id, now)
        .map_err(|e| ToolError::msg(format!("count held slots: {e}")))?;
    Ok((active + reserved, cap))
}

/// Grant waiting slot claims wherever a slot has come free.
pub(crate) fn grant_free_slots(db: &Store, now: &str) {
    let Ok(agents) = db.agents_with_waiting_slot_claims() else {
        return;
    };
    for agent in agents {
        let (_, window) = agent_slot_window(db, &agent);
        if let Err(error) = db.release_slot_and_grant_next(&agent, window, now) {
            eprintln!("[slots] grant for {agent}: {error}");
        }
    }
}

/// The `session_progress` row a work session or lane counts its rounds in.
pub(crate) fn progress_key(ctx: &ToolContext) -> Option<String> {
    let work = |s: &str| {
        !s.trim().is_empty()
            && !crate::session::is_inbox_session_id(s)
            && !crate::lead::is_session_id(s)
    };
    if let Some((session, lane)) = ctx.lane_origin() {
        return work(session).then(|| format!("{session}#{lane}"));
    }
    ctx.durable_session_id()
        .filter(|s| work(s))
        .map(str::to_string)
}

pub struct DeskParallelismTool;
#[async_trait]
impl Tool for DeskParallelismTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_parallelism".into(),
            description: "Show how many work sessions you run at once (slots used/cap). max_parallel (1 to 6) pins the cap; auto: true returns to the default scaled by the work model's context window.".into(),
            input_schema: schema(
                json!({
                    "max_parallel": {"type": "integer", "minimum": 1, "maximum": 6},
                    "auto": {"type": "boolean"}
                }),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let agent = desk_agent(ctx)?;
        let db = db(ctx)?;
        let now = now_rfc3339();
        let (profile, window) = agent_slot_window(&db, &agent);
        let auto = args.get("auto").and_then(Value::as_bool).unwrap_or(false);
        let max = args.get("max_parallel").and_then(Value::as_u64);
        if auto && max.is_some() {
            return Err(ToolError::msg("pass max_parallel or auto: true, not both"));
        }
        let set = if auto {
            Some((crate::coordination::slots::auto_parallel_cap(window), "auto"))
        } else {
            max.map(|m| (m.min(u32::MAX as u64) as u32, "manual"))
        };
        if let Some((cap, source)) = set {
            db.set_desk_parallelism(&agent, cap, source, profile.as_deref(), Some(window), &now)
                .map_err(|e| ToolError::msg(e.to_string()))?;
        }
        let p = db
            .get_desk_parallelism(&agent, window)
            .map_err(|e| ToolError::msg(e.to_string()))?;
        let (used, cap) = slot_usage(&db, &agent, &now)?;
        Ok(ToolResult::success(json!({
            "max_parallel": p.max_parallel,
            "source": p.source,
            "basis_profile": p.basis_profile.or(profile),
            "basis_window": p.basis_window,
            "slots": format!("{used}/{cap}"),
        })))
    }
}

#[derive(Deserialize)]
struct SlotClaimArgs {
    #[serde(default, rename = "for")]
    requested_for: Option<String>,
}

pub struct SlotClaimTool;
#[async_trait]
impl Tool for SlotClaimTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "slot_claim".into(),
            description: "Queue for a work-session slot when you are at your cap. A slot_free event arrives when one frees (first come, first served) and holds it 10 minutes for task.start.".into(),
            input_schema: schema(
                json!({"for": {"type": "string", "description": "what the slot is for"}}),
                &[],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: SlotClaimArgs =
            serde_json::from_value(args).map_err(|e| ToolError::msg(e.to_string()))?;
        let agent = desk_agent(ctx)?;
        let db = db(ctx)?;
        let now = now_rfc3339();
        let (used, cap) = slot_usage(&db, &agent, &now)?;
        if used < cap {
            return Ok(ToolResult::success(json!({
                "slots": format!("{used}/{cap}"),
                "free": true,
                "next": "a slot is free now: task.start",
            })));
        }
        let claim = db
            .request_slot_claim(&agent, args.requested_for.as_deref(), &now)
            .map_err(|e| ToolError::msg(e.to_string()))?;
        Ok(ToolResult::success(json!({
            "claim_id": claim.id,
            "status": claim.status,
            "slots": format!("{used}/{cap}"),
            "next": "wait for the slot_free event, then task.start",
        })))
    }
}

pub struct DeskGuideTool;
#[async_trait]
impl Tool for DeskGuideTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "desk_guide".into(),
            description: "The desk handbook, one topic at a time. Without topic it lists the topics.".into(),
            input_schema: schema(json!({"topic": {"type": "string"}}), &[]),
        }
    }

    async fn execute(&self, _ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let topics = crate::prompts::desk_guide_topics();
        let Some(topic) = args
            .get("topic")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            return Ok(ToolResult::success(json!({
                "topics": topics,
                "next": "desk.guide topic=<name>",
            })));
        };
        let section = crate::prompts::desk_guide_section(&topic.to_ascii_lowercase()).ok_or_else(|| {
            ToolError::msg(format!(
                "unknown topic `{topic}`; topics: {}",
                topics.join(", ")
            ))
        })?;
        Ok(ToolResult::success(json!({
            "topic": topic.to_ascii_lowercase(),
            "guide": section,
        })))
    }
}

#[derive(Deserialize)]
struct ProgressReportArgs {
    summary: String,
    #[serde(default)]
    wake: bool,
}

pub struct ProgressReportTool;
#[async_trait]
impl Tool for ProgressReportTool {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "progress_report".into(),
            description: "Tell your desk what changed since your last update: a short delta (done, found, next, blockers), never a recap. wake: true only when the desk must act now.".into(),
            input_schema: schema(
                json!({
                    "summary": {"type": "string"},
                    "wake": {"type": "boolean", "description": "default false"}
                }),
                &["summary"],
            ),
        }
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolError> {
        let args: ProgressReportArgs =
            serde_json::from_value(args).map_err(|e| ToolError::msg(e.to_string()))?;
        let lane = ctx.lane_origin().map(|(_, lane)| lane.to_string());
        let session = ctx
            .lane_origin()
            .map(|(session, _)| session.to_string())
            .or_else(|| ctx.durable_session_id().map(str::to_string))
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ToolError::msg("progress.report needs a work session"))?;
        if crate::session::is_inbox_session_id(&session) || crate::lead::is_session_id(&session) {
            return Err(ToolError::msg("progress.report is for work sessions and lanes"));
        }
        let db = db(ctx)?;
        let desk = crate::coordination::origin::managing_desk(&db, &session).ok_or_else(|| {
            ToolError::msg("no desk manages this session; nothing to report to")
        })?;
        let summary = match lane.as_deref() {
            Some(lane) => format!("lane {lane}: {}", args.summary.trim()),
            None => args.summary.trim().to_string(),
        };
        let key = progress_key(ctx).unwrap_or_else(|| session.clone());
        let event = db
            .record_session_progress_report(&key, &desk, &summary, args.wake, &now_rfc3339())
            .map_err(|e| ToolError::msg(e.to_string()))?;
        Ok(ToolResult::success(json!({
            "event_id": event.id,
            "to": desk,
            "wake": event.wake,
        })))
    }
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

    #[test]
    fn envelope_addresses_resolve_to_the_bare_forms_the_tools_take() {
        assert_eq!(bare_address(" agent:snippet "), "snippet");
        assert_eq!(bare_address("snippet"), "snippet");
        assert_eq!(bare_address("human:local"), "human");
        assert_eq!(bare_address("human"), "human");
        assert_eq!(bare_address("session:abc/def"), "session:abc/def");
        assert_eq!(bare_address("task:t1"), "task:t1");
        assert_eq!(bare_address("group:g1"), "group:g1");
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

        // The task's own session and Lead are let in.
        let worker = context(dir.path(), "work-1");
        PostCoordinationMessage
            .execute(&worker, json!({"thread_id": room, "body": "progress"}))
            .await
            .unwrap();
        let mc = context(dir.path(), "mc").with_agent_id("lead");
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
        let ctx = context(dir.path(), "lead");

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
        assert_eq!(events[0].actor_id, "lead");
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
        let ctx = context(dir.path(), "lead");
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
        let ctx = context(dir.path(), "lead");

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
        let ctx = context(dir.path(), "lead");
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
        let ctx = context(dir.path(), "lead");
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
        let ctx = context(dir.path(), "lead");
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

        let res = MessageLead
            .execute(
                &ctx_a,
                json!({"task_id": "t1", "message": "Need clarification on auth"}),
            )
            .await
            .unwrap();
        assert_eq!(res.value["status"], "success");

        let t = db.get_task("t1").unwrap().unwrap();
        assert_eq!(t.notifications.len(), 1);
        assert_eq!(t.notifications[0].target, "lead");
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
            "reaction acknowledges the message in reply_to without words (check: done or agreed, eyes: seen and on it, thumbs_up, question: unclear, thinking); only a desk's reply to `human` quotes it."
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
                json!({"to": "peer", "body": "can you verify?"}),
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
                    json!({"to": "peer", "reaction": "eyes"})
                )
                .await
                .is_err()
        );
        assert!(
            SendAgentMessage
                .execute(&ctx, json!({"to": "peer", "body": ""}))
                .await
                .is_err()
        );

        SendAgentMessage
            .execute(
                &ctx,
                json!({
                    "to": "peer",
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
                    "to": "peer",
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
        assert!(events[2].payload.get("reply_to").is_none());

        let human = crate::coordination::LOCAL_HUMAN_ID;
        let thread = crate::coordination::direct_thread_id(("agent", "sender"), ("human", human));
        let said = db
            .send_direct_message_typed(
                ("human", human),
                ("agent", "sender"),
                "do it",
                "k-human",
                &now_rfc3339(),
                None,
                None,
                None,
                Default::default(),
            )
            .unwrap();
        let said_id = said.event_id.clone();
        for (session, quote, quoted) in [
            ("s-sender", said_id.as_str(), false),
            ("inbox-sender", said_id.as_str(), true),
            ("inbox-sender", "human", false),
            ("inbox-sender", target_id.as_str(), false),
        ] {
            let ctx = context(dir.path(), session).with_agent_id("sender");
            SendAgentMessage
                .execute(
                    &ctx,
                    json!({"to": "human", "reply_to": quote, "body": "on it"}),
                )
                .await
                .unwrap();
            let last = db
                .direct_thread_events(&thread, 0, 50)
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(
                last.payload.get("reply_to").is_some(),
                quoted,
                "{session} {quote}"
            );
        }
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
                    "to": "b",
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
                    "to": "b",
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
        assert_eq!(fwd.payload["body"], "please check");
        assert_eq!(
            fwd.payload["forwarded_from"],
            json!({
                "event_id": orig.event_id,
                "thread_id": orig.thread_id,
                "actor_kind": "human",
                "actor_id": "local",
                "created_at": "2026-01-01T00:00:00Z",
                "text": expected_clipped,
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
                    "to": "task:t-fwd",
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
                    "to": "task:t-fwd",
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

    #[tokio::test]
    async fn share_artifact_to_group_posts_the_artifact_card() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&Agent {
            id: "sender".into(),
            display_name: "sender".into(),
            handle: "sender".into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        })
        .unwrap();
        let group = db
            .create_group(
                "launch",
                ("agent", "sender"),
                &["sender".into()],
                None,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        let session = format!("test-share-group-{}", Uuid::new_v4());
        let ctx = context(dir.path(), &session).with_agent_id("sender");
        let page = dir.path().join("report.html");
        std::fs::write(&page, "<html><body>hi</body></html>").unwrap();
        let presented =
            crate::artifacts::present(&session, &page, Some("Report"), None, None).unwrap();
        let aid = presented.artifact.id.clone();

        ShareArtifactTool
            .execute(&ctx, json!({"artifact_id": aid, "to": group.id.clone()}))
            .await
            .unwrap();
        let events = db.events_for_thread(&group.id, 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["artifact"]["id"], aid);
        assert_eq!(events[0].payload["artifact"]["session"], session);
        assert!(
            events[0].payload["artifact"]["url"]
                .as_str()
                .unwrap()
                .starts_with("/a/")
        );

        // A group the caller is not in is refused.
        let other = db
            .create_group(
                "other",
                ("agent", "sender"),
                &["sender".into()],
                None,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        db.remove_group_member(&other.id, "sender").unwrap();
        let err = ShareArtifactTool
            .execute(&ctx, json!({"artifact_id": aid, "to": other.id}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a member"), "{err}");

        crate::artifacts::delete_session(&session);
    }

    fn chat_agent(id: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    const CHAT_T0: &str = "2026-01-01T00:00:00Z";

    /// `chats` lists the direct thread with the human, a group the actor is in
    /// and the room of a task it is seated on — and nothing it is not part of.
    #[tokio::test]
    async fn chats_lists_direct_group_and_task_threads() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["w", "other"] {
            db.create_agent(&chat_agent(id)).unwrap();
        }
        db.send_direct_message(
            ("agent", "w"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
            "hi human",
            "k1",
            CHAT_T0,
        )
        .unwrap();
        let human_thread_for_auto = crate::coordination::direct_thread_id(
            ("agent", "w"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        db.append_event(&CoordinationEvent {
            event_id: "auto-ack".into(),
            thread_id: human_thread_for_auto.clone(),
            partition_key: format!("thread:{human_thread_for_auto}"),
            sequence: 0,
            event_type: "direct_message.sent".into(),
            actor_kind: "agent".into(),
            actor_id: "w".into(),
            payload_version: 1,
            payload: json!({"body": "auto ack", "auto": "ack"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k-auto".into(),
            created_at: "2026-01-01T00:00:05Z".into(),
        })
        .unwrap();
        db.send_direct_message(
            ("agent", "other"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
            "other human dm",
            "k-other-dm",
            CHAT_T0,
        )
        .unwrap();
        let mine = db
            .create_group(
                "mine",
                ("agent", "w"),
                &["w".into()],
                None,
                "2026-01-01T00:00:01Z",
            )
            .unwrap();
        let theirs = db
            .create_group(
                "theirs",
                ("agent", "other"),
                &["other".into()],
                None,
                "2026-01-01T00:00:02Z",
            )
            .unwrap();
        db.create_task(&crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Task One".into(),
            "work".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:03Z".into(),
        ))
        .unwrap();
        db.add_task_agent_full("t1", "w", "implementer", None, "", "active", CHAT_T0)
            .unwrap();
        let mut done = crate::coordination::Task::filed_by_human(
            "t2".into(),
            "Done".into(),
            "work".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:04Z".into(),
        );
        done.status = crate::coordination::TaskStatus::Done;
        db.create_task(&done).unwrap();
        db.add_task_agent_full("t2", "w", "implementer", None, "", "active", CHAT_T0)
            .unwrap();
        db.create_task(&crate::coordination::Task::filed_by_human(
            "t3".into(),
            "Other Open".into(),
            "work".into(),
            "work-2".into(),
            0,
            "2026-01-01T00:00:05Z".into(),
        ))
        .unwrap();
        db.add_task_agent_full("t3", "other", "implementer", None, "", "active", CHAT_T0)
            .unwrap();

        let ctx = context(dir.path(), "s-w").with_agent_id("w");
        let result = Chats.execute(&ctx, json!({})).await.unwrap();
        let chats = result.value["data"]["chats"].as_array().unwrap();
        let ids: Vec<&str> = chats
            .iter()
            .map(|c| c["thread_id"].as_str().unwrap())
            .collect();
        let human_thread = crate::coordination::direct_thread_id(
            ("agent", "w"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        let other_human_thread = crate::coordination::direct_thread_id(
            ("agent", "other"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        assert!(ids.contains(&human_thread.as_str()), "{ids:?}");
        assert!(!ids.contains(&other_human_thread.as_str()), "{ids:?}");
        assert!(ids.contains(&mine.id.as_str()), "{ids:?}");
        assert!(!ids.contains(&theirs.id.as_str()), "{ids:?}");
        assert!(ids.contains(&crate::coordination::Task::thread_for("t1").as_str()));
        assert!(!ids.contains(&crate::coordination::Task::thread_for("t2").as_str()));
        assert!(!ids.contains(&crate::coordination::Task::thread_for("t3").as_str()));

        let direct = chats
            .iter()
            .find(|c| c["thread_id"] == human_thread)
            .unwrap();
        assert_eq!(direct["kind"], "direct");
        assert_eq!(direct["title"], "local");
        assert_eq!(direct["peer"], "human:local");
        assert!(direct.get("unread").is_some());
        assert_eq!(direct["last"]["from"], "agent:w");
        assert_eq!(direct["last"]["body"], "hi human");
        let group = chats.iter().find(|c| c["thread_id"] == mine.id).unwrap();
        assert_eq!(group["kind"], "group");
        assert_eq!(group["title"], "mine");
        assert!(group.get("unread").is_none());
        let task = chats
            .iter()
            .find(|c| c["thread_id"] == crate::coordination::Task::thread_for("t1"))
            .unwrap();
        assert_eq!(task["kind"], "task");
        assert_eq!(task["title"], "Task One");

        let only_groups = Chats.execute(&ctx, json!({"kind": "group"})).await.unwrap();
        let rows = only_groups.value["data"]["chats"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["thread_id"], mine.id.as_str());
        assert!(Chats.execute(&ctx, json!({"kind": "nope"})).await.is_err());
    }

    /// `read_chat` resolves `peer: "human"` to the actor's own human thread,
    /// reads a group by id, and refuses a task room the actor is not on.
    #[tokio::test]
    async fn read_chat_reads_threads_by_id_with_access_checks() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        for id in ["w", "other"] {
            db.create_agent(&chat_agent(id)).unwrap();
        }
        db.send_direct_message(
            ("agent", "w"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
            "hello human",
            "k1",
            CHAT_T0,
        )
        .unwrap();
        db.send_direct_message(
            ("human", crate::coordination::LOCAL_HUMAN_ID),
            ("agent", "w"),
            "yo agent",
            "k2",
            "2026-01-01T00:00:01Z",
        )
        .unwrap();
        let group = db
            .create_group(
                "build",
                ("agent", "w"),
                &["w".into(), "other".into()],
                None,
                "2026-01-01T00:00:02Z",
            )
            .unwrap();
        db.post_group_message(
            &group.id,
            ("agent", "w"),
            "group hello",
            &[],
            None,
            "k3",
            "2026-01-01T00:00:03Z",
        )
        .unwrap();
        db.create_task(&crate::coordination::Task::filed_by_human(
            "t1".into(),
            "Private".into(),
            "work".into(),
            "work-1".into(),
            0,
            "2026-01-01T00:00:04Z".into(),
        ))
        .unwrap();
        db.add_task_agent_full("t1", "other", "implementer", None, "", "active", CHAT_T0)
            .unwrap();

        let ctx = context(dir.path(), "s-w").with_agent_id("w");
        let human_thread = crate::coordination::direct_thread_id(
            ("agent", "w"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        let human = ReadChat
            .execute(&ctx, json!({"thread_id": human_thread}))
            .await
            .unwrap();
        assert_eq!(human.value["data"]["thread_kind"], "direct");
        let bodies: Vec<&str> = human.value["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies, ["hello human", "yo agent"]);

        let group_page = ReadChat
            .execute(&ctx, json!({"thread_id": group.id}))
            .await
            .unwrap();
        assert_eq!(group_page.value["data"]["thread_kind"], "group");
        assert_eq!(
            group_page.value["data"]["messages"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let room = crate::coordination::Task::thread_for("t1");
        assert!(
            ReadChat
                .execute(&ctx, json!({"thread_id": room}))
                .await
                .is_err()
        );
        let mc = context(dir.path(), "mc").with_agent_id("lead");
        ReadChat
            .execute(&mc, json!({"thread_id": room}))
            .await
            .unwrap();
        assert!(
            ReadChat
                .execute(&ctx, json!({"thread_id": room, "peer": "human"}))
                .await
                .is_err()
        );

        // Refused on another pair's direct thread and on a group the actor is not in.
        db.send_direct_message(
            ("agent", "other"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
            "private dm",
            "k-priv",
            CHAT_T0,
        )
        .unwrap();
        let other_dm = crate::coordination::direct_thread_id(
            ("agent", "other"),
            ("human", crate::coordination::LOCAL_HUMAN_ID),
        );
        assert!(
            ReadChat
                .execute(&ctx, json!({"thread_id": other_dm}))
                .await
                .is_err()
        );
        let foreign_group = db
            .create_group(
                "foreign",
                ("agent", "other"),
                &["other".into()],
                None,
                "2026-01-01T00:00:05Z",
            )
            .unwrap();
        assert!(
            ReadChat
                .execute(&ctx, json!({"thread_id": foreign_group.id}))
                .await
                .is_err()
        );

        // Plain session whose id equals a member agent id is refused on non-direct threads too.
        let spoof_session = context(dir.path(), "other");
        assert!(
            ReadChat
                .execute(&spoof_session, json!({"thread_id": foreign_group.id}))
                .await
                .is_err()
        );
        assert!(
            ReadChat
                .execute(&spoof_session, json!({"thread_id": room}))
                .await
                .is_err()
        );

        // `read_chat` hides events with `payload.auto`.
        db.append_event(&CoordinationEvent {
            event_id: "auto-1".into(),
            thread_id: human_thread.clone(),
            partition_key: format!("thread:{human_thread}"),
            sequence: 0,
            event_type: "direct_message.sent".into(),
            actor_kind: "agent".into(),
            actor_id: "w".into(),
            payload_version: 1,
            payload: json!({"body": "auto ack", "auto": "ack"}),
            causation_id: None,
            correlation_id: None,
            idempotency_key: "k-auto-1".into(),
            created_at: "2026-01-01T00:00:06Z".into(),
        })
        .unwrap();
        let after_auto = ReadChat
            .execute(&ctx, json!({"thread_id": human_thread}))
            .await
            .unwrap();
        let bodies_after: Vec<&str> = after_auto.value["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["body"].as_str().unwrap())
            .collect();
        assert_eq!(bodies_after, ["hello human", "yo agent"]);
    }

    #[tokio::test]
    async fn share_artifact_from_path_creates_and_shares_and_is_available_on_desks() {
        let desk_registry = crate::session::coordination_tool_registry(None);
        assert!(
            desk_registry.contains("share_artifact"),
            "desk tool registry must include share_artifact for CLI access"
        );
        let (desk_defs, _) = crate::lanes::role_tool_definitions("desk", None).unwrap();
        assert!(
            !desk_defs.iter().any(|d| d.name == "share_artifact"),
            "desk advertised tool definitions must hide share_artifact"
        );

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/brief.md"), "# Launch Brief\nReady.\n").unwrap();

        let outside = tempfile::tempdir().unwrap();
        let abs_file = outside.path().join("summary.html");
        std::fs::write(&abs_file, "<html><body>Summary</body></html>").unwrap();

        let db = migrate(dir.path());
        db.create_agent(&chat_agent("desk-agent")).unwrap();
        let group = db
            .create_group(
                "launch",
                ("agent", "desk-agent"),
                &["desk-agent".into()],
                None,
                CHAT_T0,
            )
            .unwrap();

        let session = format!("inbox-test-share-path-{}", Uuid::new_v4());
        let ctx = ToolContext::new(&workspace)
            .unwrap()
            .with_durable_session_id(&session)
            .with_store_path(dir.path().join("snippet.db"))
            .with_agent_id("desk-agent");

        let both_err = ShareArtifactTool
            .execute(
                &ctx,
                json!({"artifact_id": "a1", "path": "notes/brief.md", "to": "human"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(both_err.contains("not both"), "{both_err}");

        let neither_err = ShareArtifactTool
            .execute(&ctx, json!({"to": "human"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(neither_err.contains("artifact_id"), "{neither_err}");

        let missing_err = ShareArtifactTool
            .execute(&ctx, json!({"path": "notes/missing.md", "to": "human"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(missing_err.contains("does not exist"), "{missing_err}");

        let rel_shared = ShareArtifactTool
            .execute(
                &ctx,
                json!({
                    "path": "notes/brief.md",
                    "title": "Launch Brief",
                    "to": group.id.clone(),
                }),
            )
            .await
            .unwrap();
        let rel_art = &rel_shared.value["data"]["artifact"];
        assert_eq!(rel_art["session"], session);
        assert_eq!(rel_art["title"], "Launch Brief");
        assert_eq!(rel_art["kind"], "markdown");
        assert!(rel_art["url"].as_str().unwrap().starts_with("/a/"));

        let abs_shared = ShareArtifactTool
            .execute(
                &ctx,
                json!({
                    "path": abs_file.to_string_lossy(),
                    "to": group.id.clone(),
                    "note": "Read this HTML summary.",
                }),
            )
            .await
            .unwrap();
        let abs_art = &abs_shared.value["data"]["artifact"];
        assert_eq!(abs_art["title"], "summary");
        assert_eq!(abs_art["kind"], "html");

        let events = db.events_for_thread(&group.id, 0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["body"], "Launch Brief");
        assert_eq!(events[0].payload["artifact"], *rel_art);
        assert_eq!(events[1].payload["body"], "Read this HTML summary.");
        assert_eq!(events[1].payload["artifact"], *abs_art);

        crate::artifacts::delete_session(&session);
    }

    #[tokio::test]
    async fn group_thread_post_validates_and_stores_choices() {
        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&chat_agent("lead")).unwrap();
        let group = db
            .create_group(
                "decisions",
                ("agent", "lead"),
                &["lead".into()],
                Some("lead"),
                CHAT_T0,
            )
            .unwrap();
        let ctx = context(dir.path(), "s-lead").with_agent_id("lead");

        for bad in [
            json!("not-an-array"),
            json!([]),
            json!(["Only one"]),
            json!(["1", "2", "3", "4", "5", "6"]),
            json!(["Valid", 42]),
            json!(["Valid", "   "]),
            json!(["Valid", "a".repeat(41)]),
        ] {
            let err = GroupThread
                .execute(
                    &ctx,
                    json!({
                        "action": "post",
                        "thread_id": group.id,
                        "body": "Which option should we pick?",
                        "choices": bad,
                    }),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(!err.is_empty());
        }

        let exact_40 = "x".repeat(40);
        let posted = GroupThread
            .execute(
                &ctx,
                json!({
                    "action": "post",
                    "thread_id": group.id,
                    "body": "Pick a release track.",
                    "choices": ["  Ship today  ", exact_40.clone()],
                }),
            )
            .await
            .unwrap();
        let event_id = posted.value["data"]["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            posted.value["data"]["payload"]["choices"],
            json!(["Ship today", exact_40])
        );

        let react_err = GroupThread
            .execute(
                &ctx,
                json!({
                    "action": "post",
                    "thread_id": group.id,
                    "in_reply_to": event_id,
                    "reaction": "check",
                    "choices": ["A", "B"],
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(react_err.contains("choices"), "{react_err}");

        let answer = db
            .post_group_message(
                &group.id,
                ("human", "human"),
                "Ship today",
                &[],
                Some(&event_id),
                "k-human-choice",
                "2026-01-01T00:00:05Z",
            )
            .unwrap();
        assert_eq!(answer.payload["body"], "Ship today");
        assert_eq!(answer.payload["in_reply_to"], event_id);
    }

    #[tokio::test]
    async fn group_post_chatter_rules_refuse_bare_acks_and_oversized_posts() {
        for ack in [
            "ack",
            "Acked.",
            "COPY",
            "copied!",
            "seen",
            "Noted.",
            "agreed",
            "Got it",
            "on it",
            "ok",
            "Okay.",
            "thanks",
            "Thank you!",
            "sounds good",
            "will do",
            "Understood",
            "roger",
            "confirmed",
            "holding",
            "standing by",
            "@sanjay ok",
            "  @lead, @peer: noted, standing by  ",
        ] {
            let err = check_group_post_chatter(ack).unwrap_err();
            assert_eq!(
                err,
                "That only acknowledges. React to the post instead: reaction \"check\" with in_reply_to.",
                "expected refusal for `{ack}`"
            );
        }

        let long_ok = format!(
            "Ok, Sujith's draft is below: {}",
            "please review the wording for the partner note. ".repeat(2)
        );
        assert!(long_ok.chars().count() > 80);
        assert!(check_group_post_chatter(&long_ok).is_ok());

        assert!(check_group_post_chatter("Should we ship today?").is_ok());
        assert!(check_group_post_chatter("Ok, which draft should we send?").is_ok());
        assert!(check_group_post_chatter("Okayama office confirmed the booking.").is_ok());
        assert!(check_group_post_chatter("Copyright check passed.").is_ok());
        assert!(check_group_post_chatter(&"a".repeat(2500)).is_ok());
        let draft = format!("Draft for you:\n> {}", "word ".repeat(1000));
        assert!(check_group_post_chatter(&draft).is_ok());
        let too_long = "a".repeat(2501);
        let err = check_group_post_chatter(&too_long).unwrap_err();
        assert!(err.starts_with("Too long for one post."), "{err}");
        assert!(!err.contains("several short posts"), "{err}");
        assert!(check_group_post_chatter(&format!("> {}", "a".repeat(6000))).is_err());

        let dir = tempfile::tempdir().unwrap();
        let db = migrate(dir.path());
        db.create_agent(&chat_agent("a")).unwrap();
        let group = db
            .create_group("ops", ("agent", "a"), &["a".into()], Some("a"), CHAT_T0)
            .unwrap();
        let ctx = context(dir.path(), "s-a").with_agent_id("a");

        let seed = GroupThread
            .execute(
                &ctx,
                json!({
                    "action": "post",
                    "thread_id": group.id,
                    "body": "Build passed; ready to tag?",
                }),
            )
            .await
            .unwrap();
        let seed_id = seed.value["data"]["event_id"].as_str().unwrap().to_string();

        let ack_err = GroupThread
            .execute(
                &ctx,
                json!({
                    "action": "post",
                    "thread_id": group.id,
                    "body": "@a Noted.",
                    "in_reply_to": seed_id,
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(ack_err.contains("React to the post instead"), "{ack_err}");

        let len_err = GroupThread
            .execute(
                &ctx,
                json!({
                    "action": "post",
                    "thread_id": group.id,
                    "body": "x".repeat(2501),
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(len_err.contains("Too long for one post"), "{len_err}");

        // Reactions on the same post still succeed without a body.
        GroupThread
            .execute(
                &ctx,
                json!({
                    "action": "post",
                    "thread_id": group.id,
                    "in_reply_to": seed_id,
                    "reaction": "check",
                }),
            )
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod group_round_tool_tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    fn agent(id: &str) -> Agent {
        Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    fn desk(root: &std::path::Path, id: &str) -> ToolContext {
        ToolContext::new(root)
            .unwrap()
            .with_durable_session_id(format!("inbox-{id}"))
            .with_store_path(root.join("snippet.db"))
            .with_agent_id(id)
    }

    fn setup(root: &std::path::Path) -> (Store, String) {
        let db = Store::open(root.join("snippet.db")).unwrap();
        for id in ["a", "b", "c", "outsider"] {
            db.create_agent(&agent(id)).unwrap();
        }
        let group = db
            .create_group(
                "ops",
                ("human", "human"),
                &["a".into(), "b".into(), "c".into()],
                Some("a"),
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        (db, group.id)
    }

    fn board_count(db: &Store, thread: &str) -> usize {
        db.events_for_thread(thread, 0, 200).unwrap().len()
    }

    #[tokio::test]
    async fn hidden_events_reach_only_the_target_desk_and_never_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let (db, group) = setup(dir.path());
        let before = board_count(&db, &group);
        let out = DeskEventHidden
            .execute(
                &desk(dir.path(), "a"),
                json!({"thread_id": group, "to": "b", "summary": "take the CI question", "in_minutes": 5}),
            )
            .await
            .unwrap();
        assert_eq!(out.value["data"]["to"], "b");
        assert_eq!(board_count(&db, &group), before, "a hidden event writes no board event");
        let open = db.list_open_desk_events("b", 10).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].kind, "hidden");
        assert_eq!(open[0].thread_id.as_deref(), Some(group.as_str()));
        assert_eq!(open[0].reply_thread.as_deref(), Some(group.as_str()));
        assert!(open[0].wake && open[0].deadline_at.is_some());
        assert!(open[0].summary.contains("from a"), "{}", open[0].summary);
        assert!(db.list_open_desk_events("c", 10).unwrap().is_empty());

        let err = DeskEventHidden
            .execute(
                &desk(dir.path(), "outsider"),
                json!({"thread_id": group, "to": "b", "summary": "x"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a member"), "{err}");
        let err = DeskEventHidden
            .execute(
                &desk(dir.path(), "a"),
                json!({"thread_id": group, "to": "outsider", "summary": "x"}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an agent member"), "{err}");
    }

    #[tokio::test]
    async fn release_wakes_named_agents_and_passes_the_decision() {
        let dir = tempfile::tempdir().unwrap();
        let (db, group) = setup(dir.path());
        db.post_group_message(&group, ("human", "human"), "what ships today?", &[], None, "k1", "2026-01-01T00:01:00Z")
            .unwrap();
        let round = db.open_group_round(&group).unwrap().unwrap();
        assert_eq!(round.lead_agent, "a");

        let err = DeskEventRelease
            .execute(&desk(dir.path(), "b"), json!({"thread_id": group, "decision": "ship"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("led by a"), "{err}");

        let out = DeskEventRelease
            .execute(
                &desk(dir.path(), "a"),
                json!({"thread_id": group, "wake_agents": "b", "decision": "ship the API today"}),
            )
            .await
            .unwrap();
        assert_eq!(out.value["data"]["woken"], json!(["b"]));
        assert_eq!(out.value["data"]["told"], json!(["c"]));
        assert_eq!(out.value["data"]["decision"], "ship the API today");
        let b = db.list_open_desk_events("b", 10).unwrap();
        assert_eq!(b.len(), 1);
        assert!(b[0].wake && b[0].is_mention && b[0].priority >= 40);
        assert_eq!(b[0].payload["decision"], "ship the API today");
        let c = db.list_open_desk_events("c", 10).unwrap();
        assert_eq!(c.len(), 1);
        assert!(!c[0].wake, "with someone named, peers get the decision quietly");
        assert!(c[0].summary.contains("ship the API today"));
        assert!(db.open_group_round(&group).unwrap().is_none());
    }

    #[tokio::test]
    async fn snooze_is_timed_until_pinged_or_off() {
        let dir = tempfile::tempdir().unwrap();
        let (db, group) = setup(dir.path());
        let ctx = desk(dir.path(), "b");
        let now = chrono::Utc::now().to_rfc3339();

        let out = GroupSnooze
            .execute(&ctx, json!({"thread_id": group, "in_minutes": 30}))
            .await
            .unwrap();
        assert_ne!(out.value["data"]["until"], "pinged");
        assert!(db.is_group_snoozed("b", &group, &now).unwrap());
        let later = (chrono::Utc::now() + chrono::Duration::minutes(31)).to_rfc3339();
        assert!(!db.is_group_snoozed("b", &group, &later).unwrap());

        let out = GroupSnooze.execute(&ctx, json!({"thread_id": group})).await.unwrap();
        assert_eq!(out.value["data"]["until"], "pinged");
        assert!(db.is_group_snoozed("b", &group, &later).unwrap());
        let state = crate::progress::desk_state_render(&db, "b");
        assert!(state.contains(&format!("{group} until pinged")), "{state}");

        GroupSnooze
            .execute(&ctx, json!({"thread_id": group, "off": true}))
            .await
            .unwrap();
        assert!(!db.is_group_snoozed("b", &group, &now).unwrap());
        assert!(
            GroupSnooze
                .execute(&desk(dir.path(), "outsider"), json!({"thread_id": group}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn desk_guide_lists_topics_and_returns_one_section() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = desk(dir.path(), "a");
        let out = DeskGuideTool.execute(&ctx, json!({})).await.unwrap();
        assert_eq!(
            out.value["data"]["topics"],
            json!(["loop", "threads", "drafting", "orchestration", "parallelism"])
        );
        let out = DeskGuideTool
            .execute(&ctx, json!({"topic": "Threads"}))
            .await
            .unwrap();
        let guide = out.value["data"]["guide"].as_str().unwrap();
        assert!(guide.starts_with("## threads"), "{guide}");
        assert!(!guide.contains("## drafting"), "{guide}");
        let err = DeskGuideTool
            .execute(&ctx, json!({"topic": "bogus"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("loop, threads"), "{err}");
    }

    #[tokio::test]
    async fn parallelism_and_slot_claims_on_the_desk() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = setup(dir.path());
        let ctx = desk(dir.path(), "a");
        let out = DeskParallelismTool
            .execute(&ctx, json!({"max_parallel": 1}))
            .await
            .unwrap();
        assert_eq!(out.value["data"]["max_parallel"], 1);
        assert_eq!(out.value["data"]["source"], "manual");
        assert_eq!(out.value["data"]["slots"], "0/1");
        assert!(
            DeskParallelismTool
                .execute(&ctx, json!({"max_parallel": 1, "auto": true}))
                .await
                .is_err()
        );

        let out = SlotClaimTool.execute(&ctx, json!({"for": "docs"})).await.unwrap();
        assert_eq!(out.value["data"]["free"], true, "under the cap there is nothing to queue");

        db.with_connection(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, workspace_key, workspace, status, created_at, updated_at, agent_id, kind, managed_by)
                 VALUES ('busy', 'wk', '/tmp/wk', 'running', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'a', 'conversation', 'agent')",
                [],
            )
        })
        .unwrap();
        let out = SlotClaimTool.execute(&ctx, json!({"for": "docs"})).await.unwrap();
        assert_eq!(out.value["data"]["slots"], "1/1");
        assert_eq!(out.value["data"]["status"], "waiting");
        let state = crate::progress::desk_state_render(&db, "a");
        assert!(state.is_empty() || state.contains("slots: 1/1"), "{state}");

        let out = DeskParallelismTool
            .execute(&ctx, json!({"auto": true}))
            .await
            .unwrap();
        assert_eq!(out.value["data"]["source"], "auto");
        assert!(out.value["data"]["max_parallel"].as_u64().unwrap() >= 2);
        grant_free_slots(&db, &chrono::Utc::now().to_rfc3339());
        let events = db.list_open_desk_events("a", 10).unwrap();
        assert!(events.iter().any(|e| e.kind == "slot_free"), "{events:?}");
        let state = crate::progress::desk_state_render(&db, "a");
        assert!(state.contains("slots: 2/"), "{state}");
    }

    #[tokio::test]
    async fn progress_report_reaches_the_managing_desk_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _) = setup(dir.path());
        db.import_session("work-1", "wk", &crate::harness::HarnessState::blank("/tmp", None), &Default::default())
            .unwrap();
        db.set_session_managed_by("work-1", "agent").unwrap();
        let ctx = ToolContext::new(dir.path())
            .unwrap()
            .with_durable_session_id("work-1")
            .with_store_path(dir.path().join("snippet.db"));
        for round in 0..3 {
            db.record_session_tool_round("work-1", None, &chrono::Utc::now().to_rfc3339())
                .unwrap();
            let _ = round;
        }
        let first = ProgressReportTool
            .execute(&ctx, json!({"summary": "parser done"}))
            .await
            .unwrap();
        assert_eq!(first.value["data"]["to"], crate::coordination::SNIPPET_AGENT_ID);
        assert_eq!(first.value["data"]["wake"], false);
        let second = ProgressReportTool
            .execute(&ctx, json!({"summary": "tests pass", "wake": true}))
            .await
            .unwrap();
        let open = db
            .list_open_desk_events(crate::coordination::SNIPPET_AGENT_ID, 10)
            .unwrap();
        let ids: Vec<&str> = open.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                first.value["data"]["event_id"].as_str().unwrap(),
                second.value["data"]["event_id"].as_str().unwrap()
            ]
        );
        assert!(open[0].summary.contains("r3: parser done"), "{}", open[0].summary);
        assert!(open[1].wake);
        assert!(
            ProgressReportTool
                .execute(&desk(dir.path(), "a"), json!({"summary": "x"}))
                .await
                .is_err()
        );
    }
}
