use super::api_error;
use super::*;

// A shell belongs to the machine: switching or closing a session must not kill
// it. `/attach` cannot serve this because it calls `ensure_live`, so a socket
// with no live session gets a 404. This route carries the same `wire: term`
// frames over a socket that requires no session.
//
// Lifecycle is still explicit: the client creates and closes shells through the
// same `open`/`new`/`close` ops, so a dropped socket never destroys a pty.
pub(crate) async fn shells_ws(ws: WebSocketUpgrade, State(d): State<Shared>) -> Response {
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
        && sender.send(Message::Text(json.into())).await.is_err()
    {
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
                    && sender.send(Message::Text(json.into())).await.is_err()
                {
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
                    && val.get("wire").and_then(|w| w.as_str()) == Some("term")
                {
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
    let stream_delta = on(&q.stream_delta);
    let packed = q.enc.as_deref() == Some("gzip");
    let resume = q.from.zip(q.at).filter(|(from, _)| *from > 0);
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
            handle_ws(
                socket,
                daemon,
                session,
                state_path,
                stream,
                None,
                AttachOptions {
                    stream_delta,
                    read_only: true,
                    packed,
                    resume,
                },
            )
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
                handle_ws(
                    socket,
                    daemon,
                    session,
                    state_path,
                    stream,
                    terms,
                    AttachOptions {
                        stream_delta,
                        read_only: false,
                        packed,
                        resume,
                    },
                )
            })
        }
        None => api_error(StatusCode::NOT_FOUND, "no such session"),
    }
}

