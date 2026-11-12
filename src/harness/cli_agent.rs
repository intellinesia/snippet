use super::*;

use std::collections::{HashMap, HashSet};
use std::process::Stdio;

use axum::extract::{Path as AxPath, Request, State as AxState};
use axum::http::StatusCode;
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use crate::config::InferenceProfileConfig;
use crate::llm::{ModelOutput, StreamBuffer, StreamHandle};

const MCP_SERVER: &str = "snippet";
const MCP_PREFIX: &str = "mcp__snippet__";
const QUIET_TOOLS: [&str; 3] = ["update_plan", "ask_user", "set_session_title"];
const COMMAND_MARK: &str = "\u{1}command\u{1}";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CliCompaction {
    Idle,
    Saving,
    Compacting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliKind {
    ClaudeCode,
    Antigravity,
    Devin,
}

impl CliKind {
    fn memory_save_after(self) -> std::time::Duration {
        match self {
            CliKind::ClaudeCode | CliKind::Devin => std::time::Duration::from_secs(50 * 60),
            CliKind::Antigravity => std::time::Duration::from_secs(3 * 60),
        }
    }

    fn of(provider: &str) -> Self {
        match provider {
            "antigravity" => Self::Antigravity,
            "devin" => Self::Devin,
            _ => Self::ClaudeCode,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Antigravity => "Antigravity",
            Self::Devin => "Devin",
        }
    }

    fn dir(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Antigravity => "antigravity",
            Self::Devin => "devin",
        }
    }

    fn pooled_provider(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("claude-code"),
            Self::Devin => Some("devin"),
            Self::Antigravity => None,
        }
    }

    fn user_line(self, text: &str) -> Value {
        let message = json!({"role": "user", "content": text});
        match self {
            Self::ClaudeCode => json!({"type": "user", "message": message}),
            Self::Antigravity => json!({"event": "user", "message": message}),
            Self::Devin => json!({"type": "text", "text": text}),
        }
    }
}

struct CliSession {
    kind: CliKind,
    key: String,
    id: Option<String>,
    started: bool,
    synced: usize,
    tool_guide: String,
    forking: bool,
    mark: String,
    tokens: u64,
    compactions: u64,
}

impl CliSession {
    fn open(kind: CliKind, snippet_session: &str, reuse: bool) -> Self {
        let key = sanitize_key(snippet_session);
        let file = Self::sidecar(kind, &key);
        let raw = std::fs::read_to_string(&file).unwrap_or_default();
        let mut lines = raw.lines();
        let existing = lines
            .next()
            .map(|id| id.trim().to_string())
            .filter(|id| reuse && !id.is_empty());
        let synced = lines
            .next()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        let (mut forking, mut mark, mut tokens, mut compactions) =
            (false, String::new(), 0u64, 0u64);
        for line in lines {
            let line = line.trim();
            if line == "fork" {
                forking = true;
            } else if let Some(v) = line.strip_prefix("m=") {
                mark = v.to_string();
            } else if let Some(v) = line.strip_prefix("t=") {
                tokens = v.parse().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("c=") {
                compactions = v.parse().unwrap_or(0);
            }
        }
        let fresh = |id: Option<String>| Self {
            kind,
            key: key.clone(),
            id,
            started: false,
            synced: 0,
            tool_guide: String::new(),
            forking: false,
            mark: String::new(),
            tokens: 0,
            compactions: 0,
        };
        let session = match (kind, existing) {
            (_, Some(id)) => Self {
                kind,
                key: key.clone(),
                id: Some(id),
                started: true,
                synced,
                tool_guide: String::new(),
                forking,
                mark,
                tokens,
                compactions,
            },
            (CliKind::ClaudeCode, None) => fresh(Some(uuid::Uuid::new_v4().to_string())),
            (CliKind::Antigravity | CliKind::Devin, None) => fresh(None),
        };
        if !session.started {
            session.save();
        }
        session
    }

    fn sidecar(kind: CliKind, key: &str) -> PathBuf {
        let dir = crate::config::snippet_home()
            .join(kind.dir())
            .join("sessions");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(key)
    }

    fn save(&self) {
        let file = Self::sidecar(self.kind, &self.key);
        match self.id.as_deref() {
            Some(id) => {
                let flag = if self.forking { "\nfork" } else { "" };
                let _ = std::fs::write(
                    file,
                    format!(
                        "{id}\n{}{flag}\nm={}\nt={}\nc={}",
                        self.synced, self.mark, self.tokens, self.compactions
                    ),
                );
            }
            None => {
                let _ = std::fs::remove_file(file);
            }
        }
    }

    fn adopt(&mut self, id: &str) {
        self.started = true;
        if self.id.as_deref() != Some(id) {
            self.id = Some(id.to_string());
            self.save();
        }
    }

    fn mark_synced(&mut self, messages: &[HarnessMessage], compactions: u64, tokens: u64) {
        let upto = messages.len();
        let mark = transcript_mark(messages, upto);
        if self.synced != upto
            || self.mark != mark
            || self.compactions != compactions
            || self.tokens != tokens
        {
            self.synced = upto;
            self.mark = mark;
            self.compactions = compactions;
            self.tokens = tokens;
            self.save();
        }
    }

    fn restart(&mut self) {
        self.id = match self.kind {
            CliKind::ClaudeCode => Some(uuid::Uuid::new_v4().to_string()),
            CliKind::Antigravity | CliKind::Devin => None,
        };
        self.started = false;
        self.forking = false;
        self.synced = 0;
        self.mark.clear();
        self.tokens = 0;
        self.save();
    }
}

fn transcript_mark(messages: &[HarnessMessage], upto: usize) -> String {
    use std::hash::{Hash, Hasher};
    if upto == 0 || upto > messages.len() {
        return String::new();
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    upto.hash(&mut h);
    match &messages[upto - 1] {
        HarnessMessage::User { content } => ("user", content.as_str()).hash(&mut h),
        HarnessMessage::Assistant {
            content,
            tool_calls,
        } => {
            ("assistant", content.as_str()).hash(&mut h);
            for call in tool_calls {
                call.name.hash(&mut h);
            }
        }
        HarnessMessage::ToolResult {
            tool_name,
            tool_call_id,
            ..
        } => ("tool", tool_name.as_str(), tool_call_id.as_str()).hash(&mut h),
        HarnessMessage::Summary { kind, content } => {
            ("summary", kind.as_str(), content.as_str()).hash(&mut h)
        }
        HarnessMessage::System { .. } => "system".hash(&mut h),
    }
    format!("{:016x}", h.finish())
}

const HANDOFF_BUDGET: usize = 40_000;

fn clip_chars(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>())
    } else {
        text.to_string()
    }
}

fn call_target(arguments: &Value) -> Option<String> {
    for key in [
        "file_path",
        "path",
        "command",
        "cmd",
        "query",
        "pattern",
        "url",
        "description",
        "title",
    ] {
        if let Some(v) = arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return Some(clip_chars(&v.replace('\n', " "), 100));
        }
    }
    None
}

fn touched_paths(arguments: &Value, out: &mut Vec<String>) {
    let mut push = |p: &str| {
        let p = p.trim();
        if !p.is_empty() && !out.iter().any(|x| x == p) {
            out.push(p.to_string());
        }
    };
    for key in ["file_path", "path", "new_path"] {
        if let Some(p) = arguments.get(key).and_then(Value::as_str) {
            push(p);
        }
    }
    for key in ["paths", "files", "changes"] {
        if let Some(list) = arguments.get(key).and_then(Value::as_array) {
            for item in list {
                match item {
                    Value::String(p) => push(p),
                    Value::Object(_) => {
                        for k in ["path", "file_path", "new_path"] {
                            if let Some(p) = item.get(k).and_then(Value::as_str) {
                                push(p);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn writes_files(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "edit", "write", "change", "patch", "create", "delete", "move", "rename", "replace",
    ]
    .iter()
    .any(|w| n.contains(w))
}

struct Handoff {
    text: String,
    dropped: usize,
}

const DURABLE_SAVE: &str = "First save to global, project or agent memory what holds beyond this conversation: user preferences and rules, decisions and why, root causes, project facts and conventions. This conversation's own working state belongs in the summary, not in memory. Replace stale entries instead of adding duplicates, remove what is no longer true, and commit. An agent curates its own agent memory here and nowhere else: keep what still earns its place, rewrite what changed, and evict what is stale. ";

fn handoff(messages: &[HarnessMessage], fresh: bool) -> Option<Handoff> {
    let mut entries: Vec<String> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    let mut summary: Option<String> = None;
    for m in messages {
        match m {
            HarnessMessage::User { content } if !content.trim().is_empty() => {
                entries.push(format!("user: {}", clip_chars(content, 2_000)));
            }
            HarnessMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut line = format!("assistant: {}", clip_chars(content, 2_000));
                for call in tool_calls {
                    match call_target(&call.arguments) {
                        Some(t) => line.push_str(&format!("\n  · {} {t}", call.name)),
                        None => line.push_str(&format!("\n  · {}", call.name)),
                    }
                    if writes_files(&call.name) {
                        touched_paths(&call.arguments, &mut touched);
                    }
                }
                if !content.trim().is_empty() || !tool_calls.is_empty() {
                    entries.push(line);
                }
            }
            HarnessMessage::Summary { content, .. } if !content.trim().is_empty() => {
                summary = Some(clip_chars(content, 8_000));
            }
            _ => {}
        }
    }
    if entries.is_empty() && summary.is_none() {
        return None;
    }
    let mut used = 0;
    let mut keep = entries.len();
    while keep > 0 && used + entries[keep - 1].len() <= HANDOFF_BUDGET {
        used += entries[keep - 1].len();
        keep -= 1;
    }
    let dropped = keep;
    let dropped_requests: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            HarnessMessage::User { content } if !content.trim().is_empty() => {
                Some(content.as_str())
            }
            _ => None,
        })
        .take(
            entries[..keep]
                .iter()
                .filter(|e| e.starts_with("user: "))
                .count(),
        )
        .map(|c| clip_chars(&c.replace('\n', " "), 200))
        .collect();
    entries.drain(..keep);
    let mut out = String::from("<conversation_so_far>\n");
    out.push_str(if fresh {
        "This session's conversation so far, from before this runtime joined it. Treat it as your own history.\n"
    } else {
        "What happened in this session while another runtime was handling it, since your last turn. Treat it as your own history.\n"
    });
    if let Some(summary) = summary {
        out.push_str(&format!("\nSummary of earlier work:\n{summary}\n"));
    }
    if !dropped_requests.is_empty() {
        let skip = dropped_requests.len().saturating_sub(40);
        let earlier = if skip > 0 {
            format!("({skip} earlier requests not listed)\n")
        } else {
            String::new()
        };
        out.push_str(&format!(
            "\nEarlier requests, oldest first ({dropped} older messages are not shown in full):\n{earlier}- {}\n",
            dropped_requests[skip..].join("\n- ")
        ));
    }
    if !entries.is_empty() {
        out.push_str("\n");
        out.push_str(&entries.join("\n\n"));
        out.push('\n');
    }
    if !touched.is_empty() {
        let shown: Vec<&String> = touched.iter().take(40).collect();
        out.push_str(&format!(
            "\nFiles changed in that time{}:\n{}\n",
            if touched.len() > shown.len() {
                format!(" (first {})", shown.len())
            } else {
                String::new()
            },
            shown
                .iter()
                .map(|p| format!("- {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    out.push_str("</conversation_so_far>\n\n");
    Some(Handoff { text: out, dropped })
}

#[derive(Default)]
struct CliParse {
    tool_names: HashMap<String, String>,
    agy_text: HashMap<i64, String>,
    agy_tools: HashSet<i64>,
    devin_text: String,
    devin_tools: HashSet<String>,
    devin_window: u64,
    calls: Vec<crate::llm::TokenUsage>,
    claude_call: Option<crate::llm::TokenUsage>,
    served: ServedResults,
    envelope_marker: Option<PathBuf>,
}

pub struct CliAgentModel {
    profile: InferenceProfileConfig,
}

impl CliAgentModel {
    pub fn new(profile: InferenceProfileConfig) -> Self {
        Self { profile }
    }
}

#[async_trait::async_trait]
impl AgentModel for CliAgentModel {
    async fn generate(
        &mut self,
        _messages: &[HarnessMessage],
        _tools: &[NativeToolDefinition],
        _force_tool: bool,
        _sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        Err(ToolError::msg(format!(
            "the {} profile runs through its CLI and can't serve a direct model call",
            self.profile.provider
        )))
    }

    fn supports_images(&self) -> bool {
        true
    }

    fn cli_agent_profile(&self) -> Option<InferenceProfileConfig> {
        Some(self.profile.clone())
    }
}

async fn run_probe(bin: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn claude_rate_limits_path() -> PathBuf {
    crate::config::snippet_home().join("claude-code/rate_limits.json")
}

pub fn claude_rate_limits() -> Option<crate::llm::RateLimitSnapshot> {
    serde_json::from_str(&std::fs::read_to_string(claude_rate_limits_path()).ok()?).ok()
}

pub async fn cli_agent_status(provider: &str) -> Value {
    let kind = CliKind::of(provider);
    let bin = match kind {
        CliKind::ClaudeCode => claude_binary(),
        CliKind::Antigravity => crate::antigravity::binary(),
        CliKind::Devin => crate::devin::binary(),
    };
    let Some(bin) = bin else {
        return json!({"provider": provider, "label": kind.label(), "installed": false, "signed_in": false});
    };
    let version = run_probe(&bin, &["--version"]).await.map(|v| {
        let mut parts = v.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("devin"), Some(ver)) => ver.to_string(),
            (Some(first), _) => first.to_string(),
            _ => String::new(),
        }
    });
    let (signed_in, account) = match kind {
        CliKind::ClaudeCode => {
            let status = run_probe(&bin, &["auth", "status"])
                .await
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or(Value::Null);
            (
                status
                    .get("loggedIn")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                status
                    .get("authMethod")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )
        }
        CliKind::Antigravity => (crate::antigravity::models().await.is_ok(), None),
        CliKind::Devin => crate::devin::auth_status().await,
    };
    json!({
        "provider": provider,
        "label": kind.label(),
        "installed": true,
        "path": bin.display().to_string(),
        "version": version,
        "signed_in": signed_in,
        "account": account,
    })
}

struct McpCall {
    name: String,
    arguments: Value,
    reply: oneshot::Sender<Value>,
}

type SteerQueue = std::sync::Arc<std::sync::Mutex<Vec<String>>>;
type ServedResults =
    std::sync::Arc<std::sync::Mutex<HashMap<String, std::collections::VecDeque<Value>>>>;
type LateResults = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<(String, String)>>>;

#[derive(Clone)]
struct McpState {
    token: String,
    tools: Vec<NativeToolDefinition>,
    calls: mpsc::UnboundedSender<McpCall>,
    steers: SteerQueue,
    served: ServedResults,
    late: LateResults,
    images: bool,
}

struct CliProcess {
    child: Child,
    stdin: ChildStdin,
    account: Option<String>,
    acp_next_id: u64,
    acp_supports_images: bool,
}

/// The CLI's own `/model` list, read from its `initialize` control response; no model call is made.
pub(crate) async fn claude_supported_models() -> Result<(Option<String>, Vec<Value>), String> {
    let bin = claude_binary().ok_or("Claude Code isn't installed or isn't on PATH")?;
    let account = crate::accounts::active("claude-code");
    let mut cmd = Command::new(bin);
    cmd.current_dir(std::env::temp_dir())
        .arg("-p")
        .args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ])
        .args(["--setting-sources", ""])
        .env("CLAUDE_CODE_ENTRYPOINT", "snippet")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(token) = account.as_deref().and_then(claude_account_token) {
        cmd.env("CLAUDE_CODE_OAUTH_TOKEN", token);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start Claude Code: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let read = async {
        stdin
            .write_all(b"{\"type\":\"control_request\",\"request_id\":\"snippet-models\",\"request\":{\"subtype\":\"initialize\"}}\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            let r = &v["response"];
            if r.get("subtype").and_then(Value::as_str) == Some("error") {
                return Err(r
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("initialize failed")
                    .to_string());
            }
            return Ok(r["response"]["models"]
                .as_array()
                .cloned()
                .unwrap_or_default());
        }
        Err("Claude Code exited without listing its models".to_string())
    };
    let models = tokio::time::timeout(std::time::Duration::from_secs(8), read)
        .await
        .map_err(|_| "Claude Code took too long to list its models".to_string())??;
    let _ = child.kill().await;
    Ok((account, models))
}

fn claude_account_token(id: &str) -> Option<String> {
    if id == crate::accounts::MAIN {
        return None;
    }
    let raw = crate::accounts::read_credential("claude-code", id)?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("token")
        .and_then(Value::as_str)
        .map(str::to_string)
}

enum CliLine {
    Event(u64, Value),
    Exited(u64, String),
}

fn claude_transcript_lost(workspace: &Path, id: &str) -> bool {
    let Some(base) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))
    else {
        return false;
    };
    let projects = base.join("projects");
    if !projects.is_dir() {
        return false;
    }
    let file = format!("{id}.jsonl");
    let present = |p: PathBuf| std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
    let slug: String = workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if present(projects.join(slug).join(&file)) {
        return false;
    }
    let Ok(dirs) = std::fs::read_dir(&projects) else {
        return false;
    };
    !dirs.flatten().any(|d| present(d.path().join(&file)))
}

pub(crate) fn claude_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("SNIPPET_CLAUDE_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(found) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("claude"))
            .find(|candidate| candidate.is_file())
    }) {
        return Some(found);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

fn sanitize_key(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn tool_prompt_preamble(kind: CliKind) -> String {
    match kind {
        CliKind::ClaudeCode => format!(
            "## Tools in this runtime\n\nThe tools named in this prompt are served to you as `{MCP_PREFIX}<name>`: where it says `bash`, call `{MCP_PREFIX}bash`, and so on. These are your only tools: you have no built-in ones in this session. Read and search files with `bash` (cat, sed -n, rg, ls), and make every change, command, question and delegation through the snippet tools."
        ),
        CliKind::Devin => format!(
            "## Tools in this runtime\n\nThe tools named in these instructions are served by the `{MCP_SERVER}` MCP server. Call them via `mcp_call_tool` with `server_name: \"{MCP_SERVER}\"`, `tool_name` set to the exact tool name (`bash`, `change_files`, `update_plan`, `ask_user`, `delegate_task`, and the rest), and `arguments` matching its schema (call `mcp_list_tools` for `{MCP_SERVER}` once first if your runtime requires it). Every built-in tool of yours (`exec`, `read`, `write`, `edit`, `grep`, `glob`, `skill`, `ask_user_question`, `todo_write`) is disabled in this session: read and search files with `bash` (`cat`, `sed -n`, `rg`, `ls`), edit files with `change_files`, and make every command, question and delegation through the `{MCP_SERVER}` MCP tools."
        ),
        CliKind::Antigravity => format!(
            "## Tools in this runtime\n\nThe tools named in these instructions are served by the `{}` MCP server: call them with call_mcp_tool, ServerName `{}` and ToolName exactly as named here (`bash`, `change_files`, `update_plan`, `ask_user`, `delegate_task`, and the rest). These are your only working tools: every built-in tool of yours is switched off in this session. Read and search files with the snippet `bash` tool (cat, sed -n, rg, ls), research with its `web_search` and `web_read`, and make every change, command, question and delegation through the snippet tools. The one exception: when a tool result says its output was saved to a file inside your own Antigravity folder, read that file with view_file.",
            crate::antigravity::MCP_SERVER_NAME,
            crate::antigravity::MCP_SERVER_NAME,
        ),
    }
}

fn tool_guide(kind: CliKind, defs: &[NativeToolDefinition]) -> String {
    let header = match kind {
        CliKind::Devin => {
            "## Snippet tools and their arguments\n\nCall each through `mcp_call_tool` with `server_name: \"snippet\"` and `tool_name` set to the name below. Pass every required argument by its exact name; optional ones in brackets.\n"
        }
        _ => {
            "## Snippet tools and their arguments\n\nCall each through call_mcp_tool with ServerName `snippet_snippet`. Pass every required argument by its exact name; optional ones in brackets.\n"
        }
    };
    let mut out = String::from(header);
    for def in defs {
        let schema = &def.input_schema;
        let args = guide_args(schema, 0);
        let summary: String = def
            .description
            .split(". ")
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        out.push_str(&format!(
            "- `{}`({}) — {}\n",
            def.name,
            args.join(", "),
            summary.trim_end_matches('.')
        ));
    }
    out
}

fn guide_args(schema: &Value, depth: usize) -> Vec<String> {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    props
        .iter()
        .map(|(name, prop)| {
            let ty = guide_type(prop, depth);
            if required.contains(&name.as_str()) {
                format!("{name}: {ty}")
            } else {
                format!("[{name}: {ty}]")
            }
        })
        .collect()
}

fn guide_type(prop: &Value, depth: usize) -> String {
    if let Some(values) = prop.get("enum").and_then(Value::as_array) {
        let names: Vec<&str> = values.iter().filter_map(Value::as_str).collect();
        if !names.is_empty() {
            return names.join("|");
        }
    }
    let ty = prop.get("type").and_then(Value::as_str).unwrap_or("any");
    match ty {
        "array" if depth == 0 => match prop.get("items") {
            Some(items) if items.get("properties").is_some() => {
                format!("[{{{}}}]", guide_args(items, depth + 1).join(", "))
            }
            Some(items) => format!("[{}]", guide_type(items, depth + 1)),
            None => "array".to_string(),
        },
        "object" if depth == 0 && prop.get("properties").is_some() => {
            format!("{{{}}}", guide_args(prop, depth + 1).join(", "))
        }
        _ => ty.to_string(),
    }
}

fn vault_names(arguments: &Value) -> Vec<String> {
    arguments
        .get("command")
        .and_then(Value::as_str)
        .map(|command| crate::vault::Vault::load().referenced_names(command))
        .unwrap_or_default()
}

fn unwrap_custom(name: String, arguments: Value) -> (String, Value) {
    if name != "custom_tool" {
        return (name, arguments);
    }
    let inner = arguments
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("custom_tool")
        .to_string();
    let args = arguments
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    (inner, args)
}

const IMAGE_PATH_KEY: &str = "__image_path";
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

fn image_block(path: &str) -> Option<(&'static str, String)> {
    use base64::Engine;
    if std::fs::metadata(path).ok()?.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let (mime, bytes) = crate::builtins::fs::image_for_model(&bytes)?;
    Some((
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes),
    ))
}

/// A user message for the CLI, with the app's `[attached image — …: /path]`
/// markers turned into the images themselves for Claude Code, and into a
fn devin_prompt_blocks(text: &str, supports_images: bool) -> Vec<Value> {
    static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[attached (image|file) — [^\]/]*(/[^\]]+)\]").expect("valid regex")
    });
    if !MARKER.is_match(text) {
        return vec![json!({"type": "text", "text": text})];
    }
    let mut images = Vec::new();
    let cleaned = MARKER
        .replace_all(text, |caps: &regex::Captures| {
            let path = caps[2].trim().to_string();
            let kind_word = &caps[1];
            if kind_word == "image" {
                images.push(path.clone());
            }
            format!("[attached {kind_word}: {path}]")
        })
        .into_owned();
    let mut blocks = vec![json!({"type": "text", "text": cleaned})];
    if supports_images {
        for path in &images {
            if let Some((mime, data)) = image_block(path) {
                blocks.push(json!({"type": "image", "mimeType": mime, "data": data}));
            }
        }
    }
    blocks
}

/// pointer to its own file viewer for agy, whose input takes text only.
fn user_line_with_attachments(kind: CliKind, text: &str) -> Value {
    static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\[attached (image|file) — [^\]/]*(/[^\]]+)\]").expect("valid regex")
    });
    if !MARKER.is_match(text) {
        return kind.user_line(text);
    }
    let mut images = Vec::new();
    let text = MARKER
        .replace_all(text, |caps: &regex::Captures| {
            let path = caps[2].trim().to_string();
            let kind_word = &caps[1];
            if kind_word == "image" {
                images.push(path.clone());
                if kind == CliKind::Antigravity {
                    return format!("[attached image: {path} — open it with view_file to see it]");
                }
            }
            format!("[attached {kind_word}: {path}]")
        })
        .into_owned();
    if kind != CliKind::ClaudeCode {
        return kind.user_line(&text);
    }
    let mut content = vec![json!({"type": "text", "text": text})];
    for path in &images {
        if let Some((mime, data)) = image_block(path) {
            content.push(json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}));
        }
    }
    json!({"type": "user", "message": {"role": "user", "content": content}})
}

