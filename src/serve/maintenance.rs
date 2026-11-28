use super::*;

/// model so edits (image support, model swap, new profile) apply immediately.
/// A running turn is never interrupted — a busy session stays queued and is
/// rebuilt the moment it goes idle (its model is only used at the next turn
/// anyway, so nothing is lost by waiting).
pub(crate) async fn config_watch_loop(daemon: Shared) {
    use std::collections::HashSet;
    use std::time::Duration;

    let path = daemon.config_path.clone();
    let mut last_mtime = tokio::fs::metadata(&path)
        .await
        .ok()
        .and_then(|m| m.modified().ok());
    let mut pending: HashSet<String> = HashSet::new();

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        if let Ok(meta) = tokio::fs::metadata(&path).await
            && let Ok(mtime) = meta.modified()
            && Some(mtime) != last_mtime
        {
            last_mtime = Some(mtime);
            daemon.reload_config().await;
            let ids: Vec<String> = daemon.sessions.lock().await.keys().cloned().collect();
            eprintln!(
                "config.toml changed — reloaded; rebuilding {} live session model(s)",
                ids.len()
            );
            pending.extend(ids);
        }

        if pending.is_empty() {
            continue;
        }
        let mut done = Vec::new();
        for id in pending.iter() {
            match daemon.rebuild_session_model(id).await {
                RebuildOutcome::Rebuilt | RebuildOutcome::Gone => done.push(id.clone()),
                RebuildOutcome::Busy => {} // retry next tick
            }
        }
        for id in done {
            pending.remove(&id);
        }
    }
}

/// Keep worktree sessions' diff totals current: every row once at start, then whatever is running
/// or was active in the last few minutes, so a turn's edits and a finished turn both show up.
/// Every 15 minutes (outside quiet hours), find human-managed sessions that
/// have been idle 90m..24h and ask their desk once per idle spell to check if
/// concrete work is still pending and offer a one-tap takeover DM.
pub(crate) async fn idle_takeover_loop(daemon: Shared) {
    use std::time::Duration;

    const IDLE_MIN_SECS: i64 = 90 * 60;
    const IDLE_MAX_SECS: i64 = 24 * 3600;
    tokio::time::sleep(Duration::from_secs(5 * 60)).await;
    loop {
        if !crate::mission_autonomy::in_quiet_hours(&crate::mission_autonomy::load().settings) {
            let now_secs = crate::session::now_unix_secs();
            let max_last_active = now_secs - IDLE_MIN_SECS;
            let min_last_active = now_secs - IDLE_MAX_SECS;
            let candidates: Vec<(String, String, String, i64)> = daemon
                .store
                .with_connection(|conn| {
                    let mut stmt = conn.prepare(
                        "SELECT id, COALESCE(NULLIF(title, ''), id), COALESCE(agent_id, ''), last_active
                         FROM sessions
                         WHERE managed_by = 'human'
                           AND status = 'idle'
                           AND settled_at IS NULL
                           AND last_active IS NOT NULL
                           AND last_active <= ?1
                           AND last_active >= ?2
                           AND id NOT LIKE '%/lanes/%'
                           AND id NOT LIKE 'inbox-%'
                           AND id != 'lead'
                         ORDER BY last_active DESC
                         LIMIT 8",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![max_last_active, min_last_active], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .unwrap_or_default();

            let now_iso = chrono::Utc::now().to_rfc3339();
            for (id, title, agent_id, last_active) in candidates {
                let desk = if !agent_id.is_empty() && agent_id != crate::lead::SESSION_ID {
                    agent_id
                } else {
                    "snippet".to_string()
                };
                if daemon.store.get_agent(&desk).ok().flatten().is_none() {
                    continue;
                }
                let idle_mins = (now_secs - last_active).max(0) / 60;
                let _ = daemon.store.enqueue_desk_event(
                    &crate::coordination::desk_inbox::NewDeskEvent {
                        agent_id: desk,
                        kind: "idle_check".into(),
                        thread_id: None,
                        reply_thread: Some("human".into()),
                        source_ref: Some(format!("idle-check:{id}:{last_active}")),
                        priority: 15,
                        is_mention: false,
                        summary: format!(
                            "Human session {title} ({id}) has been idle {idle_mins}m. Read its recent history (`snippet tool read.session session_id={id} event_limit=15`) and judge whether the user's latest ask/feedback was actually finished (see `desk.guide` orchestration for examples). Offer takeover via one short DM with choices='[\"Take over\",\"Leave with me\",\"Mark done\"]' when work stopped mid-turn (rate/usage limit, error, uncommitted work, or unfinished user feedback). Call `event.noop` silently only when the user's latest ask was clearly answered/completed or it was pure Q&A."
                        ),
                        payload: Some(serde_json::json!({
                            "session": id,
                            "idle_mins": idle_mins,
                            "event": "idle_takeover_check",
                        })),
                        deadline_at: None,
                        wake: true,
                    },
                    &now_iso,
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(15 * 60)).await;
    }
}

pub(crate) async fn worktree_totals_loop(daemon: Shared) {
    use std::time::Duration;

    let mut active_since = None;
    loop {
        let d = daemon.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::session_stats::refresh_worktree_totals(&d.store, active_since);
        })
        .await;
        tokio::time::sleep(Duration::from_secs(20)).await;
        active_since = Some(chrono::Utc::now().timestamp() - 300);
    }
}

/// Result of an attempt to rebuild a live session's model from the current config.
pub(crate) enum RebuildOutcome {
    Rebuilt,
    Busy, // mid-turn — try again once idle
    Gone, // session no longer live; nothing to do
}

/// Periodic self-update loop for the daemon. On a new release: replace the
/// binary, wait for sessions to be idle, then hand off to the service manager.
pub(crate) async fn self_update_loop(daemon: Shared, supervised: bool) {
    use std::time::Duration;
    const CHECK_EVERY: Duration = Duration::from_secs(30 * 60);
    let client = reqwest::Client::new();
    // The version already staged on disk THIS run. Without a supervisor the
    // running process keeps its old CARGO_PKG_VERSION, so `is_newer` would stay
    // true and we'd re-download the same release every cycle — this guards it.
    let mut staged: Option<String> = None;
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        if crate::update::disabled() {
            continue;
        }
        let Some(latest) = crate::update::latest_version(&client).await else {
            continue;
        };
        if !crate::update::is_newer(&latest) || staged.as_deref() == Some(latest.as_str()) {
            continue;
        }
        if crate::update::download_and_replace(&client, &latest)
            .await
            .is_err()
        {
            continue;
        }
        #[allow(unused_assignments)]
        {
            staged = Some(latest);
        }
        wait_for_idle(&daemon).await;
        if supervised {
            trigger_restart();
        } else {
            self_restart_process();
        }
        return;
    }
}

/// Resolve the real filesystem path of the running binary, bypassing
/// `/proc/self/exe` which keeps the old inode after `mv`.
fn resolve_exe_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(exe) = std::env::current_exe() {
            // current_exe() returns the path string (e.g. /home/.../snippet)
            // even though /proc/self/exe points at the old inode — the PathBuf
            // itself is just the string, so stat() on it will follow the
            // current directory entry.
            if exe.exists() {
                return Some(exe);
            }
        }
        let link = std::fs::read_link("/proc/self/exe").ok()?;
        return Some(link);
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().ok()
    }
}

