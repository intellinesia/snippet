//! HTTP routes for the agent-first canvas backend.

use std::path::PathBuf;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{Shared, api_error};
use crate::canvas::{
    Author, CanvasError, HIT_TEST_TIMEOUT, Op, PinStatus, RENDER_TIMEOUT, RenderError,
    THUMB_LINK_TTL_SECS, canvas_id_for_workspace, emit_canvas_rev_event, links::is_safe_component,
    normalize_scale, parse_canvas_ref, signed_thumb_path, verify_signed_thumb,
};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/canvas", get(get_canvas))
        .route("/canvases", get(list_canvases).post(create_canvas))
        .route("/canvas/frames/{id}", get(get_frame))
        .route("/canvas/frames/{id}/thumb.png", get(get_frame_thumb))
        .route("/canvas/frames/{id}/layout.json", get(get_frame_layout))
        .route("/canvas/ops", get(get_ops))
        .route("/canvas/batch", post(post_batch))
        .route("/canvas/revert", post(post_revert))
        .route(
            "/canvas/pins",
            get(list_pins).post(create_pin).patch(patch_pin_body),
        )
        .route("/canvas/pins/{id}", patch(patch_pin_by_id))
        .route("/canvas/hit", post(post_hit))
        .route("/canvas/link", get(resolve_link))
        .route(
            "/a/canvas/{exp}/{sig}/{canvas_id}/{frame_id}/{rev}/{scale_millis}/thumb.png",
            get(serve_signed_thumb),
        )
}

fn resolve_canvas_id(d: &Shared, canvas_id: Option<&str>, workspace: Option<&str>) -> String {
    if let Some(cid) = canvas_id.map(str::trim).filter(|s| !s.is_empty()) {
        return cid.to_string();
    }
    if let Some(ws) = workspace.map(str::trim).filter(|s| !s.is_empty()) {
        return canvas_id_for_workspace(&PathBuf::from(ws));
    }
    let default_ws = d
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .clone();
    canvas_id_for_workspace(&default_ws)
}

