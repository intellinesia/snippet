use rusqlite::{OptionalExtension, params};

use crate::store::{Store, StoreError};

pub const OFFER_PREFIX: &str = "lo-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneOffer {
    pub id: String,
    pub session_id: String,
    pub agent_id: String,
    pub title: String,
    pub brief: String,
    pub read_only: bool,
    pub profile: Option<String>,
    pub paths: Vec<String>,
    pub status: String,
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub fn ensure_schema(connection: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS lane_offers (
             id TEXT PRIMARY KEY NOT NULL,
             session_id TEXT NOT NULL,
             agent_id TEXT NOT NULL,
             title TEXT NOT NULL,
             brief TEXT NOT NULL,
             read_only INTEGER NOT NULL DEFAULT 0,
             profile TEXT,
             paths_json TEXT NOT NULL DEFAULT '[]',
             status TEXT NOT NULL DEFAULT 'offered',
             note TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS lane_offers_status ON lane_offers(status);",
    )
}

const COLUMNS: &str = "id, session_id, agent_id, title, brief, read_only, profile, paths_json, status, note, created_at, updated_at";

fn offer_from(row: &rusqlite::Row<'_>) -> Result<LaneOffer, rusqlite::Error> {
    let paths: String = row.get(7)?;
    Ok(LaneOffer {
        id: row.get(0)?,
        session_id: row.get(1)?,
        agent_id: row.get(2)?,
        title: row.get(3)?,
        brief: row.get(4)?,
        read_only: row.get::<_, i64>(5)? != 0,
        profile: row.get(6)?,
        paths: serde_json::from_str(&paths).unwrap_or_default(),
        status: row.get(8)?,
        note: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

impl Store {
    pub fn get_lane_offer(&self, id: &str) -> Result<Option<LaneOffer>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM lane_offers WHERE id = ?1"),
                params![id],
                offer_from,
            )
            .optional()
        })
    }

    pub fn pending_lane_offer(
        &self,
        session_id: &str,
        agent_id: &str,
        title: &str,
    ) -> Result<Option<LaneOffer>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM lane_offers
                     WHERE session_id = ?1 AND agent_id = ?2 AND title = ?3 AND status = 'offered'
                     ORDER BY created_at DESC LIMIT 1"
                ),
                params![session_id, agent_id, title],
                offer_from,
            )
            .optional()
        })
    }

    pub fn recently_claimed_lane_offer(
        &self,
        session_id: &str,
        agent_id: &str,
        title: &str,
        since: &str,
    ) -> Result<Option<LaneOffer>, StoreError> {
        self.with_connection(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM lane_offers
                     WHERE session_id = ?1 AND agent_id = ?2 AND title = ?3
                       AND status IN ('claimed', 'started') AND updated_at >= ?4
                     ORDER BY updated_at DESC LIMIT 1"
                ),
                params![session_id, agent_id, title, since],
                offer_from,
            )
            .optional()
        })
    }

    pub fn pending_lane_offers_for(&self, agent_id: &str) -> Result<Vec<LaneOffer>, StoreError> {
        self.with_connection(|conn| {
            conn.prepare(&format!(
                "SELECT {COLUMNS} FROM lane_offers WHERE agent_id = ?1 AND status = 'offered'
                 ORDER BY created_at"
            ))?
            .query_map(params![agent_id], offer_from)?
            .collect()
        })
    }

    pub fn insert_lane_offer(&self, offer: &LaneOffer) -> Result<(), StoreError> {
        let paths = serde_json::to_string(&offer.paths).unwrap_or_else(|_| "[]".into());
        self.with_connection(|conn| {
            conn.execute(
                "INSERT INTO lane_offers (id, session_id, agent_id, title, brief, read_only, profile,
                    paths_json, status, note, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    offer.id,
                    offer.session_id,
                    offer.agent_id,
                    offer.title,
                    offer.brief,
                    offer.read_only as i64,
                    offer.profile,
                    paths,
                    offer.status,
                    offer.note,
                    offer.created_at,
                    offer.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn merge_lane_offer(&self, id: &str, addition: &str, now: &str) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE lane_offers SET brief = brief || ?2, updated_at = ?3
                 WHERE id = ?1 AND status = 'offered'",
                params![
                    id,
                    format!("\n\nUpdate from the requester:\n{addition}"),
                    now
                ],
            )? == 1)
        })
    }

    pub fn resolve_lane_offer(
        &self,
        id: &str,
        status: &str,
        note: Option<&str>,
        now: &str,
    ) -> Result<bool, StoreError> {
        self.with_connection(|conn| {
            Ok(conn.execute(
                "UPDATE lane_offers SET status = ?2, note = ?3, updated_at = ?4
                 WHERE id = ?1 AND status = 'offered'",
                params![id, status, note, now],
            )? == 1)
        })
    }

    pub fn take_resolved_lane_offers(&self, now: &str) -> Result<Vec<LaneOffer>, StoreError> {
        self.with_connection(|conn| {
            let tx = conn.unchecked_transaction()?;
            let offers: Vec<LaneOffer> = tx
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM lane_offers WHERE status IN ('claimed', 'declined')"
                ))?
                .query_map([], offer_from)?
                .collect::<Result<_, _>>()?;
            for offer in &offers {
                tx.execute(
                    "UPDATE lane_offers SET status = CASE status WHEN 'claimed' THEN 'started' ELSE 'closed' END,
                        updated_at = ?2
                     WHERE id = ?1",
                    params![offer.id, now],
                )?;
            }
            tx.commit()?;
            Ok(offers)
        })
    }
}

