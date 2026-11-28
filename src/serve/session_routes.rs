use super::api_error;
use super::*;

pub(crate) fn session_event_page(
    events: &[crate::harness::HarnessEvent],
    before: Option<usize>,
    limit: Option<usize>,
) -> (usize, usize, bool) {
    let end = before.unwrap_or(events.len()).min(events.len());
    let start = match limit {
        Some(size) => end.saturating_sub(size.clamp(1, 500)),
        None => turn_page_start(&events[..end]),
    };
    (start, end, start > 0)
}

fn turn_page_start(events: &[crate::harness::HarnessEvent]) -> usize {
    const MAX_EVENTS: usize = 160;
    // Wakes count as turns, so a few quiet wakes must not empty the opening view.
    const MIN_EVENTS: usize = 120;
    const TURNS: usize = 8;
    let len = events.len();
    let floor = len.saturating_sub(MAX_EVENTS);
    let mut seen = 0;
    let mut start = floor;
    for i in (floor..len).rev() {
        if matches!(events[i], crate::harness::HarnessEvent::UserInput { .. }) {
            seen += 1;
            if seen == TURNS {
                start = i;
                break;
            }
        }
    }
    start.min(len.saturating_sub(MIN_EVENTS))
}

pub(crate) fn clip_json(
    value: &mut serde_json::Value,
    string_limit: usize,
    array_limit: usize,
) -> bool {
    match value {
        serde_json::Value::String(text) if text.len() > string_limit => {
            let mut cut = string_limit;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            true
        }
        serde_json::Value::Array(items) => {
            let mut clipped = items.len() > array_limit;
            items.truncate(array_limit);
            for item in items.iter_mut() {
                clipped |= clip_json(item, string_limit, array_limit);
            }
            clipped
        }
        serde_json::Value::Object(map) => {
            let mut clipped = false;
            for item in map.values_mut() {
                clipped |= clip_json(item, string_limit, array_limit);
            }
            clipped
        }
        _ => false,
    }
}

pub(crate) fn wire_events(events: &[crate::harness::HarnessEvent]) -> serde_json::Value {
    wire_events_slice(events, 0)
}

pub(crate) fn wire_events_slice(
    all: &[crate::harness::HarnessEvent],
    start: usize,
) -> serde_json::Value {
    let mut pending: std::collections::VecDeque<(String, String)> =
        std::collections::VecDeque::new();
    let mut out = Vec::with_capacity(all.len().saturating_sub(start));
    for (idx, event) in all.iter().enumerate() {
        let mut rewritten: Option<(String, Option<serde_json::Value>, String)> = None;
        match event {
            crate::harness::HarnessEvent::ToolCall {
                tool_name,
                arguments,
            } => {
                if let Ok(Some((internal, lowered))) = crate::surface::lower(tool_name, arguments)
                    && internal != *tool_name
                {
                    pending.push_back((tool_name.clone(), internal.clone()));
                    rewritten = Some((internal, Some(lowered), tool_name.clone()));
                }
            }
            crate::harness::HarnessEvent::ToolResult { tool_name, .. }
            | crate::harness::HarnessEvent::InvalidToolCall { tool_name, .. } => {
                if let Some(pos) = pending.iter().position(|(s, _)| s == tool_name)
                    && let Some((surface, internal)) = pending.remove(pos)
                {
                    rewritten = Some((internal, None, surface));
                }
            }
            _ => {}
        }
        if idx < start {
            continue;
        }
        let mut v = serde_json::to_value(event).unwrap_or_default();
        if let (Some((internal, lowered_args, surface)), Some(o)) = (rewritten, v.as_object_mut()) {
            o.insert("tool_name".into(), serde_json::Value::String(internal));
            if let Some(args) = lowered_args {
                o.insert("arguments".into(), args);
            }
            o.insert("surface_tool".into(), serde_json::Value::String(surface));
        }
        out.push(v);
    }
    serde_json::Value::Array(out)
}

const TASK_SUMMARY_TEXT_LIMIT: usize = 280;

