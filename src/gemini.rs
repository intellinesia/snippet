//! Native Gemini adapter speaking `generateContent` / `streamGenerateContent`.

use async_trait::async_trait;
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, sleep};

use crate::llm::{
    AgentModel, GeneratedToolCall, HarnessMessage, ModelOutput, NativeToolDefinition, StreamBuffer,
    StreamHandle, TokenUsage,
};
use crate::tools::ToolError;

const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Google's documented skip sentinel for replayed functionCall parts on Gemini 3+
/// (base64 of "skip_thought_signature_validator") — 2.5 doesn't require it.
const SKIP_THOUGHT_SIGNATURE: &str = "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I=";

#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub api_key: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub max_retries: u32,
    pub initial_retry_ms: u64,
    pub max_retry_ms: u64,
    pub supports_images: bool,
    pub reasoning_effort: Option<String>,
}

pub struct GeminiModel {
    config: GeminiConfig,
    client: reqwest::Client,
    skip_thought_signatures: bool,
}

impl GeminiModel {
    pub fn new(config: GeminiConfig) -> Self {
        Self {
            config,
            client: crate::llm::model_http_client(None),
            skip_thought_signatures: false,
        }
    }
}

#[async_trait]
impl AgentModel for GeminiModel {
    fn is_configured(&self) -> bool {
        !self.config.api_key.trim().is_empty()
    }

    fn swap_reasoning_effort(&mut self, effort: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.config.reasoning_effort, effort)
    }

    fn supports_images(&self) -> bool {
        self.config.supports_images
    }

    async fn generate(
        &mut self,
        messages: &[HarnessMessage],
        tools: &[NativeToolDefinition],
        force_tool: bool,
        sink: Option<StreamHandle>,
    ) -> Result<ModelOutput, ToolError> {
        let (system, contents) =
            build_contents(messages, &self.config.model, self.skip_thought_signatures);

        let mut body = serde_json::Map::new();
        body.insert(
            "system_instruction".to_string(),
            json!({ "parts": [{ "text": system }] }),
        );
        body.insert("contents".to_string(), Value::Array(contents));
        body.insert("safetySettings".to_string(), safety_settings());
        if !tools.is_empty() {
            let declarations: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        // Gemini's function schema is a strict OpenAPI subset: it
                        // rejects JSON Schema keywords like `additionalProperties`.
                        // Strip everything it doesn't understand.
                        "parameters": normalize_gemini_function_schema(tool.input_schema.clone()),
                    })
                })
                .collect();
            body.insert(
                "tools".to_string(),
                json!([{ "functionDeclarations": declarations }]),
            );
            let mode = if force_tool { "ANY" } else { "AUTO" };
            body.insert(
                "toolConfig".to_string(),
                json!({ "functionCallingConfig": { "mode": mode } }),
            );
        }
        let mut generation_config = serde_json::Map::new();
        if let Some(temperature) = self.config.temperature {
            generation_config.insert("temperature".to_string(), json!(temperature));
        }
        // Ask thinking-capable models to return their reasoning summary as parts
        // tagged `thought: true`, so we can surface it as the last-thought context.
        // Apply reasoning effort when set: Gemini 3 uses thinkingLevel (low|high),
        // 2.5 uses thinkingBudget (tokens). Default (unset) keeps dynamic thinking.
        if supports_thoughts(&self.config.model) {
            let mut thinking = serde_json::Map::new();
            thinking.insert("includeThoughts".to_string(), json!(true));
            let levels = thinking_kind(&self.config.model) == Thinking::Level;
            if let Some(effort) = self.config.reasoning_effort.as_deref() {
                match effort.to_ascii_lowercase().as_str() {
                    "off" if !levels => {
                        thinking.insert("thinkingBudget".to_string(), json!(0));
                    }
                    e @ ("low" | "medium" | "high" | "xhigh" | "max") if levels => {
                        let level = match e {
                            "low" => "low",
                            "medium" => "medium",
                            _ => "high",
                        };
                        thinking.insert("thinkingLevel".to_string(), json!(level));
                    }
                    "low" => {
                        thinking.insert("thinkingBudget".to_string(), json!(2048));
                    }
                    "medium" => {
                        thinking.insert("thinkingBudget".to_string(), json!(8192));
                    }
                    "high" => {
                        thinking.insert("thinkingBudget".to_string(), json!(24576));
                    }
                    "xhigh" | "max" => {
                        // Above `high` for Gemini 2.5 — the top of the Pro budget range.
                        thinking.insert("thinkingBudget".to_string(), json!(32768));
                    }
                    _ => {}
                }
            }
            generation_config.insert("thinkingConfig".to_string(), Value::Object(thinking));
        }
        if !generation_config.is_empty() {
            body.insert(
                "generationConfig".to_string(),
                Value::Object(generation_config),
            );
        }
        let body = Value::Object(body);

        // Stream when the caller wants live output (interactive); lanes pass no
        // sink and stay buffered. Streaming pushes text + thought deltas to the
        // sink as they arrive, so Gemini's output and reasoning surface live like
        // the other providers instead of landing all at once at end of turn.
        let parsed = if let Some(sink) = sink.as_ref() {
            let url = format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                API_BASE, self.config.model
            );
            self.stream_generate(&url, &body, sink).await?
        } else {
            let url = format!("{}/models/{}:generateContent", API_BASE, self.config.model);
            self.post_generate(&url, &body).await?
        };

        // The streaming path already pushed thought deltas to the sink live, so the
        // thought return is only needed for the buffered path (which has no sink).
        let (output, _thought) = map_response(parsed, &self.config.model);
        Ok(output)
    }
}

