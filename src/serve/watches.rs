//! Agent watches: the runner and the HTTP surface.
//!
//! The store half lives in [`crate::coordination::watches`]. This module owns
//! the processes: a supervisor reconciles the table against live tasks every
//! couple of seconds, poll watches run their script on an interval, stream
//! watches tail stdout into batched wakes, and every delivery goes to the
//! owner's desk through the same path group wakes use.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Shared, api_error};
use crate::coordination::types::AgentStatus;
use crate::coordination::watches as cw;
use crate::store::Store;

/// Supervisor cadence: reconcile store vs tasks, release held pendings.
const TICK: Duration = Duration::from_secs(2);
/// A poll run is killed after this long.
const POLL_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_STDOUT_CAP: usize = 64 * 1024;
const POLL_STDERR_CAP: usize = 2 * 1024;
/// Stream batching: flush after this much quiet, or this long after the first
/// buffered line, whichever comes first.
const STREAM_QUIET: Duration = Duration::from_secs(5);
const STREAM_MAX_BATCH: Duration = Duration::from_secs(30);
const STREAM_LINE_CAP: usize = 200;
/// Crash-loop guard: this many exits inside the window is an error state.
const STREAM_EXIT_WINDOW: Duration = Duration::from_secs(600);
const STREAM_ERROR_EXITS: usize = 5;
const STREAM_BASE_BACKOFF: Duration = Duration::from_secs(5);
const STREAM_MAX_BACKOFF: Duration = Duration::from_secs(300);
const STREAM_ALIVE_RESET: Duration = Duration::from_secs(600);
const ERROR_EXCERPT_CAP: usize = 4 * 1024;

// ---------------------------------------------------------------------------
// Pure pieces (unit-tested without a daemon)
// ---------------------------------------------------------------------------

/// Trimmed, non-empty stdout lines, optionally filtered.
pub fn output_lines(text: &str, filter: Option<&Regex>) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .filter(|line| filter.is_none_or(|re| re.is_match(line)))
        .map(str::to_string)
        .collect()
}

/// Lines in `current` not present in `prev` (multiset difference, order kept).
/// `None` on the first successful run — the caller stores the baseline only.
pub fn new_lines(prev: Option<&str>, current: &str, filter: Option<&Regex>) -> Option<Vec<String>> {
    let prev = prev?;
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for line in prev.lines() {
        *seen.entry(line).or_default() += 1;
    }
    let mut out = Vec::new();
    for line in output_lines(current, filter) {
        match seen.get_mut(line.as_str()) {
            Some(count) if *count > 0 => *count -= 1,
            _ => out.push(line),
        }
    }
    Some(out)
}

/// Buffered stream lines with the 5s-quiet / 30s-max flush rule and a
/// 200-line cap that drops the oldest.
#[derive(Default)]
pub struct StreamBatcher {
    pending: VecDeque<String>,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
    dropped: usize,
}

impl StreamBatcher {
    pub fn push(&mut self, now: Instant, line: String) {
        if self.pending.is_empty() {
            self.first_at = Some(now);
        }
        self.last_at = Some(now);
        if self.pending.len() >= STREAM_LINE_CAP {
            self.pending.pop_front();
            self.dropped += 1;
        }
        self.pending.push_back(line);
    }

    pub fn due(&self, now: Instant) -> bool {
        let (Some(first), Some(last)) = (self.first_at, self.last_at) else {
            return false;
        };
        now.duration_since(last) >= STREAM_QUIET || now.duration_since(first) >= STREAM_MAX_BATCH
    }

    /// The instant the current buffer becomes due, if it holds anything.
    pub fn next_deadline(&self) -> Option<Instant> {
        let first = self.first_at?;
        let last = self.last_at?;
        Some((last + STREAM_QUIET).min(first + STREAM_MAX_BATCH))
    }

    /// Take the buffer; returns the lines and how many were dropped at the cap.
    pub fn flush(&mut self) -> Option<(Vec<String>, usize)> {
        if self.pending.is_empty() {
            return None;
        }
        let lines: Vec<String> = self.pending.drain(..).collect();
        let dropped = std::mem::take(&mut self.dropped);
        self.first_at = None;
        self.last_at = None;
        Some((lines, dropped))
    }

    pub fn flush_due(&mut self, now: Instant) -> Option<(Vec<String>, usize)> {
        if self.due(now) { self.flush() } else { None }
    }
}

/// Whether an emit may fire now: the watch's own quiet hours or the daily cap
/// hold it. `fires_last_day` counts 'fired' events in the last 24 h.
pub fn gate_open(
    watch: &cw::Watch,
    offset: chrono::FixedOffset,
    now: chrono::DateTime<chrono::Utc>,
    fires_last_day: i64,
) -> bool {
    if crate::mission_autonomy::quiet_ends_after(
        watch.quiet_start.as_deref(),
        watch.quiet_end.as_deref(),
        offset,
        now,
    )
    .is_some()
    {
        return false;
    }
    fires_last_day < watch.max_wakes_per_day as i64
}

/// The `[watch]` envelope. The body is capped at 4 KB, cut at a line boundary.
pub fn watch_envelope(watch: &cw::Watch, event: &str, body: &[String], held: usize) -> String {
    let mut text = body.join("\n");
    const CAP: usize = 4096;
    if text.len() > CAP {
        let mut end = CAP.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if let Some(nl) = text[..end].rfind('\n') {
            end = nl;
        }
        text.truncate(end);
    }
    let mut out = format!(
        "[watch]\nname: {}\nwatch_id: {}\nmode: {}\nevent: {}\nlines: {}",
        watch.name,
        watch.id,
        watch.mode,
        event,
        body.len()
    );
    if held > 0 {
        out.push_str(&format!("\nheld: {held}"));
    }
    out.push_str(&format!(
        "\n---\n{text}\n---\nnote: your own watch fired. Act on what matters (answer, file a task, tell the user per your reach policy); otherwise end quietly. If it misfires, fix it with watch update or remove it.\n[/watch]"
    ));
    out
}

