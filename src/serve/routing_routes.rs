use super::api_error;
use super::*;
use crate::harness::HarnessEvent;
use crate::routing::{self, LimitHit, Route};

fn route_for(id: &str) -> Option<Route> {
    if id == crate::lead::SESSION_ID {
        Some(Route::Lead)
    } else if crate::session::is_inbox_session_id(id) {
        Some(Route::Inbox)
    } else {
        None
    }
}

fn pinned_desk_profile(d: &Daemon, id: &str) -> Option<String> {
    if !crate::session::is_inbox_session_id(id) {
        return None;
    }
    let agent = id.strip_prefix("inbox-")?;
    let pinned = d
        .store
        .agent_desk_profile(agent)
        .ok()
        .flatten()
        .filter(|p| !p.trim().is_empty())?;
    let cfg = d.config.lock().unwrap();
    let valid = match cfg.setups.as_ref() {
        Some(setups) if !setups.is_empty() => setups.contains_key(&pinned),
        _ => true,
    };
    valid.then_some(pinned)
}

/// Where a limited session moves next; a desk pinned to a profile stays on it.
fn reroute_target(
    cfg: &crate::config::SnippetConfig,
    id: &str,
    pinned: Option<&str>,
    profile: &str,
) -> Option<String> {
    if pinned.is_some() {
        return None;
    }
    route_for(id).and_then(|route| routing::next_after(cfg, route, profile))
}

/// Whether the profile the agent's desk runs on is in a usage-limit window, so
/// desk batches wait instead of failing into it.
pub(crate) async fn is_desk_profile_limited(d: &Daemon, agent_id: &str) -> bool {
    let id = crate::session::inbox_session_id(agent_id);
    let profile = match pinned_desk_profile(d, &id) {
        Some(pinned) => pinned,
        None => profile_of(d, &id).await,
    };
    !profile.is_empty() && routing::limited_until(&profile).is_some()
}

async fn profile_of(d: &Daemon, id: &str) -> String {
    let live = d
        .sessions
        .lock()
        .await
        .get(id)
        .and_then(|s| s.profile.clone());
    live.or_else(|| {
        crate::session::state_path_for_id(id).and_then(|p| crate::session::read_session_profile(&p))
    })
    .or_else(|| d.config.lock().unwrap().active_setup.clone())
    .unwrap_or_default()
}

async fn send_input(d: &Daemon, id: &str, text: String) {
    let tx = d
        .sessions
        .lock()
        .await
        .get(id)
        .filter(|s| !s.join.is_finished())
        .map(|s| s.input_tx.clone());
    if let Some(tx) = tx {
        let _ = tx.send(LoopInput::UserMessage(text));
    }
}

async fn continue_on(
    d: &Shared,
    id: &str,
    from: &str,
    to: &str,
    resets_at: i64,
) -> Result<(), String> {
    d.run_session_on_profile(id, to, true).await?;
    record_notice(
        d,
        id,
        HarnessEvent::ProfileSwitched {
            from: from.to_string(),
            to: to.to_string(),
            resets_at,
        },
    )
    .await;
    send_input(
        d,
        id,
        crate::envelope::Envelope::new("routing")
            .attr("kind", "switched")
            .attr("from", &from)
            .attr("to", &to)
            .child("note", format!("{from} hit its usage limit, so this session now runs on {to}. The last turn stopped unfinished."))
            .render(),
    )
    .await;
    if crate::session::is_inbox_session_id(id)
        && let Some(agent) = id.strip_prefix("inbox-")
    {
        d.on_desk_provider_changed(agent, from, to, true);
    }
    Ok(())
}

pub(crate) async fn routing_loop(
    d: Shared,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LimitHit>,
) {
    while let Some(hit) = rx.recv().await {
        let id = hit.session_id.as_str();
        if crate::session::is_lane_session_id(id) || !d.store.has_conversation(id).unwrap_or(false)
        {
            continue;
        }
        let profile = profile_of(&d, id).await;
        let pinned = pinned_desk_profile(&d, id);
        let already_limited = routing::limited_until(&profile).is_some();
        routing::mark_limited(&profile, hit.resets_at);
        if pinned.is_some() && already_limited {
            continue;
        }
        let resets_at = routing::limited_until(&profile).unwrap_or(hit.resets_at);
        let cfg = d.config.lock().unwrap().clone();
        let next = reroute_target(&cfg, id, pinned.as_deref(), &profile);
        if let Some(next) = next {
            if let Err(e) = continue_on(&d, id, &profile, &next, resets_at).await {
                eprintln!("routing {id}: could not switch to {next}: {e}");
            } else {
                continue;
            }
        }
        let options = if route_for(id).is_some() {
            Vec::new()
        } else {
            routing::alternatives(&cfg, &profile)
        };
        if let Ok(Some(member)) = d.store.member_by_session(id) {
            let message = format!(
                "`{}` can't run: its conversation {id} hit the limit on profile `{profile}`. It will miss anything sent to it until that session has a working profile.",
                member.agent_id
            );
            let now = chrono::Utc::now().to_rfc3339();
            let _ = d.store.update_task_in(&member.task_id, &now, |t| {
                t.notify_once("member_stopped", &message)
            });
        }
        record_notice(
            &d,
            id,
            HarnessEvent::LimitReached {
                profile,
                resets_at,
                options,
            },
        )
        .await;
    }
}

#[derive(Deserialize)]
pub(crate) struct LimitReq {
    session: String,
    action: String,
    #[serde(default)]
    profile: Option<String>,
}