fn error_result(code: &str, message: &str) -> Value {
    json!({"schema_version": 1, "status": "error", "error": {"code": code, "message": message}})
}

fn tool_result_from_block(block: &Value) -> Value {
    let text = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    if let Ok(value) = serde_json::from_str::<Value>(&text)
        && value.get("status").is_some()
    {
        return value;
    }
    let failed = block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if failed {
        error_result("tool_error", &text)
    } else {
        json!({"schema_version": 1, "status": "success", "data": {"output": text}})
    }
}

async fn mcp_handler(
    AxState(state): AxState<McpState>,
    AxPath(token): AxPath<String>,
    Json(body): Json<Value>,
) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut res = match body {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(reply) = mcp_dispatch(&state, item).await {
                    out.push(reply);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Json(Value::Array(out)).into_response()
            }
        }
        item => match mcp_dispatch(&state, item).await {
            Some(reply) => Json(reply).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    };
    insert_session_id(&mut res, &token);
    res
}

fn insert_session_id(res: &mut Response, token: &str) {
    let id = format!("snippet-{}", token.get(..8).unwrap_or(token));
    if let Ok(value) = axum::http::HeaderValue::from_str(&id) {
        res.headers_mut().insert("mcp-session-id", value);
    }
}

async fn mcp_get(
    AxState(state): AxState<McpState>,
    AxPath(token): AxPath<String>,
    request: Request,
) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    if request.method() == axum::http::Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let stream = futures_util::stream::pending::<
        Result<axum::response::sse::Event, std::convert::Infallible>,
    >();
    let mut res = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("ping"),
        )
        .into_response();
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    insert_session_id(&mut res, &token);
    res
}

async fn mcp_delete(AxState(state): AxState<McpState>, AxPath(token): AxPath<String>) -> Response {
    if token != state.token {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut res = StatusCode::NO_CONTENT.into_response();
    insert_session_id(&mut res, &token);
    res
}

fn sh_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

fn read_command(arguments: &Value) -> String {
    let path = sh_quote(arguments.get("path").and_then(Value::as_str).unwrap_or(""));
    let num = |key: &str| arguments.get(key).and_then(Value::as_u64);
    let range = match (num("start_line"), num("end_line")) {
        (Some(start), end) => Some((start, end)),
        _ => match (num("offset"), num("limit")) {
            (Some(offset), limit) => Some((offset + 1, limit.map(|l| offset + l))),
            _ => None,
        },
    };
    match range {
        Some((start, Some(end))) => format!("sed -n '{start},{end}p' {path}"),
        Some((start, None)) => format!("sed -n '{start},$p' {path}"),
        None => format!("cat -n {path}"),
    }
}

fn normalise_call(name: &str, arguments: Value, known: &[String]) -> (String, Value) {
    let known_tool = |tool: &str| known.iter().any(|k| k == tool);
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    if name == "bash"
        && !arguments
            .as_object()
            .is_some_and(|o| o.contains_key("command"))
    {
        for key in ["cmd", "script", "shell"] {
            if let Some(value) = arguments.get(key).cloned() {
                let mut fixed = arguments.clone();
                if let Some(o) = fixed.as_object_mut() {
                    o.remove(key);
                    o.insert("command".to_string(), value);
                }
                return (name.to_string(), fixed);
            }
        }
        return (name.to_string(), arguments);
    }

    if name == "change_files" {
        let mut fixed = arguments.clone();
        let mut changed = false;
        if let Some(changes) = fixed.get_mut("changes").and_then(Value::as_array_mut) {
            for change in changes.iter_mut() {
                let Some(fields) = change.as_object_mut() else {
                    continue;
                };
                if fields.get("action").and_then(Value::as_str) == Some("overwrite") {
                    fields.insert("action".to_string(), json!("create"));
                    fields.insert("overwrite".to_string(), json!(true));
                    changed = true;
                }
                if !fields.contains_key("path") {
                    for key in ["file", "file_path"] {
                        if let Some(value) = fields.remove(key) {
                            fields.insert("path".to_string(), value);
                            changed = true;
                            break;
                        }
                    }
                }
            }
        }
        return (name.to_string(), if changed { fixed } else { arguments });
    }

    if known_tool(name) {
        return (name.to_string(), arguments);
    }

    let path = text("path");
    let rewritten = match name {
        "read_file" | "read" | "view_file" | "cat" if path.is_some() => {
            known_tool("bash").then(|| ("bash".to_string(), json!({"command": read_command(&arguments)})))
        }
        "write" | "write_file" | "create_file" => match (&path, text("content")) {
            (Some(path), Some(content)) => known_tool("change_files").then(|| {
                ("change_files".to_string(), json!({"changes": [{"action": "create", "path": path, "content": content, "overwrite": true}]}))
            }),
            _ => None,
        },
        "edit" | "edit_file" | "str_replace" | "replace" => {
            let find = text("old_string").or_else(|| text("find"));
            let with = text("new_string").or_else(|| text("with"));
            match (&path, find, with) {
                (Some(path), Some(find), Some(with)) => known_tool("change_files").then(|| {
                    ("change_files".to_string(), json!({"changes": [{"action": "replace", "path": path, "find": find, "with": with}]}))
                }),
                _ => None,
            }
        }
        "list_dir" | "ls" => match &path {
            Some(path) => known_tool("bash").then(|| ("bash".to_string(), json!({"command": format!("ls -la {}", sh_quote(path))}))),
            None => None,
        },
        "grep" | "search" => text("pattern").and_then(|pattern| {
            known_tool("bash").then(|| {
                let command = match &path {
                    Some(path) => format!("rg -n {} {}", sh_quote(&pattern), sh_quote(path)),
                    None => format!("rg -n {}", sh_quote(&pattern)),
                };
                ("bash".to_string(), json!({"command": command}))
            })
        }),
        _ => None,
    };
    rewritten.unwrap_or_else(|| (name.to_string(), arguments))
}

async fn mcp_dispatch(state: &McpState, request: Value) -> Option<Value> {
    let id = request.get("id").cloned()?;
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": MCP_SERVER, "version": env!("CARGO_PKG_VERSION")},
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": state.tools.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })).collect::<Vec<_>>()
        }),
        "tools/call" => {
            let raw_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let raw_arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let known: Vec<String> = state.tools.iter().map(|t| t.name.clone()).collect();
            let (name, arguments) = normalise_call(&raw_name, raw_arguments, &known);
            if name != raw_name {
                tracing::debug!("normalised tool call {raw_name} -> {name}");
            }
            let (reply, rx) = oneshot::channel();
            let mut rx = Box::pin(rx);
            let name_for_record = name.clone();
            if state
                .calls
                .send(McpCall {
                    name,
                    arguments,
                    reply,
                })
                .is_err()
            {
                return Some(
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "session ended"}}),
                );
            }
            let mut value = match tokio::time::timeout(MCP_SYNC_WAIT, rx.as_mut()).await {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => error_result("interrupted", "The call was interrupted."),
                Err(_) => {
                    let late_state = state.clone();
                    let late_name = name_for_record.clone();
                    tokio::spawn(async move {
                        let Ok(mut value) = rx.await else { return };
                        if let Some(o) = value.as_object_mut() {
                            o.remove(IMAGE_PATH_KEY);
                        }
                        let vault = crate::vault::Vault::load();
                        if !vault.is_empty() {
                            vault.scrub_value(&mut value);
                        }
                        let rendered = crate::llm::render_tool_result(&value);
                        if let Ok(mut late) = late_state.late.lock() {
                            late.push_back((late_name, rendered));
                        }
                    });
                    let pending = format!(
                        "{name_for_record} is still running after 200s. Its result will be attached to your next tool result as <late_tool_result name=\"{name_for_record}\">. Carry on with other work; do not rerun it."
                    );
                    // Keeps `served` one entry per call so the next call to this tool records its own value.
                    if let Ok(mut served) = state.served.lock() {
                        let queue = served.entry(name_for_record.clone()).or_default();
                        queue.push_back(json!({"schema_version": 1, "status": "success", "data": {"output": pending.clone()}}));
                        if queue.len() > 32 {
                            queue.pop_front();
                        }
                    }
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": pending}],
                            "isError": false,
                        }
                    }));
                }
            };
            let image_path = value
                .as_object_mut()
                .and_then(|o| o.remove(IMAGE_PATH_KEY))
                .and_then(|v| v.as_str().map(str::to_string));
            let vault = crate::vault::Vault::load();
            if !vault.is_empty() {
                vault.scrub_value(&mut value);
            }
            if let Ok(mut served) = state.served.lock() {
                let queue = served.entry(name_for_record).or_default();
                queue.push_back(value.clone());
                if queue.len() > 32 {
                    queue.pop_front();
                }
            }
            let failed = value.get("status").and_then(Value::as_str) == Some("error");
            let mut content =
                vec![json!({"type": "text", "text": crate::llm::render_tool_result(&value)})];
            if let Some(path) = image_path {
                match image_block(&path).filter(|_| state.images) {
                    Some((mime, data)) => content.push(json!({"type": "image", "data": data, "mimeType": mime})),
                    None => content.push(json!({"type": "text", "text": format!("To see the image itself, open {path} with your own file viewer (view_file).")})),
                }
            }
            if let Some(marker) =
                crate::antigravity::envelope_marker(&format!("/mcp/{}", state.token))
                && std::fs::remove_file(&marker).is_ok()
            {
                content.push(json!({"type": "text", "text": "<system-reminder>Your previous call_mcp_tool call was rejected before it ran because it left out toolSummary or toolAction. Include both on every call.</system-reminder>"}));
            }
            let late: Vec<(String, String)> = state
                .late
                .lock()
                .map(|mut q| std::mem::take(&mut *q).into_iter().collect())
                .unwrap_or_default();
            for (late_name, late_text) in late {
                content.push(json!({"type": "text", "text": format!("<late_tool_result name=\"{late_name}\">\n{late_text}\n</late_tool_result>")}));
            }
            let steers: Vec<String> = state
                .steers
                .lock()
                .map(|mut q| std::mem::take(&mut *q))
                .unwrap_or_default();
            if !steers.is_empty() {
                content.push(json!({"type": "text", "text": format!(
                    "<user_message_mid_turn>\nThe user sent this while you were working. It takes priority: adjust what you are doing now.\n{}\n</user_message_mid_turn>",
                    steers.join("\n\n")
                )}));
            }
            json!({
                "content": content,
                "isError": failed,
            })
        }
        _ => {
            return Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}}),
            );
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Give a forked Snippet session its own copy of the source's CLI
/// conversation, so it resumes with the real history and its prompt cache
/// instead of a replayed digest. Only for a fork at the latest point: neither
/// CLI can branch an earlier message, so an earlier fork keeps the replay.
/// Claude Code forks on the session's first run (--fork-session); agy's
/// conversation file is copied now.
pub fn fork_cli_conversations(source: &str, fork: &str, source_len: usize, fork_len: usize) {
    if fork_len < source_len {
        return;
    }
    for kind in [CliKind::ClaudeCode, CliKind::Antigravity, CliKind::Devin] {
        let raw = std::fs::read_to_string(CliSession::sidecar(kind, &sanitize_key(source)))
            .unwrap_or_default();
        let mut lines = raw.lines();
        let Some(id) = lines.next().map(str::trim).filter(|id| !id.is_empty()) else {
            continue;
        };
        let synced: usize = lines
            .next()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0);
        let target = CliSession::sidecar(kind, &sanitize_key(fork));
        let written = match kind {
            CliKind::ClaudeCode => {
                std::fs::write(&target, format!("{id}\n{synced}\nfork")).map_err(|e| e.to_string())
            }
            CliKind::Antigravity => crate::antigravity::fork_conversation(id).and_then(|copy| {
                std::fs::write(&target, format!("{copy}\n{synced}")).map_err(|e| e.to_string())
            }),
            CliKind::Devin => crate::devin::fork_conversation(id).and_then(|copy| {
                std::fs::write(&target, format!("{copy}\n{synced}")).map_err(|e| e.to_string())
            }),
        };
        if let Err(e) = written {
            eprintln!("fork {} conversation for {fork}: {e}", kind.label());
        }
    }
}

