use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::llm::NativeToolDefinition;
use crate::serve::{Daemon, Shared, api_error};
use crate::session::{
    SessionRole, read_session_meta, read_session_sidecar, read_session_sidecar_in,
    session_id_for_state_path, state_path_for_id,
};
use crate::tools::{ToolContext, ToolRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seat {
    MissionControl,
    Desk,
    Work,
}

impl Seat {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MissionControl => "mission_control",
            Self::Desk => "desk",
            Self::Work => "work",
        }
    }

    pub(crate) fn allowlist(self) -> &'static [&'static str] {
        match self {
            Self::Work => &[
                "record_coordination_note",
                "list_coordination_agents",
                "list_profiles",
                "keep_warm",
                "context_limit",
                "reach_policy",
                "schedule_wake",
                "wake_schedule",
                "watch",
            ],
            Self::Desk => &[
                "record_coordination_note",
                "list_coordination_agents",
                "keep_warm",
                "context_limit",
                "reach_policy",
                "share_artifact",
            ],
            Self::MissionControl => &[
                "list_coordination_agents",
                "list_profiles",
                "keep_warm",
                "context_limit",
                "reach_policy",
                "register_agent",
            ],
        }
    }
}

pub(crate) struct ResolvedSessionTools {
    pub(crate) seat: Seat,
    pub(crate) session_id: String,
    pub(crate) context: ToolContext,
    pub(crate) registry: ToolRegistry,
}

#[derive(Deserialize)]
pub(crate) struct ToolsListQuery {
    #[serde(default)]
    session_id: String,
}

#[derive(Deserialize)]
pub(crate) struct ToolRunReq {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    tool: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Serialize)]
struct ToolsListResponse {
    session_id: String,
    seat: &'static str,
    tools: Vec<NativeToolDefinition>,
}

pub(crate) fn build_session_tools(
    d: &Daemon,
    raw_session_id: &str,
) -> Result<ResolvedSessionTools, Response> {
    let session_id = raw_session_id.trim();
    if session_id.is_empty() {
        return Err(api_error(StatusCode::BAD_REQUEST, "session_id is required"));
    }
    let sp = state_path_for_id(session_id).ok_or_else(|| {
        api_error(
            StatusCode::BAD_REQUEST,
            format!("invalid session id `{session_id}`"),
        )
    })?;
    let canonical_id = session_id_for_state_path(&sp);
    let row = d
        .store
        .get_session_row(&canonical_id)
        .ok()
        .flatten()
        .or_else(|| d.store.get_session_row(session_id).ok().flatten());
    let sidecar = read_session_sidecar_in(&d.store, &sp).or_else(|| read_session_sidecar(&sp));
    let exa_api_key = d.config.lock().unwrap().exa_api_key.clone();
    let store_path = d
        .store
        .path()
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::store::default_db_path);

    if crate::mission_control::is_session_id(&canonical_id)
        || sidecar
            .as_ref()
            .is_some_and(|s| s.role == SessionRole::MissionControl)
    {
        let context = ToolContext::mission_control(d.mission_control_root.clone())
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e))?
            .with_durable_session_id(canonical_id.clone())
            .with_store_path(store_path)
            .with_agent_id(crate::mission_control::SESSION_ID);
        let registry = crate::session::mission_control_tool_registry(exa_api_key);
        return Ok(ResolvedSessionTools {
            seat: Seat::MissionControl,
            session_id: canonical_id,
            context,
            registry,
        });
    }

    if crate::session::is_inbox_session_id(&canonical_id)
        || sidecar
            .as_ref()
            .is_some_and(|s| s.role == SessionRole::Coordination)
    {
        let agent_id = sidecar
            .as_ref()
            .and_then(|s| s.agent_id.clone())
            .or_else(|| canonical_id.strip_prefix("inbox-").map(str::to_string))
            .ok_or_else(|| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    "desk session requires an agent identity",
                )
            })?;
        let workspace = row
            .as_ref()
            .map(|r| PathBuf::from(&r.workspace))
            .filter(|p| !p.as_os_str().is_empty() && p.is_dir())
            .or_else(|| {
                crate::coordination::AgentHome::new(
                    crate::coordination::agents_root(&d.mission_control_root),
                    &agent_id,
                )
                .ok()
                .map(|home| home.root().to_path_buf())
            })
            .unwrap_or_else(|| d.mission_control_root.clone());
        let context = ToolContext::new(workspace)
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e))?
            .with_durable_session_id(canonical_id.clone())
            .with_store_path(store_path)
            .with_agent_id(agent_id);
        let registry = crate::session::coordination_tool_registry(exa_api_key);
        return Ok(ResolvedSessionTools {
            seat: Seat::Desk,
            session_id: canonical_id,
            context,
            registry,
        });
    }

    let workspace_str = row
        .as_ref()
        .map(|r| r.workspace.clone())
        .or_else(|| read_session_meta(&sp).map(|m| m.workspace))
        .ok_or_else(|| {
            api_error(
                StatusCode::NOT_FOUND,
                format!("unknown session `{session_id}`"),
            )
        })?;
    if workspace_str.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "session workspace missing",
        ));
    }
    let agent_id = row
        .as_ref()
        .and_then(|r| r.agent_id.clone())
        .or_else(|| sidecar.and_then(|s| s.agent_id));
    let context = ToolContext::new(PathBuf::from(workspace_str))
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, e))?
        .with_durable_session_id(canonical_id.clone())
        .with_store_path(store_path)
        .with_agent_id_opt(agent_id.clone());
    let custom_dir = crate::agent_tools::tools_dir(agent_id.as_deref().unwrap_or("snippet"));
    let registry = crate::session::work_tool_registry(exa_api_key, custom_dir);
    Ok(ResolvedSessionTools {
        seat: Seat::Work,
        session_id: canonical_id,
        context,
        registry,
    })
}