/// Watch the on-disk binary for external replacement (manual `cp` + `mv`).
/// When the inode or mtime of the exe path differs from what we were started
/// with, the binary has been swapped — restart to pick it up.
pub(crate) async fn binary_watch_loop(daemon: Shared, supervised: bool) {
    use std::time::Duration;
    const CHECK_EVERY: Duration = Duration::from_secs(30);
    let exe = match resolve_exe_path() {
        Some(p) => p,
        None => return,
    };
    let initial_meta = match std::fs::metadata(&exe) {
        Ok(m) => Some((inode_from_meta(&m), mtime_from_meta(&m))),
        Err(_) => None,
    };
    let (initial_inode, initial_mtime) = match initial_meta {
        Some(v) => v,
        None => return,
    };
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        let meta = match std::fs::metadata(&exe) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let cur_inode = inode_from_meta(&meta);
        let cur_mtime = mtime_from_meta(&meta);
        if cur_inode != initial_inode || cur_mtime != initial_mtime {
            wait_for_idle(&daemon).await;
            if supervised {
                trigger_restart();
            } else {
                self_restart_process();
            }
            return;
        }
    }
}

#[cfg(unix)]
fn inode_from_meta(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.ino()
}
#[cfg(not(unix))]
fn inode_from_meta(_m: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn mtime_from_meta(m: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime()
}
#[cfg(not(unix))]
fn mtime_from_meta(_m: &std::fs::Metadata) -> i64 {
    0
}

/// Replace the current process with a fresh execution of itself. On Unix this
/// uses `exec()` so the PID, env vars (including `__SNIPPET_SERVE_WORKER`), and
/// file descriptors are preserved — the new binary picks up exactly where we
/// left off.
fn self_restart_process() {
    use std::os::unix::process::CommandExt;
    let exe = match resolve_exe_path() {
        Some(p) => p,
        None => return,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    // exec() replaces us in-place; it only returns on failure.
    let err = std::process::Command::new(&exe).args(&args).exec();
    eprintln!("failed to exec restart: {err}");
}

/// Whether any live session is mid-turn (persisted status `Running`).
async fn any_session_busy(daemon: &Shared) -> bool {
    let sessions = daemon.sessions.lock().await;
    for s in sessions.values() {
        if read_session_meta(&s.state_path)
            .is_some_and(|state| state.status == crate::harness::HarnessStatus::Running)
        {
            return true;
        }
    }
    false
}

/// Block until no session is mid-turn, capped at ~5 minutes so a perpetually
/// busy session can't defer the update forever (a restart never loses persisted
/// state — at worst it interrupts one in-flight turn, which resumes cleanly).
async fn wait_for_idle(daemon: &Shared) {
    for _ in 0..60 {
        if !any_session_busy(daemon).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Ask the OS service manager to restart this daemon (systemd --user on Linux,
/// launchd on macOS) so it comes back on the freshly-installed binary.
fn trigger_restart() {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "restart", "snippet-serve.service"])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(uid) = current_uid() {
            let _ = std::process::Command::new("launchctl")
                .args(["kickstart", "-k", &format!("gui/{uid}/{SERVICE_LABEL}")])
                .spawn();
        }
    }
}
