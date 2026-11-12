use std::collections::BTreeMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::store::{Store, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Chat,
    Push,
    Voice,
    TaskRoom,
}

impl Channel {
    pub const ALL: [Channel; 4] = [
        Channel::Chat,
        Channel::Push,
        Channel::Voice,
        Channel::TaskRoom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Chat => "chat",
            Channel::Push => "push",
            Channel::Voice => "voice",
            Channel::TaskRoom => "task_room",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "chat" => Some(Channel::Chat),
            "push" => Some(Channel::Push),
            "voice" => Some(Channel::Voice),
            "task_room" => Some(Channel::TaskRoom),
            _ => None,
        }
    }

    pub fn always_enabled(self) -> bool {
        matches!(self, Channel::Chat | Channel::TaskRoom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Question,
    Blocker,
    Result,
    Status,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Question, Kind::Blocker, Kind::Result, Kind::Status];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Question => "question",
            Kind::Blocker => "blocker",
            Kind::Result => "result",
            Kind::Status => "status",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "question" => Some(Kind::Question),
            "blocker" => Some(Kind::Blocker),
            "result" => Some(Kind::Result),
            "status" => Some(Kind::Status),
            _ => None,
        }
    }

    pub fn from_ping_kind(raw: &str) -> Self {
        match raw.trim() {
            "decision" | "question" => Kind::Question,
            "blocked" | "blocker" | "urgent" => Kind::Blocker,
            "done" | "result" => Kind::Result,
            _ => Kind::Status,
        }
    }

    pub fn ping_kind(self, urgent: bool) -> &'static str {
        if urgent {
            return "urgent";
        }
        match self {
            Kind::Question => "decision",
            Kind::Blocker => "blocked",
            Kind::Result => "done",
            Kind::Status => "update",
        }
    }

    pub fn from_reply(text: &str, payload: Option<&Value>, awaiting_reply: bool) -> Self {
        match payload.and_then(|p| p.get("kind")).and_then(Value::as_str) {
            Some("blocker") => Kind::Blocker,
            Some("question") => Kind::Question,
            Some("status") => Kind::Status,
            Some("result") => Kind::Result,
            _ if awaiting_reply || ends_with_question(text) => Kind::Question,
            _ => Kind::Result,
        }
    }
}

