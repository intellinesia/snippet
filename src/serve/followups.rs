//! Delivers due `schedule_wake` rows as a `<wake>` envelope to the session that
//! set them, opening an idle desk or work session first.

use std::time::Duration;

use serde_json::json;

use super::Shared;
use crate::coordination::followups::Followup;

const TICK: Duration = Duration::from_secs(5);

pub fn wake_envelope(f: &Followup) -> String {
    let tool = if crate::mission_control::is_session_id(&f.session_id) {
        "schedule.once"
    } else if crate::session::is_inbox_session_id(&f.session_id) {
        "wake.once"
    } else {
        "snippet tool schedule_wake"
    };
    crate::envelope::Envelope::new("wake")
        .attr("set", &f.created_at)
        .child(
            "note",
            format!("You asked to be woken now (set {}).", f.created_at),
        )
        .child("wake_note", &f.note)
        .child(
            "instructions",
            format!("Do what the note says; {tool} again if it still needs waiting."),
        )
        .render()
}

async fn deliver(d: &Shared, f: &Followup) -> Result<(), String> {
    if crate::session::is_inbox_session_id(&f.session_id)
        && let Some(agent) = f.owner.as_deref()
    {
        super::direct::ensure_agent_inbox(d, agent).await?;
    }
    let Some((tx, _path, _stream)) = d.ensure_live(&f.session_id).await else {
        return Err(format!("session `{}` could not be opened", f.session_id));
    };
    drop(tx);
    super::direct::deliver_by_urgency(d, &f.session_id, wake_envelope(f), &json!({}), false).await;
    Ok(())
}

pub async fn followup_loop(d: Shared) {
    let mut tick = tokio::time::interval(TICK);
    loop {
        tick.tick().await;
        let now = crate::coordination::watches::now_rfc3339();
        let due = match d.store.due_followups(&now, 20) {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "followups: due query failed");
                continue;
            }
        };
        for f in due {
            let error = deliver(&d, &f).await.err();
            if let Some(error) = &error {
                tracing::warn!(id = %f.id, session = %f.session_id, %error, "followup dropped");
            }
            let at = crate::coordination::watches::now_rfc3339();
            if let Err(e) = d
                .store
                .mark_followup_delivered(&f.id, &at, error.as_deref())
            {
                tracing::warn!(id = %f.id, error = %e, "followups: mark delivered failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_carries_the_note() {
        let f = Followup {
            id: "f".into(),
            session_id: "inbox-a".into(),
            owner: Some("a".into()),
            due_at: "2026-11-14T10:30:00Z".into(),
            note: "check CI on #150".into(),
            created_at: "2026-11-14T10:00:00Z".into(),
        };
        let e = wake_envelope(&f);
        assert!(crate::envelope::is(&e, "wake"));
        assert_eq!(
            crate::envelope::child(&e, "wake_note").as_deref(),
            Some("check CI on #150")
        );
    }
}