pub(crate) async fn resolve_limit(State(d): State<Shared>, Json(req): Json<LimitReq>) -> Response {
    let id = req.session.as_str();
    let current = profile_of(&d, id).await;
    let resets_at = routing::limited_until(&current).unwrap_or(0);
    match req.action.as_str() {
        "switch" => {
            let Some(to) = req.profile.filter(|p| !p.trim().is_empty()) else {
                return api_error(StatusCode::BAD_REQUEST, "profile required");
            };
            match continue_on(&d, id, &current, &to, resets_at).await {
                Ok(()) => Json(serde_json::json!({ "profile": to })).into_response(),
                Err(e) => api_error(StatusCode::BAD_REQUEST, e),
            }
        }
        "wait" => {
            let until = resets_at.max(chrono::Utc::now().timestamp() + 60);
            record_notice(
                &d,
                id,
                HarnessEvent::LimitWait {
                    profile: current.clone(),
                    resets_at: until,
                },
            )
            .await;
            let d2 = d.clone();
            let session = id.to_string();
            tokio::spawn(async move {
                let wait = (until - chrono::Utc::now().timestamp()).max(0) as u64;
                tokio::time::sleep(std::time::Duration::from_secs(wait + 5)).await;
                let live = d2
                    .sessions
                    .lock()
                    .await
                    .get(&session)
                    .is_some_and(|s| !s.join.is_finished());
                if !live
                    && d2
                        .run_session_on_profile(&session, &current, false)
                        .await
                        .is_err()
                {
                    return;
                }
                send_input(
                    &d2,
                    &session,
                    crate::envelope::Envelope::new("routing")
                        .attr("kind", "window_reset")
                        .attr("from", &current)
                        .child("note", format!("{current}'s usage window has reset. The last turn stopped unfinished."))
                        .render(),
                )
                .await;
            });
            Json(serde_json::json!({ "resets_at": until })).into_response()
        }
        _ => api_error(StatusCode::BAD_REQUEST, "action must be switch or wait"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> Shared {
        std::sync::Arc::new(Daemon {
            config: std::sync::Mutex::new(crate::config::SnippetConfig::default()),
            config_path: std::path::PathBuf::new(),
            token: String::new(),
            hostname: "test".into(),
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            shells: crate::term::SessionTerms::new(std::env::temp_dir()),
            git_write: tokio::sync::Mutex::new(()),
            seen_nonces: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_hidden: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_revision: std::sync::atomic::AtomicU64::new(0),
            lead_root: tempfile::tempdir().expect("tempdir").keep(),
            store: crate::store::Store::open_in_memory().unwrap(),
            coordination_events: tokio::sync::broadcast::channel(16).0,
            recurring_root: tempfile::tempdir().expect("tempdir").keep(),
            canvas: std::sync::Arc::new(crate::canvas::CanvasService::with_stub_renderer(
                tempfile::tempdir().expect("tempdir").keep(),
            )),
        })
    }

    fn agent(id: &str) -> crate::coordination::types::Agent {
        use crate::coordination::types::{Agent, AgentKind, AgentStatus};
        Agent {
            id: id.into(),
            display_name: id.into(),
            handle: id.into(),
            kind: AgentKind::Worker,
            status: AgentStatus::Active,
            capabilities: vec![],
        }
    }

    fn config_with_inbox_route(names: &[&str]) -> crate::config::SnippetConfig {
        let mut cfg = crate::config::SnippetConfig::default();
        let mut setups = std::collections::HashMap::new();
        for name in names {
            let profile: crate::config::InferenceProfileConfig =
                serde_json::from_value(serde_json::json!({"api_key": "k", "model": "m"})).unwrap();
            setups.insert(name.to_string(), profile);
        }
        cfg.setups = Some(setups.into_iter().collect());
        cfg.routing.inbox = names.iter().map(|n| n.to_string()).collect();
        cfg
    }

    #[test]
    fn a_pinned_desk_keeps_its_profile_when_limited() {
        let d = daemon();
        d.store.create_agent(&agent("pinned-desk")).unwrap();
        let cfg = config_with_inbox_route(&["pin-route-a", "pin-route-b"]);
        let id = "inbox-pinned-desk";
        assert_eq!(pinned_desk_profile(&d, id), None);
        assert_eq!(
            reroute_target(&cfg, id, None, "pin-route-a").as_deref(),
            Some("pin-route-b")
        );
        d.store
            .set_agent_desk_profile("pinned-desk", Some("pin-route-a"))
            .unwrap();
        let pinned = pinned_desk_profile(&d, id);
        assert_eq!(pinned.as_deref(), Some("pin-route-a"));
        assert_eq!(reroute_target(&cfg, id, pinned.as_deref(), "pin-route-a"), None);
        assert_eq!(pinned_desk_profile(&d, "work-session"), None);
    }

    #[tokio::test]
    async fn desk_batches_hold_while_the_pinned_profile_is_limited() {
        let d = daemon();
        d.store.create_agent(&agent("held-desk")).unwrap();
        d.store
            .set_agent_desk_profile("held-desk", Some("held-desk-profile"))
            .unwrap();
        assert!(!is_desk_profile_limited(&d, "held-desk").await);
        routing::mark_limited("held-desk-profile", chrono::Utc::now().timestamp() + 600);
        assert!(is_desk_profile_limited(&d, "held-desk").await);
    }
}
