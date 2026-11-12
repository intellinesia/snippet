use serde_json::{Value, json};

use super::*;

const DETAIL_MAX: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPresence {
    pub state: String,
    pub detail: String,
    pub since: i64,
    pub agent: Option<String>,
}

fn live_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, SessionPresence>> {
    static LIVE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, SessionPresence>>,
    > = std::sync::OnceLock::new();
    LIVE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub fn record_session_presence(
    session: &str,
    agent: Option<String>,
    state: &str,
    detail: &str,
    since: i64,
) {
    let detail: String = detail.trim().chars().take(DETAIL_MAX).collect();
    if let Ok(mut map) = live_map().lock() {
        map.insert(
            session.to_string(),
            SessionPresence {
                state: state.to_string(),
                detail,
                since,
                agent,
            },
        );
    }
}

pub fn session_presence(session: &str) -> Option<SessionPresence> {
    live_map().lock().ok()?.get(session).cloned()
}

pub fn snapshot_presence() -> std::collections::HashMap<String, SessionPresence> {
    live_map().lock().map(|m| m.clone()).unwrap_or_default()
}

#[cfg(test)]
pub fn clear_presence_for_test() {
    if let Ok(mut map) = live_map().lock() {
        map.clear();
    }
}

/// What the agent is doing right now, published on `/events` when it changes.
#[derive(Default)]
pub(super) struct Presence {
    state: &'static str,
    detail: String,
    since: i64,
    agent: Option<Option<String>>,
}

impl Presence {
    pub(super) fn transition(
        &mut self,
        session: &str,
        agent: impl FnOnce() -> Option<String>,
        state: &'static str,
        detail: &str,
        now: i64,
    ) -> Option<Value> {
        let detail: String = detail.trim().chars().take(DETAIL_MAX).collect();
        if self.state == state && self.detail == detail {
            return None;
        }
        self.state = state;
        self.detail = detail.clone();
        self.since = now;
        let agent = self.agent.get_or_insert_with(agent).clone();
        record_session_presence(session, agent.clone(), state, &detail, now);
        Some(json!({
            "kind": "agent_presence",
            "session": session,
            "agent": agent,
            "state": state,
            "detail": self.detail,
            "since": now,
        }))
    }

    pub(super) fn set(
        &mut self,
        session: &str,
        agent: impl FnOnce() -> Option<String>,
        state: &'static str,
        detail: &str,
    ) {
        if let Some(frame) = self.transition(session, agent, state, detail, Utc::now().timestamp())
        {
            crate::session::emit_device_event(frame);
        }
    }
}

/// `bash` shows its first command word and label; other tools their activity line.
pub(super) fn tool_detail(name: &str, args: &Value) -> String {
    if name == "bash" {
        let word = args
            .get("command")
            .and_then(Value::as_str)
            .and_then(|c| c.split_whitespace().next())
            .unwrap_or("");
        let label = args
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        return match (word.is_empty(), label.is_empty()) {
            (false, false) => format!("{word} {label}"),
            (false, true) => word.to_string(),
            (true, false) => label.to_string(),
            (true, true) => "bash".to_string(),
        };
    }
    super::dispatch::tool_activity_text(name, args)
}

impl CodingHarness {
    pub(super) fn presence(&self, state: &'static str, detail: &str) {
        if self.config.progress_id.is_some() {
            return;
        }
        let Some(session) = self.session_id() else {
            return;
        };
        let agent = || {
            self.store()?
                .get_session_row(&session)
                .ok()
                .flatten()
                .and_then(|row| row.agent_id)
        };
        if let Ok(mut presence) = self.presence.lock() {
            presence.set(&session, agent, state, detail);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_emit_only_on_change() {
        let mut p = Presence::default();
        let ls = json!({"command": "ls -la", "label": "list"});
        let steps: [(&'static str, String); 6] = [
            ("thinking", String::new()),
            ("thinking", String::new()),
            ("typing", String::new()),
            ("tool", tool_detail("bash", &ls)),
            ("tool", tool_detail("bash", &ls)),
            ("idle", String::new()),
        ];
        let frames: Vec<Value> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, (state, detail))| {
                p.transition("s1", || Some("ag".into()), state, detail, 100 + i as i64)
            })
            .collect();
        let states: Vec<&str> = frames
            .iter()
            .map(|f| f["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, ["thinking", "typing", "tool", "idle"]);
        assert_eq!(frames[2]["detail"], "ls list");
        let since: Vec<i64> = frames
            .iter()
            .map(|f| f["since"].as_i64().unwrap())
            .collect();
        assert_eq!(since, [100, 102, 103, 105]);
        assert_eq!(frames[0]["kind"], "agent_presence");
        assert_eq!(frames[0]["session"], "s1");
        assert_eq!(frames[0]["agent"], "ag");
    }

    #[test]
    fn long_detail_is_trimmed() {
        let mut p = Presence::default();
        let f = p
            .transition("s", || None, "tool", &"x".repeat(200), 1)
            .unwrap();
        assert_eq!(f["detail"].as_str().unwrap().len(), 80);
        assert!(f["agent"].is_null());
    }
}
