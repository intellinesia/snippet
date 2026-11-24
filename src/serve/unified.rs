//! Unified artifacts: one list and one link form over documents and canvases.
//!
//! Contract:
//! - `GET /artifacts` (no `session` query) -> `{"artifacts": [Card]}`, newest first, documents and canvases merged.
//! - `GET /artifacts/resolve?id=<ref>` -> `{"card": Card}` or 404.
//! - `Card = {id, kind: document|canvas|image|pdf|file, subkind, title, session, updated_at, rev, author, thumb_url}`.
//! - `id` is the link target: `artifact:<id>[@rev]`. A canvas id has no slash (`q4-board-1a2b3c`); a document id is
//!   `<session>/<artifact id>`. `canvas:<canvas>/<frame>[/<el>][@rev]` deep links also resolve, to the canvas card.
//! - `thumb_url` is a signed, expiring path (images and canvases), else null.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Shared, api_error};
use crate::artifacts::Artifact;
use crate::canvas::{CanvasSummary, THUMB_LINK_TTL_SECS, parse_canvas_ref, signed_thumb_path};

const LINK_TTL_SECS: i64 = 6 * 60 * 60;
const CARD_THUMB_SCALE_MILLIS: u32 = 500;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Card {
    pub id: String,
    pub kind: &'static str,
    pub subkind: String,
    pub title: String,
    pub session: Option<String>,
    pub updated_at: i64,
    pub rev: u64,
    pub author: Option<String>,
    pub thumb_url: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum ArtifactRef {
    Canvas { canvas_id: String },
    Document { session: String, id: String },
}

pub fn parse_artifact_ref(raw: &str) -> Option<ArtifactRef> {
    let raw = raw.trim();
    if raw.starts_with("canvas:") {
        let link = parse_canvas_ref(raw).ok()?;
        return Some(ArtifactRef::Canvas {
            canvas_id: link.canvas_id,
        });
    }
    let body = raw.strip_prefix("artifact:").unwrap_or(raw);
    let body = body.rsplit_once('@').map_or(body, |(b, v)| {
        if v.chars().all(|c| c.is_ascii_digit()) {
            b
        } else {
            body
        }
    });
    match body.rsplit_once('/') {
        Some((session, id))
            if body
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != "..") =>
        {
            Some(ArtifactRef::Document {
                session: session.to_string(),
                id: id.to_string(),
            })
        }
        None if crate::canvas::links::is_safe_component(body) => Some(ArtifactRef::Canvas {
            canvas_id: body.to_string(),
        }),
        _ => None,
    }
}

fn document_kind(subkind: &str) -> &'static str {
    match subkind {
        "image" => "image",
        "pdf" => "pdf",
        "html" | "markdown" | "svg" | "csv" | "code" => "document",
        _ => "file",
    }
}

pub fn document_card(secret: &str, session: &str, a: &Artifact) -> Card {
    let latest = a.latest();
    let thumb_url = latest
        .filter(|_| a.kind == "image")
        .map(|v| crate::artifacts::signed_path(secret, session, &a.id, v, LINK_TTL_SECS));
    Card {
        id: format!("{session}/{}", a.id),
        kind: document_kind(&a.kind),
        subkind: a.kind.clone(),
        title: a.title.clone(),
        session: Some(session.to_string()),
        updated_at: latest.map(|v| v.created_at).unwrap_or(0),
        rev: latest.map(|v| v.n as u64).unwrap_or(0),
        author: latest.map(|v| v.source.clone()),
        thumb_url,
    }
}

pub fn canvas_card(secret: &str, c: &CanvasSummary) -> Card {
    let thumb_url = c.thumb_frame.as_ref().map(|(frame, rev)| {
        signed_thumb_path(
            secret,
            &c.canvas_id,
            frame,
            *rev,
            CARD_THUMB_SCALE_MILLIS,
            THUMB_LINK_TTL_SECS,
        )
    });
    Card {
        id: c.canvas_id.clone(),
        kind: "canvas",
        subkind: "canvas".into(),
        title: c.title.clone(),
        session: None,
        updated_at: c.updated_at,
        rev: c.rev,
        author: c.author.clone(),
        thumb_url,
    }
}

/// Merge cards from both sources, newest first, ties broken by id.
pub fn merge_cards(mut cards: Vec<Card>) -> Vec<Card> {
    cards.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    cards
}

#[derive(Deserialize)]
pub struct ResolveQuery {
    id: String,
}

pub async fn list_all(State(d): State<Shared>) -> Response {
    let canvas = d.canvas.clone();
    let secret = d.token.clone();
    let res = tokio::task::spawn_blocking(move || {
        let mut cards: Vec<Card> = crate::artifacts::list_all()
            .iter()
            .map(|(s, a)| document_card(&secret, s, a))
            .collect();
        let canvases = canvas.list_canvases().map_err(|e| e.to_string())?;
        cards.extend(canvases.iter().map(|c| canvas_card(&secret, c)));
        Ok::<_, String>(merge_cards(cards))
    })
    .await;
    match res {
        Ok(Ok(cards)) => Json(json!({ "artifacts": cards })).into_response(),
        Ok(Err(e)) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn resolve(State(d): State<Shared>, Query(q): Query<ResolveQuery>) -> Response {
    let Some(r) = parse_artifact_ref(&q.id) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "id must be artifact:<canvas id>, artifact:<session>/<artifact id> or canvas:<canvas>/<frame>",
        );
    };
    let canvas = d.canvas.clone();
    let secret = d.token.clone();
    let res = tokio::task::spawn_blocking(move || match r {
        ArtifactRef::Document { session, id } => {
            crate::artifacts::get(&session, &id).map(|a| document_card(&secret, &session, &a))
        }
        ArtifactRef::Canvas { canvas_id } => canvas
            .list_canvases()
            .ok()
            .and_then(|l| l.into_iter().find(|c| c.canvas_id == canvas_id))
            .map(|c| canvas_card(&secret, &c)),
    })
    .await;
    match res {
        Ok(Some(card)) => Json(json!({ "card": card })).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such artifact or canvas"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, t: i64) -> Card {
        Card {
            id: id.into(),
            kind: "canvas",
            subkind: "canvas".into(),
            title: id.into(),
            session: None,
            updated_at: t,
            rev: 1,
            author: None,
            thumb_url: None,
        }
    }

    #[test]
    fn merge_orders_newest_first_with_stable_ties() {
        let m = merge_cards(vec![card("b", 5), card("a", 5), card("c", 9), card("d", 1)]);
        let ids: Vec<_> = m.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c", "a", "b", "d"]);
    }

    #[test]
    fn refs_resolve_to_either_kind() {
        assert_eq!(
            parse_artifact_ref("artifact:q4-board-1a2b3c"),
            Some(ArtifactRef::Canvas {
                canvas_id: "q4-board-1a2b3c".into()
            })
        );
        assert_eq!(
            parse_artifact_ref("artifact:s1/conversations/c9/report@3"),
            Some(ArtifactRef::Document {
                session: "s1/conversations/c9".into(),
                id: "report".into()
            })
        );
        assert_eq!(
            parse_artifact_ref("canvas:proj/hero/btn@7"),
            Some(ArtifactRef::Canvas {
                canvas_id: "proj".into()
            })
        );
        assert_eq!(parse_artifact_ref("artifact:../x"), None);
        assert_eq!(parse_artifact_ref("artifact:"), None);
    }
}
