//! Theme routes: list, read, write, apply and validate theme documents, plus
//! the `themes_changed` device event on every mutation.
//!
//! There is only one daemon token, so "user only" for `PUT /themes/active` is
//! enforced by there being no agent tool for it — agents get
//! theme_list/theme_get/theme_schema/theme_save/theme_delete and nothing that
//! applies.

use axum::Router;
use axum::extract::{Json, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Shared, api_error};
use crate::app_store::ThemeRow;
use crate::themes;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/themes", get(list_themes))
        .route("/themes/active", get(get_active).put(set_active))
        .route("/themes/validate", post(validate_route))
        .route(
            "/themes/{id}",
            get(get_theme).put(put_theme).delete(delete_theme),
        )
}

fn invalid_theme(problems: Vec<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "theme invalid", "problems": problems })),
    )
        .into_response()
}

fn store_error(e: crate::store::StoreError) -> Response {
    api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("store: {e}"))
}

fn theme_entry(row: &ThemeRow, resolved: Option<&Value>) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "author": row.author,
        "builtin": row.builtin,
        "base": row.doc.get("base").cloned().unwrap_or(Value::Null),
        "updated_at": row.updated_at,
        "preview": resolved.map(themes::preview).unwrap_or(Value::Null),
    })
}

async fn list_themes(State(d): State<Shared>) -> Response {
    let rows = match d.store.list_themes() {
        Ok(rows) => rows,
        Err(e) => return store_error(e),
    };
    let active = themes::active_theme_id(&d.store).unwrap_or_else(|_| "black".into());
    let active_set = d.store.active_theme_is_set().unwrap_or(false);
    let themes_out: Vec<Value> = rows
        .iter()
        .map(|row| {
            let resolved = themes::resolve(&d.store, &row.doc).ok();
            theme_entry(row, resolved.as_ref())
        })
        .collect();
    Json(json!({
        "live_groups": themes::LIVE_GROUPS,
        "active": active,
        "active_set": active_set,
        "themes": themes_out,
    }))
    .into_response()
}

async fn get_theme(State(d): State<Shared>, AxumPath(id): AxumPath<String>) -> Response {
    let row = match themes::get_theme(&d.store, &id) {
        Ok(Some(row)) => row,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "no such theme"),
        Err(e) => return store_error(e),
    };
    let resolved = match themes::resolve(&d.store, &row.doc) {
        Ok(r) => r,
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let mut doc = row.doc.clone();
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("resolved".into(), resolved);
        obj.insert("author".into(), json!(row.author));
        obj.insert("builtin".into(), json!(row.builtin));
        obj.insert("updated_at".into(), json!(row.updated_at));
    }
    Json(doc).into_response()
}

#[derive(Deserialize)]
struct ActiveReq {
    id: String,
}

async fn get_active(State(d): State<Shared>) -> Response {
    let id = match themes::active_theme_id(&d.store) {
        Ok(id) => id,
        Err(e) => return store_error(e),
    };
    let row = match themes::get_theme(&d.store, &id) {
        Ok(Some(row)) => row,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "no such theme"),
        Err(e) => return store_error(e),
    };
    match themes::resolve(&d.store, &row.doc) {
        Ok(mut resolved) => {
            let active_set = d.store.active_theme_is_set().unwrap_or(false);
            if let Some(obj) = resolved.as_object_mut() {
                obj.insert("active_set".into(), json!(active_set));
            }
            Json(resolved).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn set_active(State(d): State<Shared>, Json(req): Json<ActiveReq>) -> Response {
    let id = req.id.trim();
    if id.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "id required");
    }
    let row = match themes::get_theme(&d.store, id) {
        Ok(Some(row)) => row,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "no such theme"),
        Err(e) => return store_error(e),
    };
    if let Err(e) = d.store.set_active_theme_id(id) {
        return store_error(e);
    }
    let resolved = themes::resolve(&d.store, &row.doc).unwrap_or(Value::Null);
    themes::emit_changed(id, id);
    Json(json!({ "active": id, "resolved": resolved })).into_response()
}

async fn put_theme(
    State(d): State<Shared>,
    AxumPath(id): AxumPath<String>,
    Json(mut doc): Json<Value>,
) -> Response {
    let Some(obj) = doc.as_object() else {
        return api_error(StatusCode::BAD_REQUEST, "theme must be a JSON object");
    };
    if obj.get("id").and_then(Value::as_str) != Some(id.as_str()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "document id must match the path id",
        );
    }
    match themes::get_theme(&d.store, &id) {
        Ok(Some(row)) if row.builtin => {
            return api_error(StatusCode::FORBIDDEN, "builtin themes are immutable");
        }
        Ok(_) => {}
        Err(e) => return store_error(e),
    }
    if let Some(o) = doc.as_object_mut() {
        o.insert("author".into(), json!("user"));
    }
    let problems = themes::validate(&d.store, &doc, false);
    if !problems.is_empty() {
        return invalid_theme(problems);
    }
    let name = doc
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(id.as_str())
        .to_string();
    let now = chrono::Utc::now().timestamp();
    if let Err(e) = d.store.upsert_theme(&id, &name, "user", false, &doc, now) {
        return store_error(e);
    }
    let resolved = themes::resolve(&d.store, &doc).unwrap_or(Value::Null);
    if let Some(o) = doc.as_object_mut() {
        o.insert("resolved".into(), resolved);
        o.insert("builtin".into(), json!(false));
        o.insert("updated_at".into(), json!(now));
    }
    let active = themes::active_theme_id(&d.store).unwrap_or_else(|_| "black".into());
    themes::emit_changed(&active, &id);
    Json(doc).into_response()
}

async fn delete_theme(State(d): State<Shared>, AxumPath(id): AxumPath<String>) -> Response {
    let row = match themes::get_theme(&d.store, &id) {
        Ok(Some(row)) => row,
        Ok(None) => return api_error(StatusCode::NOT_FOUND, "no such theme"),
        Err(e) => return store_error(e),
    };
    if row.builtin {
        return api_error(StatusCode::FORBIDDEN, "builtin themes cannot be deleted");
    }
    let active = themes::active_theme_id(&d.store).unwrap_or_else(|_| "black".into());
    if let Err(e) = d.store.delete_theme(&id) {
        return store_error(e);
    }
    if active == id {
        if let Err(e) = d.store.set_active_theme_id(themes::DEFAULT_THEME_ID) {
            return store_error(e);
        }
    }
    let active = themes::active_theme_id(&d.store).unwrap_or_else(|_| "black".into());
    themes::emit_changed(&active, &id);
    Json(json!({ "deleted": id, "active": active })).into_response()
}

async fn validate_route(State(d): State<Shared>, Json(doc): Json<Value>) -> Response {
    let problems = themes::validate(&d.store, &doc, false);
    if problems.is_empty() {
        Json(json!({ "valid": true })).into_response()
    } else {
        invalid_theme(problems)
    }
}