pub fn claim_or_decline(
    store: &Store,
    offer_id: &str,
    actor_id: &str,
    claim: bool,
    note: Option<&str>,
) -> Result<String, String> {
    let offer = store
        .get_lane_offer(offer_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("lane offer `{offer_id}` not found"))?;
    if offer.agent_id != actor_id {
        return Err(format!(
            "lane offer `{offer_id}` was made to `{}`, not to you",
            offer.agent_id
        ));
    }
    let now = chrono::Utc::now().to_rfc3339();
    let status = if claim { "claimed" } else { "declined" };
    if !store
        .resolve_lane_offer(offer_id, status, note, &now)
        .map_err(|e| e.to_string())?
    {
        let current = store
            .get_lane_offer(offer_id)
            .ok()
            .flatten()
            .map_or(offer.status, |o| o.status);
        return Err(format!("lane offer `{offer_id}` is already {current}"));
    }
    let (kind, summary) = if claim {
        (
            crate::coordination::BoardEntryKind::Dispatched,
            format!("{offer_id} — claimed lane: {}", offer.title),
        )
    } else {
        (
            crate::coordination::BoardEntryKind::Noted,
            format!(
                "{offer_id} — declined lane {}: {}",
                offer.title,
                note.unwrap_or("")
            ),
        )
    };
    let _ = store.record_board_entry(
        actor_id,
        kind,
        &crate::coordination::NewBoardEntry {
            session_id: Some(&offer.session_id),
            workspace: None,
            summary: &summary,
            correlation_id: Some(offer_id),
            created_at: &now,
        },
    );
    Ok(if claim {
        format!(
            "Claimed. The lane \"{}\" starts in session {} with your context; its report goes to that session, not to you.",
            offer.title, offer.session_id
        )
    } else {
        format!("Declined. Session {} is told why.", offer.session_id)
    })
}

pub fn withdraw(
    store: &Store,
    offer_id: &str,
    session_id: &str,
    reason: &str,
) -> Result<LaneOffer, String> {
    let offer = store
        .get_lane_offer(offer_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("lane offer `{offer_id}` not found"))?;
    if offer.session_id != session_id {
        return Err(format!("lane offer `{offer_id}` was made by another session"));
    }
    let now = chrono::Utc::now().to_rfc3339();
    if !store
        .resolve_lane_offer(offer_id, "withdrawn", Some(reason), &now)
        .map_err(|e| e.to_string())?
    {
        let current = store
            .get_lane_offer(offer_id)
            .ok()
            .flatten()
            .map_or(offer.status.clone(), |o| o.status);
        return Err(format!(
            "lane offer `{offer_id}` is already {current}; once claimed it runs as a lane you can cancel by its lane id"
        ));
    }
    let _ = store.record_board_entry(
        &offer.agent_id,
        crate::coordination::BoardEntryKind::Noted,
        &crate::coordination::NewBoardEntry {
            session_id: Some(session_id),
            workspace: None,
            summary: &format!("{offer_id} — lane offer withdrawn: {} — {reason}", offer.title),
            correlation_id: Some(offer_id),
            created_at: &now,
        },
    );
    Ok(offer)
}

pub fn declined_notice(offer: &LaneOffer) -> String {
    format!(
        "[lane_offer_declined]\noffer_id: {}\nagent: {}\ntitle: {}\nreason: {}\n[/lane_offer_declined]\nDo the piece yourself, offer it to another agent, or change the brief and offer again.",
        offer.id,
        offer.agent_id,
        offer.title,
        offer.note.as_deref().unwrap_or("none given")
    )
}

pub fn is_offer_id(id: &str) -> bool {
    id.starts_with(OFFER_PREFIX)
}

pub fn offer_body(offer: &LaneOffer, merged: bool) -> String {
    let access = if offer.read_only { "read_only" } else { "full" };
    let paths = if offer.paths.is_empty() {
        "none".to_string()
    } else {
        offer.paths.join(", ")
    };
    let heading = if merged {
        "The requester resent this lane offer with more to it."
    } else {
        "A session wants a lane worked as you."
    };
    format!(
        "[lane_offer]\noffer_id: {}\nfrom_session: {}\ntitle: {}\naccess: {access}\npaths: {paths}\nprofile: {}\nbrief:\n{}\n[/lane_offer]\n{heading} Claim it with task.claim (task_id: the offer id) and pass `context` with what the lane should know from your memory, skills or past work; it is added to the lane's brief. Decline with a reason if it isn't yours.",
        offer.id,
        offer.session_id,
        offer.title,
        offer.profile.as_deref().unwrap_or("the session's"),
        offer.brief,
    )
}