// The MCP tool list is fixed when the bridge starts, so complete_goal is always served
// (it errors without an active goal); without it a CLI goal could never finish.
fn cli_meta_tool_definitions(
    once: bool,
    lanes: bool,
    hidden: &[&str],
) -> Vec<NativeToolDefinition> {
    meta::conversation_meta_definitions_for(!once, lanes)
        .into_iter()
        .filter(|d| !once || !matches!(d.name.as_str(), "ask_user" | "monitor"))
        .filter(|d| !hidden.contains(&d.name.as_str()))
        .collect()
}

const CLI_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const MCP_SYNC_WAIT: std::time::Duration = std::time::Duration::from_secs(200);

fn cli_stall_watched(state: &HarnessState, process_alive: bool, tools_running: bool) -> bool {
    state.status == HarnessStatus::Running && process_alive && !tools_running
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StallAction {
    Restart,
    Fail,
}

fn stall_action(restarts: u8, kind: CliKind) -> StallAction {
    if restarts == 0 && matches!(kind, CliKind::Devin | CliKind::ClaudeCode) {
        StallAction::Restart
    } else {
        StallAction::Fail
    }
}

fn cli_goal_due(state: &HarnessState, idle_pass: bool, outbox_empty: bool, once: bool) -> bool {
    !once
        && idle_pass
        && outbox_empty
        && state.status == HarnessStatus::Idle
        && state.pending_question.is_none()
        && state
            .goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Active)
}

async fn start_mcp_server(
    tools: Vec<NativeToolDefinition>,
    steers: SteerQueue,
    served: ServedResults,
    images: bool,
) -> Result<(String, mpsc::UnboundedReceiver<McpCall>), String> {
    let (calls, rx) = mpsc::unbounded_channel();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let state = McpState {
        token: token.clone(),
        tools,
        calls,
        steers,
        served,
        late: Default::default(),
        images,
    };
    let app = Router::new()
        .route(
            "/mcp/{token}",
            post(mcp_handler).get(mcp_get).delete(mcp_delete),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind tool bridge: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}/mcp/{token}"), rx))
}

impl CodingHarness {
    fn cli_meta_definitions(&self, once: bool, lanes: bool) -> Vec<NativeToolDefinition> {
        cli_meta_tool_definitions(once, lanes, self.config.hidden_meta)
    }

    fn cli_system_prompt(&self, state: &HarnessState, session: &CliSession) -> String {
        let kind = session.kind;
        let system = state
            .messages
            .iter()
            .find_map(|m| match m {
                HarnessMessage::System { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| self.config.system_prompt.clone());
        let mut prompt = format!("{system}\n\n{}", tool_prompt_preamble(kind));
        if !session.tool_guide.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&session.tool_guide);
        }
        prompt
    }

    fn memory_on(&self) -> bool {
        self.config.memory_enabled && self.context.lane_origin().is_none()
    }

    fn compaction_save_prompt(&self) -> Option<String> {
        self.memory_on().then(|| format!("[compaction] This conversation is about to be compacted. {}Then reply with one line saying what you saved.", DURABLE_SAVE))
    }

    fn idle_memory_prompt(&self) -> Option<String> {
        self.memory_on().then(|| "[memory] The user has stepped away. Before they're back, save what this conversation established since your last memory save that holds beyond it: user preferences and rules, decisions and why, root causes, project facts and conventions, in global, project or agent memory. Update or remove stale entries instead of adding duplicates, keep entries short, and commit. If nothing durable came up, change nothing. Don't continue the work. Then reply with one line saying what you saved, or that nothing needed saving.".to_string())
    }

    fn agy_compaction_prompt(&self) -> String {
        let save = if self.memory_on() { DURABLE_SAVE } else { "" };
        format!(
            "[compaction] This conversation is about to be compacted: it will continue in a fresh conversation that starts from your summary. {save}Then reply with only the summary, about 2k tokens: the user's requests in order (one line each), the goal and what counts as done, progress (done, in progress, left), decisions and constraints, the next steps and the exact values (paths, ids, error strings) they need."
        )
    }

    fn compact_command(&self) -> String {
        "/compact Keep the summary lean, about 2k tokens: the user's requests in order (one line each), the goal and what counts as done, progress (done, in progress, left), decisions and constraints, the next steps and the exact values (paths, ids, error strings) they need.".to_string()
    }

    fn effort_arg(profile: &InferenceProfileConfig) -> Option<&str> {
        profile
            .reasoning_effort
            .as_deref()
            .filter(|e| matches!(*e, "low" | "medium" | "high" | "xhigh" | "max"))
    }

