use super::api_error;
use super::*;

#[derive(Deserialize)]
pub(crate) struct AccountReq {
    provider: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    order: Option<Vec<String>>,
}

fn pooled(provider: &str) -> Result<(), Response> {
    if crate::accounts::is_pooled(provider) {
        Ok(())
    } else {
        Err(api_error(StatusCode::BAD_REQUEST, "provider must be chatgpt, xai, claude-code or devin"))
    }
}

fn listing() -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for provider in crate::accounts::PROVIDERS {
        out.insert(provider.to_string(), serde_json::to_value(crate::accounts::list(provider)).unwrap_or_default());
    }
    serde_json::Value::Object(out)
}

pub(crate) async fn list_accounts() -> Response {
    Json(listing()).into_response()
}

pub(crate) async fn add_account(
    Json(req): Json<AccountReq>,
) -> Response {
    if let Err(r) = pooled(&req.provider) {
        return r;
    }
    let provider = req.provider.clone();
    let id = crate::accounts::new_id(&provider);
    let label = req.label.clone().unwrap_or_default();
    match provider.as_str() {
        "claude-code" => {
            let token = req.token.as_deref().map(str::trim).unwrap_or("");
            if !token.starts_with("sk-ant-") {
                return api_error(StatusCode::BAD_REQUEST, "paste the token `claude setup-token` prints (it starts with sk-ant-)");
            }
            let body = serde_json::json!({ "token": token }).to_string();
            if let Err(e) = crate::accounts::write_credential(&provider, &id, &body) {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e);
            }
            crate::accounts::set_label(&provider, &id, &label);
            Json(serde_json::json!({ "id": id, "accounts": listing() })).into_response()
        }
        "devin" => {
            let raw = req.token.as_deref().unwrap_or("");
            let Some(token) = crate::devin::parse_credential_token(raw) else {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "paste the `windsurf_api_key` token (or full credentials.toml) from `~/.local/share/devin/credentials.toml`",
                );
            };
            let body = serde_json::json!({ "token": token }).to_string();
            if let Err(e) = crate::accounts::write_credential(&provider, &id, &body) {
                return api_error(StatusCode::INTERNAL_SERVER_ERROR, e);
            }
            crate::accounts::set_label(&provider, &id, &label);
            Json(serde_json::json!({ "id": id, "accounts": listing() })).into_response()
        }
        "chatgpt" => match crate::chatgpt_auth::begin_device_code_login().await {
            Ok(device) => {
                let user_code = device.user_code.clone();
                let url = device.verification_url.clone();
                let target = id.clone();
                tokio::spawn(async move {
                    if let Ok(tokens) = crate::chatgpt_auth::complete_device_code_login(device).await
                        && crate::chatgpt_auth::save_account(&target, &tokens).is_ok() {
                            crate::accounts::set_label("chatgpt", &target, &label);
                        }
                });
                Json(serde_json::json!({ "id": id, "user_code": user_code, "verification_uri": url })).into_response()
            }
            Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
        },
        _ => match crate::xai_auth::begin_device_code_login().await {
            Ok(device) => {
                let poll = device.clone();
                let target = id.clone();
                tokio::spawn(async move {
                    if let Ok(tokens) = crate::xai_auth::poll_for_tokens(poll).await
                        && crate::xai_auth::save_account(&target, &tokens).is_ok() {
                            crate::accounts::set_label("xai", &target, &label);
                        }
                });
                Json(serde_json::json!({
                    "id": id,
                    "user_code": device.user_code,
                    "verification_uri": device.verification_uri,
                    "expires_in": device.expires_in_s,
                }))
                .into_response()
            }
            Err(e) => api_error(StatusCode::BAD_GATEWAY, e),
        },
    }
}

pub(crate) async fn remove_account(
    Json(req): Json<AccountReq>,
) -> Response {
    if let Err(r) = pooled(&req.provider) {
        return r;
    }
    let Some(id) = req.id.as_deref().filter(|id| !id.is_empty()) else {
        return api_error(StatusCode::BAD_REQUEST, "id required");
    };
    match crate::accounts::remove(&req.provider, id) {
        Ok(()) => Json(listing()).into_response(),
        Err(e) => api_error(StatusCode::BAD_REQUEST, e),
    }
}

pub(crate) async fn update_account(
    Json(req): Json<AccountReq>,
) -> Response {
    if let Err(r) = pooled(&req.provider) {
        return r;
    }
    if let Some(order) = req.order.clone() {
        crate::accounts::set_order(&req.provider, order);
    }
    if let (Some(id), Some(label)) = (req.id.as_deref(), req.label.as_deref()) {
        crate::accounts::set_label(&req.provider, id, label);
    }
    Json(listing()).into_response()
}