pub fn lane_brief(offer: &LaneOffer) -> String {
    match offer
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(context) => format!(
            "{}\n\nFrom your desk ({}), when it took this on:\n{context}",
            offer.brief, offer.agent_id
        ),
        None => offer.brief.clone(),
    }
}

pub fn offer_lane(
    store: &Store,
    session_id: &str,
    agent_id: &str,
    request: &crate::meta::DelegateBrief,
    brief: &str,
    read_only: bool,
) -> Result<OfferOutcome, StoreError> {
    let title = request.title.as_str();
    let now = chrono::Utc::now().to_rfc3339();
    let since = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    if store.pending_lane_offer(session_id, agent_id, title)?.is_none()
        && let Some(claimed) =
            store.recently_claimed_lane_offer(session_id, agent_id, title, &since)?
    {
        return Ok(OfferOutcome::AlreadyClaimed(claimed.id));
    }
    let pending = match store.pending_lane_offer(session_id, agent_id, title)? {
        Some(existing) if store.merge_lane_offer(&existing.id, brief, &now)? => Some(existing),
        _ => None,
    };
    let (offer, merged) = match pending {
        Some(existing) => {
            let offer = store.get_lane_offer(&existing.id)?.unwrap_or(existing);
            (offer, true)
        }
        None => {
            let offer = LaneOffer {
                id: format!(
                    "{OFFER_PREFIX}{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                ),
                session_id: session_id.to_string(),
                agent_id: agent_id.to_string(),
                title: title.to_string(),
                brief: brief.to_string(),
                read_only,
                profile: request.profile.clone(),
                paths: request.paths.clone(),
                status: "offered".into(),
                note: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            store.insert_lane_offer(&offer)?;
            (offer, false)
        }
    };
    store.send_direct_message(
        ("agent", crate::mission_control::SESSION_ID),
        ("agent", agent_id),
        &offer_body(&offer, merged),
        &uuid::Uuid::new_v4().to_string(),
        &now,
    )?;
    let summary = if merged {
        format!("{} — lane offer resent: {}", offer.id, offer.title)
    } else {
        format!("{} — lane offered to you: {}", offer.id, offer.title)
    };
    let _ = store.record_board_entry(
        agent_id,
        crate::coordination::BoardEntryKind::Noted,
        &crate::coordination::NewBoardEntry {
            session_id: Some(session_id),
            workspace: None,
            summary: &summary,
            correlation_id: Some(&offer.id),
            created_at: &now,
        },
    );
    Ok(OfferOutcome::Offered {
        id: offer.id,
        merged,
    })
}

pub enum OfferOutcome {
    Offered { id: String, merged: bool },
    AlreadyClaimed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_merges_then_claim_starts_once_with_desk_context() {
        let store = Store::open_in_memory().unwrap();
        let brief = crate::meta::parse_delegate_brief(&serde_json::json!({
            "title": "review diff",
            "description": "Review the diff in src/lanes.rs and report correctness findings with file and line.",
            "agent": "reviewer",
            "paths": ["src/lanes.rs"]
        }))
        .unwrap();
        let offer = |text: &str| match offer_lane(&store, "s1", "reviewer", &brief, text, true) {
            Ok(OfferOutcome::Offered { id, merged }) => (id, merged),
            Ok(OfferOutcome::AlreadyClaimed(id)) => (id, false),
            Err(e) => panic!("{e}"),
        };
        let (id, merged) = offer(&brief.description);
        assert!(!merged);
        let (again, merged) = offer("Also check tests.");
        assert_eq!((again.as_str(), merged), (id.as_str(), true));
        assert!(claim_or_decline(&store, &id, "someone-else", true, None).is_err());
        claim_or_decline(
            &store,
            &id,
            "reviewer",
            true,
            Some("Watch the lease token."),
        )
        .unwrap();
        assert!(claim_or_decline(&store, &id, "reviewer", false, Some("no")).is_err());
        let taken = store
            .take_resolved_lane_offers(&chrono::Utc::now().to_rfc3339())
            .unwrap();
        assert_eq!(taken.len(), 1);
        let lane = lane_brief(&taken[0]);
        assert!(lane.contains("Also check tests."), "{lane}");
        assert!(lane.contains("Watch the lease token."), "{lane}");
        assert_eq!(taken[0].paths, vec!["src/lanes.rs".to_string()]);
        assert!(store.take_resolved_lane_offers("now").unwrap().is_empty());
        assert!(matches!(
            offer_lane(&store, "s1", "reviewer", &brief, "Late resend.", true),
            Ok(OfferOutcome::AlreadyClaimed(claimed)) if claimed == id
        ));
        assert!(withdraw(&store, &id, "s1", "not needed").is_err());
    }
}
