use super::api_error;
use axum::Router;
use axum::extract::{Json, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Shared, mission_error};
use crate::agent_wakes::{self, OwnerKind, Setter, WakeSpec};
use crate::coordination::followups::Followup;
use crate::coordination::watches as cw;
use crate::recurring::{self, RecurringJob, Schedule};

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/wakes", get(list_wakes).post(create_wake))
        .route("/wakes/runs", get(list_runs))
        .route(
            "/wakes/{id}",
            get(get_wake).patch(patch_wake).delete(delete_wake),
        )
        .route("/wakes/{id}/runs", get(wake_runs))
        .route("/wakes/{id}/script", get(wake_script))
}

fn now() -> u64 {
    chrono::Utc::now().timestamp() as u64
}

fn unix(rfc3339: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|t| t.timestamp())
}

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"id": id, "error": "no such wake"})),
    )
        .into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    api_error(StatusCode::BAD_REQUEST, message.into())
}

fn every_view(d: &Shared, job: &RecurringJob) -> Value {
    let mut view = agent_wakes::view_with_runs(&d.recurring_root, job, now());
    view["id"] = json!(job.id);
    view["kind"] = json!("every");
    view["plan_path"] = json!(job.plan_path);
    if job.wake.is_none() {
        view["owner"] = json!(job.session_id);
        view["owner_kind"] = json!(if job.session_id == crate::lead::SESSION_ID {
            OwnerKind::Lead
        } else {
            OwnerKind::Session
        });
    }
    view
}

fn once_view(f: &Followup) -> Value {
    let owner_kind = if f.owner.is_some() {
        OwnerKind::Agent
    } else if f.session_id == crate::lead::SESSION_ID {
        OwnerKind::Lead
    } else {
        OwnerKind::Session
    };
    json!({
        "id": f.id,
        "kind": "once",
        "owner": f.owner.as_deref().unwrap_or(&f.session_id),
        "owner_kind": owner_kind,
        "title": f.note.lines().next().unwrap_or_default().chars().take(80).collect::<String>(),
        "note": f.note,
        "session_id": f.session_id,
        "enabled": true,
        "next_run_at": unix(&f.due_at),
        "last_run_at": null,
        "last_error": null,
        "created_at": unix(&f.created_at),
    })
}

fn watch_view(d: &Shared, w: &cw::Watch) -> Result<Value, Response> {
    let mut view = d
        .store
        .watch_view(w)
        .map_err(|e| mission_error(e.to_string()))?;
    let last_run = w.last_run_at.as_deref().and_then(unix);
    let next = (w.enabled && w.mode == "poll")
        .then(|| last_run.map(|t| t + w.interval_secs))
        .flatten();
    view["kind"] = json!("watch");
    view["owner_kind"] = json!(OwnerKind::Agent);
    view["title"] = json!(w.name);
    view["next_run_at"] = json!(next);
    view["last_run_at"] = json!(last_run);
    view["last_fired_at"] = json!(w.last_fired_at.as_deref().and_then(unix));
    view["created_at"] = json!(unix(&w.created_at));
    view["updated_at"] = json!(unix(&w.updated_at));
    Ok(view)
}

enum Found {
    Every(RecurringJob),
    Once(Followup),
    Watch(cw::Watch),
}

fn find(d: &Shared, id: &str) -> Result<Option<Found>, Response> {
    let jobs = recurring::list_jobs(&d.recurring_root).map_err(mission_error)?;
    if let Some(job) = jobs.into_iter().find(|j| j.id == id) {
        return Ok(Some(Found::Every(job)));
    }
    if let Some(w) = d
        .store
        .watch_by_id(id)
        .map_err(|e| mission_error(e.to_string()))?
    {
        return Ok(Some(Found::Watch(w)));
    }
    let pending = d
        .store
        .get_followup(id)
        .map_err(|e| mission_error(e.to_string()))?;
    Ok(pending
        .filter(|f| {
            d.store
                .pending_followups(&f.session_id)
                .is_ok_and(|p| p.iter().any(|x| x.id == f.id))
        })
        .map(Found::Once))
}

