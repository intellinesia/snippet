//! Typed agent-to-agent messages: what a message is (`kind`) and how fast the
//! recipient must see it (`urgency`). Both ride in the event payload, so they
//! need no schema of their own and old events without them read as plain,
//! normal-urgency messages.

use serde_json::{Map, Value};

pub const KINDS: &[&str] = &[
    "note",
    "question",
    "status",
    "handoff_note",
    "blocker",
    "result",
    "interrupt",
];
pub const REACTIONS: &[&str] = &["check", "eyes", "thumbs_up", "question", "thinking"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Normal,
    Urgent,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Normal => "normal",
            Urgency::Urgent => "urgent",
        }
    }
}

/// Validated kind and urgency from optional tool arguments. An `interrupt`
/// is urgent by definition; a blocker is only urgent when the sender says so.
pub fn parse(
    kind: Option<&str>,
    urgency: Option<&str>,
) -> Result<(Option<&'static str>, Urgency), String> {
    let kind = match kind.map(str::trim).filter(|k| !k.is_empty()) {
        None => None,
        Some(k) => Some(
            KINDS
                .iter()
                .find(|known| **known == k)
                .copied()
                .ok_or_else(|| format!("unknown kind `{k}`; use one of {}", KINDS.join(", ")))?,
        ),
    };
    let urgency = match urgency.map(str::trim).filter(|u| !u.is_empty()) {
        None | Some("normal") => Urgency::Normal,
        Some("urgent") => Urgency::Urgent,
        Some(other) => return Err(format!("unknown urgency `{other}`; use normal or urgent")),
    };
    let urgency = if kind == Some("interrupt") {
        Urgency::Urgent
    } else {
        urgency
    };
    Ok((kind, urgency))
}

/// One emoji, ZWJ sequences and skin tones included; no ASCII text.
fn is_emoji(r: &str) -> bool {
    let n = r.chars().count();
    (1..=10).contains(&n) && r.chars().all(|c| !c.is_ascii() && !c.is_whitespace())
}

pub fn parse_reply_and_reaction(
    reply_to: Option<&str>,
    reaction: Option<&str>,
) -> Result<(Option<String>, Option<String>), String> {
    let reply_to = reply_to
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let reaction = match reaction.map(str::trim).filter(|r| !r.is_empty()) {
        None => None,
        Some(r) => {
            if !REACTIONS.contains(&r) && !is_emoji(r) {
                return Err(format!(
                    "unknown reaction `{r}`; use one of {} or a single emoji",
                    REACTIONS.join(", ")
                ));
            }
            let valid = r.to_string();
            if reply_to.is_none() {
                return Err("reaction requires reply_to".into());
            }
            Some(valid)
        }
    };
    Ok((reply_to, reaction))
}

pub fn apply_reply_and_reaction(
    fields: &mut Map<String, Value>,
    reply_to: Option<&str>,
    reaction: Option<&str>,
) {
    if let Some(id) = reply_to.map(str::trim).filter(|id| !id.is_empty()) {
        fields.insert("reply_to".into(), Value::from(id));
    }
    if let Some(name) = reaction.map(str::trim).filter(|r| !r.is_empty()) {
        fields.insert("reaction".into(), Value::from(name));
        fields.insert("kind".into(), Value::from("reaction"));
        fields.remove("urgency");
    }
}

/// Payload fields for a typed message; empty for an untyped normal one.
pub fn payload_fields(
    kind: Option<&str>,
    urgency: Urgency,
    task_id: Option<&str>,
) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(kind) = kind {
        map.insert("kind".into(), Value::from(kind));
    }
    if urgency == Urgency::Urgent {
        map.insert("urgency".into(), Value::from("urgent"));
    }
    if let Some(task) = task_id.filter(|t| !t.trim().is_empty()) {
        map.insert("task_id".into(), Value::from(task));
    }
    map
}

pub fn urgency_of(payload: &Value) -> Urgency {
    if payload.get("reaction").is_some()
        || payload.get("kind").and_then(Value::as_str) == Some("reaction")
    {
        return Urgency::Normal;
    }
    match payload.get("urgency").and_then(Value::as_str) {
        Some("urgent") => Urgency::Urgent,
        _ => Urgency::Normal,
    }
}

/// Envelope lines for a typed message, empty when it is untyped and normal.
pub fn envelope_lines(payload: &Value) -> String {
    let mut out = String::new();
    if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
        out.push_str(&format!("kind: {kind}\n"));
    }
    if let Some(to) = payload.get("to").and_then(Value::as_array) {
        let names: Vec<&str> = to.iter().filter_map(Value::as_str).collect();
        out.push_str(&format!("addressed_to: {}\n", names.join(", ")));
    }
    if urgency_of(payload) == Urgency::Urgent {
        out.push_str(
            "urgency: urgent (the sender interrupted your run for this; deal with it first)\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_is_always_urgent() {
        assert_eq!(
            parse(Some("interrupt"), None).unwrap(),
            (Some("interrupt"), Urgency::Urgent)
        );
    }

    #[test]
    fn untyped_is_normal_and_bad_values_are_refused() {
        assert_eq!(parse(None, None).unwrap(), (None, Urgency::Normal));
        assert!(parse(Some("shout"), None).is_err());
        assert!(parse(None, Some("asap")).is_err());
    }

    #[test]
    fn payload_round_trips() {
        let fields = payload_fields(Some("blocker"), Urgency::Urgent, Some("t1"));
        let payload = Value::Object(fields);
        assert_eq!(urgency_of(&payload), Urgency::Urgent);
        assert!(envelope_lines(&payload).contains("kind: blocker"));
        assert_eq!(envelope_lines(&serde_json::json!({"body": "x"})), "");
    }

    #[test]
    fn reaction_requires_reply_to_and_stays_normal_urgency() {
        assert!(parse_reply_and_reaction(None, Some("check")).is_err());
        assert!(parse_reply_and_reaction(Some("e1"), Some("confetti")).is_err());
        let (reply_to, reaction) =
            parse_reply_and_reaction(Some(" e1 "), Some("thumbs_up")).unwrap();
        assert_eq!(reply_to.as_deref(), Some("e1"));
        assert_eq!(reaction.as_deref(), Some("thumbs_up"));
        let (_, emoji) = parse_reply_and_reaction(Some("e1"), Some("👍🏽")).unwrap();
        assert_eq!(emoji.as_deref(), Some("👍🏽"));
        assert!(parse_reply_and_reaction(Some("e1"), Some("👍 nice")).is_err());

        let mut fields = payload_fields(Some("interrupt"), Urgency::Urgent, None);
        apply_reply_and_reaction(&mut fields, reply_to.as_deref(), reaction.as_deref());
        let payload = Value::Object(fields);
        assert_eq!(payload["kind"], "reaction");
        assert_eq!(payload["reaction"], "thumbs_up");
        assert_eq!(payload["reply_to"], "e1");
        assert_eq!(urgency_of(&payload), Urgency::Normal);
    }
}