pub(crate) fn task_summary(mut task: serde_json::Value) -> serde_json::Value {
    let Some(o) = task.as_object_mut() else {
        return task;
    };
    let mut clipped = false;
    for key in ["description", "plan", "result", "handoff", "ledger"] {
        if let Some(field) = o.get_mut(key) {
            clipped |= clip_json(field, TASK_SUMMARY_TEXT_LIMIT, 8);
        }
    }
    if let Some(serde_json::Value::Array(markers)) = o.get_mut("notifications") {
        let total = markers.len();
        markers.retain(|m| m.get("delivered").and_then(|d| d.as_bool()) != Some(true));
        for marker in markers.iter_mut() {
            clip_json(marker, TASK_SUMMARY_TEXT_LIMIT, 8);
        }
        clipped |= markers.len() != total;
        o.insert("notification_count".into(), serde_json::json!(total));
    }
    o.insert("summary".into(), serde_json::json!(clipped));
    task
}

#[derive(Deserialize)]
pub(crate) struct RewindReq {
    session: String,
    checkpoint: String,
}

// POST /session/rewind {session, checkpoint} — restore workspace files AND
// truncate conversation history to that checkpoint. Always updates the state
// file so clients (mobile/TUI) see the truncated transcript immediately; also
// notifies a live loop when present so in-memory state matches.
pub(crate) async fn rewind_session(
    State(d): State<Shared>,
    Json(req): Json<RewindReq>,
) -> Response {
    let (sp, workspace) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let checkpoint_id = req.checkpoint.clone();

    // Load current state (store or file — authoritative for history indices).
    let Some(mut state) = read_session_state(&sp) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read session state",
        );
    };

    let label = match state.apply_checkpoint_rewind(&checkpoint_id) {
        Ok(label) => label,
        Err(_) => return api_error(StatusCode::NOT_FOUND, "checkpoint not found"),
    };

    // Persist truncated history first so any client re-read sees the cut.
    // `write_session_state` targets whichever store holds the session, so a
    // rewind does not silently write a file a DB session will never read.
    if let Err(error) = crate::session::write_session_state(&sp, &state) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write state: {error}"),
        );
    }
    // Keep a live loop in sync (it may overwrite disk on its next persist otherwise).
    {
        let sessions = d.sessions.lock().await;
        if let Some(s) = sessions.get(&req.session)
            && !s.join.is_finished()
        {
            let _ = s.input_tx.send(LoopInput::Rewind {
                checkpoint: checkpoint_id.clone(),
            });
        }
    }

    // Restore workspace files to the shadow commit.
    let checkpoint_for_restore = checkpoint_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        crate::checkpoint::restore(&workspace, &checkpoint_for_restore)
    })
    .await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({
            "restored": checkpoint_id,
            "label": label,
            "event_end": state.events.len(),
            "message_end": state.messages.len(),
        }))
        .into_response(),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub(crate) struct ExecReq {
    session: String,
    command: String,
}