impl GeminiModel {
    async fn post_generate(
        &mut self,
        url: &str,
        body: &Value,
    ) -> Result<GeminiResponse, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let can_degrade =
            thinking_kind(&self.config.model) != Thinking::Budget && !self.skip_thought_signatures;
        let mut last_error = String::new();
        let mut empty_retries = 0u32;
        let mut invalid_arg_count = 0u32;
        let mut degraded_body: Option<Value> = None;
        let mut fatal = false;
        let mut attempts = 0u32;

        for attempt in 1..=max_attempts {
            attempts = attempt;
            let active_body = degraded_body.as_ref().unwrap_or(body);
            let response = self
                .client
                .post(url)
                .header("x-goog-api-key", &self.config.api_key)
                .header(CONTENT_TYPE, "application/json")
                .json(active_body)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    let bytes = match response.bytes().await {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            last_error = crate::llm::humanize_transport_error(&error);
                            if attempt < max_attempts {
                                sleep(self.delay(attempt)).await;
                            }
                            continue;
                        }
                    };
                    if !status.is_success() {
                        let detail = String::from_utf8_lossy(&bytes);
                        last_error = crate::llm::humanize_http_error(status, detail.as_ref());
                        if is_invalid_argument(status, detail.as_ref()) {
                            log_invalid_argument(
                                attempt,
                                max_attempts,
                                &self.config.model,
                                active_body,
                                detail.as_ref(),
                            );
                        }
                        let state = RetryAttemptState {
                            attempt,
                            max_attempts,
                            invalid_arg_count,
                            can_degrade_signatures: can_degrade && degraded_body.is_none(),
                        };
                        match decide_error_retry(status, detail.as_ref(), state) {
                            ErrorRetryDecision::RetrySame => {
                                if is_invalid_argument(status, detail.as_ref()) {
                                    invalid_arg_count += 1;
                                }
                                sleep(self.delay(attempt)).await;
                                continue;
                            }
                            ErrorRetryDecision::RetryDegraded => {
                                invalid_arg_count += 1;
                                degraded_body = Some(rewrite_thought_signatures(body));
                                sleep(self.delay(attempt)).await;
                                continue;
                            }
                            ErrorRetryDecision::GiveUp { retryable } => {
                                fatal = !retryable;
                                break;
                            }
                        }
                    }
                    let parsed: GeminiResponse =
                        serde_json::from_slice(&bytes).map_err(|error| {
                            ToolError::msg(format!("invalid gemini response: {error}"))
                        })?;
                    // Gemini flash sometimes returns an empty turn (no parts) even
                    // with tools available — transient. Retry a few times before
                    // handing the empty response back.
                    if response_empty(&parsed) && empty_retries < 3 && attempt < max_attempts {
                        empty_retries += 1;
                        sleep(self.delay(attempt)).await;
                        continue;
                    }
                    if degraded_body.is_some() {
                        self.skip_thought_signatures = true;
                    }
                    return Ok(parsed);
                }
                Err(error) => {
                    last_error = crate::llm::humanize_transport_error(&error);
                    let retryable = error.is_timeout() || error.is_connect() || error.is_request();
                    if retryable && attempt < max_attempts {
                        sleep(self.delay(attempt)).await;
                        continue;
                    }
                    fatal = !retryable;
                    break;
                }
            }
        }

        Err(ToolError::model_request(
            crate::llm::final_model_error(&last_error, attempts),
            !fatal,
        ))
    }

    /// Streaming counterpart to `post_generate`: hit `streamGenerateContent?alt=sse`
    /// and assemble the response from the SSE chunks, pushing deltas to `sink` live.
    /// Retries mirror `post_generate`; each attempt clears the sink so a retry never
    /// double-renders already-streamed text.
    async fn stream_generate(
        &mut self,
        url: &str,
        body: &Value,
        sink: &StreamHandle,
    ) -> Result<GeminiResponse, ToolError> {
        let max_attempts = self.config.max_retries.saturating_add(1).max(1);
        let can_degrade =
            thinking_kind(&self.config.model) != Thinking::Budget && !self.skip_thought_signatures;
        let mut last_error = String::new();
        let mut invalid_arg_count = 0u32;
        let mut degraded_body: Option<Value> = None;
        let mut fatal = false;
        let mut attempts = 0u32;

        for attempt in 1..=max_attempts {
            attempts = attempt;
            StreamBuffer::clear(sink);
            let active_body = degraded_body.as_ref().unwrap_or(body);
            let response = self
                .client
                .post(url)
                .header("x-goog-api-key", &self.config.api_key)
                .header(CONTENT_TYPE, "application/json")
                .json(active_body)
                .send()
                .await;

            match response {
                Ok(response) => {
                    let status = response.status();
                    if !status.is_success() {
                        let detail = response.text().await.unwrap_or_default();
                        last_error = crate::llm::humanize_http_error(status, &detail);
                        if is_invalid_argument(status, &detail) {
                            log_invalid_argument(
                                attempt,
                                max_attempts,
                                &self.config.model,
                                active_body,
                                &detail,
                            );
                        }
                        let state = RetryAttemptState {
                            attempt,
                            max_attempts,
                            invalid_arg_count,
                            can_degrade_signatures: can_degrade && degraded_body.is_none(),
                        };
                        match decide_error_retry(status, &detail, state) {
                            ErrorRetryDecision::RetrySame => {
                                if is_invalid_argument(status, &detail) {
                                    invalid_arg_count += 1;
                                }
                                sleep(self.delay(attempt)).await;
                                continue;
                            }
                            ErrorRetryDecision::RetryDegraded => {
                                invalid_arg_count += 1;
                                degraded_body = Some(rewrite_thought_signatures(body));
                                sleep(self.delay(attempt)).await;
                                continue;
                            }
                            ErrorRetryDecision::GiveUp { retryable } => {
                                fatal = !retryable;
                                break;
                            }
                        }
                    }
                    match parse_gemini_sse(response, sink).await {
                        Ok(parsed) => {
                            // Same empty-turn quirk guard as the buffered path.
                            if response_empty(&parsed) && attempt < max_attempts {
                                sleep(self.delay(attempt)).await;
                                continue;
                            }
                            if degraded_body.is_some() {
                                self.skip_thought_signatures = true;
                            }
                            return Ok(parsed);
                        }
                        Err(error) => {
                            last_error = error;
                            if attempt < max_attempts {
                                sleep(self.delay(attempt)).await;
                                continue;
                            }
                            break;
                        }
                    }
                }
                Err(error) => {
                    last_error = crate::llm::humanize_transport_error(&error);
                    let retryable = error.is_timeout() || error.is_connect() || error.is_request();
                    if retryable && attempt < max_attempts {
                        sleep(self.delay(attempt)).await;
                        continue;
                    }
                    fatal = !retryable;
                    break;
                }
            }
        }

        StreamBuffer::clear(sink);
        Err(ToolError::model_request(
            crate::llm::final_model_error(&last_error, attempts),
            !fatal,
        ))
    }

    fn delay(&self, attempt: u32) -> Duration {
        let multiplier = 2u64.saturating_pow(attempt.saturating_sub(1).min(10));
        Duration::from_millis(
            self.config
                .initial_retry_ms
                .max(1)
                .saturating_mul(multiplier)
                .min(self.config.max_retry_ms.max(1)),
        )
    }
}

