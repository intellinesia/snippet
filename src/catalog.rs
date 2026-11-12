//! Live model catalog — query a provider's own models API so editors can offer
//! real model IDs and (where the provider reports them) real capabilities,
//! instead of a hardcoded list that goes stale.
//!
//! Coverage is uneven by design, because the providers are uneven (verified
//! against live docs, July 2026):
//! - Anthropic: full — `GET /v1/models` returns a `capabilities` tree with per-
//!   level effort flags (low/medium/high/xhigh/max), thinking types, image
//!   input, and `max_input_tokens`.
//! - OpenRouter: partial — `supported_parameters` says whether `reasoning` /
//!   `reasoning_effort` are accepted (not which levels) + `context_length`.
//! - OpenAI-compatible: IDs only — `/models` carries no capability metadata.
//! - Gemini: ListModels has `thinking: bool` + token limits, no levels.
//! - ChatGPT (Codex subscription): no catalog endpoint; we return the known
//!   Codex slugs so the editor can still offer a picker.
//!
//! Anything unknown stays `None` — the runtime effort auto-degrade in the
//! harness is the universal fallback for what discovery can't tell us.

use serde::Serialize;
use serde_json::Value;

use crate::config::InferenceProfileConfig;

#[derive(Debug, Clone, Serialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Input context window in tokens, when the provider reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Effort tiers the provider explicitly supports, ladder order. Only
    /// Anthropic reports this; `None` means "unknown", not "unsupported".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Vec<String>>,
    /// Whether the model supports reasoning/thinking at all; `None` = unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_images: Option<bool>,
}

fn catalog_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

/// Fetch the provider's model list for a profile. Returns a normalized list
/// (possibly empty for providers with no catalog endpoint) or a human-readable
/// error. The API key never leaves this process.
pub async fn fetch_models(cfg: &InferenceProfileConfig) -> Result<Vec<CatalogModel>, String> {
    let mut models = fetch_provider_models(cfg).await?;
    if matches!(
        cfg.provider.as_str(),
        "chatgpt" | "claude-code" | "antigravity" | "devin" | "gemini" | "openai" | "xai" | "grok"
    ) {
        for model in &mut models {
            if matches!(cfg.provider.as_str(), "claude-code" | "devin") && model.display_name.is_some() && model.efforts.is_some() {
                continue;
            }
            let spec = crate::reasoning::spec(&cfg.provider, &model.id);
            model.reasoning = Some(spec.control != crate::reasoning::Control::None);
            model.efforts = Some(spec.options.iter().map(|o| o.to_string()).collect());
        }
    }
    Ok(models)
}

async fn fetch_provider_models(cfg: &InferenceProfileConfig) -> Result<Vec<CatalogModel>, String> {
    match cfg.provider.as_str() {
        "anthropic" | "anthropic-compatible" => fetch_anthropic(cfg).await,
        "openai" | "openai-compatible" => fetch_openai_compatible(cfg).await,
        // The openrouter provider pins its base URL in build_model; mirror that
        // here so a fresh editor draft (empty base_url) still resolves.
        "openrouter" => {
            let mut c = cfg.clone();
            c.base_url = "https://openrouter.ai/api/v1".to_string();
            fetch_openai_compatible(&c).await
        }
        "opencode-zen" => {
            let mut c = cfg.clone();
            c.base_url = "https://opencode.ai/zen/v1".to_string();
            fetch_openai_compatible(&c).await
        }
        "opencode-go" => {
            let mut c = cfg.clone();
            c.base_url = "https://opencode.ai/zen/go/v1".to_string();
            fetch_openai_compatible(&c).await
        }
        "xai" | "grok" => {
            let mut c = cfg.clone();
            c.base_url = "https://api.x.ai/v1".to_string();
            if let Ok(token) = crate::xai_auth::access_token().await.map(|(_, t)| t) {
                c.api_key = token;
            }
            fetch_openai_compatible(&c).await
        }
        "gemini" => fetch_gemini(cfg).await,
        // Codex subscription backend has no models endpoint — offer the
        // known slugs so the editor picker is not empty.
        "chatgpt" => Ok(chatgpt_catalog()),
        "claude-code" => Ok(claude_code_live_catalog().await.unwrap_or_else(|_| claude_code_catalog())),
        "antigravity" => Ok(crate::antigravity::models()
            .await?
            .into_iter()
            .map(|id| CatalogModel {
                id,
                display_name: None,
                context_window: None,
                efforts: None,
                reasoning: Some(true),
                supports_images: Some(true),
            })
            .collect()),
        "devin" => Ok(devin_live_catalog().await.unwrap_or_else(|_| devin_catalog())),
        other => Err(format!("no model catalog for provider `{other}`")),
    }
}