#[derive(Clone, Copy)]
struct AttachOptions {
    stream_delta: bool,
    read_only: bool,
    packed: bool,
    resume: Option<(usize, i64)>,
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
    let AttachOptions {
        stream_delta,
        read_only,
        packed,
        resume: resume_request,
    } = options;
    let (mut sender, mut receiver) = socket.split();
    let (history_tx, history_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, Option<usize>)>();
    let history_request_tx = history_tx.clone();
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
        let mut pending_history: Option<(usize, Option<usize>)> = None;
        let mut pinged = false;
        let mut last_state_fingerprint = None;
        let mut last_events: Vec<crate::harness::HarnessEvent> = Vec::new();
        let mut last_event_offset = 0usize;
        let mut last_lanes_json: Option<String> = None;
        let mut last_lane_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut last_stream_fp: u64 = 0;
        let mut sent_text = String::new();
        let mut sent_thinking = String::new();
        let mut sent_once = false;
        let mut last_queue_revision = 0;
        let mut attach_revision: u64 = 0;
        let mut term_seq: u64 = 0;
        let mut last_counts: Option<SessionCounts> = None;
        let mut last_counts_bg_rev: u64 = 0;
        let mut last_counts_pr_rev: u64 = 0;
        let mut last_counts_check: Option<tokio::time::Instant> = None;
        let mut workspace_dir: Option<PathBuf> = None;
        let mut resume_request = resume_request;
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
            let state_changed = fingerprint.is_some_and(|fp| Some(fp) != last_state_fingerprint)
                || queue_revision != last_queue_revision;
            let bg_rev = crate::bg::revision();
            let pr_rev = super::github_routes::pr_cache_revision();
            let mut precomputed_counts: Option<SessionCounts> = None;
            let mut counts_changed = false;
            if check_state
                && !state_changed
                && let Some(ws) = workspace_dir.as_ref()
                && (last_counts.is_none()
                    || bg_rev != last_counts_bg_rev
                    || pr_rev != last_counts_pr_rev
                    || last_counts_check.is_none_or(|t| t.elapsed() >= COUNTS_TTL_CHECK))
            {
                let store = daemon.store.clone();
                let sid = session.clone();
                let ws = ws.clone();
                if let Ok(counts) =
                    tokio::task::spawn_blocking(move || compute_session_counts(&store, &sid, &ws))
                        .await
                {
                    last_counts_bg_rev = bg_rev;
                    last_counts_pr_rev = pr_rev;
                    last_counts_check = Some(tokio::time::Instant::now());
                    counts_changed = Some(counts) != last_counts;
                    precomputed_counts = Some(counts);
                }
            }
            if let Some(fingerprint) = fingerprint
                && (state_changed || counts_changed)
            {
                let resume = !last_events.is_empty();
                let resumed = match resume_request.take() {
                    Some((from, at)) => resume_tail(&state_path, from, at).await,
                    None => None,
                };
                let was_resumed = resumed.is_some();
                let loaded = if let Some((state, base, from)) = resumed {
                    last_events = state.events[..from - base].to_vec();
                    last_event_offset = base;
                    Some((state, true, base))
                } else if resume {
                    let path = state_path.clone();
                    let from = last_event_offset;
                    let tail =
                        tokio::task::spawn_blocking(move || read_session_state_tail(&path, from))
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
                        entries.retain(|id| state.queued_inputs.iter().any(|item| &item.id == id));
                        entries.clone()
                    };
                    if !hidden.is_empty() {
                        state
                            .queued_inputs
                            .retain(|item| !hidden.contains(&item.id));
                    }
                    workspace_dir =
                        (!state.workspace.is_empty()).then(|| PathBuf::from(&state.workspace));
                    let counts = match precomputed_counts {
                        Some(c) => c,
                        None => {
                            let store = daemon.store.clone();
                            let sid = session.clone();
                            let ws = workspace_dir.clone();
                            let computed = tokio::task::spawn_blocking(move || match ws {
                                Some(ref dir) if dir.is_dir() => {
                                    compute_session_counts(&store, &sid, dir)
                                }
                                _ => SessionCounts::default(),
                            })
                            .await
                            .unwrap_or_default();
                            last_counts_bg_rev = crate::bg::revision();
                            last_counts_pr_rev = super::github_routes::pr_cache_revision();
                            last_counts_check = Some(tokio::time::Instant::now());
                            computed
                        }
                    };
                    last_counts = Some(counts);
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
                                    && let Ok(gv) = serde_json::to_value(&g)
                                {
                                    o.insert("rate_limit".into(), gv);
                                }
                            } else {
                                o.remove("rate_limit");
                            }
                        }
                        if let Some(lanes) = v.get_mut("lanes").and_then(|l| l.as_array_mut()) {
                            for (lane_json, record) in lanes.iter_mut().zip(state.lanes.iter()) {
                                crate::lanes::attach_recent(lane_json, record);
                            }
                        }
                        let lane_map: std::collections::HashMap<String, String> = v
                            .get("lanes")
                            .and_then(|l| l.as_array())
                            .map(|lanes| {
                                lanes
                                    .iter()
                                    .filter_map(|lane| {
                                        let id = lane.get("id")?.as_str()?.to_string();
                                        Some((id, lane.to_string()))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let lanes_json = v
                            .get("lanes")
                            .map(|l| l.to_string())
                            .unwrap_or_else(|| "[]".to_string());
                        attach_revision = attach_revision.wrapping_add(1);
                        if let Some(o) = v.as_object_mut() {
                            o.insert("revision".into(), serde_json::json!(attach_revision));
                            o.insert("counts".into(), serde_json::json!(counts));
                            if continuous {
                                let total_events = last_event_offset + events.len();
                                o.insert("wire".into(), serde_json::json!("delta"));
                                let mut added = wire_events_slice(&events, last_events.len());
                                stamp_event_times(
                                    &state_path,
                                    last_event_offset + last_events.len(),
                                    &mut added,
                                )
                                .await;
                                o.insert("new_events".into(), added);
                                o.insert("event_count".into(), serde_json::json!(events.len()));
                                o.insert("total_events".into(), serde_json::json!(total_events));
                                if was_resumed {
                                    o.insert("resumed".into(), serde_json::json!(true));
                                }
                                if last_lanes_json.as_deref() == Some(lanes_json.as_str()) {
                                    o.remove("lanes");
                                } else if packed
                                    && !last_lane_map.is_empty()
                                    && let Some(serde_json::Value::Array(lanes)) = o.remove("lanes")
                                {
                                    let ids: Vec<serde_json::Value> = lanes
                                        .iter()
                                        .map(|lane| lane.get("id").cloned().unwrap_or_default())
                                        .collect();
                                    let patch: Vec<serde_json::Value> = lanes
                                        .into_iter()
                                        .filter(|lane| {
                                            let id = lane.get("id").and_then(|i| i.as_str());
                                            id.and_then(|id| last_lane_map.get(id))
                                                != Some(&lane.to_string())
                                        })
                                        .collect();
                                    o.insert("lanes_patch".into(), serde_json::json!(patch));
                                    o.insert("lane_ids".into(), serde_json::json!(ids));
                                } else if !o.contains_key("lanes") {
                                    o.insert("lanes".into(), serde_json::json!([]));
                                }
                                last_events = events;
                            } else {
                                let (rel_start, _, _) = session_event_page(&events, None, None);
                                let start = base_offset + rel_start;
                                last_event_offset = start;
                                o.insert("wire".into(), serde_json::json!("snapshot"));
                                let mut window = wire_events_slice(&events, rel_start);
                                stamp_event_times(&state_path, start, &mut window).await;
                                o.insert("events".into(), window);
                                o.insert("event_offset".into(), serde_json::json!(start));
                                last_events = events[rel_start..].to_vec();
                            }
                        }
                        last_lanes_json = Some(lanes_json);
                        last_lane_map = lane_map;
                        last_queue_revision = queue_revision;
                        if let Ok(json) = serde_json::to_string(&v) {
                            if !send_wire(&mut sender, json, packed).await {
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
                        stream_frame(
                            true,
                            &snap[sent_text.len()..],
                            &think[sent_thinking.len()..],
                            visible,
                            voice,
                        )
                    } else {
                        stream_frame(false, &snap, &think, visible, voice)
                    };
                    sent_once = true;
                    sent_text = snap;
                    sent_thinking = think;
                    if let Ok(json) = serde_json::to_string(&frame)
                        && !send_wire(&mut sender, json, packed).await
                    {
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
                        && !send_wire(&mut sender, json, packed).await
                    {
                        break 'attach;
                    }
                }
            }
            if let Some((before, limit)) = pending_history
                .take()
                .or_else(|| history_rx.try_recv().ok())
            {
                let paged = match limit {
                    Some(size) => {
                        let path = state_path.clone();
                        tokio::task::spawn_blocking(move || {
                            crate::session::read_session_event_page(
                                &path,
                                Some(before),
                                size.clamp(1, 500),
                            )
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
                            let (start, end, _) =
                                session_event_page(&state.events, Some(before), limit);
                            Some((state.events[start..end].to_vec(), start, end))
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                };
                if let Some((events, start, end)) = paged {
                    let has_older = start > 0;
                    let mut page = wire_events(&events);
                    stamp_event_times(&state_path, start, &mut page).await;
                    let frame = serde_json::json!({
                        "wire": "history",
                        "events": page,
                        "start": start,
                        "end": end,
                        "has_older": has_older,
                    });
                    if !send_wire(&mut sender, frame.to_string(), packed).await {
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
            let deadline =
                (last_frame_sent + HEARTBEAT).min(tokio::time::Instant::now() + PUSH_FALLBACK_TICK);
            tokio::select! {
                _ = signals.changed() => {}
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
                        let limit = val
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize);
                        let _ = history_request_tx.send((before, limit));
                        continue;
                    }
                    if let Some(nonce) = val.get("nonce").and_then(|n| n.as_str())
                        && let Some(kind) = val.get("kind").and_then(|k| k.as_str())
                    {
                        // User messages and approval/question answers are both
                        // retried across reconnects and must be idempotent.
                        let idempotent = matches!(
                            kind,
                            "user_message"
                                | "answer"
                                | "approve"
                                | "approve_all"
                                | "approve_rule"
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
                let parsed_val = serde_json::from_str::<serde_json::Value>(t.as_str()).ok();
                if let Ok(input) = serde_json::from_str::<LoopInput>(t.as_str()) {
                    let via = parsed_val
                        .as_ref()
                        .and_then(|v| v.get("via"))
                        .and_then(|v| v.as_str())
                        .filter(|v| {
                            matches!(
                                *v,
                                "app" | "notification" | "desk" | "tui" | "voice" | "agent"
                            )
                        })
                        .unwrap_or("app");
                    let by = parsed_val
                        .as_ref()
                        .and_then(|v| v.get("by"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or("human");
                    if matches!(
                        input.inner(),
                        LoopInput::UserMessage(_)
                            | LoopInput::Answer(_)
                            | LoopInput::Queue(_)
                            | LoopInput::SteerQueued(_)
                            | LoopInput::Approve
                            | LoopInput::ApproveAll
                            | LoopInput::ApproveRule
                            | LoopInput::Deny
                    ) {
                        let _ = daemon
                            .store
                            .set_session_user_seen(&session, crate::session::now_unix_secs());
                    }
                    if let LoopInput::Answer(text) = input.inner()
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
                        input.inner(),
                        LoopInput::UserMessage(_)
                            | LoopInput::Queue(_)
                            | LoopInput::Answer(_)
                            | LoopInput::SteerQueued(_)
                            | LoopInput::Interrupt
                    ) {
                        crate::speech::cancel_all("interrupted");
                    }
                    if matches!(input.inner(), LoopInput::UserMessage(_))
                        && resume_lean(&session, true)
                    {
                        daemon.deliver(&session, LoopInput::ContinueLean).await;
                    }
                    let input = match input {
                        LoopInput::Approve
                        | LoopInput::ApproveAll
                        | LoopInput::ApproveRule
                        | LoopInput::Deny
                        | LoopInput::Answer(_)
                        | LoopInput::UserMessage(_)
                        | LoopInput::Interrupt => input.with_source(by, via),
                        other => other,
                    };
                    daemon.deliver(&session, input).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    push.abort();
}

const HEARTBEAT: Duration = Duration::from_secs(5);
const RESUME_CONTEXT_EVENTS: usize = 64;
const PACK_MIN_BYTES: usize = 1024;
const PACK_CHUNK_BYTES: usize = 16 * 1024;
const PACK_BLOCKING_BYTES: usize = 256 * 1024;
const PACK_FLAG_BASE: u8 = 0x80;
const PACK_FLAG_FINAL: u8 = 0x01;
const PACK_FLAG_GZIP: u8 = 0x02;

fn gzip_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(
        Vec::with_capacity(raw.len() / 4),
        flate2::Compression::new(5),
    );
    enc.write_all(raw).ok()?;
    enc.finish().ok()
}

async fn send_wire<S>(sender: &mut S, json: String, packed: bool) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
{
    if !packed || json.len() < PACK_MIN_BYTES {
        return sender.send(Message::Text(json.into())).await.is_ok();
    }
    let gz = if json.len() >= PACK_BLOCKING_BYTES {
        let raw = json.clone();
        tokio::task::spawn_blocking(move || gzip_bytes(raw.as_bytes()))
            .await
            .ok()
            .flatten()
    } else {
        gzip_bytes(json.as_bytes())
    };
    let Some(gz) = gz else {
        return sender.send(Message::Text(json.into())).await.is_ok();
    };
    drop(json);
    for message in pack_messages(&gz) {
        if sender.send(Message::Binary(message.into())).await.is_err() {
            return false;
        }
    }
    true
}

fn pack_messages(gz: &[u8]) -> Vec<Vec<u8>> {
    let count = gz.len().div_ceil(PACK_CHUNK_BYTES).max(1);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let chunk =
            &gz[(i * PACK_CHUNK_BYTES).min(gz.len())..((i + 1) * PACK_CHUNK_BYTES).min(gz.len())];
        let mut flag = PACK_FLAG_BASE | PACK_FLAG_GZIP;
        if i + 1 == count {
            flag |= PACK_FLAG_FINAL;
        }
        let mut message = Vec::with_capacity(chunk.len() + 1);
        message.push(flag);
        message.extend_from_slice(chunk);
        out.push(message);
    }
    out
}

async fn resume_tail(
    path: &Path,
    from: usize,
    at: i64,
) -> Option<(crate::harness::HarnessState, usize, usize)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let held = crate::session::read_event_times(&path, from - 1, 1)?;
        if held.first().copied().flatten() != Some(at) {
            return None;
        }
        let base = from.saturating_sub(RESUME_CONTEXT_EVENTS);
        let (state, count) = read_session_state_tail(&path, base)?;
        (count >= from && base + state.events.len() == count).then_some((state, base, from))
    })
    .await
    .ok()
    .flatten()
}
// Backstop for writers that bypass the change signals (e.g. another process).
const PUSH_FALLBACK_TICK: Duration = Duration::from_secs(1);
const COUNTS_TTL_CHECK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct SessionCounts {
    changed_files: usize,
    open_prs: usize,
    processes: usize,
}

fn compute_session_counts(
    store: &crate::store::Store,
    session: &str,
    workspace: &Path,
) -> SessionCounts {
    let changed_files = store
        .get_session_row(session)
        .ok()
        .flatten()
        .and_then(|row| {
            let is_git = crate::session_stats::git_branch(workspace).is_some();
            let raw = if is_git {
                row.total_changes.or(row.last_changes)
            } else {
                row.last_changes
            };
            raw.as_deref()
                .and_then(|j| serde_json::from_str::<crate::session_stats::Changes>(j).ok())
        })
        .map(|c| c.files as usize)
        .unwrap_or(0);
    let open_prs = super::github_routes::cached_open_pr_count(session, workspace);
    let processes = crate::bg::running_count(workspace);
    SessionCounts {
        changed_files,
        open_prs,
        processes,
    }
}

/// Adds each wire event's persisted write time (`at_ms`, unix ms) so clients
/// can time a turn from its events; `start` is the first event's absolute
/// index. A separate key, because `Decision` and `SessionHandoff` already
/// carry their own `at` in seconds.
async fn stamp_event_times(path: &Path, start: usize, events: &mut serde_json::Value) {
    let Some(list) = events.as_array_mut() else {
        return;
    };
    if list.is_empty() {
        return;
    }
    let (path, len) = (path.to_path_buf(), list.len());
    let times =
        tokio::task::spawn_blocking(move || crate::session::read_event_times(&path, start, len))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
    for (event, at) in list.iter_mut().zip(times) {
        if let (Some(o), Some(at)) = (event.as_object_mut(), at) {
            o.insert("at_ms".into(), serde_json::json!(at));
        }
    }
}

fn load_initial_state(path: &Path) -> Option<(crate::harness::HarnessState, usize)> {
    if let Some(mut meta) = crate::session::read_session_meta(path)
        && let Some((events, win_start, _)) =
            crate::session::read_session_event_page(path, None, 200)
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
        let session =
            std::mem::take(&mut self.session_woke) | self.session.borrow_and_update().has_changed();
        for rx in [self.stream.as_mut(), self.term.as_mut()]
            .into_iter()
            .flatten()
        {
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
                    && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
                {
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
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    r#"{"kind":"resync"}"#.to_string()
                }
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

    fn signals_for(
        stream: &crate::llm::StreamHandle,
    ) -> (AttachSignals, tokio::sync::watch::Sender<()>) {
        let (session_tx, session) = tokio::sync::watch::channel(());
        let signals = AttachSignals {
            session,
            stream: crate::llm::StreamBuffer::changes(stream),
            term: None,
            session_woke: false,
        };
        (signals, session_tx)
    }

    #[tokio::test]
    async fn stream_append_wakes_push_before_fallback_tick() {
        let stream: crate::llm::StreamHandle =
            Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default()));
        let (mut signals, _session_tx) = signals_for(&stream);
        signals.mark_seen();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), signals.changed())
                .await
                .is_err()
        );
        let writer = stream.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            crate::llm::StreamBuffer::append(&writer, "hi");
        });
        let started = tokio::time::Instant::now();
        tokio::time::timeout(PUSH_FALLBACK_TICK, signals.changed())
            .await
            .expect("append must wake the push");
        assert!(started.elapsed() < PUSH_FALLBACK_TICK / 2);
        assert!(
            !signals.mark_seen(),
            "a stream change must not force a store read"
        );
    }

    #[tokio::test]
    async fn signal_between_mark_and_wait_is_not_lost() {
        let stream: crate::llm::StreamHandle =
            Arc::new(std::sync::Mutex::new(crate::llm::StreamBuffer::default()));
        let (mut signals, session_tx) = signals_for(&stream);
        signals.mark_seen();
        session_tx.send_replace(());
        tokio::time::timeout(Duration::from_millis(50), signals.changed())
            .await
            .expect("pending signal wakes at once");
        assert!(
            signals.mark_seen(),
            "a session change must trigger a state check"
        );
    }
}

fn stream_frame(
    append: bool,
    text: &str,
    thinking: &str,
    visible: bool,
    voice: bool,
) -> serde_json::Value {
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
    use super::{SessionCounts, compute_session_counts, stream_frame};

    #[test]
    fn voice_flag_only_on_voice_turns() {
        let voice = stream_frame(false, "hi", "", true, true);
        assert_eq!(voice["voice"], true);
        let plain = stream_frame(true, "hi", "", true, false);
        assert!(plain.get("voice").is_none());
        assert_eq!(plain["append"], true);
    }

    #[test]
    fn compute_session_counts_reads_diff_stats_and_bg_processes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        store
            .save_session_scalar(
                "s1",
                "k1",
                &tmp.path().display().to_string(),
                Some("t"),
                "idle",
                "{}",
                "2026-01-01T00:00:00Z",
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        store.record_turn_changes("s1", 4, 10, 2).unwrap();

        let counts = compute_session_counts(&store, "s1", tmp.path());
        assert_eq!(
            counts,
            SessionCounts {
                changed_files: 4,
                open_prs: 0,
                processes: 0,
            }
        );

        let pid = std::process::id();
        crate::bg::record(tmp.path(), "p1", "sleep 10", None, pid, None).unwrap();
        let counts = compute_session_counts(&store, "s1", tmp.path());
        assert_eq!(counts.processes, 1);
    }
}