fn build_contents(
    messages: &[HarnessMessage],
    model: &str,
    skip_thought_signatures: bool,
) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut contents: Vec<Value> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            HarnessMessage::System { content } if index == 0 => system = content.clone(),
            HarnessMessage::System { content } => push_content(
                &mut contents,
                "user",
                vec![json!({ "text": crate::llm::system_reminder(content) })],
            ),
            HarnessMessage::User { content } => {
                push_content(&mut contents, "user", vec![json!({ "text": content })])
            }
            HarnessMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut parts: Vec<Value> = Vec::new();
                if !content.is_empty() {
                    parts.push(json!({ "text": content }));
                }
                for call in tool_calls {
                    let mut part = json!({
                        "functionCall": {
                            "name": call.name,
                            "args": crate::llm::arguments_as_object(&call.arguments),
                        }
                    });
                    // Replay the REAL thought signature when it came from this same
                    // model (signatures are model-specific). Otherwise Gemini 3+
                    // rejects a replayed functionCall without one, so attach the
                    // skip sentinel; Gemini 2.5 needs neither.
                    let own_signature = (!skip_thought_signatures)
                        .then_some(call.signature.as_deref())
                        .flatten()
                        .filter(|sig| {
                            !sig.is_empty() && call.origin_model.as_deref() == Some(model)
                        });
                    if let Some(signature) = own_signature {
                        part["thoughtSignature"] = json!(signature);
                    } else if thinking_kind(model) != Thinking::Budget {
                        part["thoughtSignature"] = json!(SKIP_THOUGHT_SIGNATURE);
                    }
                    parts.push(part);
                }
                if !parts.is_empty() {
                    push_content(&mut contents, "model", parts);
                }
            }
            HarnessMessage::ToolResult {
                tool_name, content, ..
            } => {
                let (cleaned, image) = crate::llm::split_inlined_image(content);
                let response = json!({ "output": crate::llm::render_tool_result(&cleaned) });
                let mut parts = vec![json!({
                    "functionResponse": { "name": tool_name, "response": response }
                })];
                if let Some((mime, base64)) = image {
                    parts.push(json!({ "inline_data": { "mime_type": mime, "data": base64 } }));
                }
                push_content(&mut contents, "user", parts);
            }
            HarnessMessage::Summary { kind, content } => push_content(
                &mut contents,
                "user",
                vec![json!({ "text": format!("[summary:{kind}]\n{content}\n[/summary]") })],
            ),
        }
    }
    (system, contents)
}