/// Merge held lines with new ones, capped at 16 KB dropping the oldest.
fn merge_pending(old: Option<&str>, new: &[String]) -> String {
    let mut lines: Vec<String> = old
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default();
    lines.extend(new.iter().cloned());
    let bytes = |ls: &[String]| ls.iter().map(|l| l.len() + 1).sum::<usize>();
    while lines.len() > 1 && bytes(&lines) > cw::PENDING_CAP_BYTES {
        lines.remove(0);
    }
    if bytes(&lines) > cw::PENDING_CAP_BYTES
        && let Some(first) = lines.first_mut()
    {
        let mut end = cw::PENDING_CAP_BYTES;
        while !first.is_char_boundary(end.min(first.len())) {
            end -= 1;
        }
        first.truncate(end.min(first.len()));
    }
    lines.join("\n")
}

fn excerpt_cap(text: &str) -> String {
    let scrubbed = crate::vault::Vault::load().scrub_str(text);
    if scrubbed.len() <= ERROR_EXCERPT_CAP {
        return scrubbed;
    }
    let mut end = ERROR_EXCERPT_CAP;
    while !scrubbed.is_char_boundary(end) {
        end -= 1;
    }
    scrubbed[..end].to_string()
}

// ---------------------------------------------------------------------------
// Process plumbing
// ---------------------------------------------------------------------------

fn systemd_run_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("systemd-run").is_file())
        })
    })
}

pub(crate) fn watch_command(
    script: &Path,
    name: &str,
    state_dir: &Path,
    script_text: &str,
) -> Command {
    let mut cmd = if systemd_run_available() {
        let mut cmd = Command::new("systemd-run");
        cmd.args([
            "--user",
            "--scope",
            "--quiet",
            "-p",
            "MemoryMax=256M",
            "-p",
            "CPUQuota=50%",
            "bash",
            &script.to_string_lossy(),
        ]);
        cmd
    } else {
        let mut cmd = Command::new("bash");
        cmd.arg(script);
        cmd
    };
    cmd.current_dir(script.parent().unwrap_or_else(|| Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    cmd.env("SNIPPET_WATCH_NAME", name)
        .env("SNIPPET_WATCH_STATE_DIR", state_dir);
    for (key, value) in crate::vault::Vault::load().env_for_command(script_text) {
        cmd.env(key, value);
    }
    cmd
}

/// SIGTERM the whole process group, SIGKILL 3 s later if it is still there.
pub(crate) fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    });
}

// ---------------------------------------------------------------------------
// Delivery sink
// ---------------------------------------------------------------------------

/// Where a fired watch's envelope goes. The daemon implementation wakes the
/// owner's desk; tests collect envelopes instead.
#[async_trait]
pub trait WatchSink: Send + Sync {
    async fn deliver(&self, owner: &str, envelope: String) -> Result<(), String>;
}

/// The production sink: same wake path `deliver_group` uses.
pub struct DaemonSink {
    d: Shared,
}

impl DaemonSink {
    pub fn new(d: Shared) -> Self {
        Self { d }
    }
}

#[async_trait]
impl WatchSink for DaemonSink {
    async fn deliver(&self, owner: &str, envelope: String) -> Result<(), String> {
        if owner != crate::mission_control::SESSION_ID {
            super::direct::ensure_agent_inbox(&self.d, owner).await?;
        }
        let session_id = super::direct::recipient_session_id(owner);
        let Some((tx, _path, _stream)) = self.d.ensure_live(&session_id).await else {
            return Err(format!("could not start session `{session_id}`"));
        };
        drop(tx);
        super::direct::deliver_by_urgency(&self.d, &session_id, envelope, &json!({}), false).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct TaskEntry {
    updated_at: String,
    pid: u32,
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct RestartState {
    exits: VecDeque<Instant>,
    not_before: Option<Instant>,
}

enum WatchMsg {
    PollDone {
        watch_id: String,
        updated_at: String,
        outcome: PollOutcome,
    },
    Batch {
        watch_id: String,
        updated_at: String,
        lines: Vec<String>,
        dropped: usize,
    },
    StreamExited {
        watch_id: String,
        updated_at: String,
        code: Option<i32>,
        stderr_tail: String,
        alive: Duration,
    },
}

struct PollOutcome {
    exit_code: Option<i32>,
    timed_out: bool,
    spawn_error: Option<String>,
    stdout: String,
    stderr_tail: String,
}

pub struct WatchRunner {
    store: Store,
    mc_root: PathBuf,
    sink: Arc<dyn WatchSink>,
    tx: mpsc::UnboundedSender<WatchMsg>,
    rx: mpsc::UnboundedReceiver<WatchMsg>,
    tasks: HashMap<String, TaskEntry>,
    restarts: HashMap<String, RestartState>,
}

impl WatchRunner {
    pub fn new(store: Store, mc_root: PathBuf, sink: Arc<dyn WatchSink>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            store,
            mc_root,
            sink,
            tx,
            rx,
            tasks: HashMap::new(),
            restarts: HashMap::new(),
        }
    }

    fn owner_active(&self, owner: &str) -> bool {
        if owner == crate::mission_control::SESSION_ID {
            return true;
        }
        match self.store.get_agent(owner) {
            Ok(Some(agent)) => !matches!(agent.status, AgentStatus::Paused | AgentStatus::Disabled),
            _ => false,
        }
    }

    fn watch_regex(filter: Option<&str>) -> Option<Regex> {
        filter
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .and_then(|f| Regex::new(f).ok())
    }

    fn poll_due(watch: &cw::Watch, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(last) = watch.last_run_at.as_deref() else {
            return true;
        };
        let Ok(last) = chrono::DateTime::parse_from_rfc3339(last) else {
            return true;
        };
        now >= last.with_timezone(&chrono::Utc) + chrono::Duration::seconds(watch.interval_secs)
    }

    /// Spawn the watch's process here so the supervisor knows its pid and can
    /// kill the whole group on stop; the reader task owns the child after.
    async fn spawn_script_task(&mut self, watch: &cw::Watch, stream: bool) {
        let watches_dir = cw::watches_dir(&self.mc_root, &watch.owner);
        let state_dir = cw::state_dir(&self.mc_root, &watch.owner, &watch.name);
        let _ = std::fs::create_dir_all(&state_dir);
        let script = PathBuf::from(&watch.script_path);
        let script_text = std::fs::read_to_string(&script).unwrap_or_default();
        let mut cmd = watch_command(&script, &watch.name, &state_dir, &script_text);
        cmd.current_dir(&watches_dir);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                if stream {
                    let _ = self.tx.send(WatchMsg::StreamExited {
                        watch_id: watch.id.clone(),
                        updated_at: watch.updated_at.clone(),
                        code: None,
                        stderr_tail: format!("spawn failed: {error}"),
                        alive: Duration::ZERO,
                    });
                } else {
                    let _ = self.tx.send(WatchMsg::PollDone {
                        watch_id: watch.id.clone(),
                        updated_at: watch.updated_at.clone(),
                        outcome: PollOutcome {
                            exit_code: None,
                            timed_out: false,
                            spawn_error: Some(format!("spawn failed: {error}")),
                            stdout: String::new(),
                            stderr_tail: String::new(),
                        },
                    });
                }
                return;
            }
        };
        let pid = child.id().unwrap_or(0);
        let tx = self.tx.clone();
        let watch_id = watch.id.clone();
        let updated_at = watch.updated_at.clone();
        let handle = if stream {
            let filter = Self::watch_regex(watch.filter.as_deref());
            tokio::spawn(async move {
                stream_loop(child, tx, watch_id, updated_at, filter).await;
            })
        } else {
            tokio::spawn(async move {
                poll_wait(child, pid, tx, watch_id, updated_at).await;
            })
        };
        self.tasks.insert(
            watch.id.clone(),
            TaskEntry {
                updated_at: watch.updated_at.clone(),
                pid,
                handle,
            },
        );
        if stream {
            let _ = self.store.record_watch_event(&watch.id, "started", None);
        }
    }