fn ends_with_question(text: &str) -> bool {
    let trimmed = text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ']' | '*')
    });
    trimmed.ends_with('?') || trimmed.ends_with('？')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelState {
    pub enabled: bool,
    pub quiet_hours: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reach {
    pub policy: BTreeMap<Kind, Vec<Channel>>,
    pub channels: BTreeMap<Channel, ChannelState>,
}

impl Reach {
    pub fn default_for(agent_id: &str) -> Self {
        let mut channels = BTreeMap::new();
        channels.insert(
            Channel::Chat,
            ChannelState {
                enabled: true,
                quiet_hours: false,
            },
        );
        channels.insert(
            Channel::Push,
            ChannelState {
                enabled: true,
                quiet_hours: true,
            },
        );
        channels.insert(
            Channel::Voice,
            ChannelState {
                enabled: agent_id == crate::mission_control::SESSION_ID,
                quiet_hours: true,
            },
        );
        channels.insert(
            Channel::TaskRoom,
            ChannelState {
                enabled: true,
                quiet_hours: false,
            },
        );

        let mut policy = BTreeMap::new();
        policy.insert(Kind::Question, vec![Channel::Push, Channel::Chat]);
        policy.insert(Kind::Blocker, vec![Channel::Push, Channel::Chat]);
        policy.insert(Kind::Result, vec![Channel::Chat]);
        policy.insert(Kind::Status, vec![Channel::TaskRoom]);

        Self { policy, channels }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub used: Vec<Channel>,
    pub held: Vec<Channel>,
    pub reason: &'static str,
}

fn dedup_channels<I: IntoIterator<Item = Channel>>(items: I) -> Vec<Channel> {
    let mut out = Vec::new();
    for ch in items {
        if !out.contains(&ch) {
            out.push(ch);
        }
    }
    out
}

pub fn load(store: &Store, agent_id: &str) -> Reach {
    let mut reach = Reach::default_for(agent_id);
    let _ = store.with_connection(|conn| {
        {
            let mut stmt = conn.prepare(
                "SELECT channel, enabled, quiet_hours FROM agent_channels WHERE agent_id = ?1",
            )?;
            let rows = stmt.query_map(params![agent_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (raw_channel, enabled, quiet_hours) = row?;
                if let Some(ch) = Channel::parse(&raw_channel) {
                    reach.channels.insert(
                        ch,
                        ChannelState {
                            enabled: ch.always_enabled() || enabled != 0,
                            quiet_hours: quiet_hours != 0,
                        },
                    );
                }
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT kind, channels FROM agent_reach_policy WHERE agent_id = ?1",
            )?;
            let rows = stmt.query_map(params![agent_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (raw_kind, raw_channels) = row?;
                if let Some(kind) = Kind::parse(&raw_kind) {
                    let parsed = dedup_channels(
                        raw_channels
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .filter_map(Channel::parse),
                    );
                    reach.policy.insert(kind, parsed);
                }
            }
        }
        Ok(())
    });
    reach
}

pub fn save(store: &Store, agent_id: &str, reach: &Reach) -> Result<(), StoreError> {
    store.with_connection(|conn| {
        let tx = conn.unchecked_transaction()?;
        for (&ch, state) in &reach.channels {
            let enabled = ch.always_enabled() || state.enabled;
            tx.execute(
                "INSERT INTO agent_channels (agent_id, channel, enabled, quiet_hours)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent_id, channel) DO UPDATE SET
                     enabled = excluded.enabled,
                     quiet_hours = excluded.quiet_hours",
                params![agent_id, ch.as_str(), i64::from(enabled), i64::from(state.quiet_hours)],
            )?;
        }
        for (&kind, channels) in &reach.policy {
            let deduped = dedup_channels(channels.iter().copied());
            let joined = deduped
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(",");
            tx.execute(
                "INSERT INTO agent_reach_policy (agent_id, kind, channels)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(agent_id, kind) DO UPDATE SET channels = excluded.channels",
                params![agent_id, kind.as_str(), joined],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

pub fn decide(reach: &Reach, kind: Kind, quiet: bool, on_call: bool, urgent: bool) -> Decision {
    let mut used = Vec::new();
    let mut held = Vec::new();
    let mut reason: Option<&'static str> = None;

    if let Some(ordered) = reach.policy.get(&kind) {
        for &channel in ordered {
            if used.contains(&channel) || held.contains(&channel) {
                continue;
            }
            let fallback = ChannelState {
                enabled: channel != Channel::Voice,
                quiet_hours: matches!(channel, Channel::Push | Channel::Voice),
            };
            let mut state = reach.channels.get(&channel).copied().unwrap_or(fallback);
            if channel.always_enabled() {
                state.enabled = true;
            }
            if !state.enabled {
                reason.get_or_insert("disabled");
                continue;
            }
            if state.quiet_hours && !urgent && (quiet || on_call) {
                reason.get_or_insert(if quiet { "quiet hours" } else { "on call" });
                held.push(channel);
                continue;
            }
            used.push(channel);
        }
    }

    if used.is_empty() {
        held.retain(|&c| c != Channel::Chat);
        used.push(Channel::Chat);
    }

    Decision {
        used,
        held,
        reason: reason.unwrap_or("policy"),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn reach(
    store: &Store,
    agent_id: &str,
    kind: Kind,
    title: &str,
    body: &str,
    urgent: bool,
    origin_session: Option<&str>,
    about_task: Option<&str>,
) -> Decision {
    reach_with_key(
        store,
        agent_id,
        kind,
        title,
        body,
        urgent,
        origin_session,
        about_task,
        None,
        Map::new(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn reach_with_key(
    store: &Store,
    agent_id: &str,
    kind: Kind,
    title: &str,
    body: &str,
    urgent: bool,
    origin_session: Option<&str>,
    about_task: Option<&str>,
    idempotency_key: Option<&str>,
    payload_extra: Map<String, Value>,
) -> Decision {
    let cfg = load(store, agent_id);
    let (quiet, on_call) = crate::mission_autonomy::quiet_and_call_in(store);
    let decision = decide(&cfg, kind, quiet, on_call, urgent);
    let ping_kind = kind.ping_kind(urgent);

    if decision.held.contains(&Channel::Push) {
        let _ = crate::mission_autonomy::hold_ping_in(store, agent_id, title, body, ping_kind);
    }

    for &channel in &decision.used {
        match channel {
            Channel::Chat => {
                let mut extra = payload_extra.clone();
                extra.insert("channel".into(), json!("chat"));
                extra.insert("kind".into(), json!(kind.as_str()));
                if urgent {
                    extra.insert("urgency".into(), json!("urgent"));
                }
                if !decision.held.is_empty() {
                    extra.insert(
                        "held".into(),
                        json!(decision.held.iter().map(|c| c.as_str()).collect::<Vec<_>>()),
                    );
                }
                let generated;
                let key = match idempotency_key {
                    Some(k) => k,
                    None => {
                        generated = uuid::Uuid::new_v4().to_string();
                        &generated
                    }
                };
                let session = origin_session.unwrap_or(agent_id);
                crate::serve::post_to_human(
                    store,
                    agent_id,
                    session,
                    origin_session,
                    about_task,
                    key,
                    body,
                    extra,
                );
            }
            Channel::Push => {
                let _ = crate::mission_autonomy::send_ping_in(
                    store, agent_id, title, body, ping_kind, urgent,
                );
            }
            Channel::TaskRoom | Channel::Voice => {}
        }
    }

    decision
}

pub(crate) fn apply_outbound_human(
    store: &Store,
    agent_id: &str,
    body: &str,
    extra: &mut Map<String, Value>,
) -> Decision {
    extra.insert("channel".into(), json!("chat"));
    let payload = Value::Object(extra.clone());
    let awaiting = matches!(
        payload.get("kind").and_then(Value::as_str),
        Some("question" | "blocker")
    );
    let kind = Kind::from_reply(body, Some(&payload), awaiting);
    let urgent = crate::coordination::typed::urgency_of(&payload)
        == crate::coordination::typed::Urgency::Urgent;
    let cfg = load(store, agent_id);
    let (quiet, on_call) = crate::mission_autonomy::quiet_and_call_in(store);
    let decision = decide(&cfg, kind, quiet, on_call, urgent);
    let title = store
        .get_agent(agent_id)
        .ok()
        .flatten()
        .map(|a| a.display_name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| agent_id.to_string());
    let ping_kind = kind.ping_kind(urgent);

    if !decision.held.is_empty() {
        extra.insert(
            "held".into(),
            json!(decision.held.iter().map(|c| c.as_str()).collect::<Vec<_>>()),
        );
    }
    if decision.held.contains(&Channel::Push) {
        let _ = crate::mission_autonomy::hold_ping_in(store, agent_id, &title, body, ping_kind);
    }
    if decision.used.contains(&Channel::Push) {
        let _ = crate::mission_autonomy::send_ping_in(
            store, agent_id, &title, body, ping_kind, urgent,
        );
    }
    decision
}

#[allow(clippy::too_many_arguments)]
pub fn deliver_channel(
    store: &Store,
    sender: (&str, &str),
    raw_channel: &str,
    body: &str,
    origin_session: Option<&str>,
    origin_lane: Option<&str>,
    about_task: Option<&str>,
    mut extra: Map<String, Value>,
) -> Result<crate::coordination::types::CoordinationEvent, String> {
    let channel = Channel::parse(raw_channel).ok_or_else(|| {
        format!(
            "unsupported channel `{raw_channel}`; supported targets are `human`, an agent id, `channel:chat`, or `channel:push`"
        )
    })?;
    match channel {
        Channel::Chat | Channel::Push => {}
        Channel::Voice | Channel::TaskRoom => {
            return Err(format!(
                "channel `{}` has no direct delivery path; supported targets are `human`, an agent id, `channel:chat`, or `channel:push`",
                channel.as_str()
            ));
        }
    }
    let cfg = load(store, sender.1);
    if channel == Channel::Push {
        let enabled = cfg
            .channels
            .get(&Channel::Push)
            .map(|s| s.enabled)
            .unwrap_or(true);
        if !enabled {
            return Err(
                "channel `push` is disabled in reach policy; supported active targets are `human`, an agent id, or `channel:chat`".into(),
            );
        }
        let quiet_gated = cfg
            .channels
            .get(&Channel::Push)
            .map(|s| s.quiet_hours)
            .unwrap_or(true);
        let (quiet, on_call) = crate::mission_autonomy::quiet_and_call_in(store);
        let title = store
            .get_agent(sender.1)
            .ok()
            .flatten()
            .map(|a| a.display_name)
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| sender.1.to_string());
        if quiet_gated && (quiet || on_call) {
            let _ = crate::mission_autonomy::hold_ping_in(store, sender.1, &title, body, "result");
            extra.insert("held".into(), json!(["push"]));
        } else {
            let _ = crate::mission_autonomy::send_ping_in(store, sender.1, &title, body, "result", false);
        }
    }
    extra.insert("channel".into(), json!(channel.as_str()));
    extra.entry("kind".to_string()).or_insert_with(|| json!("result"));
    let key = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let saved = store
        .send_direct_message_typed(
            sender,
            ("human", crate::coordination::LOCAL_HUMAN_ID),
            body,
            &key,
            &now,
            origin_session,
            origin_lane,
            about_task,
            extra,
        )
        .map_err(|e| format!("send channel message: {e}"))?;
    crate::session::emit_device_event(json!({
        "kind": "coordination_event",
        "event": saved.clone(),
    }));
    Ok(saved)
}

pub fn view(store: &Store, agent_id: &str) -> Value {
    let reach = load(store, agent_id);
    let (quiet, on_call) = crate::mission_autonomy::quiet_and_call_in(store);
    json!({
        "agent_id": agent_id,
        "channels": reach.channels,
        "policy": reach.policy,
        "in_quiet_hours": quiet,
        "on_call": on_call,
    })
}

pub fn apply_patch(reach: &mut Reach, patch: &Value) -> Result<(), String> {
    let obj = patch
        .as_object()
        .ok_or_else(|| "reach policy body must be a JSON object".to_string())?;

    let mut changed = false;

    if let Some(policy_val) = obj.get("policy").filter(|v| !v.is_null()) {
        let policy_map = policy_val
            .as_object()
            .ok_or_else(|| "`policy` must be an object mapping kind to channel array".to_string())?;
        for (raw_kind, raw_list) in policy_map {
            let kind = Kind::parse(raw_kind).ok_or_else(|| {
                format!("unknown kind `{raw_kind}`; use question, blocker, result, or status")
            })?;
            let list = parse_channel_array(raw_list)?;
            reach.policy.insert(kind, list);
            changed = true;
        }
    }

    if let Some(channels_val) = obj.get("channels").filter(|v| !v.is_null()) {
        if let Some(chan_map) = channels_val.as_object() {
            for (raw_ch, raw_state) in chan_map {
                let ch = Channel::parse(raw_ch).ok_or_else(|| {
                    format!("unknown channel `{raw_ch}`; use chat, push, voice, or task_room")
                })?;
                let state_obj = raw_state
                    .as_object()
                    .ok_or_else(|| format!("channel `{raw_ch}` state must be an object"))?;
                let current = reach.channels.get(&ch).copied().unwrap_or(ChannelState {
                    enabled: ch != Channel::Voice,
                    quiet_hours: matches!(ch, Channel::Push | Channel::Voice),
                });
                let enabled = match state_obj.get("enabled").and_then(Value::as_bool) {
                    Some(false) if ch.always_enabled() => {
                        return Err(format!("`{}` cannot be disabled", ch.as_str()));
                    }
                    Some(v) => v,
                    None => current.enabled,
                };
                let quiet_hours = state_obj
                    .get("quiet_hours")
                    .and_then(Value::as_bool)
                    .unwrap_or(current.quiet_hours);
                reach.channels.insert(ch, ChannelState { enabled, quiet_hours });
                changed = true;
            }
        } else if channels_val.is_array() {
            let raw_kind = obj
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| "`kind` is required when `channels` is an array".to_string())?;
            let kind = Kind::parse(raw_kind).ok_or_else(|| {
                format!("unknown kind `{raw_kind}`; use question, blocker, result, or status")
            })?;
            let list = parse_channel_array(channels_val)?;
            reach.policy.insert(kind, list);
            changed = true;
        } else {
            return Err("`channels` must be an array (with `kind`) or an object".into());
        }
    } else if let Some(raw_kind) = obj.get("kind").and_then(Value::as_str) {
        let _ = Kind::parse(raw_kind).ok_or_else(|| {
            format!("unknown kind `{raw_kind}`; use question, blocker, result, or status")
        })?;
        return Err("`channels` array is required when `kind` is set".into());
    }

    let enabled_opt = match obj.get("enabled") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_bool().ok_or_else(|| "`enabled` must be a boolean".to_string())?),
    };
    let quiet_opt = match obj.get("quiet_hours") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| "`quiet_hours` must be a boolean".to_string())?,
        ),
    };

    if let Some(raw_ch) = obj.get("channel").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        let ch = Channel::parse(raw_ch).ok_or_else(|| {
            format!("unknown channel `{raw_ch}`; use chat, push, voice, or task_room")
        })?;
        if enabled_opt.is_none() && quiet_opt.is_none() {
            return Err("`channel` requires `enabled` and/or `quiet_hours`".into());
        }
        if enabled_opt == Some(false) && ch.always_enabled() {
            return Err(format!("`{}` cannot be disabled", ch.as_str()));
        }
        let current = reach.channels.get(&ch).copied().unwrap_or(ChannelState {
            enabled: ch != Channel::Voice,
            quiet_hours: matches!(ch, Channel::Push | Channel::Voice),
        });
        reach.channels.insert(
            ch,
            ChannelState {
                enabled: enabled_opt.unwrap_or(current.enabled),
                quiet_hours: quiet_opt.unwrap_or(current.quiet_hours),
            },
        );
        changed = true;
    } else {
        if enabled_opt.is_some() {
            return Err("`channel` is required when setting `enabled`".into());
        }
        if let Some(qh) = quiet_opt {
            for ch in [Channel::Push, Channel::Voice] {
                if let Some(state) = reach.channels.get_mut(&ch) {
                    state.quiet_hours = qh;
                }
            }
            changed = true;
        }
    }

    if !changed {
        return Err("set requires `kind` + `channels`, `channel` + `enabled`/`quiet_hours`, or `policy`/`channels` maps".into());
    }

    Ok(())
}

fn parse_channel_array(raw: &Value) -> Result<Vec<Channel>, String> {
    let arr = raw
        .as_array()
        .ok_or_else(|| "`channels` must be an array of channel names".to_string())?;
    let mut parsed = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| "channel names must be strings".to_string())?;
        let ch = Channel::parse(s).ok_or_else(|| {
            format!("unknown channel `{s}`; use chat, push, voice, or task_room")
        })?;
        if !parsed.contains(&ch) {
            parsed.push(ch);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::types::{Agent, AgentKind, AgentStatus};

    #[test]
    fn decide_table_across_kinds_quiet_call_urgent_and_push_disabled() {
        struct Case {
            kind: Kind,
            quiet: bool,
            on_call: bool,
            urgent: bool,
            push_disabled: bool,
            used: &'static [Channel],
            held: &'static [Channel],
            reason: &'static str,
        }

        let mut cases = Vec::new();
        for &kind in &Kind::ALL {
            for &push_disabled in &[false, true] {
                for &quiet in &[false, true] {
                    for &on_call in &[false, true] {
                        for &urgent in &[false, true] {
                            let (used, held, reason): (&[Channel], &[Channel], &str) = match kind {
                                Kind::Question | Kind::Blocker => {
                                    if push_disabled {
                                        (&[Channel::Chat], &[], "disabled")
                                    } else if !urgent && (quiet || on_call) {
                                        let why = if quiet { "quiet hours" } else { "on call" };
                                        (&[Channel::Chat], &[Channel::Push], why)
                                    } else {
                                        (&[Channel::Push, Channel::Chat], &[], "policy")
                                    }
                                }
                                Kind::Result => (&[Channel::Chat], &[], "policy"),
                                Kind::Status => (&[Channel::TaskRoom], &[], "policy"),
                            };
                            cases.push(Case {
                                kind,
                                quiet,
                                on_call,
                                urgent,
                                push_disabled,
                                used,
                                held,
                                reason,
                            });
                        }
                    }
                }
            }
        }

        for c in cases {
            let mut r = Reach::default_for("scout");
            if c.push_disabled {
                r.channels.get_mut(&Channel::Push).unwrap().enabled = false;
            }
            let got = decide(&r, c.kind, c.quiet, c.on_call, c.urgent);
            assert_eq!(
                got.used.as_slice(),
                c.used,
                "used mismatch for {:?} quiet={} on_call={} urgent={} push_disabled={}",
                c.kind,
                c.quiet,
                c.on_call,
                c.urgent,
                c.push_disabled
            );
            assert_eq!(
                got.held.as_slice(),
                c.held,
                "held mismatch for {:?} quiet={} on_call={} urgent={} push_disabled={}",
                c.kind,
                c.quiet,
                c.on_call,
                c.urgent,
                c.push_disabled
            );
            assert_eq!(
                got.reason, c.reason,
                "reason mismatch for {:?} quiet={} on_call={} urgent={} push_disabled={}",
                c.kind, c.quiet, c.on_call, c.urgent, c.push_disabled
            );
            if c.kind == Kind::Result {
                assert!(!got.used.contains(&Channel::Push), "result never pings");
            }
        }

        // Last-resort chat fallback when policy omits chat and push is held or disabled.
        let mut only_push = Reach::default_for("scout");
        only_push.policy.insert(Kind::Question, vec![Channel::Push]);
        let held_fallback = decide(&only_push, Kind::Question, true, false, false);
        assert_eq!(held_fallback.used, vec![Channel::Chat]);
        assert_eq!(held_fallback.held, vec![Channel::Push]);
        assert_eq!(held_fallback.reason, "quiet hours");

        only_push.channels.get_mut(&Channel::Push).unwrap().enabled = false;
        let disabled_fallback = decide(&only_push, Kind::Question, false, false, false);
        assert_eq!(disabled_fallback.used, vec![Channel::Chat]);
        assert!(disabled_fallback.held.is_empty());
        assert_eq!(disabled_fallback.reason, "disabled");
    }

    #[test]
    fn store_save_and_load_round_trips_and_defaults_when_empty() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(load(&store, "scout"), Reach::default_for("scout"));
        assert!(!Reach::default_for("scout").channels[&Channel::Voice].enabled);
        assert!(
            Reach::default_for(crate::mission_control::SESSION_ID).channels[&Channel::Voice]
                .enabled
        );
        assert_eq!(
            load(&store, crate::mission_control::SESSION_ID),
            Reach::default_for(crate::mission_control::SESSION_ID)
        );

        let mut custom = Reach::default_for("scout");
        custom.channels.insert(
            Channel::Push,
            ChannelState {
                enabled: false,
                quiet_hours: false,
            },
        );
        custom
            .policy
            .insert(Kind::Question, vec![Channel::Chat]);
        custom
            .policy
            .insert(Kind::Result, vec![Channel::Push, Channel::Chat]);

        save(&store, "scout", &custom).unwrap();
        assert_eq!(load(&store, "scout"), custom);

        // Chat and TaskRoom remain enabled even if a caller constructed false in memory.
        let mut forced = custom.clone();
        forced.channels.get_mut(&Channel::Chat).unwrap().enabled = false;
        forced.channels.get_mut(&Channel::TaskRoom).unwrap().enabled = false;
        save(&store, "scout", &forced).unwrap();
        let reloaded = load(&store, "scout");
        assert!(reloaded.channels[&Channel::Chat].enabled);
        assert!(reloaded.channels[&Channel::TaskRoom].enabled);

        // Deleting the agent cleans up its reach tables.
        store
            .create_agent(&Agent {
                id: "scout".into(),
                display_name: "Scout".into(),
                handle: "scout".into(),
                kind: AgentKind::Worker,
                status: AgentStatus::Active,
                capabilities: vec![],
            })
            .unwrap();
        save(&store, "scout", &custom).unwrap();
        store.delete_agent("scout").unwrap();
        assert_eq!(load(&store, "scout"), Reach::default_for("scout"));
    }
}
