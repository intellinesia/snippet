//! Background sub-agent lanes.
//!
//! A "lane" here is a child [`CodingHarness`] run on a `tokio` task: it shares the parent
//! workspace (so produced files are visible to the conversation agent), runs the
//! plain coding-agent prompt to `complete`, and reports a [`LaneResult`] back over
//! a channel. Multiple lanes run in parallel.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::harness::{CodingHarness, HarnessConfig};
use crate::lane_log::LaneLog;
use crate::llm::AgentModel;
use crate::llm::NativeToolDefinition;
use crate::prompts::{PromptContext, lane_prompt};
use crate::tools::{Tool, ToolContext, ToolError, ToolResult, coding_tools};

/// Builds a fresh model instance for a child lane run. Accepts an optional
/// inference profile name and the lane's own durable session id so each lane
/// keeps its own sticky prompt-cache key without colliding with the parent.
pub type ModelFactory =
    Arc<dyn Fn(Option<&str>, Option<String>) -> Result<Box<dyn AgentModel>, String> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LaneStatus {
    #[default]
    Running,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneActivity {
    pub at: String,
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct LaneProgress {
    pub id: String,
    pub kind: String,
    pub text: String,
}

/// Persisted, render-friendly snapshot of a lane (kept in `HarnessState`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneRecord {
    pub id: String,
    pub title: String,
    pub status: LaneStatus,
    /// The original handoff/brief given to this lane, before reporting instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The full verified report returned by the lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<LaneStructuredReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Latest safe operational activity, never a user-addressable prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_at: Option<String>,
    /// Small durable tail for the read-only live lane viewer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activity_log: Vec<LaneActivity>,
    /// Investigation lane: file-mutation tools removed. Sticky across follow-ups.
    #[serde(default)]
    pub read_only: bool,
    /// Specialized agent identity (e.g. 'rust-pr-reviewer').
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Inference profile name chosen for this lane (omitted when using active model).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The id the lane's own transcript is stored under, for a read-only view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Paths a writing lane declared; `change_files` outside them is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LaneFinding {
    pub claim: String,
    #[serde(rename = "where")]
    pub location: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LaneStructuredReport {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<LaneFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified_citations: Option<String>,
    #[serde(default)]
    pub clipped: bool,
}

/// Terminal report delivered back to the parent loop when a lane finishes.
#[derive(Debug, Clone, Default)]
pub struct LaneResult {
    pub id: String,
    pub title: String,
    pub status: LaneStatus,
    /// Concise final summary (the lane's terminate_loop text) — shown in the TUI.
    pub summary: Option<String>,
    /// Full report for the lane record (TUI and follow-up).
    pub report: Option<String>,
    pub error: Option<String>,
    /// Structured fields for the compact `[lane_report]` envelope.
    pub structured: Option<LaneStructuredReport>,
}

/// Messages waiting for a running lane: from the agent that delegated it, or
/// replies from agents it wrote to. The lane picks them up between steps, or
/// straight away while it waits in `message_parent`.
#[derive(Debug, Default)]
pub struct LaneMailbox {
    queue: Mutex<VecDeque<String>>,
    notify: tokio::sync::Notify,
}

impl LaneMailbox {
    pub fn push(&self, text: String) {
        self.queue.lock().unwrap().push_back(text);
        self.notify.notify_one();
    }

    pub fn drain(&self) -> Vec<String> {
        self.queue.lock().unwrap().drain(..).collect()
    }

    pub(crate) async fn wait(&self, timeout: std::time::Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let waiting = self.drain();
            if !waiting.is_empty() {
                return waiting;
            }
            if tokio::time::timeout_at(deadline, self.notify.notified())
                .await
                .is_err()
            {
                return self.drain();
            }
        }
    }
}

static LIVE_LANES: LazyLock<Mutex<HashMap<String, Arc<LaneMailbox>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lane_key(session_id: &str, lane_id: &str) -> String {
    format!("{session_id}#{lane_id}")
}

/// Hand a message to a lane that is running right now. False when the lane
/// has finished, so the caller can deliver it to the parent session instead.
pub fn deliver_to_lane(session_id: &str, lane_id: &str, text: String) -> bool {
    let Some(mailbox) = LIVE_LANES
        .lock()
        .unwrap()
        .get(&lane_key(session_id, lane_id))
        .cloned()
    else {
        return false;
    };
    mailbox.push(text);
    true
}

struct LiveLane(Option<String>);

impl LiveLane {
    fn register(session_id: Option<&str>, lane_id: &str, mailbox: &Arc<LaneMailbox>) -> Self {
        let key = session_id.map(|session| lane_key(session, lane_id));
        if let Some(key) = &key {
            LIVE_LANES
                .lock()
                .unwrap()
                .insert(key.clone(), mailbox.clone());
        }
        Self(key)
    }
}

impl Drop for LiveLane {
    fn drop(&mut self) {
        if let Some(key) = &self.0 {
            LIVE_LANES.lock().unwrap().remove(key);
        }
    }
}

const PARENT_REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A lane's line back to the agent that delegated it.
pub struct MessageParent {
    lane_id: String,
    progress_tx: mpsc::UnboundedSender<LaneProgress>,
    mailbox: Arc<LaneMailbox>,
}

#[derive(Deserialize)]
struct MessageParentArgs {
    message: String,
    #[serde(default)]
    wait: bool,
}

#[async_trait::async_trait]
impl Tool for MessageParent {
    fn definition(&self) -> NativeToolDefinition {
        NativeToolDefinition {
            name: "message_parent".into(),
            description: "Write to the agent that handed you this work, while you're still on it: a question you can't settle yourself, a finding that changes its plan or another lane's, or a heads-up before something hard to undo. With wait: true you pause for its answer (up to 10 minutes) and get it back here; otherwise carry on, and an answer arrives as a [parent_message].".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string"},
                    "wait": {"type": "boolean", "description": "Pause for the answer, when you can't sensibly go on without one."}
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _ctx: &ToolContext,
        arguments: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let args: MessageParentArgs =
            serde_json::from_value(arguments).map_err(|e| ToolError::msg(e.to_string()))?;
        let message = args.message.trim();
        if message.is_empty() {
            return Err(ToolError::msg("message must not be empty"));
        }
        self.progress_tx
            .send(LaneProgress {
                id: self.lane_id.clone(),
                kind: "message".to_string(),
                text: message.to_string(),
            })
            .map_err(|_| {
                ToolError::msg("the agent that handed you this has gone; put it in your report")
            })?;
        if !args.wait {
            return Ok(ToolResult::success(serde_json::json!({
                "sent": true,
                "note": "Sent. Carry on; an answer, if one comes, arrives as a [parent_message].",
            })));
        }
        let answers = self.mailbox.wait(PARENT_REPLY_WAIT).await;
        if answers.is_empty() {
            return Ok(ToolResult::success(serde_json::json!({
                "sent": true,
                "answer": null,
                "note": "No answer yet. Go on with your best judgment and say in your report what you assumed; a later answer arrives as a [parent_message].",
            })));
        }
        Ok(ToolResult::success(serde_json::json!({
            "sent": true,
            "answer": answers.join("\n\n"),
        })))
    }
}

/// Who a lane works for, gathered from the parent session when the lane starts.
#[derive(Clone)]
struct LaneLink {
    origin_session: Option<String>,
    agent_id: Option<String>,
    store_path: Option<PathBuf>,
    context: String,
    mailbox: Arc<LaneMailbox>,
    title: String,
    write_scope: Vec<PathBuf>,
    lease_token: Option<u64>,
}

/// Max lanes running at once; set high on purpose, CPU is rationed by the caller.
const MAX_ACTIVE_LANES: usize = 1000;

/// Wall-clock cap per lane. Without one, a hung lane (stalled provider, endless
/// tool loop under the iteration backstop) never reports, and the orchestrator —
/// told that ending its turn is how it waits — waits forever.
async fn lane_timeout() -> std::time::Duration {
    let mins = crate::config::SnippetConfig::load(crate::config::default_config_path())
        .await
        .map(|c| c.lane_timeout_mins)
        .unwrap_or(30)
        .clamp(1, 24 * 60);
    std::time::Duration::from_secs(u64::from(mins) * 60)
}

/// Finished lanes older than this are dropped from the session snapshot and disk.
const LANE_FINISHED_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Cap on retained finished lanes (Running never counts). Oldest finished drop first.
const MAX_FINISHED_LANES: usize = 32;

/// Owns lane lifecycle for one conversation run. Lives in the interactive loop's
/// local scope (not in the immutable `CodingHarness`). Aborts any still-running
/// lanes when dropped (the run was interrupted / ended).
#[derive(Debug)]
pub struct LaneApprovalRequest {
    pub lane: String,
    pub tool_name: String,
    pub summary: String,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

#[derive(Debug, Clone)]
pub struct LaneApprovalRoute {
    pub lane: String,
    pub tx: mpsc::UnboundedSender<LaneApprovalRequest>,
}

impl LaneApprovalRoute {
    pub async fn ask(&self, tool_name: &str, summary: &str) -> bool {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let request = LaneApprovalRequest {
            lane: self.lane.clone(),
            tool_name: tool_name.to_string(),
            summary: summary.to_string(),
            reply,
        };
        if self.tx.send(request).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }
}

pub struct LaneManager {
    factory: Option<ModelFactory>,
    approval_tx: Option<mpsc::UnboundedSender<LaneApprovalRequest>>,
    workspace_root: PathBuf,
    lane_root: PathBuf,
    result_tx: mpsc::UnboundedSender<LaneResult>,
    progress_tx: mpsc::UnboundedSender<LaneProgress>,
    records: Vec<LaneRecord>,
    counter: usize,
    exa_api_key: Option<String>,
    handles: Vec<(String, tokio::task::JoinHandle<()>)>,
    parent: Option<ToolContext>,
    parent_profile: Option<String>,
    mailboxes: HashMap<String, Arc<LaneMailbox>>,
}

impl Drop for LaneManager {
    fn drop(&mut self) {
        // Interrupt/teardown: don't leave detached lanes burning tokens.
        for (_, handle) in &self.handles {
            handle.abort();
        }
    }
}

impl LaneManager {
    pub fn new(
        factory: Option<ModelFactory>,
        workspace_root: PathBuf,
        lane_root: PathBuf,
        result_tx: mpsc::UnboundedSender<LaneResult>,
        progress_tx: mpsc::UnboundedSender<LaneProgress>,
        exa_api_key: Option<String>,
    ) -> Self {
        Self {
            factory,
            approval_tx: None,
            workspace_root,
            lane_root,
            result_tx,
            progress_tx,
            records: Vec::new(),
            counter: 0,
            exa_api_key,
            handles: Vec::new(),
            parent: None,
            parent_profile: None,
            mailboxes: HashMap::new(),
        }
    }

    /// Profile the parent session runs on; a lane without its own profile uses it.
    pub fn with_parent_profile(mut self, profile: Option<String>) -> Self {
        self.parent_profile = profile;
        self
    }

    pub fn with_parent(mut self, parent: ToolContext) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Pass a message to a running lane. It reads it between steps, or as the
    /// answer when it is waiting on one.
    pub fn message(&mut self, lane_id: &str, text: &str) -> Result<String, String> {
        let Some(record) = self.records.iter().find(|r| r.id == lane_id) else {
            return Err(format!("no lane `{lane_id}` in this conversation."));
        };
        if record.status != LaneStatus::Running {
            return Err(format!("lane `{lane_id}` isn't running."));
        }
        let Some(mailbox) = self.mailboxes.get(lane_id) else {
            return Err(format!("lane `{lane_id}` can't take messages right now."));
        };
        mailbox.push(format!(
            "[parent_message]\n{}\n[/parent_message]",
            text.trim()
        ));
        Ok(record.title.clone())
    }

    fn transcript_id(&self, id: &str) -> Option<String> {
        let path = self.lane_root.join(format!("{id}.json"));
        path.starts_with(crate::config::workspaces_root())
            .then(|| crate::session::session_id_for_state_path(&path))
    }

    fn link_for(&mut self, id: &str, agent: Option<&str>) -> LaneLink {
        let mailbox = Arc::new(LaneMailbox::default());
        self.mailboxes.insert(id.to_string(), mailbox.clone());
        let parent = self.parent.as_ref();
        let origin_session = parent
            .and_then(|p| p.durable_session_id())
            .map(str::to_string);
        let speaker = parent.and_then(crate::coordination_tools::speaks_as);
        let task = match (parent, origin_session.as_deref()) {
            (Some(p), Some(session)) => crate::coordination_tools::session_task(p, session),
            _ => None,
        };
        let mut context = String::from("You're a co-worker taking one piece of a larger job.");
        match &speaker {
            Some(("agent", who)) => {
                context.push_str(&format!(" It was handed to you by the agent `{who}`"))
            }
            _ => {
                context.push_str(" It was handed to you by the agent working in the parent session")
            }
        }
        if let Some((task_id, title)) = &task {
            context.push_str(&format!(
                ", who is working on the task \"{title}\" ({task_id})"
            ));
        }
        context.push('.');
        if let Some(agent) = agent {
            context.push_str(&format!(
                " You bring the expertise of `{agent}`, and you speak as `{agent}`."
            ));
        }
        let siblings: Vec<String> = self
            .records
            .iter()
            .filter(|r| r.status == LaneStatus::Running && r.id != id)
            .map(|r| {
                let access = if r.read_only { ", read-only" } else { "" };
                format!("- \"{}\" ({}{access})", r.title, r.id)
            })
            .collect();
        if siblings.is_empty() {
            context.push_str(" No other lanes are running alongside you.");
        } else {
            context.push_str(&format!(
                " Other lanes are working alongside you on their own pieces; keep to yours and leave their files alone:\n{}",
                siblings.join("\n")
            ));
        }
        context.push_str(
            "\n\nYou aren't on your own. When you hit a question you can't settle, find something that changes the plan or touches another lane's piece, or are about to do something hard to undo, tell the agent that handed you this with `message_parent`. When something needs a peer, Mission Control, or the rest of your own work (your inbox knows it), use `send_agent_message`; the answer comes back to you here. Keep it to what matters: most of the job is doing your piece well.",
        );
        LaneLink {
            origin_session,
            agent_id: agent
                .map(str::to_string)
                .or_else(|| parent.and_then(|p| p.agent_id()).map(str::to_string)),
            store_path: parent.and_then(|p| p.store_path()),
            context,
            mailbox,
            title: self
                .records
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.title.clone())
                .unwrap_or_default(),
            write_scope: self
                .records
                .iter()
                .find(|r| r.id == id)
                .map(|r| {
                    r.paths
                        .iter()
                        .map(|p| self.workspace_root.join(p))
                        .collect()
                })
                .unwrap_or_default(),
            lease_token: parent
                .filter(|p| p.store_path().is_some())
                .zip(task.as_ref())
                .and_then(|(p, (task_id, _))| p.store().ok()?.current_lease(task_id).ok().flatten())
                .map(|lease| lease.fencing_token),
        }
    }

    pub fn with_approvals(
        mut self,
        tx: Option<mpsc::UnboundedSender<LaneApprovalRequest>>,
    ) -> Self {
        self.approval_tx = tx;
        self
    }

    /// Restore prior records (e.g. on resume) so the display reflects history.
    /// Runs housekeeping so aged-out finished lanes (and orphan disk files) do not
    /// accumulate across resumes.
    pub fn with_records(mut self, records: Vec<LaneRecord>) -> Self {
        // Counter must keep rising past historical ids even after prune, so new
        // spawns never collide with on-disk `lane-N.json` from dropped records.
        self.counter = records
            .iter()
            .filter_map(|r| r.id.strip_prefix("lane-")?.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        self.records = records;
        let _ = self.housekeep();
        self
    }

    pub fn enabled(&self) -> bool {
        self.factory.is_some()
    }

    pub fn records(&self) -> &[LaneRecord] {
        &self.records
    }

    pub fn active_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.status == LaneStatus::Running)
            .count()
    }

    /// Spawn a lane. Returns the new lane id, or an error string (fed back to the
    /// model as a tool error) when delegation is unavailable.
    pub fn spawn(
        &mut self,
        title: &str,
        brief: &str,
        read_only: bool,
        agent: Option<String>,
        profile: Option<String>,
    ) -> Result<String, String> {
        self.spawn_scoped(title, brief, read_only, agent, profile, Vec::new())
    }

    /// Spawn a lane; a writing lane may declare the paths it will change, and is
    /// refused while another running writing lane declared an overlapping path.
    pub fn spawn_scoped(
        &mut self,
        title: &str,
        brief: &str,
        read_only: bool,
        agent: Option<String>,
        profile: Option<String>,
        paths: Vec<String>,
    ) -> Result<String, String> {
        if self.factory.is_none() {
            return Err(
                "delegate_task is unavailable in this run (no model factory; interactive mode only)."
                    .to_string(),
            );
        };
        if self.active_count() >= MAX_ACTIVE_LANES {
            return Err(format!(
                "{MAX_ACTIVE_LANES} lanes are already running — wait for some to report before delegating more."
            ));
        }

        if let Some(clash) = self.scope_clash(read_only, &paths) {
            return Err(clash);
        }
        self.counter += 1;
        let id = format!("lane-{}", self.counter);
        self.records.push(LaneRecord {
            id: id.clone(),
            title: title.to_string(),
            status: LaneStatus::Running,
            handoff: Some(brief.to_string()),
            summary: None,
            report: None,
            structured: None,
            error: None,
            started_at: Utc::now().to_rfc3339(),
            finished_at: None,
            activity: None,
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
            read_only,
            agent: agent.clone(),
            profile: profile.clone(),
            transcript: self.transcript_id(&id),
            paths,
        });
        self.launch(&id, title, brief, false, read_only, agent, profile);
        Ok(id)
    }

    fn scope_clash(&self, read_only: bool, paths: &[String]) -> Option<String> {
        if read_only {
            return None;
        }
        let overlaps = |a: &str, b: &str| {
            let (a, b) = (Path::new(a), Path::new(b));
            a.starts_with(b) || b.starts_with(a)
        };
        self.records
            .iter()
            .filter(|r| r.status == LaneStatus::Running && !r.read_only)
            .find_map(|r| {
                let (mine, theirs) = paths.iter().find_map(|p| r.paths.iter().find(|q| overlaps(p, q)).map(|q| (p, q)))?;
                Some(format!("`{mine}` overlaps `{theirs}`, which running lane {} (\"{}\") is writing. Wait for it, split the paths, or make this lane read-only.", r.id, r.title))
            })
    }

    /// Continue a FINISHED lane with a follow-up brief: its harness state is
    /// resumed from disk, so the lane keeps everything it learned (the analog of
    /// messaging an existing agent instead of spawning a fresh one). Returns the
    /// lane's title.
    pub fn follow_up(
        &mut self,
        lane_id: &str,
        brief: &str,
        profile: Option<String>,
    ) -> Result<String, String> {
        if self.factory.is_none() {
            return Err(
                "delegate_task is unavailable in this run (no model factory; interactive mode only)."
                    .to_string(),
            );
        }
        if self.active_count() >= MAX_ACTIVE_LANES {
            return Err(format!(
                "{MAX_ACTIVE_LANES} lanes are already running — wait for some to report before delegating more."
            ));
        }
        let Some(record) = self.records.iter_mut().find(|r| r.id == lane_id) else {
            let known: Vec<String> = self
                .records
                .iter()
                .map(|r| format!("\"{}\" ({})", r.title, r.id))
                .collect();
            return Err(format!(
                "no follow_up_id `{lane_id}` in this conversation. Known: [{}]. Omit lane_id to start a new one.",
                known.join(", ")
            ));
        };
        if record.status == LaneStatus::Running {
            return Err(format!(
                "lane `{lane_id}` is still running — its report will arrive as a [lane_report]."
            ));
        }
        // A lane lost to a restart has no live task but its state file survives —
        // following up is exactly how to revive it.
        record.status = LaneStatus::Running;
        record.finished_at = None;
        record.handoff = Some(brief.to_string());
        record.summary = None;
        record.report = None;
        record.structured = None;
        record.error = None;
        if profile.is_some() {
            record.profile = profile;
        }
        let (title, read_only, agent, profile) = (
            record.title.clone(),
            record.read_only,
            record.agent.clone(),
            record.profile.clone(),
        );
        self.launch(lane_id, &title, brief, true, read_only, agent, profile);
        Ok(title)
    }

    pub fn cancel(&mut self, lane_id: &str, reason: &str) -> Result<String, String> {
        let Some(record) = self.records.iter_mut().find(|r| r.id == lane_id) else {
            return Err(format!(
                "no delegated task `{lane_id}` is known in this conversation."
            ));
        };
        if !matches!(record.status, LaneStatus::Running | LaneStatus::Paused) {
            return Err(format!(
                "delegated task `{lane_id}` is not running (status: {:?}).",
                record.status
            ));
        }
        for (id, handle) in &self.handles {
            if id == lane_id {
                handle.abort();
                break;
            }
        }
        record.status = LaneStatus::Cancelled;
        record.error = Some(format!("cancelled by parent: {reason}"));
        record.finished_at = Some(Utc::now().to_rfc3339());
        let activity = LaneActivity {
            at: Utc::now().to_rfc3339(),
            kind: "cancelled".to_string(),
            text: reason.to_string(),
        };
        record.activity = Some(activity.text.clone());
        record.activity_kind = Some(activity.kind.clone());
        record.activity_at = Some(activity.at.clone());
        record.activity_log.push(activity);
        const MAX_ACTIVITY: usize = 24;
        if record.activity_log.len() > MAX_ACTIVITY {
            let drop_count = record.activity_log.len() - MAX_ACTIVITY;
            record.activity_log.drain(..drop_count);
        }
        Ok(record.title.clone())
    }

    /// Shared spawn: run the lane on a tokio task and report back over the channel.
    fn launch(
        &mut self,
        id: &str,
        title: &str,
        brief: &str,
        resume: bool,
        read_only: bool,
        agent: Option<String>,
        profile: Option<String>,
    ) {
        let factory = self.factory.clone().expect("checked by callers");
        let result_tx = self.result_tx.clone();
        let progress_tx = self.progress_tx.clone();
        let workspace_root = self.workspace_root.clone();
        let state_path = self.lane_root.join(format!("{id}.json"));
        let brief = brief.to_string();
        let title = title.to_string();
        let lane_id = id.to_string();
        let exa_api_key = self.exa_api_key.clone();
        let approval = self.approval_tx.clone().map(|tx| LaneApprovalRoute {
            lane: title.clone(),
            tx,
        });
        let link = self.link_for(id, agent.as_deref());
        let parent_profile = self.parent_profile.clone();

        let handle = tokio::spawn(async move {
            let _live = LiveLane::register(link.origin_session.as_deref(), &lane_id, &link.mailbox);
            let limit = lane_timeout().await;
            let mut timed_out = false;
            let result = tokio::time::timeout(
                limit,
                run_lane(
                    factory,
                    workspace_root,
                    state_path,
                    brief,
                    lane_id.clone(),
                    exa_api_key,
                    resume,
                    read_only,
                    agent,
                    profile,
                    parent_profile,
                    progress_tx,
                    approval,
                    link,
                ),
            )
            .await
            .unwrap_or_else(|_| {
                timed_out = true;
                let error = format!(
                    "paused after running for {} minutes, the lane time limit",
                    limit.as_secs() / 60
                );
                if let Ok(mut log) = crate::lane_log::LaneLog::open(&lane_id) {
                    let _ = log.write_end(&lane_id, "paused", None, Some(&error), None);
                }
                Err(error)
            });
            let lane_result = match result {
                Ok((summary, report, structured)) => LaneResult {
                    id: lane_id,
                    title,
                    status: LaneStatus::Completed,
                    summary: Some(summary),
                    report: Some(report),
                    error: None,
                    structured: Some(structured),
                },
                Err(error) => LaneResult {
                    id: lane_id,
                    title,
                    status: if timed_out {
                        LaneStatus::Paused
                    } else {
                        LaneStatus::Failed
                    },
                    summary: None,
                    report: None,
                    error: Some(error),
                    structured: None,
                },
            };
            let _ = result_tx.send(lane_result);
        });
        self.handles.push((id.to_string(), handle));
    }

    pub fn record_progress(&mut self, progress: &LaneProgress) {
        let Some(record) = self.records.iter_mut().find(|r| r.id == progress.id) else {
            return;
        };
        let activity = LaneActivity {
            at: Utc::now().to_rfc3339(),
            kind: progress.kind.clone(),
            text: progress.text.chars().take(240).collect(),
        };
        record.activity = Some(activity.text.clone());
        record.activity_kind = Some(activity.kind.clone());
        record.activity_at = Some(activity.at.clone());
        record.activity_log.push(activity);
        const MAX_ACTIVITY: usize = 24;
        if record.activity_log.len() > MAX_ACTIVITY {
            let drop_count = record.activity_log.len() - MAX_ACTIVITY;
            record.activity_log.drain(..drop_count);
        }
    }

    /// Relaunch lanes that were running when the parent process stopped.
    pub fn resume_interrupted(&mut self) {
        let ids: Vec<(String, String, bool, Option<String>, Option<String>)> = self
            .records
            .iter()
            .filter(|r| r.status == LaneStatus::Running)
            .map(|r| {
                (
                    r.id.clone(),
                    r.title.clone(),
                    r.read_only,
                    r.agent.clone(),
                    r.profile.clone(),
                )
            })
            .collect();
        for (id, title, read_only, agent, profile) in ids {
            self.record_progress(&LaneProgress {
                id: id.clone(),
                kind: "restart".to_string(),
                text: "resuming from the last saved checkpoint".to_string(),
            });
            let handoff = self
                .records
                .iter()
                .find(|r| r.id == id)
                .and_then(|r| r.handoff.clone())
                .unwrap_or_else(|| {
                    "Continue the delegated task from the saved lane state.".to_string()
                });
            self.launch(&id, &title, &handoff, true, read_only, agent, profile);
        }
    }

    pub fn record_result(&mut self, result: &LaneResult) -> bool {
        if self
            .records
            .iter()
            .any(|record| record.id == result.id && record.status == LaneStatus::Cancelled)
        {
            return false;
        }
        if let Some(record) = self
            .records
            .iter_mut()
            .find(|record| record.id == result.id)
        {
            record.status = result.status;
            record.summary = result.summary.clone();
            record.report = result.report.clone();
            record.structured = result.structured.clone();
            record.error = result.error.clone();
            record.finished_at = Some(Utc::now().to_rfc3339());
            let text = match result.status {
                LaneStatus::Completed => "completed",
                LaneStatus::Failed => "failed",
                LaneStatus::Paused => "paused",
                LaneStatus::Cancelled => "cancelled",
                LaneStatus::Running => "running",
            };
            let activity = LaneActivity {
                at: Utc::now().to_rfc3339(),
                kind: "lifecycle".to_string(),
                text: text.to_string(),
            };
            record.activity = Some(activity.text.clone());
            record.activity_kind = Some(activity.kind.clone());
            record.activity_at = Some(activity.at.clone());
            record.activity_log.push(activity);
            const MAX_ACTIVITY: usize = 24;
            if record.activity_log.len() > MAX_ACTIVITY {
                let drop_count = record.activity_log.len() - MAX_ACTIVITY;
                record.activity_log.drain(..drop_count);
            }
        }
        // Best-effort: drop temp diagnostic JSONL once the lane is terminal.
        if result.status != LaneStatus::Running {
            let _ = LaneLog::cleanup_lane(&result.id);
            // Bound finished-lane growth (TTL + count) and sweep orphan disk state.
            let _ = self.housekeep();
        }
        true
    }

    /// Drop finished lanes past TTL / over the finished-count cap, delete their
    /// on-disk harness state, and sweep orphan `lane-*.json` files under
    /// `lane_root` that are no longer referenced. Never touches Running lanes.
    /// Returns how many finished records were removed from the in-memory list.
    pub fn housekeep(&mut self) -> usize {
        let now = Utc::now();
        let ttl = chrono::Duration::from_std(LANE_FINISHED_TTL)
            .unwrap_or_else(|_| chrono::Duration::days(7));

        let mut finished_idx: Vec<(usize, chrono::DateTime<Utc>)> = Vec::new();
        for (i, rec) in self.records.iter().enumerate() {
            if rec.status == LaneStatus::Running {
                continue;
            }
            let when = rec
                .finished_at
                .as_deref()
                .and_then(parse_rfc3339)
                .or_else(|| parse_rfc3339(&rec.started_at))
                .unwrap_or(now);
            finished_idx.push((i, when));
        }

        // Age-out first.
        let mut drop: std::collections::HashSet<usize> = finished_idx
            .iter()
            .filter(|(_, when)| now.signed_duration_since(*when) > ttl)
            .map(|(i, _)| *i)
            .collect();

        // Then enforce max finished count (oldest first among survivors).
        let mut survivors: Vec<(usize, chrono::DateTime<Utc>)> = finished_idx
            .into_iter()
            .filter(|(i, _)| !drop.contains(i) && self.records[*i].status != LaneStatus::Paused)
            .collect();
        if survivors.len() > MAX_FINISHED_LANES {
            survivors.sort_by_key(|(_, when)| *when); // oldest first
            let excess = survivors.len() - MAX_FINISHED_LANES;
            for (i, _) in survivors.into_iter().take(excess) {
                drop.insert(i);
            }
        }

        if drop.is_empty() {
            // Still sweep orphans (e.g. files left after a crash / older builds).
            self.sweep_orphan_lane_files();
            return 0;
        }

        let removed_ids: Vec<String> = drop
            .iter()
            .filter_map(|&i| self.records.get(i).map(|r| r.id.clone()))
            .collect();
        let mut idxs: Vec<usize> = drop.into_iter().collect();
        idxs.sort_unstable();
        for i in idxs.into_iter().rev() {
            self.records.remove(i);
        }
        for id in &removed_ids {
            self.delete_lane_files(id);
            let _ = LaneLog::cleanup_lane(id);
        }
        self.sweep_orphan_lane_files();
        removed_ids.len()
    }

    fn delete_lane_files(&self, id: &str) {
        if !is_safe_lane_file_id(id) {
            return;
        }
        let base = self.lane_root.join(id);
        let _ = std::fs::remove_file(base.with_extension("json"));
        let _ = std::fs::remove_file(PathBuf::from(format!("{}.meta.json", base.display())));
        // Some writers use `lane-N.meta.json` beside `lane-N.json`.
        let _ = std::fs::remove_file(self.lane_root.join(format!("{id}.meta.json")));
        let _ = std::fs::remove_file(self.lane_root.join(format!("{id}.json")));
        if let Some(transcript) = self.transcript_id(id)
            && let Ok(store) = crate::store::Store::open_cached(crate::store::default_db_path())
        {
            let _ = store.delete_conversation(&transcript);
        }
    }

    /// Remove `lane-*.json` / `lane-*.meta.json` under lane_root that are not
    /// referenced by any current record (including Running).
    fn sweep_orphan_lane_files(&self) {
        let Ok(entries) = std::fs::read_dir(&self.lane_root) else {
            return;
        };
        let keep: std::collections::HashSet<&str> =
            self.records.iter().map(|r| r.id.as_str()).collect();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // lane-12.json / lane-12.meta.json
            let id = if let Some(rest) = name.strip_suffix(".meta.json") {
                rest
            } else if let Some(rest) = name.strip_suffix(".json") {
                rest
            } else {
                continue;
            };
            if !id.starts_with("lane-") || !is_safe_lane_file_id(id) {
                continue;
            }
            if keep.contains(id) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn parse_rfc3339(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn is_safe_lane_file_id(id: &str) -> bool {
    // lane-<digits> only — never allow path separators or `..`.
    let Some(rest) = id.strip_prefix("lane-") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

pub(crate) fn lane_tool_registry(
    exa_api_key: Option<String>,
    agent: Option<&str>,
    message_parent: MessageParent,
    read_only: bool,
) -> crate::tools::ToolRegistry {
    let mut tools = coding_tools(exa_api_key)
        .with_custom_dir(crate::agent_tools::tools_dir(agent.unwrap_or("snippet")));
    tools.insert(message_parent);
    tools.insert(crate::coordination_tools::SendAgentMessage);
    tools.insert(crate::coordination_tools::ReadCoordinationBoard);
    tools.insert(crate::review_tools::SubmitReview);
    tools.insert(crate::profiles::ListProfiles);
    tools.remove(crate::progress::TOOL_NAME);
    tools.remove("pass_turn");
    if read_only {
        // Investigation lane: strip the file-mutation tools so a fan-out of
        // readers can't collide with the main agent's (or each other's) edits.
        // The shell remains for inspection; the brief tells the lane its role.
        tools.remove("change_files");
    }
    tools
}

pub const WORK_TOOL_BUDGET_BYTES: usize = 30_450;
pub const MISSION_CONTROL_TOOL_BUDGET_BYTES: usize = 60_200;
pub const DESK_TOOL_BUDGET_BYTES: usize = 15_400;
pub const LANE_TOOL_BUDGET_BYTES: usize = 9_500;

pub fn definition_byte_size(d: &NativeToolDefinition) -> usize {
    d.name.len() + d.description.len() + d.input_schema.to_string().len()
}

pub fn role_tool_definitions(
    role: &str,
    custom_dir: Option<PathBuf>,
) -> Option<(Vec<NativeToolDefinition>, usize)> {
    let key = Some("k".to_string());
    let with_meta = |mut defs: Vec<NativeToolDefinition>, lanes: bool| {
        defs.extend(crate::meta::conversation_meta_definitions_for(false, lanes));
        defs
    };
    match role {
        "work" => Some((
            with_meta(
                crate::session::work_tool_registry(key, custom_dir).definitions(),
                true,
            ),
            WORK_TOOL_BUDGET_BYTES,
        )),
        "mission_control" => Some((
            with_meta(
                crate::session::mission_control_tool_registry(key).definitions(),
                true,
            ),
            MISSION_CONTROL_TOOL_BUDGET_BYTES,
        )),
        "desk" => Some((
            crate::session::coordination_tool_registry(key).definitions(),
            DESK_TOOL_BUDGET_BYTES,
        )),
        "lane" => {
            let (progress_tx, _rx) = mpsc::unbounded_channel();
            let parent = MessageParent {
                lane_id: "lane-1".into(),
                progress_tx,
                mailbox: Arc::new(LaneMailbox::default()),
            };
            Some((
                lane_tool_registry(key, None, parent, false).definitions(),
                LANE_TOOL_BUDGET_BYTES,
            ))
        }
        _ => None,
    }
}

/// Profiles a lane tries in order: a pinned profile then the parent's; unpinned,
/// the parent's (`None` when unknown: the factory default), then the routing chain.
fn lane_route(
    explicit: Option<&str>,
    parent: Option<&str>,
    chain: &[String],
    limited: impl Fn(&str) -> bool,
) -> Vec<Option<String>> {
    if let Some(name) = explicit {
        let mut route = vec![Some(name.to_string())];
        route.extend(
            parent
                .filter(|p| *p != name && !limited(p))
                .map(|p| Some(p.to_string())),
        );
        return route;
    }
    let mut route = Vec::new();
    match parent {
        Some(name) if limited(name) => {}
        Some(name) => route.push(Some(name.to_string())),
        None => route.push(None),
    }
    for name in chain {
        if Some(name.as_str()) != parent && !limited(name) {
            route.push(Some(name.clone()));
        }
    }
    route
}

#[allow(clippy::too_many_arguments)]
async fn run_lane(
    factory: ModelFactory,
    workspace_root: PathBuf,
    state_path: PathBuf,
    brief: String,
    owner: String,
    exa_api_key: Option<String>,
    resume: bool,
    read_only: bool,
    agent: Option<String>,
    profile: Option<String>,
    parent_profile: Option<String>,
    progress_tx: mpsc::UnboundedSender<LaneProgress>,
    approval: Option<LaneApprovalRoute>,
    link: LaneLink,
) -> Result<(String, String, LaneStructuredReport), String> {
    let route = match profile {
        Some(name) => vec![Some(name)],
        None => {
            let chain = crate::config::SnippetConfig::load(&crate::config::default_config_path())
                .await
                .map(|c| crate::routing::configured(&c, crate::routing::Route::Lanes))
                .unwrap_or_default();
            lane_route(None, parent_profile.as_deref(), &chain, |n| {
                crate::routing::limited_until(n).is_some()
            })
        }
    };
    if route.len() <= 1 {
        return run_lane_once(
            factory,
            workspace_root,
            state_path,
            brief,
            owner,
            exa_api_key,
            resume,
            read_only,
            agent,
            route.into_iter().next().flatten(),
            progress_tx,
            approval,
            link,
        )
        .await;
    }
    let (mut brief, mut resume, mut last) = (brief, resume, None);
    for name in route {
        let result = run_lane_once(
            factory.clone(),
            workspace_root.clone(),
            state_path.clone(),
            brief.clone(),
            owner.clone(),
            exa_api_key.clone(),
            resume,
            read_only,
            agent.clone(),
            name.clone(),
            progress_tx.clone(),
            approval.clone(),
            link.clone(),
        )
        .await;
        match result {
            Err(message) if crate::routing::is_limit_error(&message) => {
                let on = name.as_deref().unwrap_or("the previous model");
                if let Some(name) = &name {
                    crate::routing::mark_limited(name, crate::routing::reset_from(None));
                }
                brief = format!(
                    "{brief}\n\n[routing] An earlier attempt at this on {on} stopped partway when that model hit its usage limit. Check the workspace for changes it already made before continuing."
                );
                resume = false;
                last = Some(message);
            }
            other => return other,
        }
    }
    match last {
        Some(message) => Err(message),
        None => {
            run_lane_once(
                factory,
                workspace_root,
                state_path,
                brief,
                owner,
                exa_api_key,
                resume,
                read_only,
                agent,
                None,
                progress_tx,
                approval,
                link,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_lane_once(
    factory: ModelFactory,
    workspace_root: PathBuf,
    state_path: PathBuf,
    brief: String,
    owner: String,
    exa_api_key: Option<String>,
    resume: bool,
    read_only: bool,
    agent: Option<String>,
    profile: Option<String>,
    progress_tx: mpsc::UnboundedSender<LaneProgress>,
    approval: Option<LaneApprovalRoute>,
    link: LaneLink,
) -> Result<(String, String, LaneStructuredReport), String> {
    let lane_session_id = state_path
        .starts_with(crate::config::workspaces_root())
        .then(|| crate::session::session_id_for_state_path(&state_path));
    let mut model = factory(profile.as_deref(), lane_session_id)?;
    let mut log = LaneLog::open(&owner).ok();
    if let Some(log) = log.as_mut() {
        let _ = log.write_start(&owner, &brief, read_only);
    }
    let mut grounding_roots = vec![workspace_root.clone()];
    grounding_roots.extend(link.write_scope.iter().filter(|p| p.is_dir()).cloned());
    let workspace_for_grounding = workspace_root.clone();
    let start_snapshots = snapshot_lane_roots(&workspace_for_grounding, &link.write_scope);
    let context = match ToolContext::with_owner(workspace_root, &owner) {
        Ok(context) => {
            let context = context
                .with_write_scope(link.write_scope.clone())
                .with_read_only_shell(read_only)
                .with_lease_token(link.lease_token)
                .with_agent_id_opt(link.agent_id.clone())
                .with_store_path(
                    link.store_path
                        .clone()
                        .unwrap_or_else(crate::store::default_db_path),
                );
            match link.origin_session.as_deref() {
                Some(session) => context.with_lane_origin(session, &owner),
                None => context,
            }
        }
        Err(error) => {
            let message = error.to_string();
            if let Some(log) = log.as_mut() {
                let _ = log.write_end(&owner, "failed", None, Some(&message), None);
            }
            return Err(message);
        }
    };
    let tools = lane_tool_registry(
        exa_api_key.clone(),
        agent.as_deref(),
        MessageParent {
            lane_id: owner.clone(),
            progress_tx: progress_tx.clone(),
            mailbox: link.mailbox.clone(),
        },
        read_only,
    );
    let identity = agent.as_deref().map(|agent_name| {
        let body = crate::coordination::AgentHome::new(
            crate::coordination::agents_root(&crate::config::snippet_home().join("mission-control")),
            agent_name,
        )
        .ok()
        .and_then(|home| home.read_identity().ok())
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| {
            format!("You are working as the specialized agent `{agent_name}`: bring its domain focus and perspective to this work.")
        });
        (agent_name.to_string(), body)
    });
    let cli_profile = model.cli_agent_profile();
    let defaults = HarnessConfig::default();
    let (context_window_tokens, compact_at_pct) = lane_context(&*model, &defaults);
    let context_window_tokens = crate::agent_context::effective_context_window(
        context_window_tokens,
        crate::agent_context::limit_for(agent.as_deref()),
    );
    let harness = CodingHarness::new(
        HarnessConfig {
            system_prompt: lane_prompt(
                &PromptContext {
                    images: model.supports_images(),
                    ..PromptContext::detect(&workspace_for_grounding, true, false)
                },
                identity
                    .as_ref()
                    .map(|(id, body)| (id.as_str(), body.as_str())),
            ),
            state_path: Some(state_path),
            resume,
            exa_api_key,
            context_window_tokens,
            compact_at_pct,
            progress_tx: Some(progress_tx),
            progress_id: Some(owner.clone()),
            lane_approval: approval,
            lane_mailbox: Some(link.mailbox.clone()),
            session_title: Some(link.title.clone()).filter(|t| !t.trim().is_empty()),
            ..defaults
        },
        tools,
        context,
    );
    let role = if read_only {
        "This is a read-only investigation: your file-editing tools are removed, and you must not change the workspace through the shell either. Investigate and report. "
    } else {
        ""
    };
    let finish_how = lane_finish_instructions(cli_profile.is_some());
    let brief = format!("{brief}\n\n---\n{}\n\n{role}{finish_how}", link.context);
    let run = match cli_profile {
        Some(mut cli) => {
            cli.context_window = context_window_tokens;
            harness.run_cli_lane(cli, brief).await
        }
        None => harness.run(&mut *model, brief).await,
    };
    if let Some(message) = crate::routing::limit_in(&run) {
        if let Some(log) = log.as_mut() {
            let _ = log.write_end(&owner, "failed", None, Some(&message), None);
        }
        return Err(message);
    }
    let outcome = match run {
        Ok(outcome) => outcome,
        Err(error) => {
            let message = error.to_string();
            if let Some(log) = log.as_mut() {
                let _ = log.write_end(&owner, "failed", None, Some(&message), None);
            }
            return Err(message);
        }
    };
    if let Some(log) = log.as_mut() {
        for event in &outcome.events {
            let _ = log.write_event(event);
        }
    }
    let changed = summarize_lane_outcome_with_snapshots(
        &outcome,
        &workspace_for_grounding,
        &link.write_scope,
        &start_snapshots,
    );
    let mut structured = build_structured_report(&outcome, changed);
    let mut grounding_text = structured.summary.clone();
    for f in &structured.findings {
        grounding_text.push('\n');
        grounding_text.push_str(&f.location);
    }
    structured.unverified_citations = verify_grounding(&grounding_roots, &grounding_text);
    let summary = structured.summary.clone();
    let full_text = outcome
        .final_text
        .clone()
        .unwrap_or_else(|| summary.clone());
    if let Some(log) = log.as_mut() {
        let _ = log.write_end(
            &owner,
            "completed",
            Some(outcome.iterations),
            None,
            Some(&summary),
        );
    }
    Ok((summary, full_text, structured))
}

pub(crate) fn lane_finish_instructions(is_cli: bool) -> &'static str {
    if is_cli {
        "When you're done, write your closing response directly (do not call `message_parent` with the final report) using the four headings `Summary` (the outcome, under 600 characters), `Findings` (one `- <claim> (<file:line>)` bullet each), `Tests` and `Open`; do not paste diffs or file contents into the report."
    } else {
        "When you're done, finish with `terminate_loop` giving `summary` (the outcome, under 600 characters), `findings` (one line each with its `file:line`), `tests` and `open`; do not paste diffs or file contents into the report."
    }
}

/// Verify every `path:line` reference in `text` against the workspace and the
/// lane's own write scope (a lane in a worktree cites paths relative to it).
/// Returns only the invalid references (at most 5), or `None` when all resolve.
pub(crate) fn verify_grounding(roots: &[PathBuf], text: &str) -> Option<String> {
    let re = regex::Regex::new(r"([A-Za-z0-9_~./+\-]+\.[A-Za-z0-9_]+):(\d{1,7})\b").ok()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut invalid: Vec<String> = Vec::new();
    for cap in re.captures_iter(text) {
        let path_str = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let line: usize = cap
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        // Require a real-looking path (letters, not a decimal like `3.5:1`).
        if line == 0 || !path_str.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if !seen.insert(format!("{path_str}:{line}")) {
            continue;
        }
        let resolved = if std::path::Path::new(path_str).is_absolute() {
            PathBuf::from(path_str)
        } else {
            roots
                .iter()
                .map(|root| root.join(path_str))
                .find(|candidate| candidate.exists())
                .unwrap_or_else(|| {
                    roots
                        .first()
                        .map(|r| r.join(path_str))
                        .unwrap_or_else(|| PathBuf::from(path_str))
                })
        };
        match std::fs::read_to_string(&resolved) {
            Ok(content) => {
                let lines = content.lines().count();
                if line > lines.max(1) {
                    invalid.push(format!("{path_str}:{line} (file has {lines} lines)"));
                }
            }
            // Missing OR unreadable-as-text (binary): only flag when the file
            // isn't there at all — a binary file's line refs just aren't checkable.
            Err(_) => {
                if !resolved.exists() {
                    invalid.push(format!("{path_str}:{line} (file not found)"));
                }
            }
        }
    }
    if invalid.is_empty() {
        return None;
    }
    invalid.truncate(5);
    Some(invalid.join(", "))
}

pub(crate) fn format_numstat_line(entries: Vec<(String, u64, u64)>) -> Option<String> {
    const MAX_FILES: usize = 12;
    let (files, total_files, add, del) = crate::checkpoint::summarize_numstat(entries, MAX_FILES)?;
    let list: Vec<String> = files
        .into_iter()
        .map(|(path, a, d)| format!("{path} (+{a}/-{d})"))
        .collect();
    Some(format!(
        "{total_files} files (+{add}/-{del}): {}",
        list.join(", ")
    ))
}

fn is_git_root(dir: &Path) -> bool {
    dir.join(".git").exists()
        || std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("rev-parse")
            .arg("--is-inside-work-tree")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

fn lane_diff_roots(workspace: &Path, write_scope: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for p in write_scope {
        if p.is_dir() && !p.starts_with(workspace) && !roots.contains(p) {
            roots.push(p.clone());
        }
    }
    if roots.is_empty() {
        roots.push(workspace.to_path_buf());
    }
    roots
}

fn snapshot_lane_roots(workspace: &Path, write_scope: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for root in lane_diff_roots(workspace, write_scope) {
        if is_git_root(&root)
            && let Ok(snap) = crate::checkpoint::tree_snapshot(&root)
        {
            out.push((root, snap));
        }
    }
    out
}

fn matches_write_scope(root: &Path, rel_path: &str, write_scope: &[PathBuf]) -> bool {
    if write_scope.is_empty() {
        return true;
    }
    let full = root.join(rel_path);
    write_scope
        .iter()
        .any(|s| full.starts_with(s) || s.starts_with(&full) || Path::new(rel_path).starts_with(s))
}

fn scan_change_files_events(outcome: &crate::harness::HarnessOutcome) -> Option<String> {
    use crate::harness::HarnessEvent;

    let mut changed: Vec<String> = Vec::new();
    for event in &outcome.events {
        let HarnessEvent::ToolCall {
            tool_name,
            arguments,
        } = event
        else {
            continue;
        };
        if tool_name != "change_files" {
            continue;
        }
        let paths = arguments
            .get("changes")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("path").and_then(|v| v.as_str()));
        for path in paths {
            if !changed.iter().any(|p| p == path) {
                changed.push(path.to_string());
            }
        }
    }
    if changed.is_empty() {
        None
    } else {
        Some(format!("{} files: {}", changed.len(), changed.join(", ")))
    }
}

fn summarize_lane_outcome_with_snapshots(
    outcome: &crate::harness::HarnessOutcome,
    workspace: &Path,
    write_scope: &[PathBuf],
    snapshots: &[(PathBuf, String)],
) -> Option<String> {
    if !snapshots.is_empty() {
        let mut all_entries = Vec::new();
        for (root, start) in snapshots {
            if let Ok(end) = crate::checkpoint::tree_snapshot(root)
                && let Some(entries) = crate::checkpoint::numstat(root, start, &end)
            {
                for (path, a, d) in entries {
                    if matches_write_scope(root, &path, write_scope) {
                        all_entries.push((path, a, d));
                    }
                }
            }
        }
        return format_numstat_line(all_entries);
    }
    summarize_lane_outcome(outcome, workspace, write_scope)
}

/// Measure changed files in the lane's write scope using git numstat (against `HEAD`
/// when no start snapshot was passed), falling back to the `change_files` tool-call
/// scan when the workspace is not a git repository.
pub(crate) fn summarize_lane_outcome(
    outcome: &crate::harness::HarnessOutcome,
    workspace: &Path,
    write_scope: &[PathBuf],
) -> Option<String> {
    let roots = lane_diff_roots(workspace, write_scope);
    if roots.iter().any(|r| is_git_root(r)) {
        let mut all_entries = Vec::new();
        for root in &roots {
            if !is_git_root(root) {
                continue;
            }
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args([
                    "-c",
                    "core.quotepath=off",
                    "diff",
                    "--numstat",
                    "--no-renames",
                    "HEAD",
                ])
                .output()
                .ok();
            if let Some(out) = output
                && out.status.success()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                for (path, a, d) in crate::checkpoint::parse_numstat(&text) {
                    if matches_write_scope(root, &path, write_scope) {
                        all_entries.push((path, a, d));
                    }
                }
            }
        }
        return format_numstat_line(all_entries);
    }
    scan_change_files_events(outcome)
}

fn clip_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

pub(crate) fn build_structured_report(
    outcome: &crate::harness::HarnessOutcome,
    changed: Option<String>,
) -> LaneStructuredReport {
    if let Some(closing) = &outcome.closing {
        let summary = closing
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| clip_chars(s, 600))
            .unwrap_or_else(|| {
                outcome
                    .final_text
                    .as_deref()
                    .map(|s| clip_chars(s.trim(), 600))
                    .unwrap_or_else(|| "completed".to_string())
            });
        let findings = closing
            .get("findings")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let claim = item.get("claim")?.as_str()?.trim();
                        let location = item
                            .get("where")
                            .or_else(|| item.get("location"))?
                            .as_str()?
                            .trim();
                        if claim.is_empty() || location.is_empty() {
                            return None;
                        }
                        Some(LaneFinding {
                            claim: clip_chars(claim, 200),
                            location: location.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tests = closing
            .get("tests")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| clip_chars(s, 300));
        let open = closing
            .get("open")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| clip_chars(s, 300));
        return LaneStructuredReport {
            summary,
            findings,
            changed,
            tests,
            open,
            unverified_citations: None,
            clipped: false,
        };
    }

    let raw = outcome
        .final_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("lane completed without a summary");

    if let Some(mut parsed) = parse_cli_headings(raw) {
        parsed.changed = changed;
        return parsed;
    }

    const PROSE_CAP: usize = 1500;
    let char_count = raw.chars().count();
    let (summary, clipped) = if char_count > PROSE_CAP {
        let head: String = raw.chars().take(PROSE_CAP).collect();
        (
            format!("{head} (clipped; full text on the lane record)"),
            true,
        )
    } else {
        (raw.to_string(), false)
    };
    LaneStructuredReport {
        summary,
        findings: Vec::new(),
        changed,
        tests: None,
        open: None,
        unverified_citations: None,
        clipped,
    }
}

pub(crate) fn parse_cli_headings(text: &str) -> Option<LaneStructuredReport> {
    let mut section: Option<&str> = None;
    let mut summary_lines = Vec::new();
    let mut findings = Vec::new();
    let mut tests_lines = Vec::new();
    let mut open_lines = Vec::new();
    let mut saw_heading = false;

    for line in text.lines() {
        let trimmed = line.trim();
        let heading = trimmed
            .trim_start_matches('#')
            .trim_start_matches('*')
            .trim_end_matches('*')
            .trim_end_matches(':')
            .trim();
        let lower = heading.to_ascii_lowercase();
        let is_marker = trimmed.starts_with('#')
            || trimmed.starts_with("**")
            || trimmed.ends_with(':')
            || matches!(lower.as_str(), "summary" | "findings" | "tests" | "open");
        if is_marker && matches!(lower.as_str(), "summary" | "findings" | "tests" | "open") {
            saw_heading = true;
            section = match lower.as_str() {
                "summary" => Some("summary"),
                "findings" => Some("findings"),
                "tests" => Some("tests"),
                "open" => Some("open"),
                _ => None,
            };
            continue;
        }
        match section {
            Some("summary") => summary_lines.push(trimmed),
            Some("findings") => {
                let item = trimmed.trim_start_matches(['-', '*']).trim();
                if item.is_empty() {
                    continue;
                }
                if let Some((claim, loc)) = split_finding_bullet(item) {
                    findings.push(LaneFinding {
                        claim: clip_chars(claim, 200),
                        location: loc.to_string(),
                    });
                }
            }
            Some("tests") => tests_lines.push(trimmed),
            Some("open") => open_lines.push(trimmed),
            _ => {}
        }
    }

    if !saw_heading {
        return None;
    }
    let summary = summary_lines.join(" ").trim().to_string();
    if summary.is_empty() {
        return None;
    }
    let join_opt = |lines: Vec<&str>, cap: usize| {
        let s = lines.join(" ").trim().to_string();
        (!s.is_empty() && !s.eq_ignore_ascii_case("none")).then(|| clip_chars(&s, cap))
    };
    Some(LaneStructuredReport {
        summary: clip_chars(&summary, 600),
        findings,
        changed: None,
        tests: join_opt(tests_lines, 300),
        open: join_opt(open_lines, 300),
        unverified_citations: None,
        clipped: false,
    })
}

fn split_finding_bullet(item: &str) -> Option<(&str, &str)> {
    if let Some(open) = item.rfind('(')
        && let Some(close) = item[open + 1..].find(')')
    {
        let loc = item[open + 1..open + 1 + close].trim().trim_matches('`');
        let claim = item[..open].trim();
        if !claim.is_empty() && loc.contains(':') {
            return Some((claim, loc));
        }
    }
    None
}

fn lane_context(model: &dyn AgentModel, defaults: &HarnessConfig) -> (u64, u8) {
    match model.cli_agent_profile() {
        Some(cli) => (cli.context_window, cli.compact_at_pct),
        None => model
            .context_settings()
            .unwrap_or((defaults.context_window_tokens, defaults.compact_at_pct)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn grounding_resolves_against_the_lane_write_scope_too() {
        let tmp = std::env::temp_dir().join(format!("grounding-{}", uuid::Uuid::new_v4().simple()));
        let workspace = tmp.join("workspace");
        let worktree = tmp.join("worktree");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(workspace.join("src/a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(worktree.join("src/b.rs"), "one\ntwo\nthree\n").unwrap();
        let roots = vec![workspace.clone(), worktree.clone()];
        assert!(super::verify_grounding(&roots, "see src/a.rs:2 and src/b.rs:3").is_none());
        let bad = super::verify_grounding(&roots, "see src/b.rs:9 and src/c.rs:1").unwrap();
        assert!(bad.contains("src/b.rs:9 (file has 3 lines)"), "{bad}");
        assert!(bad.contains("src/c.rs:1 (file not found)"), "{bad}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn tool_bytes(defs: &[crate::llm::NativeToolDefinition]) -> Vec<(String, usize)> {
        let mut sizes: Vec<(String, usize)> = defs
            .iter()
            .map(|d| {
                (
                    d.name.clone(),
                    d.name.len() + d.description.len() + d.input_schema.to_string().len(),
                )
            })
            .collect();
        sizes.sort_by(|a, b| b.1.cmp(&a.1));
        sizes
    }

    /// Bytes every request re-sends for tool definitions, per role. Bounds sit just
    /// above today's sizes; lower them as descriptions get tighter.
    #[test]
    fn tool_definition_budget_per_role() {
        for role in ["work", "mission_control", "desk", "lane"] {
            let (defs, cap) = role_tool_definitions(role, None).unwrap();
            let sizes = tool_bytes(&defs);
            let total: usize = sizes.iter().map(|s| s.1).sum();
            let top: Vec<String> = sizes
                .iter()
                .take(15)
                .map(|(n, b)| format!("{n} {b}"))
                .collect();
            println!(
                "{role}: {} tools, {total} bytes; largest: {}",
                sizes.len(),
                top.join(", ")
            );
            assert!(
                total <= cap,
                "{role} tool definitions are {total} bytes, over {cap}"
            );
        }
    }

    #[test]
    fn lane_route_order() {
        let chain = vec!["top".to_string(), "mid".to_string(), "low".to_string()];
        let none = |_: &str| false;
        assert_eq!(
            lane_route(Some("pin"), Some("mid"), &chain, none),
            vec![Some("pin".into()), Some("mid".into())]
        );
        assert_eq!(
            lane_route(Some("pin"), Some("pin"), &chain, none),
            vec![Some("pin".to_string())]
        );
        let route = lane_route(None, Some("mid"), &chain, none);
        assert_eq!(
            route,
            vec![Some("mid".into()), Some("top".into()), Some("low".into())]
        );
        let route = lane_route(None, Some("mid"), &chain, |n| n == "mid" || n == "low");
        assert_eq!(route, vec![Some("top".to_string())]);
        assert_eq!(
            lane_route(None, None, &chain[..1], none),
            vec![None, Some("top".to_string())]
        );
    }

    use super::*;

    fn test_record() -> LaneRecord {
        LaneRecord {
            id: "lane-1".to_string(),
            title: "audit".to_string(),
            status: LaneStatus::Running,
            handoff: Some("inspect the service".to_string()),
            summary: None,
            report: None,
            structured: None,
            error: None,
            started_at: "2025-01-01T00:00:00Z".to_string(),
            finished_at: None,
            activity: None,
            activity_kind: None,
            activity_at: None,
            activity_log: Vec::new(),
            read_only: true,
            agent: None,
            profile: None,
            transcript: None,
            paths: Vec::new(),
        }
    }

    #[test]
    fn activity_log_is_bounded_and_keeps_latest_entries() {
        let (result_tx, _result_rx) = mpsc::unbounded_channel();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        let mut manager = LaneManager::new(
            None,
            PathBuf::from("."),
            PathBuf::from("."),
            result_tx,
            progress_tx,
            None,
        )
        .with_records(vec![test_record()]);

        for i in 0..30 {
            manager.record_progress(&LaneProgress {
                id: "lane-1".to_string(),
                kind: "tool_call".to_string(),
                text: format!("running tool {i}"),
            });
        }

        let record = &manager.records()[0];
        assert_eq!(record.activity_log.len(), 24);
        assert_eq!(record.activity_log.first().unwrap().text, "running tool 6");
        assert_eq!(record.activity.as_deref(), Some("running tool 29"));
    }

    #[test]
    fn lane_record_accepts_state_written_before_activity_fields() {
        let mut value = serde_json::to_value(test_record()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("activity");
        object.remove("activity_kind");
        object.remove("activity_at");
        object.remove("activity_log");
        object.remove("read_only");
        object.remove("agent");
        object.remove("profile");
        let restored: LaneRecord = serde_json::from_value(value).unwrap();
        assert!(restored.activity.is_none());
        assert!(restored.activity_log.is_empty());
        assert!(!restored.read_only);
        assert!(restored.agent.is_none());
        assert!(restored.profile.is_none());
    }

    #[test]
    fn lane_record_serializes_and_deserializes_agent_and_profile() {
        let mut record = test_record();
        record.agent = Some("reviewer".to_string());
        record.profile = Some("claude-haiku".to_string());
        let val = serde_json::to_value(&record).unwrap();
        assert_eq!(val["agent"], "reviewer");
        assert_eq!(val["profile"], "claude-haiku");
        let restored: LaneRecord = serde_json::from_value(val).unwrap();
        assert_eq!(restored.agent.as_deref(), Some("reviewer"));
        assert_eq!(restored.profile.as_deref(), Some("claude-haiku"));
    }

    fn finished_record(id: &str, finished_at: &str) -> LaneRecord {
        let mut r = test_record();
        r.id = id.to_string();
        r.status = LaneStatus::Completed;
        r.finished_at = Some(finished_at.to_string());
        r.started_at = finished_at.to_string();
        r
    }

    fn empty_manager(lane_root: PathBuf) -> LaneManager {
        let (result_tx, _result_rx) = mpsc::unbounded_channel();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel();
        LaneManager::new(
            None,
            PathBuf::from("."),
            lane_root,
            result_tx,
            progress_tx,
            None,
        )
    }

    #[test]
    fn overlapping_writing_lanes_are_refused_but_readers_and_disjoint_paths_are_not() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-scope-{}", std::process::id()));
        let mut running = test_record();
        running.status = LaneStatus::Running;
        running.read_only = false;
        running.paths = vec!["src/serve".into()];
        let mgr = empty_manager(dir).with_records(vec![running]);
        let clash = mgr
            .scope_clash(false, &["src/serve/mod.rs".into()])
            .unwrap();
        assert!(clash.contains("overlaps"), "{clash}");
        assert!(mgr.scope_clash(false, &["src/lanes.rs".into()]).is_none());
        assert!(mgr.scope_clash(true, &["src/serve".into()]).is_none());
    }

    #[test]
    fn housekeep_drops_finished_past_ttl_keeps_running() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-ttl-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let old = "2015-01-01T00:00:00Z";
        let recent = Utc::now().to_rfc3339();
        let mgr = empty_manager(dir.clone()).with_records(vec![
            finished_record("lane-1", old),
            {
                let mut run = test_record();
                run.id = "lane-2".into();
                run.status = LaneStatus::Running;
                run.finished_at = None;
                run
            },
            finished_record("lane-3", &recent),
        ]);
        // with_records already housekeeps — old finished should be gone.
        let ids: Vec<_> = mgr.records().iter().map(|r| r.id.as_str()).collect();
        assert!(
            !ids.contains(&"lane-1"),
            "ttl-expired finished must drop: {ids:?}"
        );
        assert!(ids.contains(&"lane-2"), "running must stay: {ids:?}");
        assert!(
            ids.contains(&"lane-3"),
            "recent finished must stay: {ids:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn housekeep_enforces_max_finished_count() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-cap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Build MAX_FINISHED_LANES + 5 finished, all "now" so TTL doesn't bite.
        let now = Utc::now();
        let mut recs = Vec::new();
        for i in 1..=(MAX_FINISHED_LANES + 5) {
            // Stagger finished_at so oldest are lane-1..lane-5
            let when = (now - chrono::Duration::seconds(i as i64)).to_rfc3339();
            recs.push(finished_record(&format!("lane-{i}"), &when));
        }
        // One running must survive regardless of count.
        let mut run = test_record();
        run.id = format!("lane-{}", MAX_FINISHED_LANES + 100);
        run.status = LaneStatus::Running;
        recs.push(run);

        let mgr = empty_manager(dir.clone()).with_records(recs);
        let finished: Vec<_> = mgr
            .records()
            .iter()
            .filter(|r| r.status != LaneStatus::Running)
            .collect();
        assert_eq!(finished.len(), MAX_FINISHED_LANES);
        assert!(
            mgr.records()
                .iter()
                .any(|r| r.status == LaneStatus::Running),
            "running lane must be retained"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn housekeep_deletes_disk_files_and_orphans() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Referenced recent finished + orphan file with no record.
        let recent = Utc::now().to_rfc3339();
        std::fs::write(dir.join("lane-1.json"), b"{}").unwrap();
        std::fs::write(dir.join("lane-1.meta.json"), b"{}").unwrap();
        std::fs::write(dir.join("lane-99.json"), b"orphan").unwrap();
        std::fs::write(dir.join("lane-99.meta.json"), b"orphan").unwrap();
        // Ancient finished with files — should drop record + files.
        std::fs::write(dir.join("lane-2.json"), b"old").unwrap();
        std::fs::write(dir.join("lane-2.meta.json"), b"old").unwrap();

        let mgr = empty_manager(dir.clone()).with_records(vec![
            finished_record("lane-1", &recent),
            finished_record("lane-2", "2015-06-01T00:00:00Z"),
        ]);

        let ids: Vec<_> = mgr.records().iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, vec!["lane-1".to_string()]);
        assert!(dir.join("lane-1.json").exists());
        assert!(
            !dir.join("lane-2.json").exists(),
            "ttl drop must delete files"
        );
        assert!(!dir.join("lane-99.json").exists(), "orphan must be swept");
        assert!(!dir.join("lane-99.meta.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_lane_ignores_late_terminal_result() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-cancel-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut manager = empty_manager(dir.clone()).with_records(vec![test_record()]);
        assert_eq!(
            manager.cancel("lane-1", "parent taking over").unwrap(),
            "audit"
        );
        assert!(!manager.record_result(&LaneResult {
            id: "lane-1".to_string(),
            title: "audit".to_string(),
            status: LaneStatus::Completed,
            summary: Some("late result".to_string()),
            report: None,
            error: None,
            structured: None,
        }));
        let record = &manager.records()[0];
        assert_eq!(record.status, LaneStatus::Cancelled);
        assert_eq!(
            record.error.as_deref(),
            Some("cancelled by parent: parent taking over")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_records_preserves_counter_past_pruned_ids() {
        let dir = std::env::temp_dir().join(format!("snippet-lane-hk-ctr-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mgr = empty_manager(dir.clone()).with_records(vec![
            finished_record("lane-40", "2015-01-01T00:00:00Z"), // pruned by TTL
            finished_record("lane-41", &Utc::now().to_rfc3339()),
        ]);
        // Next spawn id should be lane-42, not lane-1.
        assert_eq!(mgr.counter, 41);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_context_uses_profile_for_api_and_cli_and_falls_back_to_defaults() {
        let defaults = HarnessConfig::default();

        let api_profile = crate::config::InferenceProfileConfig {
            provider: "gemini".to_string(),
            model: "gemini-4-carbon".to_string(),
            context_window: 1_048_576,
            compact_at_pct: 85,
            ..crate::config::InferenceProfileConfig::default()
        };
        let api_model = api_profile.build_model_for_session(Some("lane-api".to_string()));
        assert_eq!(lane_context(&*api_model, &defaults), (1_048_576, 85));

        let cli_profile = crate::config::InferenceProfileConfig {
            provider: "claude-code".to_string(),
            model: "sonnet".to_string(),
            context_window: 500_000,
            compact_at_pct: 80,
            ..crate::config::InferenceProfileConfig::default()
        };
        let cli_model = cli_profile.build_model_for_session(Some("lane-cli".to_string()));
        assert_eq!(lane_context(&*cli_model, &defaults), (500_000, 80));

        let unconfigured = crate::config::InferenceProfileConfig {
            provider: "openai".to_string(),
            context_window: 0,
            compact_at_pct: 75,
            ..crate::config::InferenceProfileConfig::default()
        };
        let fallback_model = unconfigured.build_model_for_session(None);
        assert_eq!(
            lane_context(&*fallback_model, &defaults),
            (defaults.context_window_tokens, defaults.compact_at_pct)
        );
    }

    #[test]
    fn summarize_lane_outcome_uses_numstat_in_git_repo_and_scan_otherwise() {
        let tmp =
            std::env::temp_dir().join(format!("lane-numstat-{}", uuid::Uuid::new_v4().simple()));
        let git_dir = tmp.join("git_ws");
        let plain_dir = tmp.join("plain_ws");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&plain_dir).unwrap();

        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&git_dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init", "-q"]);
        std::fs::write(git_dir.join("a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(git_dir.join("b.rs"), "alpha\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "init"]);

        let snaps = super::snapshot_lane_roots(&git_dir, &[]);
        std::fs::write(git_dir.join("a.rs"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(git_dir.join("b.rs"), "beta\ngamma\n").unwrap();

        let outcome = crate::harness::HarnessOutcome {
            final_text: Some("done".into()),
            events: vec![crate::harness::HarnessEvent::ToolCall {
                tool_name: "change_files".into(),
                arguments: serde_json::json!({
                    "changes": [{"path": "only_from_scan.rs"}]
                }),
            }],
            iterations: 1,
            closing: None,
        };

        let measured =
            super::summarize_lane_outcome_with_snapshots(&outcome, &git_dir, &[], &snaps)
                .expect("numstat line");
        assert_eq!(measured, "2 files (+4/-1): b.rs (+2/-1), a.rs (+2/-0)");

        let head_measured =
            super::summarize_lane_outcome(&outcome, &git_dir, &[]).expect("numstat against HEAD");
        assert_eq!(head_measured, "2 files (+4/-1): b.rs (+2/-1), a.rs (+2/-0)");

        let fallback =
            super::summarize_lane_outcome(&outcome, &plain_dir, &[]).expect("fallback scan");
        assert_eq!(fallback, "1 files: only_from_scan.rs");

        let _ = std::fs::remove_dir_all(crate::checkpoint::shadow_dir(&git_dir));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cli_headings_and_finish_instructions_match_spec() {
        let api_footer = super::lane_finish_instructions(false);
        assert!(api_footer.contains("terminate_loop"));
        assert!(api_footer.contains("summary"));
        assert!(api_footer.contains("findings"));
        assert!(api_footer.contains("do not paste diffs"));

        let cli_footer = super::lane_finish_instructions(true);
        assert!(cli_footer.contains("Summary"));
        assert!(cli_footer.contains("Findings"));
        assert!(cli_footer.contains("Tests"));
        assert!(cli_footer.contains("Open"));

        let text = "\
## Summary
Tightened lane reporting.

## Findings
- Added findings schema (src/meta.rs:163)
- Cut envelope boilerplate (`src/harness/events.rs:826`)

## Tests
cargo test --lib lanes:: ok

## Open
None
";
        let parsed = super::parse_cli_headings(text).expect("parsed headings");
        assert_eq!(parsed.summary, "Tightened lane reporting.");
        assert_eq!(parsed.findings.len(), 2);
        assert_eq!(parsed.findings[0].location, "src/meta.rs:163");
        assert_eq!(parsed.findings[1].location, "src/harness/events.rs:826");
        assert_eq!(parsed.tests.as_deref(), Some("cargo test --lib lanes:: ok"));
        assert!(parsed.open.is_none());
    }
}