fn view(d: &Shared, found: &Found) -> Result<Value, Response> {
    match found {
        Found::Every(job) => Ok(every_view(d, job)),
        Found::Once(f) => Ok(once_view(f)),
        Found::Watch(w) => watch_view(d, w),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

async fn list_wakes(State(d): State<Shared>, Query(q): Query<ListQuery>) -> Response {
    let owner = q.owner.as_deref().map(str::trim).filter(|o| !o.is_empty());
    let kind = q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if let Some(k) = kind
        && !["every", "once", "watch"].contains(&k)
    {
        return bad_request(format!("kind must be every, once or watch, got `{k}`"));
    }
    let wants = |k: &str| kind.is_none_or(|want| want == k);
    let mut wakes = Vec::new();
    if wants("every") {
        let jobs = match recurring::list_jobs(&d.recurring_root) {
            Ok(jobs) => jobs,
            Err(error) => return mission_error(error),
        };
        wakes.extend(jobs.iter().map(|j| every_view(&d, j)));
    }
    if wants("once") {
        match d.store.all_pending_followups() {
            Ok(rows) => wakes.extend(rows.iter().map(once_view)),
            Err(error) => return mission_error(error.to_string()),
        }
    }
    if wants("watch") {
        let watches = match d.store.list_watches(None) {
            Ok(w) => w,
            Err(error) => return mission_error(error.to_string()),
        };
        for w in &watches {
            match watch_view(&d, w) {
                Ok(v) => wakes.push(v),
                Err(response) => return response,
            }
        }
    }
    if let Some(owner) = owner {
        wakes.retain(|w| w["owner"].as_str() == Some(owner));
    }
    Json(json!({"wakes": wakes, "limits": agent_wakes::limits()})).into_response()
}

async fn get_wake(State(d): State<Shared>, AxumPath(id): AxumPath<String>) -> Response {
    match find(&d, &id) {
        Ok(Some(found)) => match view(&d, &found) {
            Ok(v) => Json(v).into_response(),
            Err(response) => response,
        },
        Ok(None) => not_found(&id),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct CreateReq {
    owner: String,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(flatten)]
    spec: WakeSpec,
}

fn apply_plan_path(
    d: &Shared,
    job: RecurringJob,
    plan_path: Option<String>,
) -> Result<RecurringJob, String> {
    let Some(raw) = plan_path else {
        return Ok(job);
    };
    let path = Some(raw.trim().to_string()).filter(|p| !p.is_empty());
    recurring::update_job(&d.recurring_root, &job.id, |j| j.plan_path = path)
}

async fn create_wake(State(d): State<Shared>, Json(req): Json<CreateReq>) -> Response {
    let owner = req.owner.trim().to_string();
    let kind = match agent_wakes::resolve_owner(&d.store, &owner) {
        Ok(kind) => kind,
        Err(error) => return api_error(StatusCode::NOT_FOUND, error),
    };
    let job = agent_wakes::set(
        &d.recurring_root,
        &owner,
        kind,
        req.spec,
        Setter::User,
        now(),
    )
    .and_then(|job| apply_plan_path(&d, job, req.plan_path));
    match job {
        Ok(job) => Json(every_view(&d, &job)).into_response(),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct PatchReq {
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(flatten)]
    spec: WakeSpec,
}

fn patch_legacy(d: &Shared, job: &RecurringJob, spec: WakeSpec) -> Result<RecurringJob, String> {
    let schedule = match spec.schedule.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => Some(Schedule::parse(raw)?),
        _ => None,
    };
    let now = now();
    recurring::update_job(&d.recurring_root, &job.id, |job| {
        if let Some(title) = spec
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            job.title = title.to_string();
        }
        if let Some(brief) = spec.brief.as_deref() {
            job.prompt = brief.trim().to_string();
        }
        if let Some(schedule) = schedule {
            job.schedule = schedule;
            job.next_run_at = job.schedule.next_after(now);
            job.queued = false;
        }
        if let Some(enabled) = spec.enabled {
            job.enabled = enabled;
            if !enabled {
                job.queued = false;
            } else if job.next_run_at < now {
                job.next_run_at = job.schedule.next_after(now);
            }
        }
    })
}

async fn patch_wake(
    State(d): State<Shared>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<PatchReq>,
) -> Response {
    let found = match find(&d, &id) {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(&id),
        Err(response) => return response,
    };
    match found {
        Found::Once(_) => bad_request("a one-off wake cannot be edited; delete it instead"),
        Found::Watch(w) => {
            let Some(enabled) = req.spec.enabled else {
                return bad_request("a watch only takes `enabled`");
            };
            let patch = cw::WatchPatch {
                enabled: Some(enabled),
                ..cw::WatchPatch::default()
            };
            match d.store.update_watch(&w.id, &patch) {
                Ok(Some(updated)) => match watch_view(&d, &updated) {
                    Ok(v) => Json(v).into_response(),
                    Err(response) => response,
                },
                Ok(None) => not_found(&id),
                Err(error) => mission_error(error.to_string()),
            }
        }
        Found::Every(job) => {
            let mut spec = req.spec;
            let updated = match &job.wake {
                Some(wake) => {
                    spec.name = wake.name.clone();
                    agent_wakes::set(
                        &d.recurring_root,
                        &wake.owner,
                        wake.owner_kind,
                        spec,
                        Setter::User,
                        now(),
                    )
                }
                None => patch_legacy(&d, &job, spec),
            }
            .and_then(|job| apply_plan_path(&d, job, req.plan_path));
            match updated {
                Ok(job) => Json(every_view(&d, &job)).into_response(),
                Err(error) => mission_error(error),
            }
        }
    }
}

async fn delete_wake(State(d): State<Shared>, AxumPath(id): AxumPath<String>) -> Response {
    let found = match find(&d, &id) {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(&id),
        Err(response) => return response,
    };
    let deleted = match found {
        Found::Every(job) => recurring::delete_job(&d.recurring_root, &job.id).map(|_| true),
        Found::Once(f) => d
            .store
            .cancel_followup(&f.session_id, &f.id)
            .map_err(|e| e.to_string()),
        Found::Watch(w) => {
            let removed = d.store.delete_watch(&w.id).map_err(|e| e.to_string());
            if matches!(removed, Ok(true)) {
                cw::remove_watch_files(&d.lead_root, &w.owner, &w.name);
            }
            removed
        }
    };
    match deleted {
        Ok(true) => Json(json!({"id": id, "deleted": true})).into_response(),
        Ok(false) => not_found(&id),
        Err(error) => mission_error(error),
    }
}

#[derive(Deserialize)]
struct RunsQuery {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

async fn list_runs(State(d): State<Shared>, Query(q): Query<RunsQuery>) -> Response {
    let store = match recurring::store_at(&d.recurring_root) {
        Ok(store) => store,
        Err(error) => return mission_error(error),
    };
    let owner = q.owner.as_deref().map(str::trim).filter(|o| !o.is_empty());
    match store.list_recurring_runs(None, owner, q.limit.unwrap_or(50).clamp(1, 500)) {
        Ok(runs) => Json(json!({"runs": runs})).into_response(),
        Err(error) => mission_error(error.to_string()),
    }
}

async fn wake_runs(
    State(d): State<Shared>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<RunsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(20).clamp(1, 500);
    match find(&d, &id) {
        Ok(Some(Found::Every(job))) => {
            let store = match recurring::store_at(&d.recurring_root) {
                Ok(store) => store,
                Err(error) => return mission_error(error),
            };
            match store.list_recurring_runs(Some(&job.id), None, limit) {
                Ok(runs) => Json(json!({"runs": runs})).into_response(),
                Err(error) => mission_error(error.to_string()),
            }
        }
        Ok(Some(Found::Watch(w))) => match d.store.list_watch_events(&w.id, limit) {
            Ok(events) => {
                let events: Vec<Value> = events
                    .into_iter()
                    .map(|e| json!({"kind": e.kind, "at": e.at, "excerpt": e.excerpt}))
                    .collect();
                Json(json!({"events": events})).into_response()
            }
            Err(error) => mission_error(error.to_string()),
        },
        Ok(Some(Found::Once(_))) => Json(json!({"runs": []})).into_response(),
        Ok(None) => not_found(&id),
        Err(response) => response,
    }
}

async fn wake_script(State(d): State<Shared>, AxumPath(id): AxumPath<String>) -> Response {
    match find(&d, &id) {
        Ok(Some(Found::Watch(w))) => match std::fs::read_to_string(&w.script_path) {
            Ok(script) => Json(json!({"script": script})).into_response(),
            Err(_) => api_error(StatusCode::NOT_FOUND, "script file is gone"),
        },
        Ok(Some(_)) => bad_request("only a watch has a script"),
        Ok(None) => not_found(&id),
        Err(response) => response,
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::{delete, post};

    #[test]
    fn routes_do_not_clash_with_agent_routes() {
        async fn stub() {}
        let _ = super::router()
            .route("/agents/{agent_id}/board", axum::routing::get(stub))
            .route("/agents/{agent_id}/status", post(stub))
            .route("/agents/{agent_id}", delete(stub));
    }
}