/// Append parts, merging into the previous content when it shares the role (so
/// parallel tool calls / results land in one turn, as Gemini expects).
fn push_content(contents: &mut Vec<Value>, role: &str, mut parts: Vec<Value>) {
    if let Some(last) = contents.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut)
    {
        existing.append(&mut parts);
        return;
    }
    contents.push(json!({ "role": role, "parts": parts }));
}

/// Map a Gemini response into a `ModelOutput`, returning the reasoning summary
/// (parts tagged `thought: true`) separately so it can be surfaced as last-thought
/// context rather than mixed into the user-facing answer.
fn map_response(response: GeminiResponse, model: &str) -> (ModelOutput, Option<String>) {
    let mut content_text = String::new();
    let mut thought_text = String::new();
    let mut calls = Vec::new();
    for candidate in &response.candidates {
        for part in &candidate.content.parts {
            if let Some(text) = &part.text {
                if part.thought.unwrap_or(false) {
                    thought_text.push_str(text);
                } else {
                    content_text.push_str(text);
                }
            }
            if let Some(call) = &part.function_call {
                calls.push(GeneratedToolCall {
                    tool_name: call.name.clone(),
                    arguments: call.args.clone(),
                    // Gemini has no call ids; synthesize one so the harness can
                    // pair the tool_result (functionResponse matches by name).
                    id: Some(uuid::Uuid::new_v4().to_string()),
                    // Capture the thought signature + the model it came from so it
                    // can be replayed next turn (Gemini 3 reasoning continuity).
                    signature: part.thought_signature.clone(),
                    origin_model: Some(model.to_string()),
                });
            }
        }
    }
    let finish_reason = response
        .candidates
        .first()
        .and_then(|candidate| candidate.finish_reason.clone());
    let usage = response.usage_metadata.map(|u| TokenUsage {
        prompt_tokens: u.prompt_token_count as u64,
        completion_tokens: u.candidates_token_count as u64,
        total_tokens: u.total_token_count as u64,
        cache_read_tokens: u.cached_content_token_count.unwrap_or(0) as u64,
        cache_creation_tokens: 0,
    });
    let output = ModelOutput {
        calls,
        content_text: (!content_text.is_empty()).then_some(content_text),
        usage,
        finish_reason,
        rate_limit: None,
        ..Default::default()
    };
    let thought = thought_text.trim();
    (output, (!thought.is_empty()).then(|| thought.to_string()))
}