    async fn spawn_poll(&mut self, watch: &cw::Watch) {
        self.spawn_script_task(watch, false).await;
    }

    async fn spawn_stream(&mut self, watch: &cw::Watch) {
        self.spawn_script_task(watch, true).await;
    }

    fn stop_task(&mut self, watch_id: &str) {
        if let Some(entry) = self.tasks.remove(watch_id) {
            if entry.pid != 0 {
                kill_group(entry.pid);
            }
            entry.handle.abort();
        }
    }

    /// One supervisor pass: drain messages, reconcile, release held pendings.
    pub async fn step(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.on_msg(msg).await;
        }
        self.reconcile().await;
        self.release_pending().await;
    }

    pub async fn run(&mut self) {
        loop {
            self.step().await;
            tokio::time::sleep(TICK).await;
        }
    }

    async fn reconcile(&mut self) {
        let watches = match self.store.list_watches(None) {
            Ok(watches) => watches,
            Err(error) => {
                eprintln!("[watches] could not list watches: {error}");
                return;
            }
        };
        let now = chrono::Utc::now();
        let mut wanted: HashMap<String, cw::Watch> = HashMap::new();
        for watch in &watches {
            let runnable = watch.enabled && self.owner_active(&watch.owner);
            if !runnable {
                self.stop_task(&watch.id);
                continue;
            }
            wanted.insert(watch.id.clone(), watch.clone());
            match watch.mode.as_str() {
                "poll" => {
                    if self.tasks.contains_key(&watch.id) {
                        continue;
                    }
                    if Self::poll_due(watch, now) {
                        self.spawn_poll(watch).await;
                    }
                }
                "stream" => {
                    let stale = self
                        .tasks
                        .get(&watch.id)
                        .is_some_and(|e| e.updated_at != watch.updated_at);
                    if stale {
                        self.stop_task(&watch.id);
                        self.restarts.remove(&watch.id);
                    }
                    if self.tasks.contains_key(&watch.id) {
                        continue;
                    }
                    let ready = self
                        .restarts
                        .get(&watch.id)
                        .and_then(|r| r.not_before)
                        .map(|at| Instant::now() >= at)
                        .unwrap_or(true);
                    if ready {
                        self.spawn_stream(watch).await;
                    }
                }
                _ => {}
            }
        }
        // Stop tasks whose watch is gone, disabled, or edited underneath them.
        let ids: Vec<String> = self.tasks.keys().cloned().collect();
        for id in ids {
            let keep = wanted.get(&id).is_some_and(|w| {
                self.tasks
                    .get(&id)
                    .is_some_and(|e| e.updated_at == w.updated_at)
            });
            if !keep {
                self.stop_task(&id);
                if !wanted.contains_key(&id) {
                    self.restarts.remove(&id);
                }
            }
        }
    }

    async fn release_pending(&mut self) {
        let watches = match self.store.list_watches(None) {
            Ok(watches) => watches,
            Err(_) => return,
        };
        let offset = crate::mission_autonomy::offset_or_local(
            crate::mission_autonomy::user_offset_minutes(),
        );
        let now = chrono::Utc::now();
        let day_ago =
            (now - chrono::Duration::hours(24)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        for watch in watches {
            if !watch.enabled || !self.owner_active(&watch.owner) {
                continue;
            }
            let Some(pending) = watch.pending.as_deref().filter(|p| !p.is_empty()) else {
                continue;
            };
            let fires = self
                .store
                .count_watch_fires_since(&watch.id, &day_ago)
                .unwrap_or(0);
            if !gate_open(&watch, offset, now, fires) {
                continue;
            }
            let lines: Vec<String> = pending.lines().map(str::to_string).collect();
            let held = lines.len();
            if self.deliver_fired(&watch, &lines, held).await.is_ok() {
                let _ = self.store.set_watch_runtime(
                    &watch.id,
                    &cw::WatchRuntime {
                        pending: Some(None),
                        last_fired_at: Some(cw::now_rfc3339()),
                        ..cw::WatchRuntime::default()
                    },
                );
                let excerpt = excerpt_cap(
                    &lines
                        .iter()
                        .take(20)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n"),
                );
                let _ = self
                    .store
                    .record_watch_event(&watch.id, "fired", Some(&excerpt));
            }
        }
    }

    /// Deliver pending+new lines when the gate is open, or hold them.
    async fn emit(&mut self, watch: &cw::Watch, new: &[String]) {
        let offset = crate::mission_autonomy::offset_or_local(
            crate::mission_autonomy::user_offset_minutes(),
        );
        let now = chrono::Utc::now();
        let day_ago =
            (now - chrono::Duration::hours(24)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let fires = self
            .store
            .count_watch_fires_since(&watch.id, &day_ago)
            .unwrap_or(0);
        let pending_lines: Vec<String> = watch
            .pending
            .as_deref()
            .map(|p| p.lines().map(str::to_string).collect())
            .unwrap_or_default();
        if !gate_open(watch, offset, now, fires) {
            let mut all = pending_lines.clone();
            all.extend(new.iter().cloned());
            let merged = merge_pending(None, &all);
            let _ = self.store.set_watch_runtime(
                &watch.id,
                &cw::WatchRuntime {
                    pending: Some(Some(merged.clone())),
                    ..cw::WatchRuntime::default()
                },
            );
            let excerpt = excerpt_cap(&merged.lines().take(20).collect::<Vec<_>>().join("\n"));
            let _ = self
                .store
                .record_watch_event(&watch.id, "held", Some(&excerpt));
            return;
        }
        let mut all = pending_lines.clone();
        all.extend(new.iter().cloned());
        let held = pending_lines.len();
        match self.deliver_fired(watch, &all, held).await {
            Ok(()) => {
                let _ = self.store.set_watch_runtime(
                    &watch.id,
                    &cw::WatchRuntime {
                        pending: Some(None),
                        last_fired_at: Some(cw::now_rfc3339()),
                        ..cw::WatchRuntime::default()
                    },
                );
                let excerpt =
                    excerpt_cap(&all.iter().take(20).cloned().collect::<Vec<_>>().join("\n"));
                let _ = self
                    .store
                    .record_watch_event(&watch.id, "fired", Some(&excerpt));
            }
            Err(error) => {
                eprintln!("[watches] delivery failed for {}: {error}", watch.id);
                let merged = merge_pending(watch.pending.as_deref(), &all);
                let _ = self.store.set_watch_runtime(
                    &watch.id,
                    &cw::WatchRuntime {
                        pending: Some(Some(merged)),
                        ..cw::WatchRuntime::default()
                    },
                );
                let _ =
                    self.store
                        .record_watch_event(&watch.id, "error", Some(&excerpt_cap(&error)));
            }
        }
    }

    /// Send the envelope and record the 'fired' event; the caller persists
    /// `last_fired_at` and clears `pending`.
    async fn deliver_fired(
        &self,
        watch: &cw::Watch,
        lines: &[String],
        held: usize,
    ) -> Result<(), String> {
        let envelope = watch_envelope(watch, "change", lines, held);
        self.sink.deliver(&watch.owner, envelope).await
    }

    /// The one-shot `event: error` wake; not gated.
    async fn deliver_error(&self, watch: &cw::Watch, detail: &str) {
        let body: Vec<String> = detail.lines().map(str::to_string).collect();
        let envelope = watch_envelope(watch, "error", &body, 0);
        if let Err(error) = self.sink.deliver(&watch.owner, envelope).await {
            eprintln!("[watches] error wake failed for {}: {error}", watch.id);
        }
        let _ = self
            .store
            .record_watch_event(&watch.id, "error", Some(&excerpt_cap(detail)));
    }

    async fn on_msg(&mut self, msg: WatchMsg) {
        match msg {
            WatchMsg::PollDone {
                watch_id,
                updated_at,
                outcome,
            } => self.on_poll_done(&watch_id, &updated_at, outcome).await,
            WatchMsg::Batch {
                watch_id,
                updated_at,
                lines,
                dropped,
            } => self.on_batch(&watch_id, &updated_at, lines, dropped).await,
            WatchMsg::StreamExited {
                watch_id,
                updated_at,
                code,
                stderr_tail,
                alive,
            } => {
                self.on_stream_exit(&watch_id, &updated_at, code, &stderr_tail, alive)
                    .await
            }
        }
    }

    fn current_watch(&self, watch_id: &str, updated_at: &str) -> Option<cw::Watch> {
        let watch = self.store.watch_by_id(watch_id).ok().flatten()?;
        if !watch.enabled || watch.updated_at != updated_at || !self.owner_active(&watch.owner) {
            return None;
        }
        Some(watch)
    }

    async fn on_poll_done(&mut self, watch_id: &str, updated_at: &str, outcome: PollOutcome) {
        // Only drop the task entry when it is still the run we spawned.
        if self
            .tasks
            .get(watch_id)
            .is_some_and(|e| e.updated_at == updated_at)
        {
            self.tasks.remove(watch_id);
        }
        let Some(watch) = self.current_watch(watch_id, updated_at) else {
            return;
        };
        let now = cw::now_rfc3339();
        let success =
            outcome.exit_code == Some(0) && !outcome.timed_out && outcome.spawn_error.is_none();
        if success {
            let filter = Self::watch_regex(watch.filter.as_deref());
            let current = output_lines(&outcome.stdout, filter.as_ref());
            let stored = current.join("\n");
            match new_lines(
                watch.last_output.as_deref(),
                &outcome.stdout,
                filter.as_ref(),
            ) {
                // First successful run: baseline only, never wakes.
                None => {
                    let _ = self.store.set_watch_runtime(
                        &watch.id,
                        &cw::WatchRuntime {
                            status: Some("running".into()),
                            last_error: Some(None),
                            last_run_at: Some(now),
                            last_output: Some(stored),
                            consecutive_failures: Some(0),
                            ..cw::WatchRuntime::default()
                        },
                    );
                    let _ = self.store.record_watch_event(&watch.id, "started", None);
                }
                Some(new) => {
                    let _ = self.store.set_watch_runtime(
                        &watch.id,
                        &cw::WatchRuntime {
                            status: Some("running".into()),
                            last_error: Some(None),
                            last_run_at: Some(now),
                            last_output: Some(stored),
                            consecutive_failures: Some(0),
                            ..cw::WatchRuntime::default()
                        },
                    );
                    if !new.is_empty() {
                        self.emit(&watch, &new).await;
                    }
                }
            }
            return;
        }
        let failures = watch.consecutive_failures + 1;
        let detail = if let Some(error) = &outcome.spawn_error {
            error.clone()
        } else if outcome.timed_out {
            format!("timed out after {}s", POLL_TIMEOUT.as_secs())
        } else {
            format!(
                "exit {:?}: {}",
                outcome.exit_code,
                outcome.stderr_tail.trim()
            )
        };
        let status = if failures >= 3 { "error" } else { "running" };
        let _ = self.store.set_watch_runtime(
            &watch.id,
            &cw::WatchRuntime {
                status: Some(status.into()),
                last_error: Some(Some(excerpt_cap(&detail))),
                last_run_at: Some(now),
                consecutive_failures: Some(failures),
                ..cw::WatchRuntime::default()
            },
        );
        if failures == 3 {
            self.deliver_error(&watch, &detail).await;
        }
    }

    async fn on_batch(
        &mut self,
        watch_id: &str,
        updated_at: &str,
        lines: Vec<String>,
        dropped: usize,
    ) {
        let Some(watch) = self.current_watch(watch_id, updated_at) else {
            return;
        };
        let mut all = Vec::new();
        if dropped > 0 {
            all.push(format!("[{dropped} earlier lines dropped]"));
        }
        all.extend(lines);
        if !all.is_empty() {
            self.emit(&watch, &all).await;
        }
    }

    async fn on_stream_exit(
        &mut self,
        watch_id: &str,
        updated_at: &str,
        code: Option<i32>,
        stderr_tail: &str,
        alive: Duration,
    ) {
        if self
            .tasks
            .get(watch_id)
            .is_some_and(|e| e.updated_at == updated_at)
        {
            self.tasks.remove(watch_id);
        }
        let Some(watch) = self.current_watch(watch_id, updated_at) else {
            return;
        };
        let now = Instant::now();
        let state = self.restarts.entry(watch_id.to_string()).or_default();
        if alive >= STREAM_ALIVE_RESET {
            state.exits.clear();
        }
        state.exits.push_back(now);
        while state
            .exits
            .front()
            .is_some_and(|t| now.duration_since(*t) > STREAM_EXIT_WINDOW)
        {
            state.exits.pop_front();
        }
        let attempts = state.exits.len();
        if attempts >= STREAM_ERROR_EXITS {
            state.not_before = Some(now + STREAM_MAX_BACKOFF);
            if watch.status != "error" {
                let detail = format!(
                    "stream exited {} times in {}s (last exit {code:?}): {}",
                    attempts,
                    STREAM_EXIT_WINDOW.as_secs(),
                    stderr_tail.trim()
                );
                let _ = self.store.set_watch_runtime(
                    &watch.id,
                    &cw::WatchRuntime {
                        status: Some("error".into()),
                        last_error: Some(Some(excerpt_cap(&detail))),
                        ..cw::WatchRuntime::default()
                    },
                );
                self.deliver_error(&watch, &detail).await;
            }
        } else {
            let backoff = STREAM_BASE_BACKOFF
                .saturating_mul(1u32 << attempts.saturating_sub(1).min(6) as u32)
                .min(STREAM_MAX_BACKOFF);
            state.not_before = Some(now + backoff);
        }
    }
}

/// One poll run on an already-spawned child: wait up to [`POLL_TIMEOUT`],
/// collect capped output, report it for the supervisor.
async fn poll_wait(
    child: tokio::process::Child,
    pid: u32,
    tx: mpsc::UnboundedSender<WatchMsg>,
    watch_id: String,
    updated_at: String,
) {
    let outcome = match tokio::time::timeout(POLL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => PollOutcome {
            exit_code: output.status.code(),
            timed_out: false,
            spawn_error: None,
            stdout: cap_head(&output.stdout, POLL_STDOUT_CAP),
            stderr_tail: cap_tail(&output.stderr, POLL_STDERR_CAP),
        },
        Ok(Err(error)) => PollOutcome {
            exit_code: None,
            timed_out: false,
            spawn_error: Some(format!("wait failed: {error}")),
            stdout: String::new(),
            stderr_tail: String::new(),
        },
        Err(_) => {
            if pid != 0 {
                kill_group(pid);
            }
            PollOutcome {
                exit_code: None,
                timed_out: true,
                spawn_error: None,
                stdout: String::new(),
                stderr_tail: String::new(),
            }
        }
    };
    let _ = tx.send(WatchMsg::PollDone {
        watch_id,
        updated_at,
        outcome,
    });
}

/// One-shot capture for the `watch` tool's `test` action: run the script,
/// wait up to `timeout`, return scrubbed capped output.
#[derive(Default)]
pub(crate) struct ScriptCapture {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub spawn_error: Option<String>,
    pub stdout: String,
    pub stderr_tail: String,
}

pub(crate) async fn run_script_once(
    script: &Path,
    cwd: &Path,
    name: &str,
    state_dir: &Path,
    script_text: &str,
    timeout: Duration,
    stdout_cap: usize,
) -> ScriptCapture {
    let mut cmd = watch_command(script, name, state_dir, script_text);
    cmd.current_dir(cwd);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ScriptCapture {
                exit_code: None,
                timed_out: false,
                spawn_error: Some(format!("spawn failed: {error}")),
                stdout: String::new(),
                stderr_tail: String::new(),
            };
        }
    };
    let pid = child.id();
    let mut capture = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => ScriptCapture {
            exit_code: output.status.code(),
            timed_out: false,
            spawn_error: None,
            stdout: cap_head(&output.stdout, stdout_cap),
            stderr_tail: cap_tail(&output.stderr, POLL_STDERR_CAP),
        },
        Ok(Err(error)) => ScriptCapture {
            exit_code: None,
            timed_out: false,
            spawn_error: Some(format!("wait failed: {error}")),
            stdout: String::new(),
            stderr_tail: String::new(),
        },
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            ScriptCapture {
                exit_code: None,
                timed_out: true,
                spawn_error: None,
                stdout: String::new(),
                stderr_tail: String::new(),
            }
        }
    };
    let vault = crate::vault::Vault::load();
    capture.stdout = vault.scrub_str(&capture.stdout);
    capture.stderr_tail = vault.scrub_str(&capture.stderr_tail);
    capture
}

