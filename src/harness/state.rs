use super::*;

pub fn scalar_json(state: &HarnessState) -> Result<String, String> {
    let mut probe = state.clone();
    probe.messages = Vec::new();
    probe.events = Vec::new();
    serde_json::to_string(&probe).map_err(|e| format!("serialize session scalar: {e}"))
}

pub fn scalar_json_in_place(state: &mut HarnessState) -> Result<String, String> {
    let messages = std::mem::take(&mut state.messages);
    let events = std::mem::take(&mut state.events);
    let encoded = serde_json::to_string(&*state);
    state.messages = messages;
    state.events = events;
    encoded.map_err(|e| format!("serialize session scalar: {e}"))
}

/// Rebuild a session from its stored scalar plus the logs loaded from their
/// tables.
pub fn state_from_scalar(
    scalar: &str,
    messages: Vec<HarnessMessage>,
    events: Vec<HarnessEvent>,
) -> Result<HarnessState, String> {
    let mut state: HarnessState =
        serde_json::from_str(scalar).map_err(|e| format!("deserialize session scalar: {e}"))?;
    state.messages = messages;
    state.events = events;
    state.history_rewritten = false;
    Ok(state)
}