type CachedModels = (std::time::Instant, Option<String>, Vec<CatalogModel>);
static CLAUDE_CODE_MODELS: std::sync::Mutex<Option<CachedModels>> = std::sync::Mutex::new(None);
static CLAUDE_CODE_FETCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cached_claude_code_models(account: &Option<String>) -> Option<Result<Vec<CatalogModel>, String>> {
    let guard = CLAUDE_CODE_MODELS.lock().unwrap();
    let (at, acct, models) = guard.as_ref()?;
    if acct != account {
        return None;
    }
    // An empty entry is a failed lookup, kept briefly so the fallback answers at once.
    let ttl = if models.is_empty() { 60 } else { 600 };
    (at.elapsed() < std::time::Duration::from_secs(ttl)).then(|| {
        if models.is_empty() {
            Err("Claude Code model lookup failed recently".to_string())
        } else {
            Ok(models.clone())
        }
    })
}

async fn claude_code_live_catalog() -> Result<Vec<CatalogModel>, String> {
    let account = crate::accounts::active("claude-code");
    if let Some(hit) = cached_claude_code_models(&account) {
        return hit;
    }
    let _one_fetch = CLAUDE_CODE_FETCH.lock().await;
    if let Some(hit) = cached_claude_code_models(&account) {
        return hit;
    }
    let fetched = crate::harness::claude_supported_models()
        .await
        .map(|(_, raw)| parse_claude_code_models(&raw));
    let models = match &fetched {
        Ok(m) => m.clone(),
        Err(_) => Vec::new(),
    };
    *CLAUDE_CODE_MODELS.lock().unwrap() = Some((std::time::Instant::now(), account, models.clone()));
    match fetched {
        Err(e) => Err(e),
        Ok(_) if models.is_empty() => Err("Claude Code listed no models".into()),
        Ok(_) => Ok(models),
    }
}

fn parse_claude_code_models(raw: &[Value]) -> Vec<CatalogModel> {
    raw.iter()
        .filter_map(|m| {
            let id = m.get("value").and_then(Value::as_str)?.trim();
            if id.is_empty() {
                return None;
            }
            let name = m.get("displayName").and_then(Value::as_str).unwrap_or(id);
            let display_name = match m.get("description").and_then(Value::as_str) {
                Some(d) if !d.is_empty() => format!("{name} · {d}"),
                _ => name.to_string(),
            };
            let efforts = m
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|l| l.iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>())
                .unwrap_or_default();
            Some(CatalogModel {
                id: id.to_string(),
                display_name: Some(display_name),
                context_window: None,
                reasoning: Some(!efforts.is_empty() || m.get("supportsAdaptiveThinking").and_then(Value::as_bool) == Some(true)),
                efforts: Some(efforts),
                supports_images: Some(true),
            })
        })
        .collect()
}

static DEVIN_MODELS: std::sync::Mutex<Option<CachedModels>> = std::sync::Mutex::new(None);
static DEVIN_FETCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cached_devin_models(account: &Option<String>) -> Option<Result<Vec<CatalogModel>, String>> {
    let guard = DEVIN_MODELS.lock().unwrap();
    let (at, acct, models) = guard.as_ref()?;
    if acct != account {
        return None;
    }
    let ttl = if models.is_empty() { 60 } else { 600 };
    (at.elapsed() < std::time::Duration::from_secs(ttl)).then(|| {
        if models.is_empty() {
            Err("Devin model lookup failed recently".to_string())
        } else {
            Ok(models.clone())
        }
    })
}