fn cap_head(bytes: &[u8], cap: usize) -> String {
    let slice = &bytes[..bytes.len().min(cap)];
    String::from_utf8_lossy(slice).into_owned()
}

fn cap_tail(bytes: &[u8], cap: usize) -> String {
    let slice = &bytes[bytes.len().saturating_sub(cap)..];
    String::from_utf8_lossy(slice).into_owned()
}

/// The stream watch's lifetime: read stdout lines into the batcher, flush on
/// the quiet/max deadline, report the exit for the supervisor's backoff.
async fn stream_loop(
    mut child: tokio::process::Child,
    tx: mpsc::UnboundedSender<WatchMsg>,
    watch_id: String,
    updated_at: String,
    filter: Option<Regex>,
) {
    let started = Instant::now();
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    // Drain stderr concurrently so a chatty script cannot deadlock the pipe.
    let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
    {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = tail.lock().unwrap();
                guard.push_str(&line);
                guard.push('\n');
                if guard.len() > POLL_STDERR_CAP * 2 {
                    let cut = guard.len() - POLL_STDERR_CAP;
                    let mut boundary = cut;
                    while !guard.is_char_boundary(boundary) {
                        boundary += 1;
                    }
                    guard.drain(..boundary);
                }
            }
        });
    }
    let mut batcher = StreamBatcher::default();
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let deadline = batcher.next_deadline();
        let step = match deadline {
            Some(at) => {
                tokio::select! {
                    line = lines.next_line() => line,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(at)) => {
                        if let Some((batch, dropped)) = batcher.flush_due(Instant::now()) {
                            let _ = tx.send(WatchMsg::Batch {
                                watch_id: watch_id.clone(),
                                updated_at: updated_at.clone(),
                                lines: batch,
                                dropped,
                            });
                        }
                        continue;
                    }
                }
            }
            None => lines.next_line().await,
        };
        match step {
            Ok(Some(line)) => {
                let trimmed = line.trim_end();
                if filter.as_ref().is_some_and(|re| !re.is_match(trimmed)) {
                    continue;
                }
                if !trimmed.is_empty() {
                    batcher.push(Instant::now(), trimmed.to_string());
                }
            }
            _ => break,
        }
    }
    // Flush whatever is still buffered, then report the exit.
    if let Some((batch, dropped)) = batcher.flush() {
        let _ = tx.send(WatchMsg::Batch {
            watch_id: watch_id.clone(),
            updated_at: updated_at.clone(),
            lines: batch,
            dropped,
        });
    }
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .ok()
        .and_then(|r| r.ok());
    let code = status.and_then(|s| s.code());
    if status.is_none()
        && let Some(pid) = child.id()
    {
        kill_group(pid);
    }
    let tail = stderr_tail.lock().map(|t| t.clone()).unwrap_or_default();
    let tail = cap_tail(tail.as_bytes(), POLL_STDERR_CAP);
    let _ = tx.send(WatchMsg::StreamExited {
        watch_id,
        updated_at,
        code,
        stderr_tail: tail,
        alive: started.elapsed(),
    });
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/agents/{id}/watches", get(list_watches))
        .route("/agents/{id}/watches/{watch_id}/events", get(watch_events))
        .route("/agents/{id}/watches/{watch_id}/script", get(watch_script))
        .route(
            "/agents/{id}/watches/{watch_id}",
            post(set_enabled).delete(delete_watch),
        )
}