pub(crate) fn allowed_definitions(
    seat: Seat,
    registry: &ToolRegistry,
) -> Vec<NativeToolDefinition> {
    let mut by_name: HashMap<String, NativeToolDefinition> = registry
        .definitions()
        .into_iter()
        .map(|def| (def.name.clone(), def))
        .collect();
    seat.allowlist()
        .iter()
        .filter_map(|name| by_name.remove(*name))
        .collect()
}

pub(crate) async fn list_tools(
    State(d): State<Shared>,
    Query(query): Query<ToolsListQuery>,
) -> Response {
    let resolved = match build_session_tools(&d, &query.session_id) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let tools = allowed_definitions(resolved.seat, &resolved.registry);
    (
        StatusCode::OK,
        Json(ToolsListResponse {
            session_id: resolved.session_id,
            seat: resolved.seat.as_str(),
            tools,
        }),
    )
        .into_response()
}

pub(crate) async fn run_tool(State(d): State<Shared>, Json(req): Json<ToolRunReq>) -> Response {
    let resolved = match build_session_tools(&d, &req.session_id) {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };
    let tool_name = req.tool.trim();
    if tool_name.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "tool name is required");
    }
    let allowed = allowed_definitions(resolved.seat, &resolved.registry);
    if !allowed.iter().any(|def| def.name == tool_name) {
        let names: Vec<&str> = allowed.iter().map(|def| def.name.as_str()).collect();
        return api_error(
            StatusCode::FORBIDDEN,
            format!(
                "tool `{tool_name}` is not allowed for seat `{}` (allowed tools: {})",
                resolved.seat.as_str(),
                names.join(", ")
            ),
        );
    }
    let arguments = match req.arguments {
        Value::Null => json!({}),
        Value::Object(map) => Value::Object(map),
        _ => {
            return api_error(
                StatusCode::BAD_REQUEST,
                format!("tool `{tool_name}` expects a JSON object for arguments"),
            );
        }
    };
    match resolved
        .registry
        .execute(&resolved.context, tool_name, arguments)
        .await
    {
        Ok(result) => {
            if result.value.get("status").and_then(Value::as_str) == Some("error") {
                let message = result
                    .value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("tool execution failed");
                return api_error(StatusCode::BAD_REQUEST, message);
            }
            (StatusCode::OK, Json(result.value)).into_response()
        }
        Err(err) => api_error(StatusCode::BAD_REQUEST, err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use std::sync::atomic::AtomicU64;
    use tower::ServiceExt;

    fn daemon_with_disk_store(dir: &Path) -> Shared {
        let store = crate::store::Store::open(dir.join("snippet.db")).unwrap();
        let mc_root = dir.join("mc");
        std::fs::create_dir_all(&mc_root).unwrap();
        let recurring_root = dir.join("recurring");
        std::fs::create_dir_all(&recurring_root).unwrap();
        std::sync::Arc::new(Daemon {
            config: std::sync::Mutex::new(crate::config::SnippetConfig::default()),
            config_path: dir.join("config.toml"),
            token: "secret-token".into(),
            hostname: "test".into(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            shells: crate::term::SessionTerms::new(dir.to_path_buf()),
            git_write: tokio::sync::Mutex::new(()),
            seen_nonces: std::sync::Mutex::new(HashMap::new()),
            queue_hidden: std::sync::Mutex::new(HashMap::new()),
            queue_revision: AtomicU64::new(0),
            mission_control_root: mc_root,
            store,
            coordination_events: tokio::sync::broadcast::channel(32).0,
            recurring_root,
        })
    }

    fn test_router(d: Shared) -> Router {
        Router::new()
            .route("/api/tools", get(list_tools))
            .route("/api/tools/run", post(run_tool))
            .layer(axum::middleware::from_fn_with_state(
                d.clone(),
                crate::serve::require_token,
            ))
            .with_state(d)
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn list_and_run_respect_seat_allowlist_and_token_auth() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let d = daemon_with_disk_store(dir.path());

        let state = crate::harness::HarnessState::blank(workspace.display().to_string(), None);
        d.store
            .import_session(
                "work-session-1",
                "project",
                &state,
                &crate::conversations::SessionExtras {
                    agent_id: Some("builder".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        let app = test_router(d.clone());

        let unauth = app
            .clone()
            .oneshot(
                Request::get("/api/tools?session_id=work-session-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        let work_list = app
            .clone()
            .oneshot(
                Request::get("/api/tools?token=secret-token&session_id=work-session-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(work_list.status(), StatusCode::OK);
        let work_json = body_json(work_list).await;
        assert_eq!(work_json["seat"], "work");
        let work_names: Vec<&str> = work_json["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(work_names.contains(&"record_coordination_note"));
        assert!(work_names.contains(&"schedule_wake"));
        assert!(!work_names.contains(&"bash"));
        assert!(!work_names.contains(&"register_agent"));

        let desk_list = app
            .clone()
            .oneshot(
                Request::get("/api/tools?token=secret-token&session_id=inbox-scout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(desk_list.status(), StatusCode::OK);
        let desk_json = body_json(desk_list).await;
        assert_eq!(desk_json["seat"], "desk");
        let desk_names: Vec<&str> = desk_json["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(desk_names.contains(&"share_artifact"));
        assert!(!desk_names.contains(&"schedule_wake"));

        let mc_list = app
            .clone()
            .oneshot(
                Request::get("/api/tools?token=secret-token&session_id=mission-control")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mc_list.status(), StatusCode::OK);
        let mc_json = body_json(mc_list).await;
        assert_eq!(mc_json["seat"], "mission_control");
        let mc_names: Vec<&str> = mc_json["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(mc_names.contains(&"register_agent"));
        assert!(mc_names.contains(&"list_profiles"));
        assert!(!mc_names.contains(&"record_coordination_note"));

        let denied = app
            .clone()
            .oneshot(
                Request::post("/api/tools/run?token=secret-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "session_id": "work-session-1",
                            "tool": "bash",
                            "arguments": {"command": "echo hi", "label": "test"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let denied_json = body_json(denied).await;
        let err = denied_json["error"].as_str().unwrap();
        assert!(err.contains("record_coordination_note"), "{err}");

        let run_resp = app
            .clone()
            .oneshot(
                Request::post("/api/tools/run?token=secret-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "session_id": "work-session-1",
                            "tool": "record_coordination_note",
                            "arguments": {"summary": "Verified CLI tool route on temp store"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(run_resp.status(), StatusCode::OK);
        let run_json = body_json(run_resp).await;
        assert_eq!(run_json["status"], "success");
        assert_eq!(run_json["data"]["recorded"], true);

        let board = d
            .store
            .read_board("builder", &crate::coordination::BoardQuery::default(), 10)
            .unwrap();
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].summary, "Verified CLI tool route on temp store");
    }
}