    fn claude_command(
        &self,
        profile: &InferenceProfileConfig,
        prompt: &str,
        mcp_url: &str,
        session: &CliSession,
        account: Option<&str>,
    ) -> Result<Command, String> {
        let bin = claude_binary().ok_or_else(|| {
            "Claude Code isn't installed or isn't on PATH (set SNIPPET_CLAUDE_BIN to its path)"
                .to_string()
        })?;
        let mcp_config = json!({"mcpServers": {MCP_SERVER: {"type": "http", "url": mcp_url}}});
        let mut cmd = Command::new(bin);
        cmd.current_dir(self.context.workspace_root())
            .arg("-p")
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ])
            .args(["--include-partial-messages", "--verbose"])
            .args(["--system-prompt", prompt])
            .args([
                "--mcp-config",
                &mcp_config.to_string(),
                "--strict-mcp-config",
            ])
            .args(["--tools", ""])
            .args(["--allowedTools", &format!("mcp__{MCP_SERVER}")])
            .args(["--permission-prompts", "none"])
            .args(["--setting-sources", ""])
            .env("MCP_TOOL_TIMEOUT", "3600000")
            .env("MCP_TIMEOUT", "30000")
            .env("CLAUDE_CODE_ENTRYPOINT", "snippet");
        if let Some(token) = account.and_then(claude_account_token) {
            cmd.env("CLAUDE_CODE_OAUTH_TOKEN", token);
        }
        if !profile.model.trim().is_empty() {
            cmd.args(["--model", profile.model.trim()]);
        }
        if let Some(effort) = Self::effort_arg(profile) {
            cmd.args(["--effort", effort]);
        }
        let compact_threshold = profile
            .context_window
            .saturating_mul(profile.compact_at_pct.clamp(1, 100) as u64)
            / 100;
        if compact_threshold > 0 {
            let auto_tokens = compact_threshold.clamp(100_000, 1_000_000);
            cmd.env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", auto_tokens.to_string());
        }
        if let Some(id) = session.id.as_deref() {
            cmd.args([
                if session.started {
                    "--resume"
                } else {
                    "--session-id"
                },
                id,
            ]);
            if session.forking {
                cmd.arg("--fork-session");
            }
        }
        Ok(cmd)
    }

    fn antigravity_command(
        &self,
        profile: &InferenceProfileConfig,
        prompt: &str,
        mcp_url: &str,
        session: &CliSession,
    ) -> Result<Command, String> {
        let bin = crate::antigravity::binary().ok_or_else(|| {
            "The Antigravity CLI (agy) isn't installed or isn't on PATH (set SNIPPET_AGY_BIN to its path)".to_string()
        })?;
        let home = crate::antigravity::prepare_home(&session.key, mcp_url, prompt)?;
        let real = crate::antigravity::real_home().ok_or("HOME is not set")?;
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var).unwrap_or_else(|| real.join(fallback).into_os_string())
        };
        let mut cmd = Command::new(bin);
        cmd.current_dir(self.context.workspace_root())
            .arg("--print=")
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ])
            .arg("--disable-slash-commands")
            .env("XDG_CONFIG_HOME", xdg("XDG_CONFIG_HOME", ".config"))
            .env("XDG_CACHE_HOME", xdg("XDG_CACHE_HOME", ".cache"))
            .env("XDG_DATA_HOME", xdg("XDG_DATA_HOME", ".local/share"))
            .env("HOME", home)
            .env(crate::antigravity::SESSION_ENV, mcp_url);
        let model = profile.model.trim();
        if !model.is_empty() {
            cmd.args(["--model", model]);
        }
        if let Some(id) = session.id.as_deref() {
            cmd.args(["--conversation", id]);
        }
        Ok(cmd)
    }

    fn resolved_devin_model(profile: &InferenceProfileConfig) -> String {
        let base = profile.model.trim();
        let effort = Self::effort_arg(profile);
        match (base, effort) {
            ("swe-2" | "swe", Some(tier @ ("medium" | "high" | "max"))) => format!("swe-2-{tier}"),
            ("swe-2-flash", Some(tier @ ("medium" | "high" | "max"))) => {
                format!("swe-2-flash-{tier}")
            }
            _ => base.to_string(),
        }
    }

    fn devin_command(
        &self,
        profile: &InferenceProfileConfig,
        prompt: &str,
        mcp_url: &str,
        session: &CliSession,
        account: Option<&str>,
    ) -> Result<Command, String> {
        let bin = crate::devin::binary().ok_or_else(|| {
            "The Devin CLI (devin) isn't installed or isn't on PATH (set SNIPPET_DEVIN_BIN to its path)".to_string()
        })?;
        let env = crate::devin::prepare_home(&session.key, mcp_url, prompt, account)?;
        let mut cmd = Command::new(bin);
        cmd.current_dir(self.context.workspace_root())
            .args(["--config", &env.config_path.display().to_string()]);
        let model = Self::resolved_devin_model(profile);
        if !model.is_empty() {
            cmd.args(["--model", &model]);
        }
        cmd.arg("acp")
            .env("HOME", &env.home)
            .env("XDG_CONFIG_HOME", &env.xdg_config_home)
            .env("XDG_DATA_HOME", &env.xdg_data_home)
            .env("XDG_CACHE_HOME", &env.xdg_cache_home)
            .env(crate::devin::SESSION_ENV, mcp_url)
            .env("MCP_TOOL_TIMEOUT", "3600000")
            .env("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "0");
        if let Some(token) = env.api_key {
            cmd.env("WINDSURF_API_KEY", token);
        }
        Ok(cmd)
    }

    async fn devin_rpc(
        stdin: &mut ChildStdin,
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .map_err(|e| format!("send Devin {method}: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush Devin {method}: {e}"))?;
        let wait = async {
            while let Some(line) = reader
                .next_line()
                .await
                .map_err(|e| format!("read Devin {method}: {e}"))?
            {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok(value);
                }
            }
            Err(format!("Devin exited during {method}"))
        };
        tokio::time::timeout(std::time::Duration::from_secs(20), wait)
            .await
            .map_err(|_| format!("Devin timed out during {method}"))?
    }

    async fn handshake_devin(
        &self,
        profile: &InferenceProfileConfig,
        mcp_url: &str,
        stdin: &mut ChildStdin,
        reader: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        session: &mut CliSession,
    ) -> Result<(u64, bool), String> {
        let init = Self::devin_rpc(
            stdin,
            reader,
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "snippet", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
        .await?;
        if let Some(err) = init.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Devin initialize failed");
            return Err(msg.to_string());
        }
        let cwd = self.context.workspace_root().display().to_string();
        let mcp_servers = json!([{
            "type": "http",
            "name": crate::devin::MCP_SERVER_NAME,
            "url": mcp_url,
            "headers": []
        }]);
        let mut next_id = 2u64;
        let session_reply = if let Some(existing) = session.id.clone().filter(|_| session.started) {
            let loaded = Self::devin_rpc(
                stdin,
                reader,
                next_id,
                "session/load",
                json!({"sessionId": existing, "cwd": cwd, "mcpServers": mcp_servers}),
            )
            .await?;
            next_id += 1;
            if loaded.get("error").is_some() {
                session.id = None;
                session.started = false;
                let created = Self::devin_rpc(
                    stdin,
                    reader,
                    next_id,
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": mcp_servers}),
                )
                .await?;
                next_id += 1;
                created
            } else {
                loaded
            }
        } else {
            let created = Self::devin_rpc(
                stdin,
                reader,
                next_id,
                "session/new",
                json!({"cwd": cwd, "mcpServers": mcp_servers}),
            )
            .await?;
            next_id += 1;
            created
        };
        if let Some(err) = session_reply.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Devin session setup failed");
            return Err(msg.to_string());
        }
        if let Some(new_id) = session_reply
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
        {
            session.adopt(new_id);
        }
        let Some(sid) = session.id.clone() else {
            return Err("Devin did not return a sessionId".to_string());
        };
        let _ = Self::devin_rpc(
            stdin,
            reader,
            next_id,
            "session/set_mode",
            json!({"sessionId": sid, "modeId": "bypass"}),
        )
        .await?;
        next_id += 1;
        if let Some(effort) = Self::effort_arg(profile) {
            let has_thought = session_reply
                .pointer("/result/configOptions")
                .and_then(Value::as_array)
                .is_some_and(|opts| {
                    opts.iter()
                        .any(|o| o.get("id").and_then(Value::as_str) == Some("thought_level"))
                });
            if has_thought {
                let _ = Self::devin_rpc(
                    stdin,
                    reader,
                    next_id,
                    "session/set_config_option",
                    json!({"sessionId": sid, "configId": "thought_level", "value": effort}),
                )
                .await;
                next_id += 1;
            }
        }
        let supports_images = session_reply
            .pointer("/result/configOptions")
            .and_then(Value::as_array)
            .and_then(|opts| {
                let model_opt = opts
                    .iter()
                    .find(|o| o.get("id").and_then(Value::as_str) == Some("model"))?;
                let cur = model_opt.get("currentValue").and_then(Value::as_str)?;
                model_opt
                    .get("options")
                    .and_then(Value::as_array)?
                    .iter()
                    .find(|v| v.get("value").and_then(Value::as_str) == Some(cur))?
                    .pointer("/_meta/cognition.ai~1supportsImages")
                    .or_else(|| {
                        model_opt
                            .get("options")
                            .and_then(Value::as_array)?
                            .iter()
                            .find(|v| v.get("value").and_then(Value::as_str) == Some(cur))?
                            .get("_meta")?
                            .get("cognition.ai/supportsImages")
                    })
                    .and_then(Value::as_bool)
            })
            .unwrap_or_else(|| !Self::resolved_devin_model(profile).starts_with("swe-2"));
        Ok((next_id.max(10), supports_images))
    }

    async fn spawn_cli(
        &self,
        profile: &InferenceProfileConfig,
        state: &HarnessState,
        mcp_url: &str,
        lines: &mpsc::UnboundedSender<CliLine>,
        generation: u64,
        session: &mut CliSession,
    ) -> Result<CliProcess, String> {
        let kind = session.kind;
        let prompt = self.cli_system_prompt(state, session);
        let account = kind.pooled_provider().and_then(crate::accounts::active);
        let mut cmd = match kind {
            CliKind::ClaudeCode => {
                self.claude_command(profile, &prompt, mcp_url, session, account.as_deref())?
            }
            CliKind::Antigravity => self.antigravity_command(profile, &prompt, mcp_url, session)?,
            CliKind::Devin => {
                self.devin_command(profile, &prompt, mcp_url, session, account.as_deref())?
            }
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let label = kind.label();
        let mut child = cmd.spawn().map_err(|e| format!("start {label}: {e}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(format!("{label} stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(format!("{label} stdout unavailable"))?;
        let stderr = child.stderr.take();
        let mut reader = BufReader::new(stdout).lines();
        let (acp_next_id, acp_supports_images) = if kind == CliKind::Devin {
            match self
                .handshake_devin(profile, mcp_url, &mut stdin, &mut reader, session)
                .await
            {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = child.start_kill();
                    return Err(e);
                }
            }
        } else {
            (10, true)
        };
        let err_task = tokio::spawn(async move {
            let mut tail = String::new();
            if let Some(stderr) = stderr {
                let mut err_reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = err_reader.next_line().await {
                    tail.push_str(&line);
                    tail.push('\n');
                    if tail.len() > 4000 {
                        let cut = tail.ceil_char_boundary(tail.len() - 4000);
                        tail.drain(..cut);
                    }
                }
            }
            tail
        });
        let tx = lines.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = reader.next_line().await {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(CliLine::Event(generation, value));
                }
            }
            let tail = tokio::time::timeout(std::time::Duration::from_secs(2), err_task)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            let _ = tx.send(CliLine::Exited(generation, tail.trim().to_string()));
        });
        Ok(CliProcess {
            child,
            stdin,
            account,
            acp_next_id,
            acp_supports_images,
        })
    }

    pub async fn run_cli_agent(
        &self,
        profile: InferenceProfileConfig,
        initial_request: Option<String>,
        input_rx: mpsc::UnboundedReceiver<LoopInput>,
        factory: Option<ModelFactory>,
        sink: Option<StreamHandle>,
    ) -> Result<HarnessState, ToolError> {
        self.drive_cli(profile, initial_request, input_rx, factory, sink, false)
            .await
    }

    pub async fn run_cli_lane(
        &self,
        profile: InferenceProfileConfig,
        brief: String,
    ) -> Result<HarnessOutcome, ToolError> {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let forward = self.config.lane_mailbox.clone().map(|mailbox| {
            let input_tx = input_tx.clone();
            tokio::spawn(async move {
                loop {
                    for text in mailbox.wait(std::time::Duration::from_secs(3600)).await {
                        if input_tx.send(LoopInput::UserMessage(text)).is_err() {
                            return;
                        }
                    }
                }
            })
        });
        let state = self
            .drive_cli(profile, Some(brief), input_rx, None, None, true)
            .await;
        drop(input_tx);
        if let Some(forward) = forward {
            forward.abort();
        }
        let state = state?;
        if state.final_text.is_none() {
            let message = state
                .events
                .iter()
                .rev()
                .find_map(|e| match e {
                    HarnessEvent::ModelError { message } => Some(message.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| "The CLI agent finished without a summary".to_string());
            return Err(ToolError::msg(message));
        }
        Ok(HarnessOutcome {
            final_text: state.final_text,
            events: state.events,
            iterations: state.iterations,
        })
    }

    async fn drive_cli(
        &self,
        profile: InferenceProfileConfig,
        initial_request: Option<String>,
        mut input_rx: mpsc::UnboundedReceiver<LoopInput>,
        factory: Option<ModelFactory>,
        sink: Option<StreamHandle>,
        once: bool,
    ) -> Result<HarnessState, ToolError> {
        let had_initial_request = initial_request.is_some();
        let mut state = self.load_or_initialize_state(initial_request).await?;
        let status_reset = matches!(
            state.status,
            HarnessStatus::Completed | HarnessStatus::Failed | HarnessStatus::Interrupted
        );
        if status_reset {
            state.status = if once {
                HarnessStatus::Running
            } else {
                HarnessStatus::Idle
            };
        }
        let (lane_tx, mut lane_rx) = mpsc::unbounded_channel::<LaneResult>();
        let (progress_tx, mut progress_rx) =
            mpsc::unbounded_channel::<crate::lanes::LaneProgress>();
        let lanes_enabled = !once && self.config.allow_lane_control && factory.is_some();
        let had_interrupted_lanes = state
            .lanes
            .iter()
            .any(|lane| lane.status == LaneStatus::Running);
        let (lane_approval_tx, mut lane_approval_rx) =
            mpsc::unbounded_channel::<crate::lanes::LaneApprovalRequest>();
        let mut lanes = self
            .new_lane_manager(factory, lane_tx, progress_tx, &state)
            .with_approvals((!once).then_some(lane_approval_tx));
        let mut lane_approvals: std::collections::VecDeque<crate::lanes::LaneApprovalRequest> =
            std::collections::VecDeque::new();
        let mut lane_prompt_shown = false;
        if had_interrupted_lanes {
            lanes.resume_interrupted();
        }
        let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<WatchEvent>();
        let mut watches = WatchManager::new(self.context.workspace_root().to_path_buf(), watch_tx);
        watches.restore(&state.watches);
        let mut vars = LoopVars::default();
        let meta_defs = self.cli_meta_definitions(once, lanes_enabled);
        let meta_names: Vec<String> = meta_defs.iter().map(|d| d.name.clone()).collect();
        let mut bridge_tools = self.tools.definitions();
        if self.tools.custom_dir().is_some() {
            bridge_tools.push(NativeToolDefinition {
                name: "custom_tool".into(),
                description: "Run one of your custom tools by name, including one you created or changed during this conversation that isn't in your tool list yet.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "The custom tool's name."},
                        "arguments": {"type": "object", "description": "Its arguments, as its parameters schema describes."}
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
            });
        }
        bridge_tools.extend(meta_defs);
        let kind = CliKind::of(&profile.provider);
        let guide = tool_guide(kind, &bridge_tools);
        let steers: SteerQueue = Default::default();
        let served: ServedResults = Default::default();
        let (mcp_url, mut mcp_rx) =
            start_mcp_server(bridge_tools, steers.clone(), served.clone(), true)
                .await
                .map_err(ToolError::msg)?;
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<CliLine>();
        let mut process: Option<CliProcess> = None;
        let mut account_limit: Option<i64> = None;
        let mut generation = 0u64;
        let session_key = self.session_id().unwrap_or_else(|| "main".to_string());
        let mut session = CliSession::open(kind, &session_key, self.config.resume);
        let mut last_mode_line = String::new();
        let mut last_task_note = String::new();
        let mut last_vault_line = String::new();
        let mut compaction = CliCompaction::Idle;
        let mut compact_wanted = false;
        let mut save_at: Option<tokio::time::Instant> = None;
        let mut memory_saving = false;
        let mut after_compaction: Option<String> = None;
        let mut cli_window: u64 = 0;
        if matches!(kind, CliKind::Antigravity | CliKind::Devin) {
            session.tool_guide = guide;
        }
        let mut parse = CliParse {
            served,
            envelope_marker: crate::antigravity::envelope_marker(&mcp_url),
            ..Default::default()
        };
        let mut pending_approval: Option<McpCall> = None;
        let mut pending_vault = false;
        let mut pending_custom: Option<crate::agent_tools::CustomTool> = None;
        let mut approval_index = 0usize;
        let mut running: FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>,
        > = FuturesUnordered::new();
        let mut outbox: Vec<String> = Vec::new();
        let mut held_envelopes: Vec<String> = Vec::new();
        let mut last_heard = tokio::time::Instant::now();
        let mut stall_restarts = 0u8;

        if state.status == HarnessStatus::Running
            && let Some(text) = state.messages.iter().rev().find_map(|m| match m {
                HarnessMessage::User { content } => Some(content.clone()),
                _ => None,
            })
        {
            outbox.push(text);
        }
        if had_initial_request || status_reset || had_interrupted_lanes {
            self.persist(&mut state, &lanes).await?;
        }

        loop {
            if state.status == HarnessStatus::Running {
                lane_prompt_shown = false;
            }
            if state.status == HarnessStatus::Idle {
                stall_restarts = 0;
            }
            if compact_wanted
                && compaction == CliCompaction::Idle
                && state.status == HarnessStatus::Idle
                && state.pending_question.is_none()
                && pending_approval.is_none()
                && outbox.is_empty()
            {
                compact_wanted = false;
                if session.started && session.id.is_some() {
                    state.compacting = true;
                    state.status = HarnessStatus::Running;
                    state.events.push(HarnessEvent::SystemDecision {
                        step: "history_compaction_pass".to_string(),
                        reasoning:
                            "Saving working state to memory, then compacting the conversation."
                                .to_string(),
                    });
                    let save = match kind {
                        CliKind::ClaudeCode => self.compaction_save_prompt(),
                        CliKind::Antigravity | CliKind::Devin => Some(self.agy_compaction_prompt()),
                    };
                    state.reflected_events = Some(state.events.len());
                    save_at = None;
                    match save {
                        Some(prompt) => {
                            compaction = CliCompaction::Saving;
                            outbox.push(prompt);
                        }
                        None => {
                            compaction = CliCompaction::Compacting;
                            outbox.push(format!("{COMMAND_MARK}{}", self.compact_command()));
                        }
                    }
                    self.persist(&mut state, &lanes).await?;
                }
            }
            if !lane_prompt_shown
                && pending_approval.is_none()
                && state.pending_question.is_none()
                && state.status == HarnessStatus::Idle
                && outbox.is_empty()
                && let Some(request) = lane_approvals.front()
            {
                state.events.push(HarnessEvent::ApprovalRequest {
                    tool_name: request.tool_name.clone(),
                    summary: format!("lane «{}» · {}", request.lane, request.summary),
                    index: 1,
                    total: lane_approvals.len(),
                });
                state.status = HarnessStatus::WaitingForInput;
                lane_prompt_shown = true;
                self.persist(&mut state, &lanes).await?;
            }
            if state.status == HarnessStatus::Idle
                && compaction == CliCompaction::Idle
                && !compact_wanted
                && pending_approval.is_none()
                && state.pending_question.is_none()
                && outbox.is_empty()
                && !state.queued_inputs.is_empty()
            {
                let held: Vec<QueuedInput> = std::mem::take(&mut state.queued_inputs);
                for (i, item) in held.into_iter().enumerate() {
                    let text = item.text;
                    if i == 0 {
                        self.accept_user_message(&mut state, &mut vars, text.clone())
                            .await;
                        let message = match state.messages.last() {
                            Some(HarnessMessage::User { content }) => content.clone(),
                            _ => text,
                        };
                        outbox.push(message);
                    } else {
                        let steered = super::steer_message(&text);
                        state.messages.push(HarnessMessage::User {
                            content: steered.clone(),
                        });
                        Self::note_human_wait(&mut state, &text, true);
                        state.events.push(HarnessEvent::Steer { text });
                        self.bump_activity();
                        outbox.push(steered);
                    }
                }
                self.persist(&mut state, &lanes).await?;
            }
            if cli_goal_due(
                &state,
                compaction == CliCompaction::Idle && !compact_wanted && pending_approval.is_none(),
                outbox.is_empty(),
                once,
            ) {
                self.drive_goal_turn(&mut state, &mut vars);
                if state.status == HarnessStatus::Running
                    && let Some(HarnessMessage::User { content }) = state.messages.last()
                {
                    outbox.push(content.clone());
                }
                self.persist(&mut state, &lanes).await?;
            }
            if !outbox.is_empty() {
                if session.started && session.synced > 0 {
                    let len = state.messages.len();
                    let prior_end = len.saturating_sub(outbox.len());
                    let rewritten = session.synced > len
                        || (!session.mark.is_empty()
                            && transcript_mark(&state.messages, session.synced) != session.mark);
                    let away = !rewritten && prior_end > session.synced;
                    let behind = away
                        && handoff(&state.messages[session.synced..prior_end], false)
                            .is_some_and(|h| h.dropped > 0);
                    let threshold = self
                        .config
                        .context_window_tokens
                        .saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64)
                        / 100;
                    let full = (away || once) && threshold > 0 && session.tokens >= threshold;
                    let lost = !rewritten
                        && !behind
                        && !full
                        && kind == CliKind::ClaudeCode
                        && process.is_none()
                        && session.id.as_deref().is_some_and(|id| {
                            claude_transcript_lost(self.context.workspace_root(), id)
                        });
                    if rewritten || behind || full || lost {
                        if let Some(mut p) = process.take() {
                            let _ = p.child.start_kill();
                        }
                        generation += 1;
                        session.restart();
                        state.last_prompt_tokens = 0;
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "runtime_reseeded".to_string(),
                            reasoning: if rewritten {
                                format!("This session's history was rewound or compacted elsewhere, so {} starts a fresh conversation from it.", kind.label())
                            } else if lost {
                                format!("{}'s saved conversation is missing or empty, so it starts a fresh conversation from this session's history.", kind.label())
                            } else if behind {
                                format!("{} missed more than a catch-up can carry, so it starts a fresh conversation from this session's history.", kind.label())
                            } else {
                                format!("{}'s own conversation was near its context limit, so it starts a fresh one from this session's history.", kind.label())
                            },
                        });
                        self.persist(&mut state, &lanes).await?;
                    }
                }
                if let (Some(prov), Some(p)) = (kind.pooled_provider(), process.as_mut())
                    && p.account != crate::accounts::active(prov)
                {
                    let _ = p.child.start_kill();
                    process = None;
                }
                if process.is_none() {
                    generation += 1;
                    match self
                        .spawn_cli(
                            &profile,
                            &state,
                            &mcp_url,
                            &line_tx,
                            generation,
                            &mut session,
                        )
                        .await
                    {
                        Ok(p) => {
                            if matches!(kind, CliKind::ClaudeCode | CliKind::Devin) {
                                session.started = true;
                            }
                            process = Some(p);
                        }
                        Err(error) => {
                            outbox.clear();
                            state
                                .events
                                .push(HarnessEvent::ModelError { message: error });
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            if once {
                                break;
                            }
                        }
                    }
                }
                if let Some(p) = process.as_mut() {
                    if session_key == crate::mission_control::SESSION_ID {
                        let mode = format!(
                            "{} {}",
                            crate::mission_autonomy::clock_line(),
                            crate::mission_autonomy::mode_line()
                        );
                        if mode != last_mode_line
                            && let Some(first) = outbox.first_mut()
                        {
                            *first =
                                format!("<system-reminder>{mode}</system-reminder>\n\n{first}");
                            last_mode_line = mode;
                        }
                    }
                    let prior_end = state.messages.len().saturating_sub(outbox.len());
                    let fresh = session.synced == 0;
                    if prior_end > session.synced.max(1)
                        && let Some(context) =
                            handoff(&state.messages[session.synced.max(1)..prior_end], fresh)
                        && let Some(first) = outbox.first_mut()
                    {
                        *first = format!("{}{first}", context.text);
                    }
                    let note = crate::coordination_tools::task_awareness(&self.context);
                    let mut reminder: Vec<String> = after_compaction.take().into_iter().collect();
                    if note != last_task_note {
                        reminder.push(if note.is_empty() {
                            "You have no open task now.".to_string()
                        } else {
                            note.clone()
                        });
                        last_task_note = note;
                    }
                    let names = crate::vault::Vault::load().names();
                    let vault_line = if names.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "Vault secrets usable as $NAME in bash (values are hidden from you): {}",
                            names.join(", ")
                        )
                    };
                    if vault_line != last_vault_line {
                        reminder.push(if vault_line.is_empty() {
                            "The vault is empty now.".to_string()
                        } else {
                            vault_line.clone()
                        });
                        last_vault_line = vault_line;
                    }
                    if !reminder.is_empty()
                        && let Some(first) =
                            outbox.iter_mut().find(|t| !t.starts_with(COMMAND_MARK))
                    {
                        *first = format!(
                            "<system-reminder>{}</system-reminder>\n\n{first}",
                            reminder.join("\n\n")
                        );
                    }
                    session.mark_synced(
                        &state.messages,
                        state.compactions,
                        state.last_prompt_tokens,
                    );
                    last_heard = tokio::time::Instant::now();
                    if kind == CliKind::Devin {
                        let combined = std::mem::take(&mut outbox)
                            .into_iter()
                            .map(|t| {
                                t.strip_prefix(COMMAND_MARK)
                                    .map(str::to_string)
                                    .unwrap_or(t)
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let sid = session.id.clone().unwrap_or_default();
                        let req_id = p.acp_next_id;
                        p.acp_next_id += 1;
                        let line = json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "method": "session/prompt",
                            "params": {
                                "sessionId": sid,
                                "prompt": devin_prompt_blocks(&combined, p.acp_supports_images),
                            }
                        });
                        let sent = p.stdin.write_all(format!("{line}\n").as_bytes()).await;
                        if sent.is_err() || p.stdin.flush().await.is_err() {
                            state.events.push(HarnessEvent::ModelError {
                                message: format!("{} stopped accepting input", kind.label()),
                            });
                            state.status = HarnessStatus::Failed;
                            process = None;
                        }
                    } else {
                        for text in std::mem::take(&mut outbox) {
                            let text = match text.strip_prefix(COMMAND_MARK) {
                                Some(command) => command.to_string(),
                                None if kind == CliKind::ClaudeCode
                                    && text.trim_start().starts_with('/') =>
                                {
                                    format!(" {}", text.trim_start())
                                }
                                None => text,
                            };
                            let line = user_line_with_attachments(kind, &text);
                            let sent = p.stdin.write_all(format!("{line}\n").as_bytes()).await;
                            if sent.is_err() || p.stdin.flush().await.is_err() {
                                state.events.push(HarnessEvent::ModelError {
                                    message: format!("{} stopped accepting input", kind.label()),
                                });
                                state.status = HarnessStatus::Failed;
                                process = None;
                                break;
                            }
                        }
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep_until(last_heard + CLI_STALL_AFTER), if cli_stall_watched(&state, process.is_some(), !running.is_empty()) => {
                    if let Some(mut p) = process.take() {
                        let _ = p.child.start_kill();
                    }
                    generation += 1;
                    let mins = CLI_STALL_AFTER.as_secs() / 60;
                    match stall_action(stall_restarts, kind) {
                        StallAction::Restart => {
                            stall_restarts = 1;
                            state.events.push(HarnessEvent::SystemDecision {
                                step: "runtime_restarted".to_string(),
                                reasoning: format!(
                                    "{} went silent for {mins} minutes mid-turn, so it was restarted and asked to carry on.",
                                    kind.label()
                                ),
                            });
                            outbox.push(
                                "Carry on from where you stopped. Your last tool results are above; do not repeat completed work."
                                    .to_string(),
                            );
                            self.persist(&mut state, &lanes).await?;
                        }
                        StallAction::Fail => {
                            if let Ok(mut queue) = steers.lock() {
                                queue.clear();
                            }
                            if let Some(s) = sink.as_ref() {
                                StreamBuffer::clear(s);
                            }
                            let message = if stall_restarts >= 1 {
                                format!(
                                    "{} went silent for {mins} minutes twice in a row, so it was stopped. Send a message to carry on.",
                                    kind.label()
                                )
                            } else {
                                format!(
                                    "{} went silent for {mins} minutes mid-turn, so it was stopped. Send a message to carry on.",
                                    kind.label()
                                )
                            };
                            state.events.push(HarnessEvent::ModelError { message });
                            state.status = HarnessStatus::Failed;
                            self.persist(&mut state, &lanes).await?;
                            if once {
                                break;
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(save_at.unwrap_or_else(tokio::time::Instant::now)), if save_at.is_some() && state.status == HarnessStatus::Idle && outbox.is_empty() => {
                    save_at = None;
                    if super::memory_reflection::reflection_scope(&state).is_some()
                        && session.started
                        && session.id.is_some()
                        && let Some(prompt) = self.idle_memory_prompt()
                    {
                        memory_saving = true;
                        state.status = HarnessStatus::Running;
                        state.events.push(HarnessEvent::SystemDecision {
                            step: "idle_memory_save".to_string(),
                            reasoning: "Saving what this conversation learned to memory while you're away.".to_string(),
                        });
                        outbox.push(prompt);
                        self.persist(&mut state, &lanes).await?;
                    }
                }
                input = input_rx.recv() => {
                    let Some(input) = input else { break };
                    save_at = None;
                    match input {
                        LoopInput::UserMessage(text) | LoopInput::Answer(text) => {
                            let text = text.trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            if state.pending_question.is_some() && is_held_envelope(&text) {
                                held_envelopes.push(text);
                                continue;
                            }
                            crate::coordination::direct_consumed(&text);
                            if state.pending_question.is_some() {
                                let mid_turn = state.status == HarnessStatus::Running && process.is_some();
                                state.status = HarnessStatus::WaitingForInput;
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                let answer = match state.messages.last() {
                                    Some(HarnessMessage::User { content }) => content.clone(),
                                    _ => text,
                                };
                                if mid_turn && matches!(kind, CliKind::Antigravity | CliKind::Devin) {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(answer);
                                    }
                                } else {
                                    outbox.push(answer);
                                }
                            } else if state.status == HarnessStatus::Running {
                                Self::note_human_wait(&mut state, &text, true);
                                state.events.push(HarnessEvent::Steer { text: text.clone() });
                                state.messages.push(HarnessMessage::User { content: text.clone() });
                                self.bump_activity();
                                if matches!(kind, CliKind::Antigravity | CliKind::Devin) && process.is_some() {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(text);
                                    }
                                } else {
                                    outbox.push(text);
                                }
                            } else {
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                outbox.push(text);
                            }
                            if state.pending_question.is_none() {
                                for held in held_envelopes.drain(..) {
                                    Self::note_human_wait(&mut state, &held, true);
                                    state.events.push(HarnessEvent::Steer { text: held.clone() });
                                    state.messages.push(HarnessMessage::User { content: held.clone() });
                                    outbox.push(held);
                                }
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Approve | LoopInput::ApproveAll | LoopInput::Deny => {
                            if pending_approval.is_none() && lane_prompt_shown {
                                if let Some(request) = lane_approvals.pop_front() {
                                    let _ = request.reply.send(!matches!(input, LoopInput::Deny));
                                }
                                lane_prompt_shown = false;
                                if state.status == HarnessStatus::WaitingForInput && state.pending_question.is_none() {
                                    state.status = HarnessStatus::Idle;
                                }
                                self.persist(&mut state, &lanes).await?;
                                continue;
                            }
                            let Some(call) = pending_approval.take() else { continue };
                            state.status = HarnessStatus::Running;
                            if matches!(input, LoopInput::ApproveAll) && !pending_vault {
                                state.approval_mode = ApprovalMode::Auto;
                            }
                            pending_vault = false;
                            let custom = pending_custom.take();
                            if !matches!(input, LoopInput::Deny)
                                && let (Some(tool), Some(dir)) = (custom.as_ref(), self.tools.custom_dir())
                            {
                                crate::agent_tools::approve(dir, tool);
                            }
                            if matches!(input, LoopInput::Deny) {
                                let _ = call.reply.send(error_result(
                                    "user_denied",
                                    "The user denied this action. Retrying it as-is would just meet the same answer, so adjust your approach or ask what they'd prefer.",
                                ));
                            } else {
                                running.push(Box::pin(self.execute_cli_call(call)));
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Interrupt => {
                            if let Some(mut p) = process.take() {
                                if kind == CliKind::Devin
                                    && let Some(sid) = session.id.as_deref()
                                {
                                    let cancel = json!({
                                        "jsonrpc": "2.0",
                                        "method": "session/cancel",
                                        "params": { "sessionId": sid }
                                    });
                                    let _ = p.stdin.write_all(format!("{cancel}\n").as_bytes()).await;
                                    let _ = p.stdin.flush().await;
                                }
                                let _ = p.child.start_kill();
                            }
                            parse.devin_text.clear();
                            parse.devin_tools.clear();
                            generation += 1;
                            running.clear();
                            // Already in history; left queued they'd fire after the next, unrelated turn.
                            if let Ok(mut queue) = steers.lock() {
                                queue.clear();
                            }
                            pending_approval = None;
                            state.pending_question = None;
                            if let Some(s) = sink.as_ref() {
                                StreamBuffer::clear(s);
                            }
                            super::runner::pause_goal_on_interrupt(&mut state);
                            state.status = HarnessStatus::Interrupted;
                            self.persist(&mut state, &lanes).await?;
                            state.status = HarnessStatus::Idle;
                        }
                        goal_input @ (LoopInput::SetGoal(_) | LoopInput::ResumeGoal | LoopInput::CancelGoal) => {
                            let mid_turn = state.status == HarnessStatus::Running && process.is_some();
                            let before = state.messages.len();
                            self.apply_input(&mut state, goal_input);
                            if state.messages.len() > before
                                && let Some(HarnessMessage::User { content }) = state.messages.last()
                            {
                                let directive = content.clone();
                                if mid_turn && matches!(kind, CliKind::Antigravity | CliKind::Devin) {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(directive);
                                    }
                                } else {
                                    outbox.push(directive);
                                }
                            } else if state.status == HarnessStatus::Running && process.is_none() && outbox.is_empty() {
                                state.status = HarnessStatus::Idle;
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::SteerQueued(id) => {
                            let Some(text) = take_queued(&mut state, &id) else {
                                self.persist(&mut state, &lanes).await?;
                                continue;
                            };
                            if state.status == HarnessStatus::Running && process.is_some() {
                                Self::note_human_wait(&mut state, &text, true);
                                state.events.push(HarnessEvent::Steer { text: text.clone() });
                                state.messages.push(HarnessMessage::User { content: text.clone() });
                                self.bump_activity();
                                if matches!(kind, CliKind::Antigravity | CliKind::Devin) {
                                    if let Ok(mut queue) = steers.lock() {
                                        queue.push(text);
                                    }
                                } else {
                                    outbox.push(text);
                                }
                            } else {
                                if state.pending_question.is_some() {
                                    state.status = HarnessStatus::WaitingForInput;
                                }
                                self.accept_user_message(&mut state, &mut vars, text.clone()).await;
                                let message = match state.messages.last() {
                                    Some(HarnessMessage::User { content }) => content.clone(),
                                    _ => text,
                                };
                                outbox.push(message);
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::Compact => {
                            compact_wanted = true;
                        }
                        LoopInput::ContinueLean => {
                            if let Some(text) = self.continue_lean(&mut state) {
                                if let Some(mut p) = process.take() {
                                    let _ = p.child.start_kill();
                                }
                                generation += 1;
                                session.restart();
                                session.mark_synced(&state.messages, state.compactions, 0);
                                after_compaction = Some(text);
                                last_task_note.clear();
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                        LoopInput::SpawnLane(spec) => {
                            self.spawn_requested_lane(&mut state, &mut lanes, spec);
                            self.persist(&mut state, &lanes).await?;
                        }
                        other => {
                            if self.apply_input(&mut state, other) {
                                break;
                            }
                            self.persist(&mut state, &lanes).await?;
                        }
                    }
                }
                line = line_rx.recv() => {
                    let Some(line) = line else { continue };
                    match line {
                        CliLine::Event(g, event) if g == generation => {
                            last_heard = tokio::time::Instant::now();
                            if let Some(id) = event.get("conversation_id").and_then(Value::as_str)
                                && kind == CliKind::Antigravity
                            {
                                session.adopt(id);
                            }
                            if session.forking
                                && kind == CliKind::ClaudeCode
                                && event.get("type").and_then(Value::as_str) == Some("system")
                                && event.get("subtype").and_then(Value::as_str) == Some("init")
                                && let Some(id) = event.get("session_id").and_then(Value::as_str)
                            {
                                session.forking = false;
                                session.id = Some(id.to_string());
                                session.save();
                            }
                            if event.get("type").and_then(Value::as_str) == Some("system")
                                && event.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
                            {
                                let meta = event.get("compact_metadata");
                                let tokens = |key: &str| meta.and_then(|m| m.get(key)).and_then(Value::as_u64).unwrap_or(0);
                                let trigger = meta.and_then(|m| m.get("trigger")).and_then(Value::as_str).unwrap_or("auto");
                                self.archive_cli_compaction(&mut state);
                                state.compactions += 1;
                                state.reflected_events = Some(state.events.len());
                                state.last_prompt_tokens = tokens("post_tokens");
                                state.events.push(HarnessEvent::SystemDecision {
                                    step: "history_compacted".to_string(),
                                    reasoning: format!(
                                        "Compacted the conversation ({trigger}) from {} to {} tokens.",
                                        tokens("pre_tokens"),
                                        tokens("post_tokens")
                                    ),
                                });
                                last_task_note.clear();
                                self.persist(&mut state, &lanes).await?;
                                session.mark_synced(&state.messages, state.compactions, tokens("post_tokens"));
                            }
                            let final_before = state.final_text.clone();
                            let (changed, finished) = match kind {
                                CliKind::ClaudeCode => (
                                    self.apply_cli_event(&mut state, &event, sink.as_ref(), &mut parse),
                                    event.get("type").and_then(Value::as_str) == Some("result"),
                                ),
                                CliKind::Antigravity => self.apply_agy_event(&mut state, &event, sink.as_ref(), &mut parse),
                                CliKind::Devin => self.apply_devin_event(&mut state, &event, sink.as_ref(), &mut parse),
                            };
                            if kind == CliKind::ClaudeCode
                                && event.get("type").and_then(Value::as_str) == Some("rate_limit_event")
                            {
                                let info = event.get("rate_limit_info");
                                let account = process.as_ref().and_then(|p| p.account.clone());
                                if let (Some(account), Some(snapshot)) = (account.as_deref(), state.rate_limit.as_ref()) {
                                    crate::accounts::record_usage("claude-code", account, snapshot);
                                }
                                if info.and_then(|i| i.get("status")).and_then(Value::as_str) == Some("rejected") {
                                    account_limit = Some(info.and_then(|i| i.get("resetsAt")).and_then(Value::as_i64).unwrap_or(0));
                                }
                            }
                            if kind == CliKind::Devin
                                && let Some(hint) = crate::devin::quota_exhausted_hint(&event)
                            {
                                account_limit = Some(hint);
                            }
                            if finished
                                && kind == CliKind::ClaudeCode
                                && event.get("is_error").and_then(Value::as_bool) == Some(true)
                                && event.get("api_error_status").and_then(Value::as_u64) == Some(429)
                                && account_limit.is_none()
                            {
                                account_limit = Some(0);
                            }
                            let turn_errored = match kind {
                                CliKind::ClaudeCode => event.get("is_error").and_then(Value::as_bool) == Some(true),
                                CliKind::Devin => event.get("error").is_some(),
                                CliKind::Antigravity => false,
                            };
                            if finished
                                && turn_errored
                                && let Some(prov) = kind.pooled_provider()
                                && let Some(hint) = account_limit.take()
                                && let Some(from) = process.as_ref().and_then(|p| p.account.clone())
                            {
                                let until = crate::accounts::reset_for(prov, &from, Some(hint));
                                if let Some(next) = crate::accounts::rotate(prov, &from, until) {
                                    if matches!(state.events.last(), Some(HarnessEvent::ModelError { .. })) {
                                        state.events.pop();
                                    }
                                    state.final_text = final_before.clone();
                                    state.events.push(HarnessEvent::SystemDecision {
                                        step: "account_switched".to_string(),
                                        reasoning: format!(
                                            "{} account {} reached its usage limit (resets {}), so this continues on {}.",
                                            kind.label(),
                                            crate::accounts::label(prov, &from),
                                            crate::routing::reset_label(until),
                                            crate::accounts::label(prov, &next),
                                        ),
                                    });
                                    if let Some(mut p) = process.take() {
                                        let _ = p.child.start_kill();
                                    }
                                    generation += 1;
                                    state.status = HarnessStatus::Running;
                                    outbox.push("The last turn stopped at a usage limit before it finished. Pick up where it stopped and finish it.".to_string());
                                    self.persist(&mut state, &lanes).await?;
                                    continue;
                                }
                            }
                            if finished {
                                account_limit = None;
                                if let Some(usage) = event.get("modelUsage").and_then(Value::as_object) {
                                    cli_window = usage
                                        .values()
                                        .filter_map(|m| m.get("contextWindow").and_then(Value::as_u64))
                                        .max()
                                        .unwrap_or(cli_window);
                                }
                                if parse.devin_window > 0 {
                                    cli_window = parse.devin_window;
                                }
                                match compaction {
                                    CliCompaction::Saving if matches!(kind, CliKind::Antigravity | CliKind::Devin) => {
                                        state.final_text = final_before.clone();
                                        let summary = state.messages.iter().rev().find_map(|m| match m {
                                            HarnessMessage::Assistant { content, .. } if !content.trim().is_empty() => Some(content.trim().to_string()),
                                            _ => None,
                                        });
                                        if let Some(mut p) = process.take() {
                                            let _ = p.child.start_kill();
                                        }
                                        generation += 1;
                                        session.id = None;
                                        session.started = false;
                                        self.archive_cli_compaction(&mut state);
                                        session.mark_synced(&state.messages, state.compactions + 1, 0);
                                        after_compaction = Some(match summary {
                                            Some(summary) => format!("This conversation continues from an earlier one that was compacted. Summary of it:\n{summary}"),
                                            None => "This conversation continues from an earlier one that was compacted.".to_string(),
                                        });
                                        last_task_note.clear();
                                        state.compactions += 1;
                                        state.reflected_events = Some(state.events.len());
                                        state.last_prompt_tokens = 0;
                                        state.events.push(HarnessEvent::SystemDecision {
                                            step: "history_compacted".to_string(),
                                            reasoning: "Compacted the conversation: working state saved to memory, and a fresh conversation starts from its summary.".to_string(),
                                        });
                                        state.compacting = false;
                                        compaction = CliCompaction::Idle;
                                        state.status = HarnessStatus::Idle;
                                    }
                                    CliCompaction::Saving => {
                                        state.final_text = final_before.clone();
                                        state.status = HarnessStatus::Running;
                                        compaction = CliCompaction::Compacting;
                                        outbox.push(format!("{COMMAND_MARK}{}", self.compact_command()));
                                    }
                                    CliCompaction::Compacting => {
                                        state.final_text = final_before.clone();
                                        state.compacting = false;
                                        compaction = CliCompaction::Idle;
                                        if state.status == HarnessStatus::Running {
                                            state.status = HarnessStatus::Idle;
                                        }
                                    }
                                    CliCompaction::Idle => {
                                        if !memory_saving && state.pending_question.is_none() {
                                            self.emit_turn_changes(&mut state, &mut vars).await;
                                        }
                                        if self.config.memory_enabled && self.context.lane_origin().is_none() {
                                            self.tidy_memory();
                                        }
                                        if memory_saving {
                                            memory_saving = false;
                                            state.reflected_events = Some(state.events.len());
                                        } else if !once
                                            && self.config.memory_enabled
                                            && self.config.memory_reflect
                                            && self.context.lane_origin().is_none()
                                            && self.context.owner() == "main"
                                            && self.context.agent_id().is_none()
                                        {
                                            save_at = Some(tokio::time::Instant::now() + kind.memory_save_after());
                                        }
                                        let window = match cli_window {
                                            0 => self.config.context_window_tokens,
                                            reported => reported.min(self.config.context_window_tokens.max(1)),
                                        };
                                        let threshold = window.saturating_mul(self.config.compact_at_pct.clamp(1, 100) as u64) / 100;
                                        if !once && threshold > 0 && state.last_prompt_tokens >= threshold {
                                            compact_wanted = true;
                                        }
                                    }
                                }
                                session.mark_synced(&state.messages, state.compactions, state.last_prompt_tokens);
                                if state.pending_question.is_some() {
                                    state.status = HarnessStatus::WaitingForInput;
                                }
                                if kind == CliKind::Antigravity {
                                    crate::antigravity::refresh_quota_if_stale(300);
                                }
                                let leftover: Vec<String> = steers.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
                                if !leftover.is_empty() {
                                    outbox.push(leftover.join("\n\n"));
                                    state.status = HarnessStatus::Running;
                                }
                            }
                            if !parse.calls.is_empty() {
                                let model = if profile.model.trim().is_empty() { "default" } else { profile.model.trim() };
                                let owner = self.context.durable_session_id().map(str::to_string).unwrap_or_else(|| session_key.clone());
                                for usage in std::mem::take(&mut parse.calls) {
                                    crate::usage_ledger::record(&owner, &profile.provider, model, &usage);
                                }
                            }
                            if changed {
                                self.persist(&mut state, &lanes).await?;
                            }
                            if once && finished && running.is_empty() {
                                break;
                            }
                        }
                        CliLine::Exited(g, stderr) if g == generation => {
                            process = None;
                            if state.status == HarnessStatus::Running {
                                let message = if stderr.is_empty() {
                                    format!("{} exited before finishing the turn", kind.label())
                                } else {
                                    format!("{} exited: {stderr}", kind.label())
                                };
                                state.events.push(HarnessEvent::ModelError { message });
                                state.status = HarnessStatus::Failed;
                                self.persist(&mut state, &lanes).await?;
                            }
                            if once {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                Some(request) = lane_approval_rx.recv() => lane_approvals.push_back(request),
                Some(result) = lane_rx.recv() => {
                    self.inject_lane_result(&mut state, &mut lanes, &result);
                    self.wake_with_last_message(&mut state, &mut outbox);
                    self.persist(&mut state, &lanes).await?;
                }
                Some(progress) = progress_rx.recv() => {
                    lanes.record_progress(&progress);
                    if progress.kind == "message" {
                        self.inject_lane_message(&mut state, &lanes, &progress);
                        self.wake_with_last_message(&mut state, &mut outbox);
                    }
                    self.persist(&mut state, &lanes).await?;
                }
                Some(event) = watch_rx.recv() => {
                    self.inject_watch_event(&mut state, &mut watches, &event);
                    self.wake_with_last_message(&mut state, &mut outbox);
                    self.persist(&mut state, &lanes).await?;
                }
                call = mcp_rx.recv() => {
                    let Some(mut call) = call else { continue };
                    last_heard = tokio::time::Instant::now();
                    if call.name == "custom_tool" {
                        call.name = call.arguments.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                        call.arguments = call.arguments.get("arguments").cloned().unwrap_or_else(|| json!({}));
                    }
                    let custom = self
                        .tools
                        .custom_dir()
                        .filter(|_| self.tools.is_custom(&call.name))
                        .and_then(|dir| crate::agent_tools::prepare(dir, &call.name, &call.arguments));
                    let custom = match custom {
                        Some(Err(message)) => {
                            let _ = call.reply.send(error_result("invalid_arguments", &format!("{}: {message}", call.name)));
                            continue;
                        }
                        Some(Ok(custom)) => Some(custom),
                        None => None,
                    };
                    if let Some(custom) = custom {
                        let gated = custom.needs_first_approval || !custom.vault.is_empty();
                        if gated && once {
                            match self.config.lane_approval.clone() {
                                Some(route) => {
                                    let summary = crate::agent_tools::approval_summary(&custom);
                                    let first = custom.needs_first_approval.then(|| custom.tool.clone());
                                    running.push(Box::pin(self.lane_gated_call(call, route, summary, first)));
                                }
                                None => {
                                    let _ = call.reply.send(error_result(
                                        "custom_tool_needs_approval",
                                        &format!("The custom tool `{}` needs the user's approval to run (it's new, or uses vault secrets), which isn't available in this run. Report that it needs approving, so it runs on the main thread.", call.name),
                                    ));
                                }
                            }
                        } else if gated || state.approval_mode == ApprovalMode::Manual {
                            approval_index += 1;
                            state.events.push(HarnessEvent::ApprovalRequest {
                                tool_name: call.name.clone(),
                                summary: crate::agent_tools::approval_summary(&custom),
                                index: approval_index,
                                total: approval_index,
                            });
                            state.status = HarnessStatus::WaitingForInput;
                            pending_vault = gated;
                            pending_custom = custom.needs_first_approval.then(|| custom.tool.clone());
                            pending_approval = Some(call);
                            self.persist(&mut state, &lanes).await?;
                        } else {
                            running.push(Box::pin(self.execute_cli_call(call)));
                        }
                        continue;
                    }
                    match call.name.as_str() {
                        "ask_user" => match parse_ask_user(&call.arguments) {
                            Ok(questions) => {
                                state.pending_question = Some(questions.clone());
                                state.events.push(HarnessEvent::UserQuestion { questions });
                                let _ = call.reply.send(json!({
                                    "schema_version": 1,
                                    "status": "success",
                                    "data": {
                                        "asked": true,
                                        "note": "The question is on the user's screen. End your turn now: make no more tool calls and don't guess the answer. Their reply arrives as your next message, starting with [answer].",
                                    }
                                }));
                                self.persist(&mut state, &lanes).await?;
                            }
                            Err(message) => {
                                let _ = call.reply.send(error_result("invalid_arguments", &message));
                            }
                        },
                        name if meta_names.iter().any(|m| m == name) => {
                            let (value, _) = self.dispatch_meta(&mut state, &mut lanes, &mut watches, name, &call.arguments);
                            let _ = call.reply.send(value);
                            self.persist(&mut state, &lanes).await?;
                        }
                        "bash" if !vault_names(&call.arguments).is_empty() => {
                            let names = vault_names(&call.arguments).join(", ");
                            if once {
                                match self.config.lane_approval.clone() {
                                    Some(route) => {
                                        let summary = format!(
                                            "⚠ uses vault secret(s) [{names}] — {}",
                                            super::prompts::approval_summary("bash", &call.arguments)
                                        );
                                        running.push(Box::pin(self.lane_gated_call(call, route, summary, None)));
                                    }
                                    None => {
                                        let _ = call.reply.send(error_result(
                                            "vault_needs_confirmation",
                                            &format!("Using vault secret(s) [{names}] requires user confirmation, which isn't available in a delegated/headless run. Report that this step needs the secret, so it's done on the main thread where the user can approve it."),
                                        ));
                                    }
                                }
                                continue;
                            }
                            approval_index += 1;
                            state.events.push(HarnessEvent::ApprovalRequest {
                                tool_name: "bash".to_string(),
                                summary: format!(
                                    "⚠ uses vault secret(s) [{names}] — {}",
                                    super::prompts::approval_summary("bash", &call.arguments)
                                ),
                                index: approval_index,
                                total: approval_index,
                            });
                            state.status = HarnessStatus::WaitingForInput;
                            pending_approval = Some(call);
                            pending_vault = true;
                            self.persist(&mut state, &lanes).await?;
                        }
                        name if state.approval_mode == ApprovalMode::Manual && MUTATING_TOOLS.contains(&name) => {
                            approval_index += 1;
                            state.events.push(HarnessEvent::ApprovalRequest {
                                tool_name: name.to_string(),
                                summary: super::prompts::approval_summary(name, &call.arguments),
                                index: approval_index,
                                total: approval_index,
                            });
                            state.status = HarnessStatus::WaitingForInput;
                            pending_approval = Some(call);
                            self.persist(&mut state, &lanes).await?;
                        }
                        _ => running.push(Box::pin(self.execute_cli_call(call))),
                    }
                }
                Some(()) = running.next(), if !running.is_empty() => {
                    last_heard = tokio::time::Instant::now();
                }
            }
        }
        if let Some(mut p) = process.take() {
            let _ = p.child.start_kill();
        }
        Ok(state)
    }

    fn wake_with_last_message(&self, state: &mut HarnessState, outbox: &mut Vec<String>) {
        if let Some(HarnessMessage::User { content }) = state.messages.last() {
            outbox.push(content.clone());
        }
        if state.status == HarnessStatus::Idle {
            state.status = HarnessStatus::Running;
        }
    }

    async fn lane_gated_call(
        &self,
        call: McpCall,
        route: crate::lanes::LaneApprovalRoute,
        summary: String,
        first_run: Option<crate::agent_tools::CustomTool>,
    ) {
        if !route.ask(&call.name, &summary).await {
            let _ = call.reply.send(error_result(
                "user_denied",
                "The user denied this action. Retrying it as-is would just meet the same answer, so adjust your approach, or report what you needed.",
            ));
            return;
        }
        if let (Some(tool), Some(dir)) = (first_run.as_ref(), self.tools.custom_dir()) {
            crate::agent_tools::approve(dir, tool);
        }
        self.execute_cli_call(call).await;
    }

    async fn execute_cli_call(&self, call: McpCall) {
        let value = match self
            .tools
            .execute(&self.context, &call.name, call.arguments)
            .await
        {
            Ok(mut result) if call.name == "view_image" => {
                let resolved = result
                    .value
                    .pointer("/data/path")
                    .and_then(Value::as_str)
                    .and_then(|p| self.context.resolve_workspace_path(p).ok());
                if let (Some(path), Some(object)) = (resolved, result.value.as_object_mut()) {
                    object.insert(
                        IMAGE_PATH_KEY.to_string(),
                        json!(path.display().to_string()),
                    );
                }
                result.value
            }
            Ok(result) => result.value,
            Err(ToolError::UnknownTool(name)) => {
                let available: Vec<String> = self
                    .tools
                    .definitions()
                    .into_iter()
                    .map(|d| d.name)
                    .collect();
                let hint = if self.context.workspace_root()
                    == crate::mission_control::workspace_path()
                {
                    " You are Mission Control: to check on, run or change anything in a project, ask the session that owns it (create_mission_task to that session, or inspect_session)."
                } else {
                    ""
                };
                error_result(
                    "unknown_tool",
                    &format!(
                        "`{name}` isn't a tool in this session.{hint} Tools you have: {}.",
                        available.join(", ")
                    ),
                )
            }
            Err(error) => {
                let text = error.to_string();
                let argument_error = matches!(
                    error,
                    ToolError::Json(_) | ToolError::InvalidArguments { .. }
                ) || [
                    "missing field",
                    "unknown field",
                    "invalid type",
                    "unknown variant",
                    "expected JSON object",
                ]
                .iter()
                .any(|marker| text.contains(marker));
                let schema = self
                    .tools
                    .definitions()
                    .into_iter()
                    .find(|d| argument_error && d.name == call.name)
                    .map(|d| d.input_schema);
                let message = match schema {
                    Some(schema) => format!(
                        "{error}\nCall `{}` again with arguments matching this schema: {schema}",
                        call.name
                    ),
                    None => error.to_string(),
                };
                error_result("tool_error", &message)
            }
        };
        let _ = call.reply.send(value);
    }

    fn apply_agy_event(
        &self,
        state: &mut HarnessState,
        event: &Value,
        sink: Option<&StreamHandle>,
        parse: &mut CliParse,
    ) -> (bool, bool) {
        match event.get("event").and_then(Value::as_str) {
            Some("step_update") => {
                let Some(step) = event.get("step_update") else {
                    return (false, false);
                };
                let index = step.get("step_index").and_then(Value::as_i64).unwrap_or(-1);
                let done = step.get("state").and_then(Value::as_str);
                match step.get("step_type").and_then(Value::as_str) {
                    Some("agent_response") => {
                        if state.status == HarnessStatus::Idle {
                            state.status = HarnessStatus::Running;
                        }
                        if let Some(delta) = step.get("text_delta").and_then(Value::as_str) {
                            parse.agy_text.entry(index).or_default().push_str(delta);
                            if let Some(sink) = sink {
                                StreamBuffer::append(sink, delta);
                            }
                        }
                        if let (Some(delta), Some(sink)) =
                            (step.get("thinking_delta").and_then(Value::as_str), sink)
                        {
                            StreamBuffer::append_thinking(sink, delta);
                        }
                        if done != Some("DONE") {
                            return (false, false);
                        }
                        if let Some(usage) = step.get("usage") {
                            let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                            let prompt = n("input_tokens") + n("cache_read_tokens");
                            let completion = n("output_tokens");
                            state.last_prompt_tokens = prompt;
                            state.prompt_tokens += prompt;
                            state.completion_tokens += completion;
                            state.total_tokens += prompt + completion;
                            state.cache_read_tokens += n("cache_read_tokens");
                            parse.calls.push(crate::llm::TokenUsage {
                                prompt_tokens: prompt,
                                completion_tokens: completion,
                                total_tokens: prompt + completion,
                                cache_read_tokens: n("cache_read_tokens"),
                                ..Default::default()
                            });
                        }
                        let text = parse
                            .agy_text
                            .remove(&index)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if text.is_empty() {
                            return (false, false);
                        }
                        if let Some(sink) = sink {
                            StreamBuffer::clear(sink);
                        }
                        state.messages.push(HarnessMessage::Assistant {
                            content: text.clone(),
                            tool_calls: Vec::new(),
                        });
                        state
                            .events
                            .push(HarnessEvent::AssistantText { text, quiet: false });
                        (true, false)
                    }
                    Some("tool") => {
                        let info = step.get("tool_info").cloned().unwrap_or(Value::Null);
                        let raw = info.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let params = info.get("parameters").cloned().unwrap_or(Value::Null);
                        let mcp = raw == "call_mcp_tool";
                        let (name, arguments) = if mcp {
                            unwrap_custom(
                                params
                                    .get("ToolName")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool")
                                    .to_string(),
                                params.get("Arguments").cloned().unwrap_or(Value::Null),
                            )
                        } else if let Some(path) = (raw == "view_file")
                            .then(|| params.get("AbsolutePath").and_then(Value::as_str))
                            .flatten()
                            .filter(|p| {
                                !p.contains("/.gemini/antigravity-cli/")
                                    && crate::antigravity::is_image(p)
                            })
                        {
                            ("view_image".to_string(), json!({ "path": path }))
                        } else {
                            (raw.to_string(), params)
                        };
                        let schema_read = raw == "view_file"
                            && arguments
                                .get("AbsolutePath")
                                .and_then(Value::as_str)
                                .is_some_and(|p| p.contains("/.gemini/antigravity-cli/"));
                        let redirected = done == Some("ERROR")
                            && info
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .is_some_and(|m| m.contains("denied by pre-tool hook"));
                        let envelope_rejected = done == Some("ERROR")
                            && info
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .is_some_and(|m| {
                                    m.contains("missing properties")
                                        && (m.contains("toolSummary") || m.contains("toolAction"))
                                });
                        let quiet = schema_read
                            || redirected
                            || envelope_rejected
                            || QUIET_TOOLS.contains(&name.as_str());
                        let mut changed = false;
                        if envelope_rejected {
                            if let Some(marker) = parse.envelope_marker.as_ref() {
                                if let Some(dir) = marker.parent() {
                                    let _ = std::fs::create_dir_all(dir);
                                }
                                let _ = std::fs::write(marker, &name);
                            }
                            if parse.agy_tools.remove(&index)
                                && let Some(at) = state.events.iter().rposition(|e| {
                                    matches!(e, HarnessEvent::ToolCall { tool_name, .. } if *tool_name == name)
                                })
                            {
                                state.events.remove(at);
                                state.events_rewritten = true;
                                changed = true;
                            }
                        }
                        let announce = mcp || done.is_some_and(|d| d == "DONE" || d == "ERROR");
                        if announce && !quiet && parse.agy_tools.insert(index) {
                            self.lane_progress(
                                "tool_call",
                                super::dispatch::tool_activity_text(&name, &arguments),
                            );
                            self.presence("tool", &super::presence::tool_detail(&name, &arguments));
                            state.events.push(HarnessEvent::ToolCall {
                                tool_name: name.clone(),
                                arguments,
                            });
                            changed = true;
                        }
                        let own = if mcp && matches!(done, Some("DONE") | Some("ERROR")) {
                            parse.served.lock().ok().and_then(|mut served| {
                                served.get_mut(&name).and_then(|q| q.pop_front())
                            })
                        } else {
                            None
                        };
                        let result = match done {
                            Some(_) if own.is_some() => own.unwrap_or_default(),
                            Some("DONE") => {
                                let output = info
                                    .get("output")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                serde_json::Deserializer::from_str(output.trim_start())
                                    .into_iter::<Value>()
                                    .next()
                                    .and_then(Result::ok)
                                    .filter(|v| mcp && v.get("status").is_some())
                                    .unwrap_or_else(|| json!({"schema_version": 1, "status": "success", "data": {"output": output}}))
                            }
                            Some("ERROR") => {
                                let message = info
                                    .pointer("/error/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool failed");
                                serde_json::from_str::<Value>(message)
                                    .ok()
                                    .filter(|v| mcp && v.get("status").is_some())
                                    .unwrap_or_else(|| error_result("tool_error", message))
                            }
                            _ => return (changed, false),
                        };
                        if !quiet {
                            state.events.push(HarnessEvent::ToolResult {
                                tool_name: name,
                                result,
                            });
                            changed = true;
                        }
                        (changed, false)
                    }
                    _ => (false, false),
                }
            }
            Some("result") => {
                let result = event.get("result").cloned().unwrap_or(Value::Null);
                if let Some(sink) = sink {
                    StreamBuffer::clear(sink);
                }
                parse.agy_text.clear();
                parse.agy_tools.clear();
                state.iterations +=
                    result.get("num_turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                let error = result.get("error").and_then(Value::as_str).unwrap_or("");
                let cancelled = error.contains("context canceled");
                let response = result
                    .get("response")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                let replied = !response.is_empty()
                    || state
                        .events
                        .iter()
                        .rev()
                        .take_while(|e| {
                            !matches!(
                                e,
                                HarnessEvent::UserInput { .. } | HarnessEvent::Steer { .. }
                            )
                        })
                        .any(|e| matches!(e, HarnessEvent::AssistantText { .. }));
                if result.get("status").and_then(Value::as_str) == Some("ERROR")
                    && !cancelled
                    && !replied
                {
                    let message = if error.is_empty() {
                        "Antigravity reported an error"
                    } else {
                        error
                    }
                    .to_string();
                    state.events.push(HarnessEvent::ModelError { message });
                } else if let Some(text) = result
                    .get("response")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    state.final_text = Some(text.to_string());
                }
                if state.status == HarnessStatus::Running {
                    state.status = HarnessStatus::Idle;
                }
                (true, true)
            }
            _ => (false, false),
        }
    }

    fn flush_devin_text(
        &self,
        state: &mut HarnessState,
        sink: Option<&StreamHandle>,
        parse: &mut CliParse,
    ) -> Option<String> {
        let text = std::mem::take(&mut parse.devin_text).trim().to_string();
        if text.is_empty() {
            return None;
        }
        if let Some(sink) = sink {
            StreamBuffer::clear(sink);
        }
        state.messages.push(HarnessMessage::Assistant {
            content: text.clone(),
            tool_calls: Vec::new(),
        });
        state.events.push(HarnessEvent::AssistantText {
            text: text.clone(),
            quiet: false,
        });
        self.lane_progress("model", super::step::MODEL_ACTIVITY_TEXT);
        Some(text)
    }

    fn apply_devin_event(
        &self,
        state: &mut HarnessState,
        event: &Value,
        sink: Option<&StreamHandle>,
        parse: &mut CliParse,
    ) -> (bool, bool) {
        if let Some(id) = event.get("id").and_then(Value::as_u64) {
            if id < 10 {
                return (false, false);
            }
            if let Some(sink) = sink {
                StreamBuffer::clear(sink);
            }
            let flushed = self.flush_devin_text(state, sink, parse);
            parse.devin_tools.clear();
            parse.tool_names.clear();
            state.iterations += 1;
            if let Some(err) = event.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Devin reported an error");
                let cancelled = msg.to_ascii_lowercase().contains("cancel");
                if !cancelled {
                    state.events.push(HarnessEvent::ModelError {
                        message: msg.to_string(),
                    });
                }
            } else if let Some(result) = event.get("result") {
                if parse.calls.is_empty()
                    && let Some(usage) = result.get("usage")
                {
                    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let prompt = n("inputTokens");
                    let completion = n("outputTokens") + n("thoughtTokens");
                    if prompt > 0 || completion > 0 {
                        let cached = n("cachedReadTokens");
                        let created = n("cachedWriteTokens");
                        state.prompt_tokens += prompt;
                        state.completion_tokens += completion;
                        state.total_tokens += prompt + completion;
                        state.cache_read_tokens += cached;
                        state.last_prompt_tokens = prompt;
                        parse.calls.push(crate::llm::TokenUsage {
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            cache_read_tokens: cached,
                            cache_creation_tokens: created,
                            ..Default::default()
                        });
                    }
                }
                let final_text = flushed.or_else(|| {
                    state.events.iter().rev().find_map(|e| match e {
                        HarnessEvent::AssistantText { text, .. } if !text.trim().is_empty() => {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                });
                if let Some(text) = final_text {
                    state.final_text = Some(text);
                }
            }
            if state.status == HarnessStatus::Running {
                state.status = HarnessStatus::Idle;
            }
            return (true, true);
        }

        if event.get("method").and_then(Value::as_str) != Some("session/update") {
            return (false, false);
        }
        let Some(update) = event.pointer("/params/update") else {
            return (false, false);
        };
        let meta = update.get("_meta").unwrap_or(&Value::Null);
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_thought_chunk") => {
                if state.status == HarnessStatus::Idle {
                    state.status = HarnessStatus::Running;
                }
                if let (Some(delta), Some(sink)) = (
                    update.pointer("/content/text").and_then(Value::as_str),
                    sink,
                ) && !delta.is_empty()
                {
                    StreamBuffer::append_thinking(sink, delta);
                }
                (false, false)
            }
            Some("agent_message_chunk") => {
                if state.status == HarnessStatus::Idle {
                    state.status = HarnessStatus::Running;
                }
                if let Some(delta) = update.pointer("/content/text").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    parse.devin_text.push_str(delta);
                    if let Some(sink) = sink {
                        StreamBuffer::append(sink, delta);
                    }
                }
                (false, false)
            }
            Some("usage_update") => {
                if meta.get("cognition.ai/subagent_context").is_some() {
                    return (false, false);
                }
                if let Some(size) = update
                    .get("size")
                    .and_then(Value::as_u64)
                    .filter(|s| *s > 0)
                {
                    parse.devin_window = size;
                }
                let n = |k: &str| meta.get(k).and_then(Value::as_u64).unwrap_or(0);
                let prompt = n("cognition.ai/inputTokens");
                let completion = n("cognition.ai/outputTokens") + n("cognition.ai/thoughtTokens");
                let cached = n("cognition.ai/cachedReadTokens");
                let created = n("cognition.ai/cachedWriteTokens");
                if prompt > 0 || completion > 0 {
                    state.prompt_tokens += prompt;
                    state.completion_tokens += completion;
                    state.total_tokens += prompt + completion;
                    state.cache_read_tokens += cached;
                    state.last_prompt_tokens = prompt;
                    parse.calls.push(crate::llm::TokenUsage {
                        prompt_tokens: prompt,
                        completion_tokens: completion,
                        cache_read_tokens: cached,
                        cache_creation_tokens: created,
                        ..Default::default()
                    });
                    return (true, false);
                }
                (false, false)
            }
            Some("tool_call") => {
                if meta.get("cognition.ai/eventType").and_then(Value::as_str)
                    == Some("hook_execution")
                {
                    return (false, false);
                }
                let mut changed = self.flush_devin_text(state, sink, parse).is_some();
                let call_id = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let raw_name = meta
                    .get("cognition.ai/toolName")
                    .and_then(Value::as_str)
                    .or_else(|| meta.get("cognition.ai/mcpToolName").and_then(Value::as_str))
                    .or_else(|| {
                        meta.get("cognition.ai/inferenceToolName")
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("tool");
                let raw_input = update.get("rawInput").cloned().unwrap_or(Value::Null);
                let is_meta_list = matches!(
                    raw_name,
                    "mcp_list_tools"
                        | "mcp_list_servers"
                        | "mcp_read_resource"
                        | "list_resources"
                        | "read_resource"
                );
                let (mcp, name, arguments) =
                    if let Some(stripped) = raw_name.strip_prefix(MCP_PREFIX) {
                        let (n, a) = unwrap_custom(stripped.to_string(), raw_input);
                        (true, n, a)
                    } else if raw_name == "mcp_call_tool" {
                        let inner = raw_input
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_string();
                        let args = raw_input.get("arguments").cloned().unwrap_or(Value::Null);
                        let (n, a) = unwrap_custom(inner, args);
                        (true, n, a)
                    } else {
                        (false, raw_name.to_string(), raw_input)
                    };
                if !call_id.is_empty() {
                    parse.tool_names.insert(
                        call_id.clone(),
                        if is_meta_list {
                            format!("__meta:{name}")
                        } else if mcp {
                            format!("__mcp:{name}")
                        } else {
                            name.clone()
                        },
                    );
                }
                let quiet = is_meta_list || QUIET_TOOLS.contains(&name.as_str());
                if mcp && !quiet && parse.devin_tools.insert(call_id) {
                    self.lane_progress(
                        "tool_call",
                        super::dispatch::tool_activity_text(&name, &arguments),
                    );
                    self.presence("tool", &super::presence::tool_detail(&name, &arguments));
                    state.events.push(HarnessEvent::ToolCall {
                        tool_name: name,
                        arguments,
                    });
                    changed = true;
                }
                (changed, false)
            }
            Some("tool_call_update") => {
                if meta.get("cognition.ai/eventType").and_then(Value::as_str)
                    == Some("hook_execution")
                {
                    return (false, false);
                }
                let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                if !matches!(status, "completed" | "failed") {
                    return (false, false);
                }
                let call_id = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let recorded = parse
                    .tool_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                if recorded.starts_with("__meta:") {
                    return (false, false);
                }
                let (mcp, name) = match recorded.strip_prefix("__mcp:") {
                    Some(n) => (true, n.to_string()),
                    None => (false, recorded),
                };
                if QUIET_TOOLS.contains(&name.as_str()) {
                    return (false, false);
                }
                let text = update
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.pointer("/content/text")
                                    .and_then(Value::as_str)
                                    .or_else(|| item.get("text").and_then(Value::as_str))
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        update
                            .get("rawOutput")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                let hook_blocked = status == "failed"
                    && (text.contains("handled by snippet: call")
                        || text.contains("Only the snippet MCP server"));
                if hook_blocked {
                    return (false, false);
                }
                if !mcp && parse.devin_tools.insert(call_id.to_string()) {
                    state.events.push(HarnessEvent::ToolCall {
                        tool_name: name.clone(),
                        arguments: json!({}),
                    });
                }
                let own =
                    if mcp {
                        parse.served.lock().ok().and_then(|mut served| {
                            served.get_mut(&name).and_then(|q| q.pop_front())
                        })
                    } else {
                        None
                    };
                let result = if let Some(val) = own {
                    val
                } else if status == "failed" {
                    serde_json::from_str::<Value>(&text)
                        .ok()
                        .filter(|v| mcp && v.get("status").is_some())
                        .unwrap_or_else(|| {
                            error_result(
                                "tool_error",
                                if text.is_empty() {
                                    "tool failed"
                                } else {
                                    &text
                                },
                            )
                        })
                } else {
                    serde_json::from_str::<Value>(text.trim_start())
                        .ok()
                        .filter(|v| mcp && v.get("status").is_some())
                        .unwrap_or_else(|| json!({"schema_version": 1, "status": "success", "data": {"output": text}}))
                };
                state.events.push(HarnessEvent::ToolResult {
                    tool_name: name,
                    result,
                });
                (true, false)
            }
            _ => (false, false),
        }
    }

    fn apply_cli_event(
        &self,
        state: &mut HarnessState,
        event: &Value,
        sink: Option<&StreamHandle>,
        parse: &mut CliParse,
    ) -> bool {
        let tool_names = &mut parse.tool_names;
        match event.get("type").and_then(Value::as_str) {
            Some("stream_event") => {
                let n = |v: Option<&Value>, k: &str| {
                    v.and_then(|u| u.get(k))
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                };
                match event.pointer("/event/type").and_then(Value::as_str) {
                    Some("message_start") => {
                        let usage = event.pointer("/event/message/usage");
                        let prompt = n(usage, "input_tokens")
                            + n(usage, "cache_read_input_tokens")
                            + n(usage, "cache_creation_input_tokens");
                        parse.claude_call = Some(crate::llm::TokenUsage {
                            prompt_tokens: prompt,
                            completion_tokens: n(usage, "output_tokens"),
                            cache_read_tokens: n(usage, "cache_read_input_tokens"),
                            cache_creation_tokens: n(usage, "cache_creation_input_tokens"),
                            ..Default::default()
                        });
                    }
                    Some("message_delta") => {
                        let out = n(event.pointer("/event/usage"), "output_tokens");
                        if let Some(call) = parse.claude_call.as_mut()
                            && out > 0
                        {
                            call.completion_tokens = out;
                        }
                    }
                    Some("message_stop") => {
                        if let Some(mut call) = parse.claude_call.take() {
                            call.total_tokens = call.prompt_tokens + call.completion_tokens;
                            state.last_prompt_tokens = call.prompt_tokens;
                            parse.calls.push(call);
                        }
                    }
                    _ => {}
                }
                if let (Some(sink), Some(delta)) = (sink, event.pointer("/event/delta")) {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                StreamBuffer::append(sink, text);
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                StreamBuffer::append_thinking(sink, text);
                            }
                        }
                        _ => {}
                    }
                }
                false
            }
            Some("assistant") => {
                if state.status == HarnessStatus::Idle {
                    state.status = HarnessStatus::Running;
                }
                let blocks = event
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut changed = false;
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim()
                                .to_string();
                            if !text.is_empty() {
                                if let Some(sink) = sink {
                                    StreamBuffer::clear(sink);
                                }
                                state.messages.push(HarnessMessage::Assistant {
                                    content: text.clone(),
                                    tool_calls: Vec::new(),
                                });
                                state
                                    .events
                                    .push(HarnessEvent::AssistantText { text, quiet: false });
                                self.lane_progress("model", super::step::MODEL_ACTIVITY_TEXT);
                                changed = true;
                            }
                        }
                        Some("tool_use") => {
                            let raw = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let (name, arguments) = unwrap_custom(
                                raw.strip_prefix(MCP_PREFIX).unwrap_or(raw).to_string(),
                                block.get("input").cloned().unwrap_or(Value::Null),
                            );
                            if let Some(id) = block.get("id").and_then(Value::as_str) {
                                tool_names.insert(id.to_string(), name.clone());
                            }
                            if !QUIET_TOOLS.contains(&name.as_str()) {
                                self.lane_progress(
                                    "tool_call",
                                    super::dispatch::tool_activity_text(&name, &arguments),
                                );
                                self.presence(
                                    "tool",
                                    &super::presence::tool_detail(&name, &arguments),
                                );
                                state.events.push(HarnessEvent::ToolCall {
                                    tool_name: name,
                                    arguments,
                                });
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                }
                changed
            }
            Some("user") => {
                let blocks = event
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut changed = false;
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = tool_names
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| "tool".to_string());
                    if QUIET_TOOLS.contains(&name.as_str()) {
                        continue;
                    }
                    state.events.push(HarnessEvent::ToolResult {
                        tool_name: name,
                        result: tool_result_from_block(&block),
                    });
                    changed = true;
                }
                changed
            }
            Some("rate_limit_event") => {
                let windows = event.pointer("/rate_limit_info/unifiedWindows");
                let window = |key: &str, minutes: i64| {
                    windows
                        .and_then(|w| w.get(key))
                        .map(|w| crate::llm::RateLimitWindow {
                            used_percent: w
                                .get("utilization")
                                .and_then(Value::as_f64)
                                .unwrap_or(0.0)
                                * 100.0,
                            window_minutes: minutes,
                            resets_at: w.get("resetsAt").and_then(Value::as_i64).unwrap_or(0),
                        })
                };
                let snapshot = crate::llm::RateLimitSnapshot {
                    primary: window("five_hour", 300),
                    secondary: window("seven_day", 10080),
                    label: None,
                };
                if let Ok(body) = serde_json::to_string(&snapshot) {
                    let path = claude_rate_limits_path();
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    let _ = std::fs::write(path, body);
                }
                state.rate_limit = Some(snapshot);
                true
            }
            Some("result") => {
                if let Some(sink) = sink {
                    StreamBuffer::clear(sink);
                }
                let usage = event.get("usage").cloned().unwrap_or(Value::Null);
                let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
                let prompt = n("input_tokens")
                    + n("cache_read_input_tokens")
                    + n("cache_creation_input_tokens");
                let completion = n("output_tokens");
                state.prompt_tokens += prompt;
                state.completion_tokens += completion;
                state.total_tokens += prompt + completion;
                state.cache_read_tokens += n("cache_read_input_tokens");
                state.iterations +=
                    event.get("num_turns").and_then(Value::as_u64).unwrap_or(1) as usize;
                let failed = event
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if failed {
                    let message = event
                        .get("result")
                        .and_then(Value::as_str)
                        .or_else(|| event.get("subtype").and_then(Value::as_str))
                        .unwrap_or("Claude Code reported an error")
                        .to_string();
                    if matches!(state.events.last(), Some(HarnessEvent::AssistantText { text, .. }) if text.trim() == message.trim())
                    {
                        state.events.pop();
                        if matches!(state.messages.last(), Some(HarnessMessage::Assistant { content, tool_calls }) if tool_calls.is_empty() && content.trim() == message.trim())
                        {
                            state.messages.pop();
                        }
                    }
                    state.events.push(HarnessEvent::ModelError { message });
                } else if let Some(text) = event.get("result").and_then(Value::as_str) {
                    state.final_text = Some(text.to_string());
                }
                if state.status == HarnessStatus::Running {
                    state.status = HarnessStatus::Idle;
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod goal_tests {
    use super::*;

    fn active_goal_state(status: HarnessStatus) -> HarnessState {
        HarnessState {
            status,
            goal: Some(Goal {
                text: "ship it".into(),
                dir: String::new(),
                status: GoalStatus::Active,
                autonomous_turns: 0,
                resume_at: 0,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn cli_agents_are_served_complete_goal() {
        let names = |once| -> Vec<String> {
            cli_meta_tool_definitions(once, false, &[])
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert!(names(false).iter().any(|n| n == "complete_goal"));
        assert!(!names(true).iter().any(|n| n == "complete_goal"));
    }

    #[test]
    fn idle_cli_loop_drives_an_active_goal() {
        assert!(cli_goal_due(
            &active_goal_state(HarnessStatus::Idle),
            true,
            true,
            false
        ));
        assert!(!cli_goal_due(
            &active_goal_state(HarnessStatus::Running),
            true,
            true,
            false
        ));
        assert!(!cli_goal_due(
            &active_goal_state(HarnessStatus::Idle),
            true,
            false,
            false
        ));
        assert!(!cli_goal_due(
            &active_goal_state(HarnessStatus::Idle),
            false,
            true,
            false
        ));
        assert!(!cli_goal_due(
            &active_goal_state(HarnessStatus::Idle),
            true,
            true,
            true
        ));
        let mut paused = active_goal_state(HarnessStatus::Idle);
        paused.goal.as_mut().unwrap().status = GoalStatus::Paused;
        assert!(!cli_goal_due(&paused, true, true, false));
    }

    #[test]
    fn stall_watch_only_covers_a_silent_running_turn() {
        let running = HarnessState {
            status: HarnessStatus::Running,
            ..Default::default()
        };
        assert!(cli_stall_watched(&running, true, false));
        assert!(!cli_stall_watched(&running, false, false));
        assert!(!cli_stall_watched(&running, true, true));
        let idle = HarnessState {
            status: HarnessStatus::Idle,
            ..Default::default()
        };
        assert!(!cli_stall_watched(&idle, true, false));
    }
}

#[cfg(test)]
mod lane_progress_tests {
    use super::*;

    #[test]
    fn claude_tool_use_publishes_lane_progress() {
        let (tx, mut rx) = mpsc::unbounded_channel::<crate::lanes::LaneProgress>();
        let config = HarnessConfig {
            progress_tx: Some(tx),
            progress_id: Some("lane-1".into()),
            ..HarnessConfig::default()
        };
        let harness = CodingHarness::new(
            config,
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        );
        let mut state = HarnessState::default();
        let mut parse = CliParse::default();
        let event = serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "Looking."},
            {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "git log", "description": "Check lane progress"}}
        ]}});
        harness.apply_cli_event(&mut state, &event, None, &mut parse);
        let got: Vec<(String, String)> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|p| (p.kind, p.text))
            .collect();
        assert_eq!(
            got,
            vec![
                ("model".into(), "Thinking".into()),
                ("tool_call".into(), "Check lane progress".into())
            ]
        );
    }
}

#[cfg(test)]
mod devin_acp_tests {
    use super::*;

    fn test_harness() -> CodingHarness {
        CodingHarness::new(
            HarnessConfig::default(),
            ToolRegistry::new(),
            ToolContext::new(std::env::temp_dir()).expect("ctx"),
        )
    }

    #[test]
    fn cli_kind_recognises_devin_and_maps_swe2_efforts() {
        assert_eq!(CliKind::of("devin"), CliKind::Devin);
        assert_eq!(CliKind::Devin.label(), "Devin");
        assert_eq!(CliKind::Devin.dir(), "devin");
        assert_eq!(CliKind::Devin.pooled_provider(), Some("devin"));

        let mut cfg = InferenceProfileConfig::default();
        cfg.provider = "devin".into();
        cfg.model = "swe-2".into();
        cfg.reasoning_effort = Some("high".into());
        assert_eq!(CodingHarness::resolved_devin_model(&cfg), "swe-2-high");

        cfg.model = "swe-2-flash".into();
        cfg.reasoning_effort = Some("medium".into());
        assert_eq!(
            CodingHarness::resolved_devin_model(&cfg),
            "swe-2-flash-medium"
        );
    }

    #[test]
    fn apply_devin_event_streams_mcp_tools_filters_hooks_and_dedupes_usage() {
        let harness = test_harness();
        let mut state = HarnessState {
            status: HarnessStatus::Running,
            ..Default::default()
        };
        let mut parse = CliParse::default();

        // Pre-turn handshake reply (id < 10) is ignored.
        let init_reply =
            json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "oval-booklet"}});
        assert_eq!(
            harness.apply_devin_event(&mut state, &init_reply, None, &mut parse),
            (false, false)
        );

        // Meta mcp_list_tools call and its completion are quiet.
        let list_call = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "c_meta",
                    "rawInput": {"server_name": "snippet"},
                    "_meta": {"cognition.ai/inferenceToolName": "mcp_list_tools"}
                }
            }
        });
        assert_eq!(
            harness.apply_devin_event(&mut state, &list_call, None, &mut parse),
            (false, false)
        );
        let list_done = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "c_meta",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "- bash"}}]
                }
            }
        });
        assert_eq!(
            harness.apply_devin_event(&mut state, &list_done, None, &mut parse),
            (false, false)
        );

        // Hook execution updates are ignored.
        let hook_ev = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "c_hook",
                    "_meta": {"cognition.ai/eventType": "hook_execution", "cognition.ai/toolName": "PreToolUse"}
                }
            }
        });
        assert_eq!(
            harness.apply_devin_event(&mut state, &hook_ev, None, &mut parse),
            (false, false)
        );

        // Snippet MCP tool call and completion emit ToolCall + ToolResult.
        let mcp_call = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "c1",
                    "rawInput": {"command": "ls", "label": "List files"},
                    "_meta": {
                        "cognition.ai/eventType": "mcp_tool_call",
                        "cognition.ai/toolName": "mcp__snippet__bash"
                    }
                }
            }
        });
        assert_eq!(
            harness.apply_devin_event(&mut state, &mcp_call, None, &mut parse),
            (true, false)
        );

        let mcp_done = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "c1",
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "{\"schema_version\":1,\"status\":\"success\",\"data\":{\"output\":\"ok\"}}"}}]
                }
            }
        });
        assert_eq!(
            harness.apply_devin_event(&mut state, &mcp_done, None, &mut parse),
            (true, false)
        );

        // Usage update + duplicate subagent_context copy: counted once.
        let u1 = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 20810,
                    "size": 262144,
                    "_meta": {"cognition.ai/inputTokens": 20798, "cognition.ai/outputTokens": 12, "cognition.ai/cachedReadTokens": 20000}
                }
            }
        });
        let u2 = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {
                    "sessionUpdate": "usage_update",
                    "used": 20810,
                    "size": 262144,
                    "_meta": {
                        "cognition.ai/inputTokens": 20798,
                        "cognition.ai/outputTokens": 12,
                        "cognition.ai/cachedReadTokens": 20000,
                        "cognition.ai/subagent_context": {"parentAgentId": "root"}
                    }
                }
            }
        });
        harness.apply_devin_event(&mut state, &u1, None, &mut parse);
        harness.apply_devin_event(&mut state, &u2, None, &mut parse);
        assert_eq!(parse.calls.len(), 1);
        assert_eq!(parse.devin_window, 262144);
        assert_eq!(state.prompt_tokens, 20798);
        assert_eq!(state.cache_read_tokens, 20000);

        // Message chunk + turn prompt result finishes the turn and sets final_text.
        let chunk = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "oval-booklet",
                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "All done."}}
            }
        });
        harness.apply_devin_event(&mut state, &chunk, None, &mut parse);
        let prompt_res = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "result": {"stopReason": "end_turn", "usage": {"totalTokens": 20810, "inputTokens": 20798, "outputTokens": 12, "cachedReadTokens": 20000}}
        });
        let (changed, finished) =
            harness.apply_devin_event(&mut state, &prompt_res, None, &mut parse);
        assert!(changed && finished);
        assert_eq!(state.status, HarnessStatus::Idle);
        assert_eq!(state.final_text.as_deref(), Some("All done."));
        assert_eq!(parse.calls.len(), 1);
        assert!(
            state.events.iter().any(
                |e| matches!(e, HarnessEvent::ToolCall { tool_name, .. } if tool_name == "bash")
            )
        );
        assert!(state.events.iter().any(
            |e| matches!(e, HarnessEvent::ToolResult { tool_name, .. } if tool_name == "bash")
        ));
    }

    #[test]
    fn apply_devin_event_surfaces_32011_quota_error_for_rotation() {
        let harness = test_harness();
        let mut state = HarnessState {
            status: HarnessStatus::Running,
            ..Default::default()
        };
        let mut parse = CliParse::default();
        let quota_err = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "error": {
                "code": -32011,
                "message": "Quota exhausted.",
                "data": {"cognition.ai/errorKind": "resource_exhausted", "cognition.ai/retryAfterSeconds": 600}
            }
        });
        assert!(crate::devin::quota_exhausted_hint(&quota_err).is_some());
        let (changed, finished) =
            harness.apply_devin_event(&mut state, &quota_err, None, &mut parse);
        assert!(changed && finished);
        assert!(
            matches!(state.events.last(), Some(HarnessEvent::ModelError { message }) if message == "Quota exhausted.")
        );
    }

    #[tokio::test]
    #[ignore = "spawns the local Devin CLI"]
    async fn devin_live_acp_turn_tool_call_and_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("probe.txt"), "sentinel-42\n").unwrap();
        let bin = crate::devin::binary().expect("devin binary");
        let fake_home = dir.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();
        if let Some(real_devin) = crate::devin::real_data_dir() {
            let fake_devin = fake_home.join(".local/share/devin");
            std::fs::create_dir_all(&fake_devin).unwrap();
            let creds = real_devin.join("credentials.toml");
            if creds.is_file() {
                std::os::unix::fs::symlink(&creds, fake_devin.join("credentials.toml")).unwrap();
            }
        }
        if let Some(real_cfg) = crate::devin::real_home()
            .map(|h| h.join(".config/devin/config.json"))
            .filter(|p| p.is_file())
        {
            let fake_cfg = fake_home.join(".config/devin");
            std::fs::create_dir_all(&fake_cfg).unwrap();
            std::os::unix::fs::symlink(&real_cfg, fake_cfg.join("config.json")).unwrap();
        }
        unsafe {
            std::env::set_var("SNIPPET_DEVIN_BIN", &bin);
            std::env::set_var("HOME", &fake_home);
        }
        let snippet_home = crate::config::snippet_home();
        let state_path = snippet_home.join("workspaces/devin-live-smoke");
        let mut profile = InferenceProfileConfig::default();
        profile.provider = "devin".into();
        profile.model = "swe-2-flash".into();

        let store_path = snippet_home.join("app.db");
        let make = |resume: bool| {
            let tools = crate::builtins::coding_tools(None);
            let ctx = ToolContext::new(dir.path().to_path_buf())
                .unwrap()
                .with_store_path(&store_path);
            let cfg = HarnessConfig {
                state_path: Some(state_path.clone()),
                resume,
                memory_enabled: false,
                ..HarnessConfig::default()
            };
            CodingHarness::new(cfg, tools, ctx)
        };

        let h1 = make(false);
        let s1 = h1
            .run_cli_lane(
                profile.clone(),
                "Read probe.txt with bash (`cat probe.txt`), remember the codeword FALCON, and reply with the exact line from probe.txt.".into(),
            )
            .await
            .expect("turn 1");
        assert!(
            s1.final_text
                .as_deref()
                .unwrap_or("")
                .contains("sentinel-42"),
            "s1 final_text: {:?}",
            s1.final_text
        );
        assert!(
            s1.events.iter().any(
                |e| matches!(e, HarnessEvent::ToolCall { tool_name, .. } if tool_name == "bash")
            ),
            "s1 events: {:?}",
            s1.events
        );
        assert!(s1.iterations >= 1);

        let h2 = make(true);
        let s2 = h2
            .run_cli_lane(
                profile,
                "What codeword did I ask you to remember in the previous turn? Reply with only that codeword.".into(),
            )
            .await
            .expect("turn 2");
        assert!(
            s2.final_text
                .as_deref()
                .unwrap_or("")
                .to_uppercase()
                .contains("FALCON"),
            "s2 final_text: {:?}",
            s2.final_text
        );
        assert!(
            !s2.events.iter().any(|e| matches!(e, HarnessEvent::SystemDecision { step, .. } if step == "runtime_reseeded")),
            "s2 should resume via session/load without reseeding: {:?}",
            s2.events
        );
    }
}