/// Consume a Gemini `streamGenerateContent?alt=sse` stream: push text and thought
/// deltas to `sink` as they arrive, and assemble a `GeminiResponse` (accumulated
/// text + thought + any function-call parts + usage) for the normal mapping path.
/// Each SSE `data:` line is a full `GenerateContentResponse` chunk.
async fn parse_gemini_sse(
    response: reqwest::Response,
    sink: &StreamHandle,
) -> Result<GeminiResponse, String> {
    let mut text = String::new();
    let mut thought = String::new();
    let mut calls: Vec<CandidatePart> = Vec::new();
    let mut usage: Option<UsageMetadata> = None;
    let mut finish: Option<String> = None;

    crate::sse::for_each_event(response, |data| {
        if data.is_empty() {
            return;
        }
        let Ok(chunk) = serde_json::from_str::<GeminiResponse>(data) else {
            return;
        };
        if chunk.usage_metadata.is_some() {
            usage = chunk.usage_metadata;
        }
        for candidate in &chunk.candidates {
            if candidate.finish_reason.is_some() {
                finish = candidate.finish_reason.clone();
            }
            for part in &candidate.content.parts {
                if part.function_call.is_some() {
                    calls.push(CandidatePart {
                        text: None,
                        thought: None,
                        function_call: part.function_call.clone(),
                        thought_signature: part.thought_signature.clone(),
                    });
                } else if let Some(delta) = &part.text {
                    if part.thought.unwrap_or(false) {
                        thought.push_str(delta);
                        StreamBuffer::append_thinking(sink, delta);
                    } else {
                        text.push_str(delta);
                        StreamBuffer::append(sink, delta);
                    }
                }
            }
        }
    })
    .await?;

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(CandidatePart {
            text: Some(text),
            thought: None,
            function_call: None,
            thought_signature: None,
        });
    }
    if !thought.is_empty() {
        parts.push(CandidatePart {
            text: Some(thought),
            thought: Some(true),
            function_call: None,
            thought_signature: None,
        });
    }
    parts.extend(calls);

    Ok(GeminiResponse {
        candidates: vec![Candidate {
            content: CandidateContent { parts },
            finish_reason: finish,
        }],
        usage_metadata: usage,
    })
}

fn response_empty(response: &GeminiResponse) -> bool {
    let has_call = response
        .candidates
        .iter()
        .flat_map(|candidate| candidate.content.parts.iter())
        .any(|part| part.function_call.is_some());
    let has_text = response
        .candidates
        .iter()
        .flat_map(|candidate| candidate.content.parts.iter())
        // A thought-only response carries no answer — treat it as empty so the
        // retry logic gives the model another shot at a real turn.
        .any(|part| {
            !part.thought.unwrap_or(false)
                && part
                    .text
                    .as_deref()
                    .map(|t| !t.trim().is_empty())
                    .unwrap_or(false)
        });
    !has_call && !has_text
}

/// Recursively coerce a JSON Schema into Gemini's function-declaration subset:
/// keep only the keys Gemini accepts (dropping `additionalProperties`, `$schema`,
/// `title`, etc.) and lowercase `type` names. Ported from wacht's
/// `normalize_gemini_function_schema` to keep both engines in lockstep.
fn normalize_gemini_function_schema(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut normalized = serde_json::Map::new();
            for (key, value) in map {
                if key == "properties" {
                    let property_map = match value {
                        Value::Object(properties) => Value::Object(
                            properties
                                .into_iter()
                                .map(|(name, schema)| {
                                    (name, normalize_gemini_function_schema(schema))
                                })
                                .collect(),
                        ),
                        other => normalize_gemini_function_schema(other),
                    };
                    normalized.insert(key, property_map);
                    continue;
                }

                let normalized_value = if key == "type" {
                    match value {
                        Value::String(type_name) => Value::String(
                            match type_name.as_str() {
                                "OBJECT" => "object",
                                "ARRAY" => "array",
                                "STRING" => "string",
                                "INTEGER" => "integer",
                                "NUMBER" => "number",
                                "BOOLEAN" => "boolean",
                                other => other,
                            }
                            .to_string(),
                        ),
                        other => normalize_gemini_function_schema(other),
                    }
                } else {
                    normalize_gemini_function_schema(value)
                };

                if !matches!(
                    key.as_str(),
                    "type"
                        | "description"
                        | "nullable"
                        | "enum"
                        | "items"
                        | "properties"
                        | "required"
                        | "anyOf"
                        | "allOf"
                        | "oneOf"
                        | "example"
                        | "default"
                        | "propertyOrdering"
                ) {
                    continue;
                }

                normalized.insert(key, normalized_value);
            }
            Value::Object(normalized)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(normalize_gemini_function_schema)
                .collect(),
        ),
        other => other,
    }
}