fn canvas_error_response(err: CanvasError) -> Response {
    let status = match &err {
        CanvasError::FrameNotFound(_)
        | CanvasError::BatchNotFound(_)
        | CanvasError::PinNotFound(_) => StatusCode::NOT_FOUND,
        CanvasError::Html(crate::canvas::html::HtmlError::TooLarge(_))
        | CanvasError::Render(RenderError::TooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
        CanvasError::InvalidId(_)
        | CanvasError::EmptyBatch
        | CanvasError::InvalidArgument(_)
        | CanvasError::Html(_) => StatusCode::BAD_REQUEST,
        CanvasError::FrameExists(_) => StatusCode::CONFLICT,
        CanvasError::Render(RenderError::Timeout) => StatusCode::GATEWAY_TIMEOUT,
        CanvasError::Render(_)
        | CanvasError::UnsupportedFormatVersion { .. }
        | CanvasError::Io(_)
        | CanvasError::Serde(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error(status, err.to_string())
}

async fn list_canvases(State(d): State<Shared>) -> Response {
    let canvas = d.canvas.clone();
    match tokio::task::spawn_blocking(move || canvas.list_canvases()).await {
        Ok(Ok(list)) => Json(list).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreateCanvasReq {
    title: String,
}

async fn create_canvas(State(d): State<Shared>, Json(req): Json<CreateCanvasReq>) -> Response {
    let canvas = d.canvas.clone();
    match tokio::task::spawn_blocking(move || canvas.create_canvas(&req.title)).await {
        Ok(Ok(c)) => Json(json!({ "canvas_id": c.canvas_id, "title": c.title })).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct CanvasQuery {
    canvas_id: Option<String>,
    workspace: Option<String>,
}

async fn get_canvas(State(d): State<Shared>, Query(q): Query<CanvasQuery>) -> Response {
    let cid = resolve_canvas_id(&d, q.canvas_id.as_deref(), q.workspace.as_deref());
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        Ok::<_, CanvasError>(json!({
            "canvas_id": s.canvas_id(),
            "rev": s.rev(),
            "frames": s.frames(),
        }))
    })
    .await;
    match res {
        Ok(Ok(val)) => Json(val).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn get_frame(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    Query(q): Query<CanvasQuery>,
) -> Response {
    if !is_safe_component(&id) {
        return canvas_error_response(CanvasError::InvalidId(id));
    }
    let cid = resolve_canvas_id(&d, q.canvas_id.as_deref(), q.workspace.as_deref());
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        match s.frame(&id) {
            Some(frame) => Ok(json!({ "canvas_id": s.canvas_id(), "frame": frame })),
            None => Err(CanvasError::FrameNotFound(id)),
        }
    })
    .await;
    match res {
        Ok(Ok(val)) => Json(val).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct OpsQuery {
    since: Option<u64>,
    canvas_id: Option<String>,
    workspace: Option<String>,
}

async fn get_ops(State(d): State<Shared>, Query(q): Query<OpsQuery>) -> Response {
    let cid = resolve_canvas_id(&d, q.canvas_id.as_deref(), q.workspace.as_deref());
    let since = q.since.unwrap_or(0);
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        Ok::<_, CanvasError>(json!({
            "canvas_id": s.canvas_id(),
            "rev": s.rev(),
            "ops": s.ops_since(since),
        }))
    })
    .await;
    match res {
        Ok(Ok(val)) => Json(val).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct BatchReq {
    label: String,
    ops: Vec<Op>,
    #[serde(default)]
    canvas_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    session: Option<String>,
}

async fn post_batch(State(d): State<Shared>, Json(req): Json<BatchReq>) -> Response {
    let cid = resolve_canvas_id(&d, req.canvas_id.as_deref(), req.workspace.as_deref());
    let author = req.author.unwrap_or_else(|| Author::user("user"));
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.apply_batch(req.label, req.ops, author, req.session)
    })
    .await;
    match res {
        Ok(Ok(commit)) => {
            emit_canvas_rev_event(&commit);
            Json(json!({
                "canvas_id": commit.canvas_id,
                "rev": commit.rev,
                "batch_id": commit.batch_id,
                "created_ids": commit.created_ids,
                "links": commit.links,
            }))
            .into_response()
        }
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct RevertReq {
    batch_id: String,
    #[serde(default)]
    canvas_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    session: Option<String>,
}

async fn post_revert(State(d): State<Shared>, Json(req): Json<RevertReq>) -> Response {
    let cid = resolve_canvas_id(&d, req.canvas_id.as_deref(), req.workspace.as_deref());
    let author = req.author.unwrap_or_else(|| Author::user("user"));
    let target_id = req.batch_id.clone();
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.revert_batch(&req.batch_id, author, req.session)
    })
    .await;
    match res {
        Ok(Ok(commit)) => {
            emit_canvas_rev_event(&commit);
            Json(json!({
                "canvas_id": commit.canvas_id,
                "rev": commit.rev,
                "batch_id": commit.batch_id,
                "reverted_batch_id": target_id,
                "created_ids": commit.created_ids,
                "links": commit.links,
            }))
            .into_response()
        }
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct PinsQuery {
    frame_id: Option<String>,
    status: Option<PinStatus>,
    canvas_id: Option<String>,
    workspace: Option<String>,
}

async fn list_pins(State(d): State<Shared>, Query(q): Query<PinsQuery>) -> Response {
    if let Some(ref fid) = q.frame_id
        && !is_safe_component(fid)
    {
        return canvas_error_response(CanvasError::InvalidId(fid.clone()));
    }
    let cid = resolve_canvas_id(&d, q.canvas_id.as_deref(), q.workspace.as_deref());
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        Ok::<_, CanvasError>(json!({
            "canvas_id": s.canvas_id(),
            "pins": s.pins(q.frame_id.as_deref(), q.status),
        }))
    })
    .await;
    match res {
        Ok(Ok(val)) => Json(val).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreatePinReq {
    frame_id: String,
    #[serde(default)]
    el_id: Option<String>,
    dx: f32,
    dy: f32,
    text: String,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    canvas_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

async fn create_pin(State(d): State<Shared>, Json(req): Json<CreatePinReq>) -> Response {
    let cid = resolve_canvas_id(&d, req.canvas_id.as_deref(), req.workspace.as_deref());
    let author = req.author.unwrap_or_else(|| Author::user("user"));
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.add_pin(req.frame_id, req.el_id, req.dx, req.dy, req.text, author)
    })
    .await;
    match res {
        Ok(Ok(pin)) => (StatusCode::CREATED, Json(json!({ "pin": pin }))).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct PatchPinReq {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<PinStatus>,
    #[serde(default)]
    reply: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    canvas_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

async fn patch_pin_body(State(d): State<Shared>, Json(req): Json<PatchPinReq>) -> Response {
    let Some(id) = req.id.clone().filter(|s| !s.is_empty()) else {
        return api_error(StatusCode::BAD_REQUEST, "missing pin id");
    };
    patch_pin_inner(d, id, req).await
}

async fn patch_pin_by_id(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    Json(req): Json<PatchPinReq>,
) -> Response {
    patch_pin_inner(d, id, req).await
}

async fn patch_pin_inner(d: Shared, id: String, req: PatchPinReq) -> Response {
    let cid = resolve_canvas_id(&d, req.canvas_id.as_deref(), req.workspace.as_deref());
    let canvas = d.canvas.clone();
    let res = tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.update_pin(&id, req.status, req.reply, req.text)
    })
    .await;
    match res {
        Ok(Ok(pin)) => Json(json!({ "pin": pin })).into_response(),
        Ok(Err(e)) => canvas_error_response(e),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize, Default)]
struct RenderQuery {
    scale: Option<f32>,
    rev: Option<u64>,
    canvas_id: Option<String>,
    workspace: Option<String>,
}

fn png_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "private, max-age=300"),
        ],
        bytes,
    )
        .into_response()
}

async fn get_frame_thumb(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    Query(q): Query<RenderQuery>,
) -> Response {
    if !is_safe_component(&id) {
        return canvas_error_response(CanvasError::InvalidId(id));
    }
    let cid = resolve_canvas_id(&d, q.canvas_id.as_deref(), q.workspace.as_deref());
    let scale = q.scale.unwrap_or(1.0);
    match d
        .canvas
        .render_frame_cached_async(cid, id, q.rev, scale, RENDER_TIMEOUT)
        .await
    {
        Ok((_, png, _)) => png_response(png),
        Err(e) => canvas_error_response(e),
    }
}

async fn get_frame_layout(
    State(d): State<Shared>,
    AxPath(id): AxPath<String>,
    Query(q): Query<RenderQuery>,
) -> Response {
    if !is_safe_component(&id) {
        return canvas_error_response(CanvasError::InvalidId(id));
    }
    let cid = resolve_canvas_id(&d, q.canvas_id.as_deref(), q.workspace.as_deref());
    let scale = q.scale.unwrap_or(1.0);
    match d
        .canvas
        .render_frame_cached_async(cid, id, q.rev, scale, RENDER_TIMEOUT)
        .await
    {
        Ok((_, _, layout)) => Json(layout).into_response(),
        Err(e) => canvas_error_response(e),
    }
}

async fn serve_signed_thumb(
    State(d): State<Shared>,
    AxPath((exp, sig, canvas_id, frame_id, rev, scale_millis)): AxPath<(
        i64,
        String,
        String,
        String,
        u64,
        u32,
    )>,
) -> Response {
    if !verify_signed_thumb(
        &d.token,
        exp,
        &sig,
        &canvas_id,
        &frame_id,
        rev,
        scale_millis,
    ) {
        return api_error(StatusCode::NOT_FOUND, "not found");
    }
    let scale = scale_millis as f32 / 1000.0;
    match d
        .canvas
        .render_frame_cached_async(canvas_id, frame_id, Some(rev), scale, RENDER_TIMEOUT)
        .await
    {
        Ok((_, png, _)) => png_response(png),
        Err(e) => canvas_error_response(e),
    }
}

#[derive(Deserialize)]
struct HitReq {
    frame_id: String,
    x: f32,
    y: f32,
    #[serde(default)]
    canvas_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
}

async fn post_hit(State(d): State<Shared>, Json(req): Json<HitReq>) -> Response {
    if !is_safe_component(&req.frame_id) {
        return canvas_error_response(CanvasError::InvalidId(req.frame_id));
    }
    let cid = resolve_canvas_id(&d, req.canvas_id.as_deref(), req.workspace.as_deref());
    match d
        .canvas
        .hit_test_async(cid, req.frame_id, req.x, req.y, HIT_TEST_TIMEOUT)
        .await
    {
        Ok(el_id) => Json(json!({ "el_id": el_id })).into_response(),
        Err(e) => canvas_error_response(e),
    }
}

#[derive(Deserialize)]
struct LinkQuery {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    scale: Option<f32>,
}

async fn resolve_link(State(d): State<Shared>, Query(q): Query<LinkQuery>) -> Response {
    let parsed = match parse_canvas_ref(&q.reference) {
        Ok(link) => link,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let (_, scale_millis) = match normalize_scale(q.scale.unwrap_or(1.0)) {
        Ok(s) => s,
        Err(e) => return canvas_error_response(e),
    };
    let canvas = d.canvas.clone();
    let cid = parsed.canvas_id.clone();
    let fid = parsed.frame_id.clone();
    let rev = parsed.rev;
    let frame = match tokio::task::spawn_blocking(move || {
        let store = canvas.store(&cid)?;
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        let found = match rev {
            Some(r) => s.frame_at_rev(&fid, r)?,
            None => s.frame(&fid),
        };
        found.ok_or(CanvasError::FrameNotFound(fid))
    })
    .await
    {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return canvas_error_response(e),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let thumb_url = signed_thumb_path(
        &d.token,
        &parsed.canvas_id,
        &frame.id,
        frame.rev,
        scale_millis,
        THUMB_LINK_TTL_SECS,
    );
    Json(json!({
        "title": frame.title,
        "canvas_id": parsed.canvas_id,
        "frame_id": frame.id,
        "el_id": parsed.el_id,
        "rev": frame.rev,
        "thumb_url": thumb_url,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_app(dir: &std::path::Path) -> Router {
        let store = crate::store::Store::open_in_memory().unwrap();
        let d: Shared = std::sync::Arc::new(crate::serve::Daemon {
            config: std::sync::Mutex::new(crate::config::SnippetConfig::default()),
            config_path: dir.join("config.toml"),
            token: "test-secret".into(),
            hostname: "test".into(),
            sessions: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            shells: crate::term::SessionTerms::new(dir.to_path_buf()),
            git_write: tokio::sync::Mutex::new(()),
            seen_nonces: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_hidden: std::sync::Mutex::new(std::collections::HashMap::new()),
            queue_revision: std::sync::atomic::AtomicU64::new(0),
            lead_root: dir.join("lead"),
            store,
            coordination_events: tokio::sync::broadcast::channel(16).0,
            recurring_root: dir.join("recurring"),
            canvas: std::sync::Arc::new(crate::canvas::CanvasService::with_stub_renderer(
                dir.join("canvases"),
            )),
        });
        Router::new()
            .merge(crate::serve::artifact_routes::router())
            .merge(router())
            .layer(axum::middleware::from_fn_with_state(
                d.clone(),
                crate::serve::require_token,
            ))
            .with_state(d)
    }

    #[tokio::test]
    async fn rest_batch_thumb_layout_pins_link_and_signed_thumb() {
        let dir = tempfile::tempdir().unwrap();
        let app = test_app(dir.path());

        // Unauthed /canvas is rejected.
        let unauthed = app
            .clone()
            .oneshot(Request::get("/canvas").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::UNAUTHORIZED);

        // Create frame via POST /canvas/batch.
        let batch_body = json!({
            "canvas_id": "demo",
            "label": "initial hero",
            "ops": [{
                "op": "create_frame",
                "id": "hero",
                "title": "Hero",
                "x": 0.0,
                "y": 0.0,
                "w": 320.0,
                "h": 200.0,
                "html": "<div data-cv-id=\"root\">Hello</div>"
            }]
        });
        let resp = app
            .clone()
            .oneshot(
                Request::post("/canvas/batch?token=test-secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(batch_body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(val["rev"], 1);
        let batch_id = val["batch_id"].as_str().unwrap().to_string();

        // GET /canvas/link returns a signed thumb_url that loads WITHOUT the daemon token.
        let resp = app
            .clone()
            .oneshot(
                Request::get("/canvas/link?token=test-secret&ref=canvas:demo/hero@1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let link_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(link_json["title"], "Hero");
        let thumb_url = link_json["thumb_url"].as_str().unwrap();
        assert!(thumb_url.starts_with("/a/canvas/"));

        let thumb_resp = app
            .clone()
            .oneshot(Request::get(thumb_url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(thumb_resp.status(), StatusCode::OK);
        let png = axum::body::to_bytes(thumb_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));

        // GET layout.json matches dimensions.
        let layout_resp = app
            .clone()
            .oneshot(
                Request::get(
                    "/canvas/frames/hero/layout.json?token=test-secret&canvas_id=demo&scale=0.5",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(layout_resp.status(), StatusCode::OK);
        let layout_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(layout_resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(layout_json["width"], 160);
        assert_eq!(layout_json["height"], 100);

        // Pins create + patch + list.
        let pin_resp = app
            .clone()
            .oneshot(
                Request::post("/canvas/pins?token=test-secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "canvas_id": "demo",
                            "frame_id": "hero",
                            "el_id": "root",
                            "dx": 0.5,
                            "dy": 0.25,
                            "text": "Tighten spacing"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pin_resp.status(), StatusCode::CREATED);
        let pin_json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(pin_resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let pin_id = pin_json["pin"]["id"].as_str().unwrap();

        let patch_resp = app
            .clone()
            .oneshot(
                Request::patch(format!("/canvas/pins/{pin_id}?token=test-secret"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "canvas_id": "demo",
                            "status": "resolved",
                            "reply": "Done"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(patch_resp.status(), StatusCode::OK);

        // POST /canvas/hit returns null el_id on StubRenderer.
        let hit_resp = app
            .clone()
            .oneshot(
                Request::post("/canvas/hit?token=test-secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"canvas_id": "demo", "frame_id": "hero", "x": 10.0, "y": 10.0})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hit_resp.status(), StatusCode::OK);

        // POST /canvas/revert reverts batch 1.
        let rev_resp = app
            .clone()
            .oneshot(
                Request::post("/canvas/revert?token=test-secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({"canvas_id": "demo", "batch_id": batch_id}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rev_resp.status(), StatusCode::OK);

        // Auth check across every /canvas/* endpoint (all must require token).
        for (method, path) in [
            ("GET", "/canvas/frames/hero"),
            ("GET", "/canvas/frames/hero/thumb.png"),
            ("GET", "/canvas/frames/hero/layout.json"),
            ("GET", "/canvas/ops"),
            ("POST", "/canvas/batch"),
            ("POST", "/canvas/revert"),
            ("GET", "/canvas/pins"),
            ("POST", "/canvas/pins"),
            ("PATCH", "/canvas/pins"),
            ("PATCH", "/canvas/pins/pin-1"),
            ("POST", "/canvas/hit"),
            ("GET", "/canvas/link?ref=canvas:demo/hero"),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let r = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                r.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path} lacked auth"
            );
        }

        // Path traversal / unsafe ids rejected with 400.
        let bad_id = app
            .clone()
            .oneshot(
                Request::get("/canvas/frames/..secret?token=test-secret&canvas_id=demo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_id.status(), StatusCode::BAD_REQUEST);

        // Signed thumb tampered scale/rev/path or expired -> 404.
        let tampered_scale = thumb_url.replace("/1000/", "/500/");
        let r = app
            .clone()
            .oneshot(Request::get(&tampered_scale).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        let expired_url = signed_thumb_path("test-secret", "demo", "hero", 1, 1000, -10);
        let r = app
            .clone()
            .oneshot(Request::get(&expired_url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
