use super::api_error;
use super::*;

// A shell belongs to the machine: switching or closing a session must not kill
// it. `/attach` cannot serve this because it calls `ensure_live`, so a socket
// with no live session gets a 404. This route carries the same `wire: term`
// frames over a socket that requires no session.
//
// Lifecycle is still explicit: the client creates and closes shells through the
// same `open`/`new`/`close` ops, so a dropped socket never destroys a pty.
pub(crate) async fn shells_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
) -> Response {
    let terms = d.shells.clone();
    ws.on_upgrade(move |socket| handle_shells_ws(socket, terms))
}

async fn handle_shells_ws(socket: WebSocket, terms: Arc<crate::term::SessionTerms>) {
    use base64::Engine;
    let (mut sender, mut receiver) = socket.split();
    let term_client = terms.subscribe();
    let push_terms = terms.clone();
    let mut term_seq: u64 = 0;

    // Reconnect support: report what already exists, so a client that
    // reattaches rebuilds its strip instead of showing nothing while the ptys
    // keep running.
    let existing: Vec<serde_json::Value> = terms
        .list()
        .into_iter()
        .map(|(id, alive)| serde_json::json!({ "id": id, "alive": alive }))
        .collect();
    let hello = serde_json::json!({ "wire": "term", "op": "list", "shells": existing });
    if let Ok(json) = serde_json::to_string(&hello)
        && sender.send(Message::Text(json.into())).await.is_err() {
            return;
        }

    let push = tokio::spawn(async move {
        let mut term_changes = push_terms.changes();
        'push: loop {
            term_changes.borrow_and_update();
            for (id, chunk, cols, rows, alive) in push_terms.poll_client(&term_client) {
                if chunk.is_empty() && alive {
                    continue;
                }
                term_seq = term_seq.wrapping_add(1);
                let frame = serde_json::json!({
                    "wire": "term",
                    "op": "out",
                    "id": id,
                    "seq": term_seq,
                    "data": base64::engine::general_purpose::STANDARD.encode(&chunk),
                    "cols": cols,
                    "rows": rows,
                    "alive": alive,
                });
                if let Ok(json) = serde_json::to_string(&frame)
                    && sender.send(Message::Text(json.into())).await.is_err() {
                        break 'push;
                    }
            }
            let _ = tokio::time::timeout(PUSH_FALLBACK_TICK, term_changes.changed()).await;
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(t.as_str())
                    && val.get("wire").and_then(|w| w.as_str()) == Some("term") {
                        apply_term_client(&terms, &val);
                    }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

// WS /attach?session= — stream this session's HarnessState + receive LoopInput.
pub(crate) async fn attach_ws(
    ws: WebSocketUpgrade,
    State(d): State<Shared>,
    Query(q): Query<AttachQuery>,
) -> Response {
    let on = |v: &Option<String>| v.as_deref().is_some_and(|v| v != "0" && v != "false");
    let compact = on(&q.compact);
    let stream_delta = on(&q.stream_delta);
    if crate::session::is_lane_session_id(&q.session) {
        let Some(state_path) = state_path_for_id(&q.session)
            .filter(|path| crate::session::read_session_meta(path).is_some())
        else {
            return api_error(StatusCode::NOT_FOUND, "no such lane");
        };
        let stream: crate::llm::StreamHandle =
            std::sync::Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default()));
        let daemon = d.clone();
        let session = q.session.clone();
        return ws.on_upgrade(move |socket| {
            handle_ws(socket, daemon, session, state_path, stream, None, AttachOptions { compact, stream_delta, read_only: true })
        });
    }
    match d.ensure_live(&q.session).await {
        Some((_, state_path, stream)) => {
            let terms = {
                let sessions = d.sessions.lock().await;
                sessions.get(&q.session).map(|s| s.terms.clone())
            };
            let daemon = d.clone();
            let session = q.session.clone();
            ws.on_upgrade(move |socket| {
                handle_ws(socket, daemon, session, state_path, stream, terms, AttachOptions { compact, stream_delta, read_only: false })
            })
        }
        None => api_error(StatusCode::NOT_FOUND, "no such session"),
    }
}

#[derive(Clone, Copy)]
struct AttachOptions {
    compact: bool,
    stream_delta: bool,
    read_only: bool,
}

async fn handle_ws(
    socket: WebSocket,
    daemon: Shared,
    session: String,
    state_path: PathBuf,
    stream: crate::llm::StreamHandle,
    terms: Option<std::sync::Arc<crate::term::SessionTerms>>,
    options: AttachOptions,
) {
    let AttachOptions { compact, stream_delta, read_only } = options;
    let (mut sender, mut receiver) = socket.split();
    let (history_tx, history_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, Option<usize>)>();
    let history_request_tx = history_tx.clone();
    let (event_request_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let (ping_tx, mut ping_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let push_daemon = daemon.clone();
    let push_session = session.clone();
    let push_state_path = state_path.clone();
    let push_stream = stream.clone();
    let push_terms = terms.clone();
    let push = tokio::spawn(async move {
        let daemon = push_daemon;
        let session = push_session;
        let state_path = push_state_path;
        let stream = push_stream;
        let terms = push_terms;
        let mut history_rx = history_rx;
        let term_client = terms.as_ref().map(|t| t.subscribe());
        let mut signals = AttachSignals {
            session: crate::conversations::session_changes(),
            stream: crate::llm::StreamBuffer::changes(&stream),
            term: terms.as_ref().map(|t| t.changes()),
            session_woke: false,
        };
        let mut check_state_next = true;
        let mut pending_event: Option<usize> = None;
        let mut pending_history: Option<(usize, Option<usize>)> = None;
        let mut pinged = false;
        let mut last_state_fingerprint = None;
        let mut last_events: Vec<crate::harness::HarnessEvent> = Vec::new();
        let mut last_event_offset = 0usize;
        let mut last_lanes_json: Option<String> = None;
        let mut last_stream_fp: u64 = 0;
        let mut sent_text = String::new();
        let mut sent_thinking = String::new();
        let mut sent_once = false;
        let mut last_queue_revision = 0;
        let mut attach_revision: u64 = 0;
        let mut term_seq: u64 = 0;
        let mut last_frame_sent = tokio::time::Instant::now();
        let load_initial = |path: &Path| {
            let path = path.to_path_buf();
            async move {
                tokio::task::spawn_blocking(move || load_initial_state(&path))
                    .await
                    .ok()
                    .flatten()
                    .map(|(s, off)| (s, false, off))
            }
        };
        'attach: loop {
            let check_state = std::mem::take(&mut check_state_next) | signals.mark_seen();
            let queue_revision = daemon.queue_revision.load(Ordering::Acquire);
            let fingerprint = if check_state {
                let store = daemon.store.clone();
                let path = state_path.clone();
                tokio::task::spawn_blocking(move || session_state_fingerprint(&store, &path))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            if let Some(fingerprint) = fingerprint
                && (Some(fingerprint) != last_state_fingerprint
                    || queue_revision != last_queue_revision)
                {
                    let resume = !last_events.is_empty();
                    let loaded = if resume {
                        let path = state_path.clone();
                        let from = last_event_offset;
                        let tail = tokio::task::spawn_blocking(move || read_session_state_tail(&path, from))
                            .await
                            .ok()
                            .flatten();
                        match tail {
                            Some((state, _))
                                if state.events.len() >= last_events.len()
                                    && state.events[..last_events.len()] == last_events[..] =>
                            {
                                Some((state, true, last_event_offset))
                            }
                            Some(_) => load_initial(&state_path).await,
                            None => None,
                        }
                    } else {
                        load_initial(&state_path).await
                    };
                    if let Some((mut state, continuous, base_offset)) = loaded {
                        let hidden = {
                            let mut overlays = daemon.queue_hidden.lock().unwrap();
                            let entries = overlays.entry(session.clone()).or_default();
                            entries
                                .retain(|id| state.queued_inputs.iter().any(|item| &item.id == id));
                            entries.clone()
                        };
                        if !hidden.is_empty() {
                            state
                                .queued_inputs
                                .retain(|item| !hidden.contains(&item.id));
                        }
                        let events = std::mem::take(&mut state.events);
                        if let Ok(mut v) = serde_json::to_value(&state) {
                            if let Some(o) = v.as_object_mut() {
                                o.remove("messages");
                                // Rate limits are PROVIDER-scoped. Only ChatGPT sessions get
                                // the account-wide overlay; every other provider gets NO
                                // rate_limit — including scrubbing a stale snapshot persisted
                                // before a model switch (it showed ChatGPT's monthly limits
                                // on an anthropic-compatible chat).
                                if daemon.session_provider(&session).await == "chatgpt" {
                                    if let Some(g) = crate::chatgpt::read_global_usage()
                                        && let Ok(gv) = serde_json::to_value(&g) {
                                            o.insert("rate_limit".into(), gv);
                                        }
                                } else {
                                    o.remove("rate_limit");
                                }
                            }
                            if compact
                                && let Some(lanes) = v.get_mut("lanes").and_then(|l| l.as_array_mut())
                            {
                                trim_finished_lanes(lanes, &session);
                            }
                            let lanes_json = v
                                .get("lanes")
                                .map(|l| l.to_string())
                                .unwrap_or_else(|| "[]".to_string());
                            attach_revision = attach_revision.wrapping_add(1);
                            if let Some(o) = v.as_object_mut() {
                                o.insert("revision".into(), serde_json::json!(attach_revision));
                                if continuous {
                                    let tail = &events[last_events.len()..];
                                    let total_events = last_event_offset + events.len();
                                    o.insert("wire".into(), serde_json::json!("delta"));
                                    o.insert("new_events".into(), wire_events(tail, compact));
                                    o.insert("event_count".into(), serde_json::json!(events.len()));
                                    o.insert("total_events".into(), serde_json::json!(total_events));
                                    if last_lanes_json.as_deref() == Some(lanes_json.as_str()) {
                                        o.remove("lanes");
                                    } else if !o.contains_key("lanes") {
                                        o.insert("lanes".into(), serde_json::json!([]));
                                    }
                                    last_events = events;
                                } else {
                                    let (rel_start, _, _) = session_event_page(&events, None, None);
                                    let start = base_offset + rel_start;
                                    last_event_offset = start;
                                    o.insert("wire".into(), serde_json::json!("snapshot"));
                                    o.insert("events".into(), wire_events(&events[rel_start..], compact));
                                    o.insert("event_offset".into(), serde_json::json!(start));
                                    last_events = events[rel_start..].to_vec();
                                }
                            }
                            last_lanes_json = Some(lanes_json);
                            last_queue_revision = queue_revision;
                            if let Ok(json) = serde_json::to_string(&v) {
                                if sender.send(Message::Text(json.into())).await.is_err() {
                                    break;
                                }
                                last_frame_sent = tokio::time::Instant::now();
                                last_state_fingerprint = Some(fingerprint);
                            }
                        }
                    }
                }
            {
                use std::hash::{Hash, Hasher};
                let snap = crate::llm::StreamBuffer::snapshot(&stream);
                let think = crate::llm::StreamBuffer::snapshot_thinking(&stream);
                let (visible, voice) = stream
                    .try_lock()
                    .map(|b| (b.text_visible, b.voice_turn))
                    .unwrap_or((false, false));
                let mut h = std::collections::hash_map::DefaultHasher::new();
                snap.hash(&mut h);
                think.hash(&mut h);
                visible.hash(&mut h);
                let fp = h.finish();
                if fp != last_stream_fp {
                    last_stream_fp = fp;
                    let append = stream_delta
                        && sent_once
                        && snap.starts_with(&sent_text)
                        && think.starts_with(&sent_thinking);
                    let frame = if append {
                        stream_frame(true, &snap[sent_text.len()..], &think[sent_thinking.len()..], visible, voice)
                    } else {
                        stream_frame(false, &snap, &think, visible, voice)
                    };
                    sent_once = true;
                    sent_text = snap;
                    sent_thinking = think;
                    if let Ok(json) = serde_json::to_string(&frame)
                        && sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                }
            }
            if let Some(terms) = terms.as_ref() {
                use base64::Engine;
                // Incremental PTY bytes only. Replaying raw scrollback into a
                // sized vt100 (tmux/ghostty-style: emulator owns the grid;
                // clients get live bytes or a cell snapshot, never CSI history)
                // is what glued `ls` onto the fish prompt.
                let frames = match term_client.as_ref() {
                    Some(c) => terms.poll_client(c),
                    None => terms.poll_all(),
                };
                for (id, chunk, cols, rows, alive) in frames {
                    if chunk.is_empty() && alive {
                        continue;
                    }
                    term_seq = term_seq.wrapping_add(1);
                    let frame = serde_json::json!({
                        "wire": "term",
                        "op": "out",
                        "id": id,
                        "seq": term_seq,
                        "data": base64::engine::general_purpose::STANDARD.encode(&chunk),
                        "cols": cols,
                        "rows": rows,
                        "alive": alive,
                    });
                    if let Ok(json) = serde_json::to_string(&frame)
                        && sender.send(Message::Text(json.into())).await.is_err() {
                            break 'attach;
                        }
                }
            }
            let event_request = match pending_event.take().or_else(|| event_rx.try_recv().ok()) {
                Some(index) => {
                    let path = state_path.clone();
                    tokio::task::spawn_blocking(move || read_session_state_tail(&path, index))
                        .await
                        .ok()
                        .flatten()
                        .map(|(state, _)| (index, state))
                }
                None => None,
            };
            if let Some((index, state)) = event_request {
                    let frame = serde_json::json!({
                        "wire": "event",
                        "index": index,
                        "event": state.events.first(),
                    });
                    if sender
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            if let Some((before, limit)) = pending_history.take().or_else(|| history_rx.try_recv().ok()) {
                let paged = match limit {
                    Some(size) => {
                        let path = state_path.clone();
                        tokio::task::spawn_blocking(move || {
                            crate::session::read_session_event_page(&path, Some(before), size.clamp(1, 500))
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                    None => None,
                };
                let paged = match paged {
                    Some(paged) => Some(paged),
                    None => {
                        let path = state_path.clone();
                        tokio::task::spawn_blocking(move || {
                            let (state, _) = read_session_state_tail(&path, 0)?;
                            let (start, end, _) = session_event_page(&state.events, Some(before), limit);
                            Some((state.events[start..end].to_vec(), start, end))
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                };
                if let Some((events, start, end)) = paged {
                    let has_older = start > 0;
                    let frame = serde_json::json!({
                        "wire": "history",
                        "events": wire_events(&events, compact),
                        "start": start,
                        "end": end,
                        "has_older": has_older,
                    });
                    if sender
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    last_frame_sent = tokio::time::Instant::now();
                }
            }
            while ping_rx.try_recv().is_ok() {
                pinged = true;
            }
            if std::mem::take(&mut pinged) || last_frame_sent.elapsed() >= HEARTBEAT {
                if sender
                    .send(Message::Text("{\"wire\":\"hb\"}".into()))
                    .await
                    .is_err()
                {
                    break;
                }
                last_frame_sent = tokio::time::Instant::now();
            }
            let deadline = (last_frame_sent + HEARTBEAT).min(tokio::time::Instant::now() + PUSH_FALLBACK_TICK);
            tokio::select! {
                _ = signals.changed() => {}
                Some(index) = event_rx.recv() => pending_event = Some(index),
                Some(request) = history_rx.recv() => pending_history = Some(request),
                Some(()) = ping_rx.recv() => pinged = true,
                _ = tokio::time::sleep_until(deadline) => check_state_next = true,
            }
        }
    });

    // Each idempotent client input may carry a nonce. On reconnect the mobile
    // client resends with the same nonce so the server drops duplicates.
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(t) => {
                // Parse as raw Value first to extract the nonce without
                // changing the LoopInput serde format.
                let dominated = serde_json::from_str::<serde_json::Value>(t.as_str());
                if let Ok(val) = dominated {
                    if val.get("kind").and_then(|k| k.as_str()) == Some("ping") {
                        let _ = ping_tx.send(());
                        continue;
                    }
                    if val.get("wire").and_then(|w| w.as_str()) == Some("term") {
                        if let Some(terms) = terms.as_ref() {
                            apply_term_client(terms, &val);
                        }
                        continue;
                    }
                    if val.get("kind").and_then(|k| k.as_str()) == Some("history") {
                        let before =
                            val.get("before").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        let limit = val.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
                        let _ = history_request_tx.send((before, limit));
                        continue;
                    }
                    if val.get("kind").and_then(|k| k.as_str()) == Some("event") {
                        if let Some(index) = val.get("index").and_then(|v| v.as_u64()) {
                            let _ = event_request_tx.send(index as usize);
                        }
                        continue;
                    }
                    if let Some(nonce) = val.get("nonce").and_then(|n| n.as_str())
                        && let Some(kind) = val.get("kind").and_then(|k| k.as_str()) {
                            // User messages and approval/question answers are both
                            // retried across reconnects and must be idempotent.
                            let idempotent = matches!(
                                kind,
                                "user_message"
                                    | "answer"
                                    | "approve"
                                    | "approve_all"
                                    | "deny"
                                    | "queue"
                                    | "unqueue"
                                    | "steer_queued"
                                    | "drop_queued"
                            );
                            if idempotent && !daemon.accept_nonce(&session, nonce) {
                                continue; // duplicate — drop silently
                            }
                        }
                }
                if read_only {
                    continue;
                }
                if let Ok(input) = serde_json::from_str::<LoopInput>(t.as_str()) {
                    if let LoopInput::Answer(text) = &input
                        && let Some(question) =
                            direct::latest_question_event(&daemon.store, &session)
                    {
                        let _ = direct::record_question_answer(
                            &daemon.store,
                            &session,
                            &question.event_id,
                            text,
                            None,
                        );
                    }
                    if matches!(
                        input,
                        LoopInput::UserMessage(_)
                            | LoopInput::Queue(_)
                            | LoopInput::Answer(_)
                            | LoopInput::SteerQueued(_)
                            | LoopInput::Interrupt
                    ) {
                        crate::speech::cancel_all("interrupted");
                    }
                    daemon.deliver(&session, input).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

const TRIMMED_SUMMARY_CHARS: usize = 600;

/// Finished lanes keep their card fields; the full text is `GET /session/lane`.
fn trim_finished_lanes(lanes: &mut [serde_json::Value], session: &str) {
    for lane in lanes {
        let Some(o) = lane.as_object_mut() else { continue };
        if matches!(o.get("status").and_then(|s| s.as_str()), Some("running" | "paused")) {
            continue;
        }
        let mut trimmed = false;
        for key in ["handoff", "report", "activity_log"] {
            trimmed |= o.remove(key).is_some();
        }
        if let Some(summary) = o.get("summary").and_then(|s| s.as_str())
            && summary.chars().count() > TRIMMED_SUMMARY_CHARS
        {
            let short: String = summary.chars().take(TRIMMED_SUMMARY_CHARS).collect();
            o.insert("summary".into(), serde_json::json!(format!("{short}…")));
            trimmed = true;
        }
        if trimmed {
            o.insert("trimmed".into(), serde_json::json!(true));
            o.insert("session".into(), serde_json::json!(session));
        }
    }
}

const HEARTBEAT: Duration = Duration::from_secs(5);
// Backstop for writers that bypass the change signals (e.g. another process).
const PUSH_FALLBACK_TICK: Duration = Duration::from_secs(1);

fn load_initial_state(path: &Path) -> Option<(crate::harness::HarnessState, usize)> {
    if let Some(mut meta) = crate::session::read_session_meta(path)
        && let Some((events, win_start, _)) = crate::session::read_session_event_page(path, None, 200)
    {
        meta.events = events;
        return Some((meta, win_start));
    }
    read_session_state_tail(path, 0).map(|(s, _)| (s, 0))
}

/// What wakes an `/attach` push loop besides client requests and the fallback tick.
struct AttachSignals {
    session: tokio::sync::watch::Receiver<()>,
    stream: Option<tokio::sync::watch::Receiver<()>>,
    term: Option<tokio::sync::watch::Receiver<()>>,
    // `changed()` marks the value seen, so remember a session wake for `mark_seen`.
    session_woke: bool,
}

impl AttachSignals {
    /// Marks every signal seen; true when the session store changed since the last call.
    fn mark_seen(&mut self) -> bool {
        let session = std::mem::take(&mut self.session_woke) | self.session.borrow_and_update().has_changed();
        for rx in [self.stream.as_mut(), self.term.as_mut()].into_iter().flatten() {
            rx.borrow_and_update();
        }
        session
    }

    async fn changed(&mut self) {
        async fn wait(rx: Option<&mut tokio::sync::watch::Receiver<()>>) {
            if let Some(rx) = rx
                && rx.changed().await.is_ok()
            {
                return;
            }
            std::future::pending().await
        }
        tokio::select! {
            _ = wait(Some(&mut self.session)) => self.session_woke = true,
            _ = wait(self.stream.as_mut()) => {}
            _ = wait(self.term.as_mut()) => {}
        }
    }
}

fn apply_term_client(terms: &crate::term::SessionTerms, val: &serde_json::Value) {
    use base64::Engine;
    let op = val.get("op").and_then(|v| v.as_str()).unwrap_or("");
    let cols = val.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
    let rows = val.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
    let id = val
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();
    match op {
        "open" | "new" => {
            // Honor the client's pane id. Allocating a different one left the
            // TUI painting an empty pane while output landed on an unseen id.
            let id = if op == "new" && id.is_empty() {
                terms.alloc_id()
            } else {
                id
            };
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                term.resize(cols, rows);
                // Do not snapshot raw scrollback — replaying it into vt100
                // at the current size scrambles fish/zsh/bash prompts.
            }
        }
        "resize" => {
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                term.resize(cols, rows);
            }
        }
        "in" => {
            // Typing must spawn the pane if `open`/`new` never landed
            // (blank Ctrl-N tab). Dropping `in` on a missing id is why
            // keys look captured but never paint.
            if let Some(term) = terms.get_or_create(&id) {
                let _ = term.ensure(cols, rows);
                if let Some(data) = val.get("data").and_then(|v| v.as_str())
                    && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        term.write(&bytes);
                    }
            }
        }
        "close" => terms.close(&id),
        _ => {}
    }
}

// WS /events — device-wide firehose. Emits a compact event on status changes
// (including running) so the session list can update live, even for chats the
// app isn't painting. Durable notifications are emitted as separate full frames.
pub(crate) async fn events_ws(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_events_ws)
}


async fn handle_events_ws(socket: WebSocket) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = subscribe_device_events();
    let push = tokio::spawn(async move {
        loop {
            let frame = match rx.recv().await {
                Ok(e) => e.to_string(),
                // A slow client missed frames: tell it to refetch rather than drift.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => r#"{"kind":"resync"}"#.to_string(),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if sender.send(Message::Text(frame.into())).await.is_err() {
                return;
            }
        }
    });
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Close(_) = msg {
            break;
        }
    }
    push.abort();
}

/// Idle PTYs still need a pump so BEL / OSC fire when nobody is attached.
pub(crate) async fn terminal_pump_loop(daemon: Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        for sess in daemon.sessions.lock().await.values() {
            sess.terms.pump();
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_lanes_are_trimmed_and_running_ones_kept_whole() {
        let mut lanes = vec![
            serde_json::json!({"id": "lane-1", "status": "completed", "report": "r", "handoff": "h", "summary": "x".repeat(700), "transcript": "t"}),
            serde_json::json!({"id": "lane-2", "status": "running", "handoff": "h", "activity_log": [1]}),
        ];
        trim_finished_lanes(&mut lanes, "s1");
        assert!(lanes[0].get("report").is_none() && lanes[0].get("handoff").is_none());
        assert_eq!(lanes[0]["trimmed"], true);
        assert_eq!(lanes[0]["session"], "s1");
        assert_eq!(lanes[0]["transcript"], "t");
        assert_eq!(lanes[0]["summary"].as_str().unwrap().chars().count(), TRIMMED_SUMMARY_CHARS + 1);
        assert_eq!(lanes[1]["handoff"], "h");
        assert!(lanes[1].get("trimmed").is_none());
    }

    fn signals_for(stream: &crate::llm::StreamHandle) -> (AttachSignals, tokio::sync::watch::Sender<()>) {
        let (session_tx, session) = tokio::sync::watch::channel(());
        let signals = AttachSignals { session, stream: crate::llm::StreamBuffer::changes(stream), term: None, session_woke: false };
        (signals, session_tx)
    }

    #[tokio::test]
    async fn stream_append_wakes_push_before_fallback_tick() {
        let stream: crate::llm::StreamHandle = Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default()));
        let (mut signals, _session_tx) = signals_for(&stream);
        signals.mark_seen();
        assert!(tokio::time::timeout(Duration::from_millis(50), signals.changed()).await.is_err());
        let writer = stream.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            crate::llm::StreamBuffer::append(&writer, "hi");
        });
        let started = tokio::time::Instant::now();
        tokio::time::timeout(PUSH_FALLBACK_TICK, signals.changed()).await.expect("append must wake the push");
        assert!(started.elapsed() < PUSH_FALLBACK_TICK / 2);
        assert!(!signals.mark_seen(), "a stream change must not force a store read");
    }

    #[tokio::test]
    async fn signal_between_mark_and_wait_is_not_lost() {
        let stream: crate::llm::StreamHandle = Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default()));
        let (mut signals, session_tx) = signals_for(&stream);
        signals.mark_seen();
        session_tx.send_replace(());
        tokio::time::timeout(Duration::from_millis(50), signals.changed()).await.expect("pending signal wakes at once");
        assert!(signals.mark_seen(), "a session change must trigger a state check");
    }
}

fn stream_frame(append: bool, text: &str, thinking: &str, visible: bool, voice: bool) -> serde_json::Value {
    let mut frame = serde_json::json!({
        "wire": "stream",
        "text": text,
        "thinking": thinking,
        "text_visible": visible,
    });
    if append {
        frame["append"] = true.into();
    }
    if voice {
        frame["voice"] = true.into();
    }
    frame
}

#[cfg(test)]
mod stream_frame_tests {
    use super::stream_frame;

    #[test]
    fn voice_flag_only_on_voice_turns() {
        let voice = stream_frame(false, "hi", "", true, true);
        assert_eq!(voice["voice"], true);
        let plain = stream_frame(true, "hi", "", true, false);
        assert!(plain.get("voice").is_none());
        assert_eq!(plain["append"], true);
    }
}