fn safety_settings() -> Value {
    json!([
        { "category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE" },
        { "category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "BLOCK_NONE" },
        { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "BLOCK_NONE" },
        { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "BLOCK_NONE" },
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Thinking {
    Budget,
    Level,
    None,
}

pub(crate) fn thinking_kind(model: &str) -> Thinking {
    let model = model.to_ascii_lowercase();
    if model.contains("gemma") || model.contains("embedding") || model.contains("imagen") {
        return Thinking::None;
    }
    let version = model.split("gemini-").nth(1).and_then(|rest| {
        let digits: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let mut parts = digits.split(['.', '-']).filter(|p| !p.is_empty());
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        Some((major, minor))
    });
    match version {
        Some((major, _)) if major >= 3 => Thinking::Level,
        Some((2, minor)) if minor >= 5 => Thinking::Budget,
        Some(_) => Thinking::None,
        None => Thinking::Level,
    }
}

fn supports_thoughts(model: &str) -> bool {
    thinking_kind(model) != Thinking::None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryAttemptState {
    attempt: u32,
    max_attempts: u32,
    invalid_arg_count: u32,
    can_degrade_signatures: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorRetryDecision {
    RetrySame,
    RetryDegraded,
    GiveUp { retryable: bool },
}

fn decide_error_retry(
    status: StatusCode,
    body: &str,
    state: RetryAttemptState,
) -> ErrorRetryDecision {
    if is_retryable_status(status) {
        if state.attempt < state.max_attempts {
            return ErrorRetryDecision::RetrySame;
        }
        return ErrorRetryDecision::GiveUp { retryable: true };
    }
    if is_invalid_argument(status, body) && state.attempt < state.max_attempts {
        if state.invalid_arg_count == 0 {
            return ErrorRetryDecision::RetrySame;
        }
        if state.invalid_arg_count == 1 && state.can_degrade_signatures {
            return ErrorRetryDecision::RetryDegraded;
        }
    }
    ErrorRetryDecision::GiveUp { retryable: false }
}

fn is_invalid_argument(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")?
                .get("status")?
                .as_str()
                .map(|s| s == "INVALID_ARGUMENT")
        })
        .unwrap_or(false)
}

fn rewrite_thought_signatures(body: &Value) -> Value {
    let mut copy = body.clone();
    if let Some(contents) = copy.get_mut("contents").and_then(Value::as_array_mut) {
        for content in contents {
            rewrite_parts_signatures(content);
        }
    }
    copy
}

fn rewrite_parts_signatures(node: &mut Value) {
    match node {
        Value::Object(obj) => {
            for (key, value) in obj.iter_mut() {
                if key == "parts" {
                    if let Value::Array(parts) = value {
                        for part in parts {
                            if let Some(part_obj) = part.as_object_mut()
                                && part_obj.contains_key("functionCall")
                            {
                                part_obj.insert(
                                    "thoughtSignature".to_string(),
                                    json!(SKIP_THOUGHT_SIGNATURE),
                                );
                            }
                            rewrite_parts_signatures(part);
                        }
                    }
                } else {
                    rewrite_parts_signatures(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_parts_signatures(item);
            }
        }
        _ => {}
    }
}

fn format_invalid_argument_log(
    attempt: u32,
    max_attempts: u32,
    model: &str,
    req_body: &Value,
    err_body: &str,
) -> String {
    let contents = req_body.get("contents").and_then(Value::as_array);
    let contents_len = contents.map(Vec::len).unwrap_or(0);
    let last = contents.and_then(|c| c.last());
    let last_role = last
        .and_then(|c| c.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    let last_parts = last
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            let kinds: Vec<&str> = parts.iter().map(part_kind).collect();
            if kinds.is_empty() {
                "-".to_string()
            } else {
                kinds.join(",")
            }
        })
        .unwrap_or_else(|| "-".to_string());

    let details = serde_json::from_str::<Value>(err_body)
        .ok()
        .and_then(|v| v.get("error")?.get("details").cloned())
        .and_then(|d| serde_json::to_string(&d).ok())
        .filter(|s| !s.is_empty() && s != "null" && s != "[]")
        .map(|s| s.chars().take(500).collect::<String>());

    match details {
        Some(d) => format!(
            "[gemini] 400 INVALID_ARGUMENT attempt {attempt}/{max_attempts} model={model} contents={contents_len} last_role={last_role} last_parts={last_parts} {d}"
        ),
        None => format!(
            "[gemini] 400 INVALID_ARGUMENT attempt {attempt}/{max_attempts} model={model} contents={contents_len} last_role={last_role} last_parts={last_parts}"
        ),
    }
}

fn log_invalid_argument(
    attempt: u32,
    max_attempts: u32,
    model: &str,
    req_body: &Value,
    err_body: &str,
) {
    eprintln!(
        "{}",
        format_invalid_argument_log(attempt, max_attempts, model, req_body, err_body)
    );
}

fn part_kind(part: &Value) -> &'static str {
    let Some(obj) = part.as_object() else {
        return "other";
    };
    if obj.contains_key("functionCall") {
        "functionCall"
    } else if obj.contains_key("functionResponse") {
        "functionResponse"
    } else if obj.contains_key("inline_data") || obj.contains_key("inlineData") {
        "inline_data"
    } else if obj.contains_key("file_data") || obj.contains_key("fileData") {
        "file_data"
    } else if obj.get("thought").and_then(Value::as_bool) == Some(true) {
        "thought"
    } else if obj.contains_key("text") {
        "text"
    } else {
        "other"
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status.is_server_error()
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: CandidateContent,
    #[serde(rename = "finishReason", default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Debug, Deserialize)]
struct CandidatePart {
    #[serde(default)]
    text: Option<String>,
    /// `true` marks a reasoning-summary part (returned when includeThoughts is on),
    /// distinct from the opaque `thoughtSignature` used for replay.
    #[serde(default)]
    thought: Option<bool>,
    #[serde(rename = "functionCall", default)]
    function_call: Option<GeminiFunctionCall>,
    #[serde(rename = "thoughtSignature", default)]
    thought_signature: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Deserialize)]
struct UsageMetadata {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    cached_content_token_count: Option<u32>,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: u32,
    #[serde(rename = "totalTokenCount", default)]
    total_token_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCallRecord;

    fn invalid_arg_body() -> &'static str {
        r#"{"error":{"code":400,"message":"Request contains an invalid argument.","status":"INVALID_ARGUMENT"}}"#
    }

    #[test]
    fn signature_rewrite_replaces_only_function_call_signatures() {
        let original = json!({
            "system_instruction": { "parts": [{ "text": "sys" }] },
            "contents": [
                {
                    "role": "user",
                    "parts": [{ "text": "hello" }]
                },
                {
                    "role": "model",
                    "parts": [
                        { "text": "thinking", "thought": true, "thoughtSignature": "keep-non-call" },
                        {
                            "functionCall": { "name": "bash", "args": { "cmd": "ls" } },
                            "thoughtSignature": "real-sig-1"
                        },
                        {
                            "functionCall": { "name": "read", "args": { "path": "a" } }
                        }
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        {
                            "functionResponse": {
                                "name": "bash",
                                "response": { "output": "ok" },
                                "parts": [
                                    {
                                        "functionCall": { "name": "nested", "args": {} },
                                        "thoughtSignature": "real-sig-nested"
                                    }
                                ]
                            }
                        }
                    ]
                }
            ],
            "generationConfig": { "temperature": 0.2 }
        });

        let rewritten = rewrite_thought_signatures(&original);
        assert_eq!(
            rewritten["system_instruction"],
            original["system_instruction"]
        );
        assert_eq!(rewritten["generationConfig"], original["generationConfig"]);
        assert_eq!(rewritten["contents"][0], original["contents"][0]);
        assert_eq!(
            rewritten["contents"][1]["parts"][0]["thoughtSignature"],
            json!("keep-non-call")
        );
        assert_eq!(
            rewritten["contents"][1]["parts"][1]["thoughtSignature"],
            json!(SKIP_THOUGHT_SIGNATURE)
        );
        assert_eq!(
            rewritten["contents"][1]["parts"][2]["thoughtSignature"],
            json!(SKIP_THOUGHT_SIGNATURE)
        );
        assert_eq!(
            rewritten["contents"][2]["parts"][0]["functionResponse"]["parts"][0]["thoughtSignature"],
            json!(SKIP_THOUGHT_SIGNATURE)
        );
        assert_eq!(
            rewritten["contents"][2]["parts"][0]["functionResponse"]["response"],
            json!({ "output": "ok" })
        );
    }

    #[test]
    fn retry_decision_handles_invalid_argument_and_limits() {
        let body = invalid_arg_body();

        assert_eq!(
            decide_error_retry(
                StatusCode::BAD_REQUEST,
                body,
                RetryAttemptState {
                    attempt: 1,
                    max_attempts: 4,
                    invalid_arg_count: 0,
                    can_degrade_signatures: true,
                }
            ),
            ErrorRetryDecision::RetrySame
        );

        assert_eq!(
            decide_error_retry(
                StatusCode::BAD_REQUEST,
                body,
                RetryAttemptState {
                    attempt: 2,
                    max_attempts: 4,
                    invalid_arg_count: 1,
                    can_degrade_signatures: true,
                }
            ),
            ErrorRetryDecision::RetryDegraded
        );

        assert_eq!(
            decide_error_retry(
                StatusCode::BAD_REQUEST,
                body,
                RetryAttemptState {
                    attempt: 3,
                    max_attempts: 4,
                    invalid_arg_count: 2,
                    can_degrade_signatures: false,
                }
            ),
            ErrorRetryDecision::GiveUp { retryable: false }
        );

        assert_eq!(
            decide_error_retry(
                StatusCode::BAD_REQUEST,
                body,
                RetryAttemptState {
                    attempt: 2,
                    max_attempts: 4,
                    invalid_arg_count: 1,
                    can_degrade_signatures: false,
                }
            ),
            ErrorRetryDecision::GiveUp { retryable: false }
        );

        assert_eq!(
            decide_error_retry(
                StatusCode::BAD_REQUEST,
                body,
                RetryAttemptState {
                    attempt: 2,
                    max_attempts: 2,
                    invalid_arg_count: 1,
                    can_degrade_signatures: true,
                }
            ),
            ErrorRetryDecision::GiveUp { retryable: false }
        );

        let other_400 = r#"{"error":{"code":400,"message":"API key not valid","status":"FAILED_PRECONDITION"}}"#;
        assert_eq!(
            decide_error_retry(
                StatusCode::BAD_REQUEST,
                other_400,
                RetryAttemptState {
                    attempt: 1,
                    max_attempts: 4,
                    invalid_arg_count: 0,
                    can_degrade_signatures: true,
                }
            ),
            ErrorRetryDecision::GiveUp { retryable: false }
        );

        assert_eq!(
            decide_error_retry(
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                RetryAttemptState {
                    attempt: 1,
                    max_attempts: 2,
                    invalid_arg_count: 0,
                    can_degrade_signatures: true,
                }
            ),
            ErrorRetryDecision::RetrySame
        );
        assert_eq!(
            decide_error_retry(
                StatusCode::SERVICE_UNAVAILABLE,
                "{}",
                RetryAttemptState {
                    attempt: 2,
                    max_attempts: 2,
                    invalid_arg_count: 0,
                    can_degrade_signatures: true,
                }
            ),
            ErrorRetryDecision::GiveUp { retryable: true }
        );
    }

    #[test]
    fn skip_thought_signatures_mode_uses_sentinel_in_build_contents() {
        let messages = vec![HarnessMessage::Assistant {
            content: String::new(),
            tool_calls: vec![ToolCallRecord {
                id: "1".to_string(),
                name: "bash".to_string(),
                arguments: json!({ "cmd": "pwd" }),
                signature: Some("real-sig".to_string()),
                origin_model: Some("gemini-4-carbon".to_string()),
            }],
        }];
        let (_, normal) = build_contents(&messages, "gemini-4-carbon", false);
        assert_eq!(normal[0]["parts"][0]["thoughtSignature"], json!("real-sig"));

        let (_, degraded) = build_contents(&messages, "gemini-4-carbon", true);
        assert_eq!(
            degraded[0]["parts"][0]["thoughtSignature"],
            json!(SKIP_THOUGHT_SIGNATURE)
        );
    }

    #[test]
    fn invalid_argument_log_formats_summary_without_request_body() {
        let req = json!({
            "system_instruction": { "parts": [{ "text": "sys-secret" }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "turn 1" }] },
                { "role": "user", "parts": [{ "functionResponse": { "name": "bash", "response": { "output": "sensitive-body" } } }] }
            ]
        });
        let err_with_details = r#"{"error":{"code":400,"message":"Request contains an invalid argument.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.BadRequest","fieldViolations":[{"field":"contents[1]"}]}]}}"#;
        let line = format_invalid_argument_log(1, 4, "gemini-4-carbon", &req, err_with_details);
        assert!(line.starts_with(
            "[gemini] 400 INVALID_ARGUMENT attempt 1/4 model=gemini-4-carbon contents=2 last_role=user last_parts=functionResponse "
        ));
        assert!(line.contains("BadRequest"));
        assert!(!line.contains("sensitive-body"));
        assert!(!line.contains("sys-secret"));
    }
}