async fn devin_live_catalog() -> Result<Vec<CatalogModel>, String> {
    let account = crate::accounts::active("devin");
    if let Some(hit) = cached_devin_models(&account) {
        return hit;
    }
    let _one_fetch = DEVIN_FETCH.lock().await;
    if let Some(hit) = cached_devin_models(&account) {
        return hit;
    }
    let fetched = crate::devin::models_json()
        .await
        .map(|(_, raw)| parse_devin_models(&raw));
    let models = match &fetched {
        Ok(m) => m.clone(),
        Err(_) => Vec::new(),
    };
    *DEVIN_MODELS.lock().unwrap() = Some((std::time::Instant::now(), account, models.clone()));
    match fetched {
        Err(e) => Err(e),
        Ok(_) if models.is_empty() => Err("Devin listed no models".into()),
        Ok(_) => Ok(models),
    }
}

fn parse_devin_models(raw: &Value) -> Vec<CatalogModel> {
    let mut out = Vec::new();
    let Some(families) = raw.get("families").and_then(Value::as_array) else {
        return out;
    };
    for family in families {
        let slug = family.get("slug").and_then(Value::as_str).unwrap_or("").trim();
        if slug == "fusion" {
            continue;
        }
        let family_label = family.get("family_label").and_then(Value::as_str).unwrap_or(slug).trim();
        let variants = family.get("variants").and_then(Value::as_array).cloned().unwrap_or_default();
        if !slug.is_empty() && variants.len() > 1 && !out.iter().any(|m: &CatalogModel| m.id == slug) {
            let max_ctx = variants.iter().filter_map(|v| v.get("max_context_tokens").and_then(Value::as_u64)).max();
            let spec = crate::reasoning::spec("devin", slug);
            let efforts = (spec.control == crate::reasoning::Control::Effort)
                .then(|| spec.options.iter().map(|s| s.to_string()).collect());
            out.push(CatalogModel {
                id: slug.to_string(),
                display_name: Some(family_label.to_string()),
                context_window: max_ctx,
                efforts,
                reasoning: Some(true),
                supports_images: Some(!slug.starts_with("swe-2")),
            });
        }
        for v in variants {
            let Some(uid) = v.get("model_uid").and_then(Value::as_str).map(str::trim) else { continue };
            if uid.is_empty() || uid.starts_with("MODEL_") || out.iter().any(|m: &CatalogModel| m.id == uid) {
                continue;
            }
            let label = v.get("label").and_then(Value::as_str).unwrap_or(uid);
            let display_name = match v.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) {
                Some(desc) => format!("{label} · {desc}"),
                None => label.to_string(),
            };
            out.push(CatalogModel {
                id: uid.to_string(),
                display_name: Some(display_name),
                context_window: v.get("max_context_tokens").and_then(Value::as_u64),
                efforts: Some(Vec::new()),
                reasoning: Some(true),
                supports_images: Some(!uid.starts_with("swe-2")),
            });
        }
    }
    out
}

fn devin_catalog() -> Vec<CatalogModel> {
    [
        ("swe-2", "SWE-2", None, Some(vec!["medium".into(), "high".into(), "max".into()]), false),
        ("swe-2-high", "SWE-2 High", None, Some(Vec::new()), false),
        ("swe-2-medium", "SWE-2 Medium", None, Some(Vec::new()), false),
        ("swe-2-max", "SWE-2 Max", None, Some(Vec::new()), false),
        ("swe-2-flash", "SWE-2 Flash", Some(262_144), Some(vec!["medium".into(), "high".into(), "max".into()]), false),
        ("swe-2-flash-medium", "SWE-2 Flash Medium", Some(262_144), Some(Vec::new()), false),
        ("adaptive", "Adaptive", None, Some(Vec::new()), true),
        ("opus", "Claude Opus", None, Some(Vec::new()), true),
        ("sonnet", "Claude Sonnet", None, Some(Vec::new()), true),
        ("gpt", "GPT", None, Some(Vec::new()), true),
    ]
    .into_iter()
    .map(|(id, name, context_window, efforts, images)| CatalogModel {
        id: id.to_string(),
        display_name: Some(name.to_string()),
        context_window,
        efforts,
        reasoning: Some(true),
        supports_images: Some(images),
    })
    .collect()
}