// POST /session/exec {session, command} — run a shell command in the session's
// workspace and return its output. Token-gated; runs as the daemon user.
pub(crate) async fn exec_in_session(Json(req): Json<ExecReq>) -> Response {
    let (_sp, dir) = match load_session_workspace(&req.session) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if req.command.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "empty command");
    }
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&req.command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        // On timeout the output future is dropped — kill the child then, or it
        // keeps running detached forever with no handle to find or stop it.
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(_) => {
            return Json(serde_json::json!({
                "exit_code": -1, "stdout": "", "stderr": "timed out after 60s", "truncated": false,
            }))
            .into_response();
        }
    };
    let (stdout, t1) = clip_output(&out.stdout, 20_000);
    let (stderr, t2) = clip_output(&out.stderr, 20_000);
    Json(serde_json::json!({
        "exit_code": out.status.code().unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "truncated": t1 || t2,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct BgReq {
    session: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
}

// GET /bg?session — snapshot of the session's background processes.
pub(crate) async fn bg_list(Query(req): Query<BgReq>) -> Response {
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    Json(serde_json::json!({ "processes": crate::bg::list(&dir) })).into_response()
}

// POST /bg/kill {session, id} — terminate one background process.
pub(crate) async fn bg_kill(Json(req): Json<BgReq>) -> Response {
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.id.as_deref() else {
        return api_error(StatusCode::BAD_REQUEST, "id required");
    };
    match crate::bg::kill_by_id(&dir, id) {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => Json(serde_json::json!({"ok": false, "error": e.to_string()})).into_response(),
    }
}

// GET /bg/log?session&id&tail? — tail a background process's log.
pub(crate) async fn bg_log(Query(req): Query<BgReq>) -> Response {
    let dir = match resolve_session_dir(&req.session) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(id) = req.id.as_deref() else {
        return api_error(StatusCode::BAD_REQUEST, "id required");
    };
    if !crate::bg::valid_id(id) {
        return api_error(StatusCode::BAD_REQUEST, "invalid id");
    }
    const MAX_LOG_WINDOW: u64 = 256 * 1024;
    let mut sought = false;
    let text = match std::fs::File::open(crate::bg::log_path(&dir, id)) {
        Ok(mut f) => {
            use std::io::{Read, Seek, SeekFrom};
            if let Ok(meta) = f.metadata()
                && meta.len() > MAX_LOG_WINDOW
                && f.seek(SeekFrom::End(-(MAX_LOG_WINDOW as i64))).is_ok()
            {
                sought = true;
            }
            let mut buf = Vec::new();
            let _ = f.take(MAX_LOG_WINDOW + 1024).read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        }
        Err(_) => String::new(),
    };
    let mut lines: Vec<&str> = text.lines().collect();
    if sought && !lines.is_empty() {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(req.tail.unwrap_or(400));
    Json(serde_json::json!({ "log": lines[start..].join("\n"), "truncated": sought || start > 0 }))
        .into_response()
}

/// Change-detector for a session, from whichever store holds it: the store's
/// row timestamp plus its log lengths, else the state file's bytes.
///
/// The attach stream re-checks on a timer, so this must stay cheap and must NOT
/// load the transcript — not loading it is the whole point of having moved it
/// into the database.
pub(crate) fn session_state_fingerprint(
    store: &crate::store::Store,
    state_path: &Path,
) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let id = session_id_for_state_path(state_path);
    if let Ok(Some(fingerprint)) = store.conversation_fingerprint(&id) {
        fingerprint.hash(&mut hasher);
        return Some(hasher.finish());
    }
    let bytes = std::fs::read(state_path).ok()?;
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

pub(crate) fn resolve_session_dir(session: &str) -> Result<PathBuf, Response> {
    if let Some(sp) = state_path_for_id(session)
        && let Some(state) = read_session_meta(&sp)
    {
        let dir = PathBuf::from(&state.workspace);
        if !state.workspace.is_empty() && dir.is_dir() {
            return Ok(dir);
        }
    }
    // Not a session id → treat it as a folder path (no-session git).
    let dir = PathBuf::from(session);
    if dir.is_dir() {
        return Ok(dir);
    }
    Err(api_error(
        StatusCode::NOT_FOUND,
        "no such session or directory",
    ))
}

/// Lossy-decode bytes and clip to `max` chars, returning (text, was_truncated).
pub(crate) fn clip_output(b: &[u8], max: usize) -> (String, bool) {
    let s = String::from_utf8_lossy(b);
    if s.chars().count() > max {
        (s.chars().take(max).collect::<String>() + "\u{2026}", true)
    } else {
        (s.into_owned(), false)
    }
}

#[derive(Deserialize)]
pub(crate) struct DeleteReq {
    session: String,
}

// POST /session/delete {session} — stop the live loop (if any) and delete the
// session's conversation file.
pub(crate) async fn delete_session(
    State(d): State<Shared>,
    Json(req): Json<DeleteReq>,
) -> Response {
    if !purge_session(&d, &req.session, false).await {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    }
    Json(serde_json::json!({"deleted": true})).into_response()
}

/// Stop a session's loop and remove its record, transcript and artifacts. Memory is removed
/// too unless `keep_memory`. False when the session does not exist.
pub(crate) async fn purge_session(d: &Shared, id: &str, keep_memory: bool) -> bool {
    let Some(sp) = state_path_for_id(id) else {
        return false;
    };
    if let Some(s) = d.sessions.lock().await.remove(id) {
        s.join.abort();
    }
    crate::session::remove_session_files(&sp);
    if !keep_memory {
        crate::memory::delete_session_memory(id);
    }
    crate::artifacts::delete_session(id);
    if let Ok(members) = d.store.session_members(id) {
        for m in members {
            if let Some(ref child_id) = m.work_session_id {
                if child_id != id {
                    if let Some(s) = d.sessions.lock().await.remove(child_id) {
                        s.join.abort();
                    }
                    if let Some(csp) = state_path_for_id(child_id) {
                        crate::session::remove_session_files(&csp);
                    }
                    if !keep_memory {
                        crate::memory::delete_session_memory(child_id);
                    }
                    crate::artifacts::delete_session(child_id);
                }
            }
        }
    }
    true
}

#[derive(Deserialize)]
pub(crate) struct InterruptReq {
    session: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    actor_kind: Option<String>,
    #[serde(default)]
    actor_id: Option<String>,
}

fn stop_decision(
    actor_kind: Option<&str>,
    actor_id: Option<&str>,
    reason: &str,
) -> (String, String) {
    let kind = actor_kind.map(str::trim).filter(|kind| !kind.is_empty());
    let id = actor_id.map(str::trim).filter(|id| !id.is_empty());
    match kind {
        Some("desk") => {
            let who = id
                .map(|id| format!("desk {id}"))
                .unwrap_or_else(|| "desk".to_string());
            (
                "stopped_by_desk".to_string(),
                format!("Stopped by {who}: {reason}"),
            )
        }
        Some("lead") => (
            "stopped_by_lead".to_string(),
            format!("Stopped by Lead: {reason}"),
        ),
        Some("human") | Some("user") => (
            "stopped_by_user".to_string(),
            format!("Stopped by user: {reason}"),
        ),
        Some("system") => (
            "stopped_by_system".to_string(),
            format!("Stopped by system: {reason}"),
        ),
        _ => ("stopped".to_string(), format!("Stopped: {reason}")),
    }
}

pub(crate) async fn interrupt_session(
    State(d): State<Shared>,
    Json(req): Json<InterruptReq>,
) -> Response {
    let Some(sp) = state_path_for_id(&req.session) else {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    };
    let live = d
        .sessions
        .lock()
        .await
        .get(&req.session)
        .is_some_and(|s| !s.join.is_finished());
    let status = crate::session::read_session_meta(&sp).map(|s| s.status);
    let working = matches!(
        status,
        Some(
            crate::harness::HarnessStatus::Running | crate::harness::HarnessStatus::WaitingForInput
        )
    );
    if !live
        && working
        && let Some(mut state) = crate::session::read_session_state(&sp)
    {
        state.status = crate::harness::HarnessStatus::Idle;
        if crate::session::write_session_state(&sp, &state).is_ok() {
            return Json(serde_json::json!({"stopped": true, "was": status, "note": "cleared a run left over from a restart"}))
                    .into_response();
        }
    }
    if !live || !working {
        return Json(serde_json::json!({"stopped": false, "status": status, "note": "the session isn't running"}))
            .into_response();
    }
    d.deliver(&req.session, LoopInput::Interrupt).await;
    if let Some(reason) = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        let (step, reasoning) = stop_decision(
            req.actor_kind.as_deref(),
            req.actor_id.as_deref(),
            reason,
        );
        d.deliver(
            &req.session,
            LoopInput::Notice(crate::harness::HarnessEvent::SystemDecision {
                step,
                reasoning,
            }),
        )
        .await;
    }
    Json(serde_json::json!({"stopped": true, "was": status})).into_response()
}

#[derive(Deserialize)]
pub(crate) struct RenameReq {
    session: String,
    title: String,
}

// POST /session/rename {session, title} — set the session's title override. A live
// session goes through its loop so the in-memory state stays in sync; otherwise the
// state file is edited directly (without reviving the loop).
pub(crate) async fn rename_session(
    State(d): State<Shared>,
    Json(req): Json<RenameReq>,
) -> Response {
    let Some(sp) = state_path_for_id(&req.session) else {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    };
    {
        let sessions = d.sessions.lock().await;
        if let Some(s) = sessions.get(&req.session)
            && !s.join.is_finished()
        {
            let _ = s.input_tx.send(LoopInput::SetTitle(req.title.clone()));
            return Json(serde_json::json!({"renamed": true})).into_response();
        }
    }
    match crate::session::set_session_title(&sp, &req.title) {
        Ok(()) => Json(serde_json::json!({"renamed": true})).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
pub(crate) struct ForkReq {
    session: String,
    /// Checkpoint id (or unique prefix) — same cut as `/session/rewind`.
    #[serde(default)]
    checkpoint: Option<String>,
    /// Inclusive event index to keep through (snapped to a provider-safe boundary).
    /// When omitted alongside `checkpoint`, forks the full conversation.
    #[serde(default)]
    event_index: Option<usize>,
    /// Optional custom title for the forked conversation.
    #[serde(default)]
    title: Option<String>,
}

// POST /session/fork {session, checkpoint?|event_index?} — branch a NEW conversation
// at the chosen history point. Source session is left untouched. Workspace files are
// shared (not snapshotted); only conversation history is forked.
pub(crate) async fn fork_session(State(d): State<Shared>, Json(req): Json<ForkReq>) -> Response {
    let Some(sp) = state_path_for_id(&req.session) else {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    };

    // Prefer the live session's own path when it is running, so the fork sees a
    // turn that has not flushed yet. The reader resolves whichever store holds
    // the session, so a database-backed session forks exactly like a file one.
    // This used to be a nested if/else cascade that repeated the same read four
    // times, which is how the file-only assumption got baked in four times over.
    let state = {
        let sessions = d.sessions.lock().await;
        let path = sessions
            .get(&req.session)
            .filter(|live| !live.join.is_finished())
            .map(|live| live.state_path.clone())
            .unwrap_or_else(|| sp.clone());
        drop(sessions);
        match read_session_state(&path) {
            Some(state) => state,
            None => {
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "session state unreadable",
                );
            }
        }
    };

    let point = match crate::session::resolve_fork_point(
        &state,
        req.checkpoint.as_deref(),
        req.event_index,
    ) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e),
    };

    match crate::session::write_forked_conversation(&sp, &state, point) {
        Ok(mut forked) => {
            if let Some(custom) = req.title.as_deref().map(str::trim).filter(|s| !s.is_empty())
                && crate::session::set_session_title(&forked.state_path, custom).is_ok()
            {
                forked.title = custom.to_string();
            }
            crate::harness::fork_cli_conversations(
                &crate::session::session_id_for_state_path(&sp),
                &crate::session::session_id_for_state_path(&forked.state_path),
                state.messages.len(),
                forked.message_end,
            );
            Json(serde_json::json!({
            "id": forked.id,
            "title": forked.title,
            "event_end": forked.event_end,
            "message_end": forked.message_end,
            }))
            .into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
pub(crate) struct AttachQuery {
    pub(crate) session: String,
    #[serde(default)]
    pub(crate) stream_delta: Option<String>,
    #[serde(default)]
    pub(crate) enc: Option<String>,
    #[serde(default)]
    pub(crate) from: Option<usize>,
    #[serde(default)]
    pub(crate) at: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct CheckpointStatsReq {
    session: String,
    ids: Vec<String>,
}

pub(crate) async fn checkpoint_stats(Json(req): Json<CheckpointStatsReq>) -> Response {
    let dir = match resolve_session_dir(&req.session) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let ids = req.ids;
    let stats = tokio::task::spawn_blocking(move || {
        let mut out = serde_json::Map::new();
        for (i, id) in ids.iter().enumerate() {
            let next = ids.get(i + 1).map(String::as_str);
            if let Some((files, add, del)) = crate::checkpoint::shortstat(&dir, id, next) {
                out.insert(
                    id.clone(),
                    serde_json::json!({"files": files, "add": add, "del": del}),
                );
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    Json(serde_json::Value::Object(stats)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct LaneQuery {
    session: String,
    id: String,
}

/// GET /session/lane — one lane in full; attach snapshots carry finished lanes trimmed.
pub(crate) async fn lane_detail(Query(q): Query<LaneQuery>) -> Response {
    let session = q.session.clone();
    let state = tokio::task::spawn_blocking(move || {
        crate::session::state_path_for_id(&session)
            .and_then(|p| crate::session::read_session_state(&p))
    })
    .await
    .ok()
    .flatten();
    let Some(state) = state else {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    };
    match state.lanes.into_iter().find(|l| l.id == q.id) {
        Some(lane) => {
            let mut v = serde_json::to_value(&lane).unwrap_or_default();
            crate::lanes::attach_recent(&mut v, &lane);
            Json(v).into_response()
        }
        None => api_error(StatusCode::NOT_FOUND, "no such lane"),
    }
}

#[derive(Deserialize)]
pub(crate) struct LaneControlReq {
    session: String,
    #[serde(alias = "id")]
    lane: String,
}

const LANE_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

fn lane_control_error(msg: String) -> Response {
    let status = if msg.starts_with("no delegated task") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    api_error(status, msg)
}

async fn run_live_lane_control(
    tx: UnboundedSender<LoopInput>,
    request_id: String,
    input: LoopInput,
) -> Result<String, Response> {
    let rx = crate::lanes::register_control(&request_id);
    if tx.send(input).is_err() {
        crate::lanes::complete_control(&request_id, Err("session closed".into()));
        return Err(api_error(StatusCode::GONE, "session closed"));
    }
    match tokio::time::timeout(LANE_CONTROL_TIMEOUT, rx).await {
        Ok(Ok(Ok(val))) => Ok(val),
        Ok(Ok(Err(msg))) => Err(lane_control_error(msg)),
        Ok(Err(_)) | Err(_) => {
            crate::lanes::complete_control(&request_id, Err("timed out".into()));
            Err(api_error(
                StatusCode::GATEWAY_TIMEOUT,
                "session loop did not answer",
            ))
        }
    }
}

/// POST /session/lane/cancel {session, lane} — stop a running or paused lane.
pub(crate) async fn cancel_lane(
    State(d): State<Shared>,
    Json(req): Json<LaneControlReq>,
) -> Response {
    let Some(sp) = state_path_for_id(&req.session) else {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    };
    let live_tx = {
        let sessions = d.sessions.lock().await;
        sessions
            .get(&req.session)
            .filter(|s| !s.join.is_finished())
            .map(|s| s.input_tx.clone())
    };
    let reason = "stopped by the user";
    if let Some(tx) = live_tx {
        let request_id = format!("lc-{}", uuid::Uuid::new_v4().simple());
        return match run_live_lane_control(
            tx,
            request_id.clone(),
            LoopInput::CancelLane {
                request_id,
                lane_id: req.lane.clone(),
                reason: reason.to_string(),
            },
        )
        .await
        {
            Ok(title) => Json(serde_json::json!({
                "cancelled": true,
                "id": req.lane,
                "title": title,
            }))
            .into_response(),
            Err(resp) => resp,
        };
    }

    let lane_id = req.lane.clone();
    let offline = tokio::task::spawn_blocking(move || {
        let Some(mut state) = crate::session::read_session_state(&sp) else {
            return Err(api_error(StatusCode::NOT_FOUND, "no such session"));
        };
        let Some(record) = state.lanes.iter_mut().find(|l| l.id == lane_id) else {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                format!("no delegated task `{lane_id}` is known in this conversation."),
            ));
        };
        let title = crate::lanes::cancel_record(record, reason).map_err(lane_control_error)?;
        state
            .events
            .push(crate::harness::HarnessEvent::LaneCancelled {
                id: lane_id.clone(),
                title: title.clone(),
                reason: reason.to_string(),
            });
        crate::session::write_session_state(&sp, &state)
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(title)
    })
    .await;
    match offline {
        Ok(Ok(title)) => Json(serde_json::json!({
            "cancelled": true,
            "id": req.lane,
            "title": title,
        }))
        .into_response(),
        Ok(Err(resp)) => resp,
        Err(_) => api_error(StatusCode::INTERNAL_SERVER_ERROR, "cancel task failed"),
    }
}

/// POST /session/lane/retry {session, lane} — re-run a failed or cancelled lane.
pub(crate) async fn retry_lane(
    State(d): State<Shared>,
    Json(req): Json<LaneControlReq>,
) -> Response {
    if state_path_for_id(&req.session).is_none() {
        return api_error(StatusCode::NOT_FOUND, "no such session");
    }
    let Some((tx, _, _)) = d.ensure_live(&req.session).await else {
        return api_error(StatusCode::BAD_REQUEST, "session cannot be resumed");
    };
    let request_id = format!("lr-{}", uuid::Uuid::new_v4().simple());
    match run_live_lane_control(
        tx,
        request_id.clone(),
        LoopInput::RetryLane {
            request_id,
            lane_id: req.lane.clone(),
        },
    )
    .await
    {
        Ok(new_id) => Json(serde_json::json!({
            "id": new_id,
            "retry_of": req.lane,
        }))
        .into_response(),
        Err(resp) => resp,
    }
}

#[derive(Deserialize)]
pub(crate) struct SeenReq {
    session: String,
}

/// POST /session/seen {session} — record that the user viewed this session.
pub(crate) async fn seen_session(State(d): State<Shared>, Json(req): Json<SeenReq>) -> Response {
    let id = state_path_for_id(&req.session)
        .map(|p| session_id_for_state_path(&p))
        .unwrap_or_else(|| crate::conversations::canonical_session_id(&req.session));
    let now = crate::session::now_unix_secs();
    match d.store.set_session_user_seen(&id, now) {
        Ok(true) => Json(serde_json::json!({ "ok": true, "session": id, "user_seen_at": now }))
            .into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no such session"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod wire_surface_tests {
    use super::*;
    use crate::harness::HarnessEvent;
    use serde_json::json;

    #[test]
    fn wire_events_rewrites_surface_calls_and_results() {
        let events = vec![
            HarnessEvent::ToolCall {
                tool_name: "web".into(),
                arguments: json!({"action": "search", "query": "rust"}),
            },
            HarnessEvent::ToolResult {
                tool_name: "web".into(),
                result: json!({"status": "success", "data": {"count": 2}}),
            },
            HarnessEvent::ToolCall {
                tool_name: "bash".into(),
                arguments: json!({"command": "ls"}),
            },
            HarnessEvent::ToolResult {
                tool_name: "bash".into(),
                result: json!({"status": "success", "data": {"exit_code": 0}}),
            },
        ];

        {
            let wired = wire_events(&events);
            let arr = wired.as_array().unwrap();
            assert_eq!(arr[0]["tool_name"], "web_search");
            assert_eq!(arr[0]["arguments"], json!({"query": "rust"}));
            assert_eq!(arr[0]["surface_tool"], "web");
            assert_eq!(arr[1]["tool_name"], "web_search");
            assert_eq!(arr[1]["surface_tool"], "web");
            assert_eq!(arr[2]["tool_name"], "bash");
            assert!(arr[2].get("surface_tool").is_none());
            assert_eq!(arr[3]["tool_name"], "bash");
            assert!(arr[3].get("surface_tool").is_none());
        }

        // Delta slice containing only ToolResult still resolves its preceding ToolCall.
        let delta = wire_events_slice(&events, 1);
        let arr = delta.as_array().unwrap();
        assert_eq!(arr[0]["tool_name"], "web_search");
        assert_eq!(arr[0]["surface_tool"], "web");
    }

    #[test]
    fn stop_decision_uses_the_actor_that_requested_the_stop() {
        assert_eq!(
            stop_decision(Some("desk"), Some("snippet"), "done"),
            (
                "stopped_by_desk".to_string(),
                "Stopped by desk snippet: done".to_string(),
            )
        );
        assert_eq!(
            stop_decision(Some("lead"), None, "done"),
            (
                "stopped_by_lead".to_string(),
                "Stopped by Lead: done".to_string(),
            )
        );
        assert_eq!(
            stop_decision(None, None, "done"),
            ("stopped".to_string(), "Stopped: done".to_string())
        );
    }
}