fn store_error(error: crate::store::StoreError) -> Response {
    match error {
        crate::store::StoreError::InvalidInput(message) => {
            api_error(StatusCode::BAD_REQUEST, message)
        }
        other => api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("watch: {other}")),
    }
}

fn find_watch(d: &Shared, owner: &str, watch_id: &str) -> Result<cw::Watch, Box<Response>> {
    match d.store.watch_by_id(watch_id) {
        Ok(Some(watch)) if watch.owner == owner => Ok(watch),
        Ok(_) => Err(Box::new(api_error(StatusCode::NOT_FOUND, "no such watch"))),
        Err(error) => Err(Box::new(store_error(error))),
    }
}

async fn list_watches(
    State(d): State<Shared>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match d.store.list_watches(Some(&id)) {
        Ok(watches) => {
            let mut views = Vec::new();
            for watch in &watches {
                match d.store.watch_view(watch) {
                    Ok(view) => views.push(view),
                    Err(error) => return store_error(error),
                }
            }
            Json(json!({"watches": views})).into_response()
        }
        Err(error) => store_error(error),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    limit: Option<u32>,
}

async fn watch_events(
    State(d): State<Shared>,
    axum::extract::Path((id, watch_id)): axum::extract::Path<(String, String)>,
    Query(q): Query<EventsQuery>,
) -> Response {
    let watch = match find_watch(&d, &id, &watch_id) {
        Ok(watch) => watch,
        Err(response) => return *response,
    };
    match d.store.list_watch_events(&watch.id, q.limit.unwrap_or(20)) {
        Ok(events) => {
            let events: Vec<Value> = events
                .into_iter()
                .map(|e| json!({"kind": e.kind, "at": e.at, "excerpt": e.excerpt}))
                .collect();
            Json(json!({"events": events})).into_response()
        }
        Err(error) => store_error(error),
    }
}

async fn watch_script(
    State(d): State<Shared>,
    axum::extract::Path((id, watch_id)): axum::extract::Path<(String, String)>,
) -> Response {
    let watch = match find_watch(&d, &id, &watch_id) {
        Ok(watch) => watch,
        Err(response) => return *response,
    };
    match std::fs::read_to_string(&watch.script_path) {
        Ok(script) => Json(json!({"script": script})).into_response(),
        Err(_) => api_error(StatusCode::NOT_FOUND, "script file is gone"),
    }
}

#[derive(Deserialize)]
struct EnabledReq {
    enabled: bool,
}

async fn set_enabled(
    State(d): State<Shared>,
    axum::extract::Path((id, watch_id)): axum::extract::Path<(String, String)>,
    Json(req): Json<EnabledReq>,
) -> Response {
    let watch = match find_watch(&d, &id, &watch_id) {
        Ok(watch) => watch,
        Err(response) => return *response,
    };
    match d.store.update_watch(
        &watch.id,
        &cw::WatchPatch {
            enabled: Some(req.enabled),
            ..cw::WatchPatch::default()
        },
    ) {
        Ok(Some(updated)) => match d.store.watch_view(&updated) {
            Ok(view) => Json(view).into_response(),
            Err(error) => store_error(error),
        },
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such watch"),
        Err(error) => store_error(error),
    }
}

async fn delete_watch(
    State(d): State<Shared>,
    axum::extract::Path((id, watch_id)): axum::extract::Path<(String, String)>,
) -> Response {
    let watch = match find_watch(&d, &id, &watch_id) {
        Ok(watch) => watch,
        Err(response) => return *response,
    };
    match d.store.delete_watch(&watch.id) {
        Ok(true) => {
            cw::remove_watch_files(&d.mission_control_root, &watch.owner, &watch.name);
            Json(json!({"removed": true})).into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no such watch"),
        Err(error) => store_error(error),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// The supervisor loop, spawned beside `group_dispatch_loop`.
pub async fn watch_loop(d: Shared) {
    let mut runner = WatchRunner::new(
        d.store.clone(),
        d.mission_control_root.clone(),
        Arc::new(DaemonSink::new(d)),
    );
    runner.run().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn watch() -> cw::Watch {
        cw::Watch {
            id: "w1".into(),
            owner: "a".into(),
            name: "inbox".into(),
            mode: "poll".into(),
            script_path: "/tmp/inbox.sh".into(),
            interval_secs: 300,
            filter: None,
            max_wakes_per_day: 48,
            quiet_start: None,
            quiet_end: None,
            enabled: true,
            status: "idle".into(),
            last_error: None,
            last_run_at: None,
            last_fired_at: None,
            last_output: None,
            pending: None,
            consecutive_failures: 0,
            created_at: cw::now_rfc3339(),
            updated_at: cw::now_rfc3339(),
        }
    }

    #[test]
    fn new_lines_diffs_multiset_and_keeps_order() {
        let prev = "a\nb\na";
        // `c` is new and the second `b` exceeds the previous count.
        assert_eq!(
            new_lines(Some(prev), "a\nb\na\nc\nb", None).unwrap(),
            vec!["c".to_string(), "b".to_string()]
        );
        // Repeated lines beyond the previous count are new.
        assert_eq!(
            new_lines(Some("x"), "x\nx", None).unwrap(),
            vec!["x".to_string()]
        );
        assert_eq!(
            new_lines(Some("a"), "a", None).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(new_lines(None, "a", None), None);
        assert_eq!(
            new_lines(Some("a"), "", None).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn new_lines_applies_filter() {
        let re = Regex::new("error").unwrap();
        assert_eq!(
            new_lines(Some("error: one"), "ok\nerror: two\nerror: one", Some(&re)).unwrap(),
            vec!["error: two".to_string()]
        );
        assert!(
            new_lines(Some("x"), "nothing", Some(&re))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn batcher_flushes_on_quiet_and_max_and_caps() {
        let mut b = StreamBatcher::default();
        let t0 = Instant::now();
        b.push(t0, "one".into());
        assert!(!b.due(t0 + Duration::from_secs(4)));
        assert!(b.due(t0 + Duration::from_secs(5)));
        let (lines, dropped) = b.flush_due(t0 + Duration::from_secs(5)).unwrap();
        assert_eq!(lines, vec!["one"]);
        assert_eq!(dropped, 0);

        // 30 s cap wins over a steady stream.
        let mut b = StreamBatcher::default();
        for i in 0..40 {
            b.push(t0 + Duration::from_millis(i * 700), format!("l{i}"));
        }
        assert!(b.due(t0 + STREAM_MAX_BATCH));
        let (lines, _) = b.flush().unwrap();
        assert_eq!(lines.len(), 40);

        // 200-line cap drops the oldest and counts them.
        let mut b = StreamBatcher::default();
        for i in 0..205 {
            b.push(t0, format!("l{i}"));
        }
        let (lines, dropped) = b.flush().unwrap();
        assert_eq!(lines.len(), 200);
        assert_eq!(dropped, 5);
        assert_eq!(lines[0], "l5");
    }

    #[test]
    fn gate_holds_in_quiet_hours_and_at_cap() {
        let w = watch();
        let offset = chrono::FixedOffset::east_opt(0).unwrap();
        // 2026-01-01 23:30 UTC.
        let night = chrono::DateTime::parse_from_rfc3339("2026-01-01T23:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let mut quiet = watch();
        quiet.quiet_start = Some("22:00".into());
        quiet.quiet_end = Some("08:00".into());
        assert!(!gate_open(&quiet, offset, night, 0));
        assert!(gate_open(&w, offset, night, 0));
        let mut capped = watch();
        capped.max_wakes_per_day = 2;
        assert!(!gate_open(&capped, offset, night, 2));
        assert!(gate_open(&capped, offset, night, 1));
    }

    #[test]
    fn pending_merges_and_caps() {
        let merged = merge_pending(Some("old1\nold2"), &["new1".to_string()]);
        assert_eq!(merged, "old1\nold2\nnew1");
        let big = vec!["x".repeat(9000)];
        let merged = merge_pending(None, &big);
        assert!(merged.len() <= cw::PENDING_CAP_BYTES + 1);
    }

    #[test]
    fn envelope_format() {
        let w = watch();
        let env = watch_envelope(&w, "change", &["hello".into(), "world".into()], 3);
        assert!(env.starts_with("[watch]\nname: inbox\nwatch_id: w1\nmode: poll\nevent: change\nlines: 2\nheld: 3\n---\n"));
        assert!(env.ends_with("[/watch]"));
        assert!(env.contains("hello\nworld"));
        // 4 KB body cap, cut at a line.
        let big: Vec<String> = (0..500)
            .map(|i| format!("line {i} {}", "x".repeat(20)))
            .collect();
        let env = watch_envelope(&w, "change", &big, 0);
        let body = env.split("\n---\n").nth(1).unwrap();
        assert!(body.len() <= 4096);
        let err = watch_envelope(&w, "error", &["boom".into()], 0);
        assert!(err.contains("event: error"));
        assert!(!err.contains("held:"));
    }

    #[derive(Default)]
    struct CollectSink {
        envelopes: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl WatchSink for CollectSink {
        async fn deliver(&self, owner: &str, envelope: String) -> Result<(), String> {
            self.envelopes
                .lock()
                .unwrap()
                .push((owner.to_string(), envelope));
            Ok(())
        }
    }

    fn agent(id: &str) -> crate::coordination::types::Agent {
        crate::coordination::types::Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: crate::coordination::types::AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    fn new_watch_row(owner: &str, name: &str, mode: &str) -> cw::NewWatch {
        cw::NewWatch {
            owner: owner.into(),
            name: name.into(),
            mode: mode.into(),
            script_path: String::new(),
            interval_secs: 1,
            filter: None,
            max_wakes_per_day: 48,
            quiet_start: None,
            quiet_end: None,
        }
    }

    #[tokio::test]
    async fn poll_script_change_produces_one_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&agent("a")).unwrap();
        let script = dir.path().join("watches").join("tick.sh");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(
            &script,
            "#!/bin/bash\nf=\"$SNIPPET_WATCH_STATE_DIR/count\"\nn=$(( $(cat \"$f\" 2>/dev/null || echo 0) + 1 ))\necho $n > \"$f\"\nif [ $n -ge 2 ]; then echo \"new-$n\"; fi\n",
        )
        .unwrap();
        let mut row = new_watch_row("a", "tick", "poll");
        row.script_path = script.to_string_lossy().into_owned();
        let watch = db.create_watch(&row).unwrap();
        let sink = Arc::new(CollectSink::default());
        let mut runner = WatchRunner::new(db.clone(), dir.path().join("mc"), sink.clone());
        // First run: baseline only.
        for _ in 0..100 {
            runner.step().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if db
                .watch_by_id(&watch.id)
                .unwrap()
                .unwrap()
                .last_output
                .is_some()
            {
                break;
            }
        }
        assert!(
            db.watch_by_id(&watch.id)
                .unwrap()
                .unwrap()
                .last_output
                .is_some()
        );
        assert!(sink.envelopes.lock().unwrap().is_empty());
        // Second run emits the new line once.
        for _ in 0..100 {
            runner.step().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !sink.envelopes.lock().unwrap().is_empty() {
                break;
            }
        }
        let got = sink.envelopes.lock().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "a");
        assert!(got[0].1.contains("new-2"), "{}", got[0].1);
        assert!(got[0].1.contains("[watch]"));
    }

    #[tokio::test]
    async fn failing_poll_reports_error_once_after_three_failures() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&agent("a")).unwrap();
        let script = dir.path().join("watches").join("bad.sh");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, "#!/bin/bash\necho boom >&2\nexit 3\n").unwrap();
        let mut row = new_watch_row("a", "bad", "poll");
        row.script_path = script.to_string_lossy().into_owned();
        let watch = db.create_watch(&row).unwrap();
        let sink = Arc::new(CollectSink::default());
        let mut runner = WatchRunner::new(db.clone(), dir.path().join("mc"), sink.clone());
        // interval_secs = 1, so three failures take a few seconds.
        for _ in 0..200 {
            runner.step().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let w = db.watch_by_id(&watch.id).unwrap().unwrap();
            if w.consecutive_failures >= 3 {
                break;
            }
        }
        let w = db.watch_by_id(&watch.id).unwrap().unwrap();
        assert_eq!(w.consecutive_failures, 3);
        assert_eq!(w.status, "error");
        // Keep stepping: no second error wake.
        for _ in 0..60 {
            runner.step().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let got = sink.envelopes.lock().unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1.contains("event: error"));
        assert!(got[0].1.contains("boom"));
    }

    #[tokio::test]
    async fn stream_script_two_lines_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open_in_memory().unwrap();
        db.create_agent(&agent("a")).unwrap();
        let script = dir.path().join("watches").join("feed.sh");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(
            &script,
            "#!/bin/bash\necho first\nsleep 0.2\necho second\nsleep 30\n",
        )
        .unwrap();
        let mut row = new_watch_row("a", "feed", "stream");
        row.script_path = script.to_string_lossy().into_owned();
        db.create_watch(&row).unwrap();
        let sink = Arc::new(CollectSink::default());
        let mut runner = WatchRunner::new(db.clone(), dir.path().join("mc"), sink.clone());
        for _ in 0..100 {
            runner.step().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !sink.envelopes.lock().unwrap().is_empty() {
                break;
            }
        }
        let got = sink.envelopes.lock().unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].1.contains("first"));
        assert!(got[0].1.contains("second"));
        drop(runner);
    }
}