fn claude_code_catalog() -> Vec<CatalogModel> {
    ["opus", "sonnet", "haiku"]
        .iter()
        .map(|id| CatalogModel {
            id: id.to_string(),
            display_name: None,
            context_window: None,
            efforts: Some(["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec()),
            reasoning: Some(true),
            supports_images: Some(true),
        })
        .collect()
}

fn chatgpt_catalog() -> Vec<CatalogModel> {
    const IDS: &[&str] = &[
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.1-codex",
        "gpt-5.4-mini",
    ];
    IDS.iter()
        .map(|id| CatalogModel {
            id: id.to_string(),
            display_name: None,
            context_window: None,
            efforts: None,
            reasoning: Some(true),
            supports_images: Some(true),
        })
        .collect()
}

/// `…/v1/messages` (from the shared URL builder) → `…/v1/models`, so catalog
/// requests honour the same base-URL quirks the chat path already handles.
fn anthropic_models_url(base_url: &str) -> String {
    let messages = crate::anthropic::anthropic_messages_url(base_url);
    format!("{}models?limit=1000", messages.trim_end_matches("messages"))
}

async fn fetch_anthropic(cfg: &InferenceProfileConfig) -> Result<Vec<CatalogModel>, String> {
    let url = anthropic_models_url(&cfg.base_url);
    let response = catalog_client()
        .get(&url)
        .header("x-api-key", cfg.api_key.trim())
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|e| format!("model list request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::llm::humanize_http_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad model list JSON: {e}"))?;
    let data = v["data"].as_array().cloned().unwrap_or_default();
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let caps = &m["capabilities"];
            // Ladder order; a level counts only when explicitly supported.
            let efforts: Vec<String> = ["low", "medium", "high", "xhigh", "max"]
                .iter()
                .filter(|l| caps["effort"][**l]["supported"] == Value::Bool(true))
                .map(|l| l.to_string())
                .collect();
            let effort_supported = caps["effort"]["supported"].as_bool();
            let thinking = caps["thinking"]["supported"].as_bool();
            let reasoning = match (effort_supported, thinking) {
                (None, None) => None,
                (e, t) => Some(e.unwrap_or(false) || t.unwrap_or(false)),
            };
            Some(CatalogModel {
                id,
                display_name: m["display_name"].as_str().map(str::to_string),
                context_window: m["max_input_tokens"].as_u64().filter(|n| *n > 0),
                efforts: effort_supported.map(|_| efforts),
                reasoning,
                supports_images: caps["image_input"]["supported"].as_bool(),
            })
        })
        .collect())
}

/// `…/chat/completions` (from the shared URL builder) → `…/models`. Works for
/// OpenAI, OpenRouter, NVIDIA NIM, DeepSeek, and most compatible gateways.
fn openai_models_url(base_url: &str) -> String {
    let chat = crate::openai::openai_chat_url(base_url);
    format!("{}models", chat.trim_end_matches("chat/completions"))
}

async fn fetch_openai_compatible(cfg: &InferenceProfileConfig) -> Result<Vec<CatalogModel>, String> {
    let url = openai_models_url(&cfg.base_url);
    let is_openrouter = cfg.base_url.to_ascii_lowercase().contains("openrouter");
    let mut req = catalog_client().get(&url);
    if !cfg.api_key.trim().is_empty() {
        req = req.bearer_auth(cfg.api_key.trim());
    }
    let response = req
        .send()
        .await
        .map_err(|e| format!("model list request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::llm::humanize_http_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad model list JSON: {e}"))?;
    let data = v["data"].as_array().cloned().unwrap_or_default();
    Ok(data
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            // OpenRouter enriches the standard shape with capability hints;
            // plain OpenAI-compatible endpoints return bare IDs.
            let (reasoning, context_window) = if is_openrouter {
                let params = m["supported_parameters"].as_array();
                let reasoning = params.map(|p| {
                    p.iter()
                        .any(|x| matches!(x.as_str(), Some("reasoning") | Some("reasoning_effort")))
                });
                (reasoning, m["context_length"].as_u64())
            } else {
                (None, None)
            };
            Some(CatalogModel {
                id,
                display_name: m["name"].as_str().map(str::to_string),
                context_window,
                efforts: None,
                reasoning,
                supports_images: None,
            })
        })
        .collect())
}