#[cfg(test)]
mod devinrel_tests {
    use super::*;
    use serde_json::json;

    fn known() -> Vec<String> {
        ["bash", "change_files", "update_plan", "ask_user"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn guide_shows_enum_values_and_array_item_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "changes": {"type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "replace"]},
                        "path": {"type": "string"},
                        "find": {"type": "string"}
                    },
                    "required": ["action", "path"]
                }}
            },
            "required": ["changes"]
        });
        assert_eq!(
            guide_args(&schema, 0),
            vec!["changes: [{action: create|replace, [find: string], path: string}]"]
        );
    }

    #[test]
    fn normalise_read_aliases_become_bash() {
        let k = known();
        for name in ["read_file", "read", "view_file", "cat"] {
            let (n, a) = normalise_call(
                name,
                json!({"path": "src/main.rs", "start_line": 3, "end_line": 9}),
                &k,
            );
            assert_eq!(n, "bash");
            assert_eq!(a, json!({"command": "sed -n '3,9p' 'src/main.rs'"}));
        }
        let (n, a) = normalise_call(
            "read_file",
            json!({"path": "a b.txt", "offset": 10, "limit": 5}),
            &k,
        );
        assert_eq!(n, "bash");
        assert_eq!(a["command"], "sed -n '11,15p' 'a b.txt'");
        let (n, a) = normalise_call("cat", json!({"path": "x"}), &k);
        assert_eq!(n, "bash");
        assert_eq!(a["command"], "cat -n 'x'");
    }

    #[test]
    fn normalise_write_aliases_become_change_files_create() {
        let k = known();
        for name in ["write", "write_file", "create_file"] {
            let (n, a) = normalise_call(name, json!({"path": "f.txt", "content": "hi"}), &k);
            assert_eq!(n, "change_files");
            assert_eq!(a["changes"][0]["action"], "create");
            assert_eq!(a["changes"][0]["path"], "f.txt");
            assert_eq!(a["changes"][0]["content"], "hi");
            assert_eq!(a["changes"][0]["overwrite"], true);
        }
    }

    #[test]
    fn normalise_edit_aliases_become_change_files_replace() {
        let k = known();
        for name in ["edit", "edit_file", "str_replace", "replace"] {
            let (n, a) = normalise_call(
                name,
                json!({"path": "f", "old_string": "a", "new_string": "b"}),
                &k,
            );
            assert_eq!(n, "change_files");
            assert_eq!(a["changes"][0]["action"], "replace");
            assert_eq!(a["changes"][0]["find"], "a");
            assert_eq!(a["changes"][0]["with"], "b");
        }
        let (n, a) = normalise_call("edit", json!({"path": "f", "find": "x", "with": "y"}), &k);
        assert_eq!(n, "change_files");
        assert_eq!(a["changes"][0]["find"], "x");
    }

    #[test]
    fn normalise_change_files_overwrite_action() {
        let k = known();
        let (n, a) = normalise_call(
            "change_files",
            json!({"changes": [{"action": "overwrite", "path": "f", "content": "x"}]}),
            &k,
        );
        assert_eq!(n, "change_files");
        assert_eq!(a["changes"][0]["action"], "create");
        assert_eq!(a["changes"][0]["overwrite"], true);
    }

    #[test]
    fn normalise_change_files_file_key_becomes_path() {
        let k = known();
        let (n, a) = normalise_call(
            "change_files",
            json!({"changes": [{"action": "delete", "file": "old.txt"}]}),
            &k,
        );
        assert_eq!(n, "change_files");
        assert_eq!(a["changes"][0]["path"], "old.txt");
        assert!(a["changes"][0].get("file").is_none());
    }

    #[test]
    fn normalise_bash_command_aliases() {
        let k = known();
        for key in ["cmd", "script", "shell"] {
            let (n, a) = normalise_call("bash", json!({key: "ls -la"}), &k);
            assert_eq!(n, "bash");
            assert_eq!(a["command"], "ls -la");
            assert!(a.get(key).is_none());
        }
        let (n, a) = normalise_call("bash", json!({}), &k);
        assert_eq!(n, "bash");
        assert_eq!(a, json!({}));
    }

    #[test]
    fn normalise_list_dir_becomes_ls() {
        let k = known();
        for name in ["list_dir", "ls"] {
            let (n, a) = normalise_call(name, json!({"path": "src"}), &k);
            assert_eq!(n, "bash");
            assert_eq!(a["command"], "ls -la 'src'");
        }
    }

    #[test]
    fn normalise_grep_becomes_rg() {
        let k = known();
        let (n, a) = normalise_call("grep", json!({"pattern": "foo bar", "path": "src"}), &k);
        assert_eq!(n, "bash");
        assert_eq!(a["command"], "rg -n 'foo bar' 'src'");
        let (n, a) = normalise_call("search", json!({"pattern": "x"}), &k);
        assert_eq!(n, "bash");
        assert_eq!(a["command"], "rg -n 'x'");
    }

    #[test]
    fn normalise_known_tool_untouched() {
        let k = known();
        let (n, a) = normalise_call("bash", json!({"command": "ls"}), &k);
        assert_eq!(n, "bash");
        assert_eq!(a, json!({"command": "ls"}));
        let (n, a) = normalise_call(
            "change_files",
            json!({"changes": [{"action": "delete", "path": "f"}]}),
            &k,
        );
        assert_eq!(n, "change_files");
        assert_eq!(a["changes"][0]["action"], "delete");
        let (n, a) = normalise_call("ask_user", json!({"question": "?"}), &k);
        assert_eq!(n, "ask_user");
        assert_eq!(a, json!({"question": "?"}));
    }

    fn test_mcp_state(token: &str) -> (McpState, mpsc::UnboundedReceiver<McpCall>) {
        let (calls, rx) = mpsc::unbounded_channel();
        (
            McpState {
                token: token.to_string(),
                tools: vec![NativeToolDefinition {
                    name: "bash".into(),
                    description: String::new(),
                    input_schema: json!({}),
                }],
                calls,
                steers: Default::default(),
                served: Default::default(),
                late: Default::default(),
                images: false,
            },
            rx,
        )
    }

    fn call_request(id: u64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "bash", "arguments": {"command": "sleep 999"}}})
    }

    #[tokio::test(start_paused = true)]
    async fn tools_call_clamps_at_sync_wait_and_delivers_late() {
        let token = "tok".to_string();
        let (state, mut calls) = test_mcp_state(&token);
        let state = std::sync::Arc::new(state);

        let st = state.clone();
        let first = tokio::spawn(async move { mcp_dispatch(&st, call_request(1)).await });
        let call = calls.recv().await.expect("dispatch sent the call");
        tokio::time::advance(std::time::Duration::from_secs(201)).await;
        let reply = first.await.expect("dispatch join").expect("dispatch reply");
        let text = reply
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .expect("reply text");
        assert!(text.contains("is still running after 200s"), "{text}");
        assert!(text.contains("<late_tool_result name=\"bash\">"), "{text}");

        call.reply
            .send(json!({"schema_version": 1, "status": "success", "data": {"output": "late output"}}))
            .expect("send late reply");
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }

        let st = state.clone();
        let second = tokio::spawn(async move { mcp_dispatch(&st, call_request(2)).await });
        let call2 = calls.recv().await.expect("second dispatch sent the call");
        call2
            .reply
            .send(json!({"schema_version": 1, "status": "success", "data": {"output": "ok"}}))
            .expect("send second reply");
        let reply2 = second
            .await
            .expect("dispatch2 join")
            .expect("dispatch2 reply");
        let content = reply2
            .pointer("/result/content")
            .and_then(Value::as_array)
            .expect("content array");
        let late_text = content
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .find(|t| t.contains("<late_tool_result"))
            .expect("late_tool_result block");
        assert!(late_text.contains("name=\"bash\""), "{late_text}");
        assert!(late_text.contains("late output"), "{late_text}");
        let served: Vec<String> = state.served.lock().unwrap()["bash"]
            .iter()
            .map(|v| v["data"]["output"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(served.len(), 2, "one served entry per call: {served:?}");
        assert!(served[0].contains("still running"), "{served:?}");
        assert_eq!(
            served[1], "ok",
            "the second call records its own value: {served:?}"
        );
    }

    fn get_request(method: &str, token: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(format!("/mcp/{token}"))
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn mcp_http_get_delete_and_session_id() {
        let token = "0123456789abcdef0123456789abcdef".to_string();
        let (state, _calls) = test_mcp_state(&token);

        let res = mcp_get(
            AxState(state.clone()),
            AxPath(token.clone()),
            get_request("GET", &token),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("text/event-stream"),
            "{content_type}"
        );
        assert_eq!(res.headers().get("cache-control").unwrap(), "no-cache");
        assert_eq!(
            res.headers().get("mcp-session-id").unwrap(),
            "snippet-01234567"
        );

        let res = mcp_get(
            AxState(state.clone()),
            AxPath("badtoken".into()),
            get_request("GET", "badtoken"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = mcp_get(
            AxState(state.clone()),
            AxPath(token.clone()),
            get_request("HEAD", &token),
        )
        .await;
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);

        let res = mcp_delete(AxState(state.clone()), AxPath(token.clone())).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let res = mcp_handler(
            AxState(state),
            AxPath(token),
            Json(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})),
        )
        .await;
        assert_eq!(
            res.headers().get("mcp-session-id").unwrap(),
            "snippet-01234567"
        );
    }

    #[test]
    fn stall_action_restarts_devin_and_claude_once() {
        assert_eq!(stall_action(0, CliKind::Devin), StallAction::Restart);
        assert_eq!(stall_action(0, CliKind::ClaudeCode), StallAction::Restart);
        assert_eq!(stall_action(1, CliKind::Devin), StallAction::Fail);
        assert_eq!(stall_action(1, CliKind::ClaudeCode), StallAction::Fail);
        assert_eq!(stall_action(0, CliKind::Antigravity), StallAction::Fail);
    }
}