async fn fetch_gemini(cfg: &InferenceProfileConfig) -> Result<Vec<CatalogModel>, String> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000&key={}",
        cfg.api_key.trim()
    );
    let response = catalog_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("model list request failed: {e}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(crate::llm::humanize_http_error(status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("bad model list JSON: {e}"))?;
    let data = v["models"].as_array().cloned().unwrap_or_default();
    Ok(data
        .iter()
        .filter_map(|m| {
            // "models/gemini-3.5-flash" → "gemini-3.5-flash"
            let id = m["name"]
                .as_str()?
                .trim_start_matches("models/")
                .to_string();
            // Only generateContent-capable models are usable as a chat model.
            let methods = m["supportedGenerationMethods"].as_array();
            if let Some(methods) = methods
                && !methods
                    .iter()
                    .any(|x| x.as_str() == Some("generateContent"))
                {
                    return None;
                }
            Some(CatalogModel {
                id,
                display_name: m["displayName"].as_str().map(str::to_string),
                context_window: m["inputTokenLimit"].as_u64(),
                efforts: None,
                reasoning: m["thinking"].as_bool(),
                supports_images: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_models_keep_cli_names_and_effort_levels() {
        let raw: Vec<Value> = serde_json::from_str(
            r#"[
              {"value":"default","displayName":"Default (recommended)","description":"Opus 5.5 · Best for everyday, complex tasks","supportedEffortLevels":["low","medium","high","xhigh","max"]},
              {"value":"claude-opus-4-6","displayName":"Opus 4.6","description":"","supportedEffortLevels":["low","medium","high","max"]},
              {"value":"claude-haiku-4-5-20251001","displayName":"Haiku 4.5"},
              {"value":"  ","displayName":"blank"}
            ]"#,
        )
        .unwrap();
        let models = parse_claude_code_models(&raw);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["default", "claude-opus-4-6", "claude-haiku-4-5-20251001"]);
        assert_eq!(models[0].display_name.as_deref(), Some("Default (recommended) · Opus 5.5 · Best for everyday, complex tasks"));
        assert_eq!(models[1].display_name.as_deref(), Some("Opus 4.6"));
        assert_eq!(models[1].efforts.as_deref().unwrap(), ["low", "medium", "high", "max"]);
        assert_eq!(models[2].efforts.as_deref(), Some(&[][..]));
        assert_eq!(models[2].reasoning, Some(false));
    }

    #[test]
    fn devin_models_parse_swe2_families_variants_and_context_window() {
        let raw: Value = serde_json::from_str(
            r#"{
              "default_model": "adaptive",
              "families": [
                {
                  "slug": "swe-2",
                  "family_label": "SWE-2",
                  "variants": [
                    {"model_uid": "swe-2-high", "label": "SWE-2 High", "description": "Default SWE-2"},
                    {"model_uid": "swe-2-medium", "label": "SWE-2 Medium", "description": "Faster"}
                  ]
                },
                {
                  "slug": "swe-2-flash",
                  "family_label": "SWE-2 Flash",
                  "variants": [
                    {"model_uid": "swe-2-flash-medium", "label": "SWE-2 Flash Medium", "max_context_tokens": 262144},
                    {"model_uid": "swe-2-flash-high", "label": "SWE-2 Flash High", "max_context_tokens": 262144}
                  ]
                },
                {
                  "slug": "fusion",
                  "family_label": "Fusion",
                  "variants": [{"model_uid": "fusion-claude-gpt", "label": "Fusion"}]
                }
              ]
            }"#,
        )
        .unwrap();
        let models = parse_devin_models(&raw);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["swe-2", "swe-2-high", "swe-2-medium", "swe-2-flash", "swe-2-flash-medium", "swe-2-flash-high"]
        );
        let swe2 = models.iter().find(|m| m.id == "swe-2").unwrap();
        assert_eq!(swe2.efforts.as_deref(), Some(&["medium".into(), "high".into(), "max".into()][..]));
        assert_eq!(swe2.supports_images, Some(false));
        let flash = models.iter().find(|m| m.id == "swe-2-flash").unwrap();
        assert_eq!(flash.context_window, Some(262_144));

        let fallback = devin_catalog();
        assert!(fallback.iter().any(|m| m.id == "swe-2"));
        assert!(fallback.iter().any(|m| m.id == "adaptive"));
    }

    #[tokio::test]
    #[ignore = "spawns the local Claude Code CLI"]
    async fn claude_code_live_catalog_lists_models() {
        let models = claude_code_live_catalog().await.unwrap();
        assert!(models.iter().any(|m| m.id == "opus"), "{models:?}");
    }
}
