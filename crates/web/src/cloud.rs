//! Online AI via OpenRouter (SPEC §4.10, capability `cloud_ai`).
//!
//! Configuration lives in the dedicated `cloud_config` table — NOT the
//! settings bag — so the API key can never surface through
//! `GET /api/settings` or `/api/config`. Reads expose `api_key_set` only.
//!
//! Routing: when enabled, dialogue (HTTP chat + voice auto-replies)
//! prefers the cloud; on request failure the local model answers unless
//! `fallback_local` is off. The persona/grounding system turn is reused
//! verbatim — going online must not lose the camera context.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::Extension;
use serde_json::json;
use sqlx::SqlitePool;

use crate::db;
use crate::errors::ApiError;
use security::middleware::AuthenticatedUser;
use streaming::llm::ChatTurn;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Static suggestions surfaced by `GET /api/cloud` — the field accepts any
/// OpenRouter model id; this list just saves a lookup.
pub const SUGGEST_CHAT: &[&str] = &[
    "openrouter/free",
    "openai/gpt-4o-mini",
    "deepseek/deepseek-chat-v3.1",
    "qwen/qwen3-8b",
    "google/gemini-2.5-flash",
];
pub const SUGGEST_VISION: &[&str] = &[
    "nvidia/nemotron-3.5-super-vl:free",
    "openai/gpt-4o-mini",
    "google/gemini-2.5-flash",
    "qwen/qwen3-vl-8b",
];

#[derive(Debug, Clone, PartialEq)]
pub struct CloudConfig {
    pub provider: String,
    pub api_key: String,
    pub chat_model: String,
    pub vision_model: String,
    pub fallback_local: bool,
    pub timeout_secs: u64,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            provider: "off".into(),
            api_key: String::new(),
            chat_model: "openai/gpt-4o-mini".into(),
            vision_model: String::new(),
            fallback_local: true,
            timeout_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CloudError {
    NotEnabled,
    NoModel,
    InvalidKey,
    Timeout,
    Http(String),
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloudError::NotEnabled => write!(f, "cloud AI is off or has no API key"),
            CloudError::NoModel => write!(f, "no cloud model configured"),
            CloudError::InvalidKey => write!(f, "invalid API key (401 from OpenRouter)"),
            CloudError::Timeout => write!(f, "cloud request timed out"),
            CloudError::Http(m) => write!(f, "{m}"),
        }
    }
}

/// Shared cloud-AI state: the live config (hot-swapped by PUT /api/cloud)
/// plus the HTTP client. `base_url` is injectable so tests run against a
/// local server instead of openrouter.ai.
pub struct CloudAi {
    cfg: std::sync::RwLock<CloudConfig>,
    http: reqwest::Client,
    base_url: std::sync::RwLock<String>,
}

impl CloudAi {
    pub fn new(cfg: CloudConfig) -> Self {
        Self {
            cfg: std::sync::RwLock::new(cfg),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            base_url: std::sync::RwLock::new(
                std::env::var("MIBEE_EYE_CLOUD_BASE_URL")
                    .ok()
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            ),
        }
    }

    #[cfg(test)]
    pub fn set_base_url(&self, url: &str) {
        *self.base_url.write().expect("cloud base url lock") = url.to_string();
    }

    pub fn config(&self) -> CloudConfig {
        self.cfg.read().expect("cloud config lock").clone()
    }

    pub fn store(&self, cfg: CloudConfig) {
        *self.cfg.write().expect("cloud config lock") = cfg;
    }

    pub fn enabled(&self) -> bool {
        let c = self.config();
        c.provider != "off" && !c.api_key.is_empty()
    }

    pub fn vision_ready(&self) -> bool {
        let c = self.config();
        c.provider != "off" && !c.api_key.is_empty() && !c.vision_model.is_empty()
    }

    /// One OpenAI-compatible chat completion. `image_jpeg` (vision models)
    /// rides the final user message as a data-URL part.
    pub async fn complete(
        &self,
        turns: &[ChatTurn],
        image_jpeg: Option<&[u8]>,
        model_override: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<String, CloudError> {
        self.complete_with_usage(turns, image_jpeg, model_override, timeout)
            .await
            .map(|(reply, _)| reply)
    }

    /// [`CloudAi::complete`] plus the usage tokens reported by the
    /// provider (fed into per-model metrics and conversation traces).
    pub async fn complete_with_usage(
        &self,
        turns: &[ChatTurn],
        image_jpeg: Option<&[u8]>,
        model_override: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(String, Option<(u64, u64)>), CloudError> {
        let cfg = self.config();
        if cfg.provider == "off" || cfg.api_key.is_empty() {
            return Err(CloudError::NotEnabled);
        }
        let model = model_override
            .map(str::to_string)
            .or_else(|| {
                if image_jpeg.is_some() && !cfg.vision_model.is_empty() {
                    Some(cfg.vision_model.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| cfg.chat_model.clone());
        if model.is_empty() {
            return Err(CloudError::NoModel);
        }
        // Per-model metrics + OTel span (SPEC appendix A #39): the model
        // id splits text/vision routing so the two route families are
        // independently monitorable; variant = the concrete provider id.
        let model_id = if image_jpeg.is_some() {
            "cloud.vision"
        } else {
            "cloud.chat"
        };
        let call = observability::model_call(model_id, &model);
        let result = self
            .complete_request(&cfg, model, turns, image_jpeg, timeout)
            .await;
        match &result {
            Ok((_, usage)) => {
                call.finish_ok(usage.map(|u| u.0), usage.map(|u| u.1));
            }
            Err(_) => {
                call.finish_err();
            }
        }
        result
    }

    /// Agent tool-calling round trip (SPEC §3.5): the OpenAI-compatible
    /// `tools` array goes out with the request; the reply may carry
    /// `tool_calls` instead of (or alongside) text. `with_tools == false`
    /// drops the tool table (the loop's forced final text answer).
    pub async fn agent_complete(
        &self,
        turns: &[ChatTurn],
        tools: &[crate::agent::ToolSpec],
        with_tools: bool,
        timeout_secs: u64,
    ) -> Result<
        (
            Option<String>,
            Vec<crate::agent::runner::ParsedToolCall>,
            Option<(u64, u64)>,
        ),
        CloudError,
    > {
        let cfg = self.config();
        if cfg.provider == "off" || cfg.api_key.is_empty() {
            return Err(CloudError::NotEnabled);
        }
        let model = cfg.chat_model.clone();
        if model.is_empty() {
            return Err(CloudError::NoModel);
        }
        let call = observability::model_call("cloud.chat", &model);
        let result = self
            .agent_request(&cfg, model, turns, tools, with_tools, timeout_secs)
            .await;
        match &result {
            Ok((_, _, usage)) => {
                call.finish_ok(usage.map(|u| u.0), usage.map(|u| u.1));
            }
            Err(_) => {
                call.finish_err();
            }
        }
        result
    }

    /// The agent variant of the raw HTTP round trip (no vision legs).
    async fn agent_request(
        &self,
        cfg: &CloudConfig,
        model: String,
        turns: &[ChatTurn],
        tools: &[crate::agent::ToolSpec],
        with_tools: bool,
        timeout_secs: u64,
    ) -> Result<
        (
            Option<String>,
            Vec<crate::agent::runner::ParsedToolCall>,
            Option<(u64, u64)>,
        ),
        CloudError,
    > {
        let messages: Vec<serde_json::Value> = turns
            .iter()
            .map(|t| json!({ "role": t.role, "content": t.content }))
            .collect();
        let mut body = json!({
            "model": model,
            "messages": messages,
            "max_tokens": 512,
            // Voice-forward posture: a security camera's spoken replies
            // want direct answers, not hidden deliberation. Several
            // OpenRouter models otherwise route the whole reply into
            // the `reasoning` field and return empty `content`
            // (measured 2026-10-30: openrouter/free → n choices[0]
            // message content on plain questions). OpenRouter ignores
            // the knob for models that do not support it.
            "reasoning": { "enabled": false },
        });
        if with_tools && !tools.is_empty() {
            let defs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(defs);
            body["tool_choice"] = json!("auto");
        }
        let url = format!(
            "{}/chat/completions",
            self.base_url.read().expect("cloud base url lock")
        );
        let timeout = Duration::from_secs(timeout_secs.max(5));
        let resp = tokio::time::timeout(
            timeout,
            self.http
                .post(&url)
                .bearer_auth(&cfg.api_key)
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| CloudError::Timeout)?
        .map_err(|e| CloudError::Http(format!("request: {e}")))?;
        let status = resp.status();
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CloudError::Http(format!("body: {e}")))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CloudError::InvalidKey);
        }
        if !status.is_success() {
            let msg = payload
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(CloudError::Http(format!("HTTP {status}: {msg}")));
        }
        // Text may be absent (null) when the model answers with tool calls.
        let text = payload
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let mut calls = Vec::new();
        for tc in payload
            .pointer("/choices/0/message/tool_calls")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let name = match tc.pointer("/function/name").and_then(|v| v.as_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            let arguments = tc
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| json!({}));
            calls.push(crate::agent::runner::ParsedToolCall { name, arguments });
        }
        let usage = payload
            .pointer("/usage/prompt_tokens")
            .and_then(|v| v.as_u64())
            .zip(
                payload
                    .pointer("/usage/completion_tokens")
                    .and_then(|v| v.as_u64()),
            );
        if text.is_none() && calls.is_empty() {
            return Err(CloudError::Http(
                "no content and no tool_calls in reply".into(),
            ));
        }
        Ok((text, calls, usage))
    }

    /// The raw OpenAI-compatible HTTP round trip (no metrics wrapper).
    async fn complete_request(
        &self,
        cfg: &CloudConfig,
        model: String,
        turns: &[ChatTurn],
        image_jpeg: Option<&[u8]>,
        timeout: Option<Duration>,
    ) -> Result<(String, Option<(u64, u64)>), CloudError> {
        let messages: Vec<serde_json::Value> = turns
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let content = if i + 1 == turns.len()
                    && let Some(jpeg) = image_jpeg
                {
                    json!([
                        { "type": "text", "text": t.content },
                        { "type": "image_url", "image_url": {
                            "url": format!("data:image/jpeg;base64,{}", base64_encode(jpeg))
                        }}
                    ])
                } else {
                    json!(t.content)
                };
                json!({ "role": t.role, "content": content })
            })
            .collect();
        let body = json!({
            "model": model,
            "messages": messages,
            "max_tokens": 512,
        });
        let url = format!(
            "{}/chat/completions",
            self.base_url.read().expect("cloud base url lock")
        );
        let timeout = timeout.unwrap_or(Duration::from_secs(cfg.timeout_secs.max(5)));
        let resp = tokio::time::timeout(
            timeout,
            self.http
                .post(&url)
                .bearer_auth(&cfg.api_key)
                .json(&body)
                .send(),
        )
        .await
        .map_err(|_| CloudError::Timeout)?
        .map_err(|e| CloudError::Http(format!("request: {e}")))?;
        let status = resp.status();
        let payload: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| CloudError::Http(format!("body: {e}")))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CloudError::InvalidKey);
        }
        if !status.is_success() {
            let msg = payload
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(CloudError::Http(format!("HTTP {status}: {msg}")));
        }
        let reply = payload
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| CloudError::Http("no choices/0/message/content".into()))?;
        let usage = payload
            .pointer("/usage/prompt_tokens")
            .and_then(|v| v.as_u64())
            .zip(
                payload
                    .pointer("/usage/completion_tokens")
                    .and_then(|v| v.as_u64()),
            );
        Ok((reply.to_string(), usage))
    }

    /// Minimal connectivity probe (`POST /api/cloud/test`): one tiny
    /// completion with a short budget.
    pub async fn test(&self) -> Result<(u64, String, String), CloudError> {
        let started = std::time::Instant::now();
        let reply = self
            .complete(
                &[ChatTurn {
                    role: "user".into(),
                    content: "Reply with exactly: OK".into(),
                }],
                None,
                None,
                Some(Duration::from_secs(20)),
            )
            .await?;
        let cfg = self.config();
        Ok((
            started.elapsed().as_millis() as u64,
            cfg.chat_model.clone(),
            reply,
        ))
    }
}

fn base64_encode(data: &[u8]) -> String {
    // Small local implementation — the web crate deliberately carries no
    // base64 crate dependency for one call site.
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn cloud_document(cfg: &CloudConfig) -> serde_json::Value {
    json!({
        "provider": cfg.provider,
        "api_key_set": !cfg.api_key.is_empty(),
        "chat_model": cfg.chat_model,
        "vision_model": cfg.vision_model,
        "fallback_local": cfg.fallback_local,
        "timeout_secs": cfg.timeout_secs,
        "suggest": { "chat": SUGGEST_CHAT, "vision": SUGGEST_VISION },
    })
}

/// `GET /api/cloud` — the config without the key (SPEC §4.10).
#[tracing::instrument(skip_all)]
pub async fn get_cloud(
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = db::get_cloud_config(&db)
        .await
        .map_err(|e| ApiError::internal(format!("cloud config: {e}")))?;
    Ok(Json(cloud_document(&cfg)))
}

/// `PUT /api/cloud` — partial merge. `api_key` is write-only: absent keeps
/// the stored key, `""` clears it.
#[tracing::instrument(skip_all)]
pub async fn put_cloud(
    Extension(db): Extension<SqlitePool>,
    Extension(cloud): Extension<Arc<CloudAi>>,
    Extension(_user): Extension<AuthenticatedUser>,
    body: axum::extract::Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let obj = body
        .as_object()
        .ok_or_else(|| ApiError::bad_request("body must be an object"))?;
    let mut cfg = db::get_cloud_config(&db)
        .await
        .map_err(|e| ApiError::internal(format!("cloud config: {e}")))?;
    for (key, value) in obj {
        match key.as_str() {
            "provider" => {
                let v = value
                    .as_str()
                    .ok_or_else(|| ApiError::bad_request("provider must be a string"))?;
                if !matches!(v, "off" | "openrouter") {
                    return Err(ApiError::bad_request("provider must be off|openrouter"));
                }
                cfg.provider = v.to_string();
            }
            "api_key" => {
                let v = value
                    .as_str()
                    .ok_or_else(|| ApiError::bad_request("api_key must be a string"))?;
                if v.len() > 256 {
                    return Err(ApiError::bad_request("api_key too long (max 256)"));
                }
                cfg.api_key = v.to_string();
            }
            "chat_model" | "vision_model" => {
                let v = value
                    .as_str()
                    .ok_or_else(|| ApiError::bad_request("model must be a string"))?;
                if v.len() > 128 {
                    return Err(ApiError::bad_request(format!("{key} too long (max 128)")));
                }
                if key == "chat_model" {
                    cfg.chat_model = v.to_string();
                } else {
                    cfg.vision_model = v.to_string();
                }
            }
            "fallback_local" => {
                cfg.fallback_local = value
                    .as_bool()
                    .ok_or_else(|| ApiError::bad_request("fallback_local must be a boolean"))?;
            }
            "timeout_secs" => {
                let v = value
                    .as_u64()
                    .ok_or_else(|| ApiError::bad_request("timeout_secs must be an integer"))?;
                if !(5..=300).contains(&v) {
                    return Err(ApiError::bad_request("timeout_secs must be within 5..=300"));
                }
                cfg.timeout_secs = v;
            }
            other => return Err(ApiError::bad_request(format!("unknown key: {other}"))),
        }
    }
    db::save_cloud_config(&db, &cfg)
        .await
        .map_err(|e| ApiError::internal(format!("cloud config: {e}")))?;
    cloud.store(cfg.clone());
    let mut doc = cloud_document(&cfg);
    doc["applied"] = json!("immediate");
    tracing::info!(provider = %cfg.provider, key_set = !cfg.api_key.is_empty(), "cloud: config updated");
    Ok(Json(doc))
}

/// `POST /api/cloud/test` — one minimal completion against the stored
/// config. Errors map honestly (401 → invalid key).
#[tracing::instrument(skip_all)]
pub async fn test_cloud(
    Extension(cloud): Extension<Arc<CloudAi>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match cloud.test().await {
        Ok((latency_ms, model, reply)) => Ok(Json(json!({
            "ok": true,
            "latency_ms": latency_ms,
            "model": model,
            "reply": reply,
        }))),
        Err(e) => Err(match e {
            CloudError::InvalidKey => {
                ApiError::new(crate::errors::ApiErrorKind::Unauthorized, e.to_string())
            }
            CloudError::Timeout => ApiError::gateway_timeout(e.to_string()),
            other => ApiError::internal(other.to_string()),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn base64_encoding_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base256_edge(), "AAAA");
    }

    fn base256_edge() -> String {
        base64_encode(&[0, 0, 0])
    }

    #[tokio::test]
    async fn complete_against_local_openai_compatible_server() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<serde_json::Value>| async move {
                // Echo the auth header presence and the model back.
                let model = body["model"].as_str().unwrap_or("").to_string();
                let n = body["messages"].as_array().map(|a| a.len()).unwrap_or(0);
                axum::Json(json!({
                    "choices": [{ "message": { "role": "assistant",
                        "content": format!("echo:{model}:{n}") } }]
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let cloud = CloudAi::new(CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-test".into(),
            chat_model: "test/model-a".into(),
            ..CloudConfig::default()
        });
        cloud.set_base_url(&format!("http://{addr}/v1"));
        let turns = vec![
            ChatTurn {
                role: "system".into(),
                content: "sys".into(),
            },
            ChatTurn {
                role: "user".into(),
                content: "hi".into(),
            },
        ];
        let reply = cloud.complete(&turns, None, None, None).await.unwrap();
        assert_eq!(reply, "echo:test/model-a:2");
    }

    fn local_cloud() -> (CloudAi, &'static str) {
        let cloud = CloudAi::new(CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-test".into(),
            chat_model: "test/agent".into(),
            ..CloudConfig::default()
        });
        (cloud, "agent base")
    }

    #[tokio::test]
    async fn agent_complete_parses_tool_calls_and_usage() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(|body: axum::extract::Json<serde_json::Value>| async move {
                // The tool table must ride the request when with_tools.
                let saw_tools = body.get("tools").is_some();
                axum::Json(json!({
                    "choices": [{ "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "weather.current",
                                "arguments": "{\"city\":\"广州\"}"
                            }
                        }]
                    }}],
                    "usage": {"prompt_tokens": 41, "completion_tokens": 7},
                    "saw_tools": saw_tools
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (cloud, _) = local_cloud();
        cloud.set_base_url(&format!("http://{addr}/v1"));

        let tools = vec![crate::agent::ToolSpec {
            name: "weather.current".into(),
            description: "weather".into(),
            input_schema: json!({"type": "object"}),
            source: "builtin".into(),
        }];
        let (text, calls, usage) = cloud
            .agent_complete(
                &[ChatTurn {
                    role: "user".into(),
                    content: "广州天气".into(),
                }],
                &tools,
                true,
                5,
            )
            .await
            .unwrap();
        assert_eq!(text, None);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "weather.current");
        assert_eq!(calls[0].arguments["city"], "广州");
        assert_eq!(usage, Some((41, 7)));
    }

    #[tokio::test]
    async fn agent_complete_without_tools_drops_the_table() {
        use axum::routing::post;
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(None::<bool>));
        let s2 = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(
                move |body: axum::extract::Json<serde_json::Value>| async move {
                    *s2.lock() = Some(body.get("tools").is_some());
                    axum::Json(json!({
                        "choices": [{ "message": {
                            "content": "plain answer", "role": "assistant" }}]
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (cloud, _) = local_cloud();
        cloud.set_base_url(&format!("http://{addr}/v1"));
        let (text, calls, usage) = cloud
            .agent_complete(
                &[ChatTurn {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                &[crate::agent::ToolSpec {
                    name: "t".into(),
                    description: "d".into(),
                    input_schema: json!({}),
                    source: "builtin".into(),
                }],
                false, // forced final text iteration
                5,
            )
            .await
            .unwrap();
        assert_eq!(text.as_deref(), Some("plain answer"));
        assert!(calls.is_empty());
        assert_eq!(usage, None);
        assert_eq!(
            *seen.lock(),
            Some(false),
            "tools key absent with with_tools=false"
        );
    }

    #[tokio::test]
    async fn agent_complete_error_paths_before_any_request() {
        // Off provider.
        let off = CloudAi::new(CloudConfig {
            provider: "off".into(),
            ..CloudConfig::default()
        });
        assert!(matches!(
            off.agent_complete(&[], &[], true, 5).await,
            Err(CloudError::NotEnabled)
        ));
        // Key present but provider off still refuses.
        let off_keyed = CloudAi::new(CloudConfig {
            provider: "off".into(),
            api_key: "sk-x".into(),
            ..CloudConfig::default()
        });
        assert!(matches!(
            off_keyed.agent_complete(&[], &[], true, 5).await,
            Err(CloudError::NotEnabled)
        ));
        // No chat model configured.
        let nomodel = CloudAi::new(CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-x".into(),
            chat_model: String::new(),
            ..CloudConfig::default()
        });
        assert!(matches!(
            nomodel.agent_complete(&[], &[], true, 5).await,
            Err(CloudError::NoModel)
        ));
    }

    #[tokio::test]
    async fn image_rides_the_last_message_as_data_url() {
        use axum::routing::post;
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<serde_json::Value>::new()));
        let s2 = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(
                move |body: axum::extract::Json<serde_json::Value>| async move {
                    let mut s = s2.lock();
                    s.push(body.0);
                    axum::Json(json!({"choices":[{"message":{"content":"ok"}}]}))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let cloud = CloudAi::new(CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-test".into(),
            chat_model: "text-model".into(),
            vision_model: "vision-model".into(),
            ..CloudConfig::default()
        });
        cloud.set_base_url(&format!("http://{addr}/v1"));
        let turns = vec![ChatTurn {
            role: "user".into(),
            content: "what is this".into(),
        }];
        cloud
            .complete(&turns, Some(&[1, 2, 3]), None, None)
            .await
            .unwrap();
        let body = seen.lock()[0].clone();
        assert_eq!(body["model"], "vision-model");
        let content = &body["messages"][0]["content"];
        assert!(
            content.is_array(),
            "vision turn content must be parts: {content}"
        );
        let img = &content[1];
        assert_eq!(img["type"], "image_url");
        assert_eq!(
            img["image_url"]["url"],
            format!("data:image/jpeg;base64,{}", base64_encode(&[1, 2, 3]))
        );
    }

    #[tokio::test]
    async fn disabled_or_keyless_config_errors_not_enabled() {
        let cloud = CloudAi::new(CloudConfig::default());
        let err = cloud
            .complete(
                &[ChatTurn {
                    role: "user".into(),
                    content: "x".into(),
                }],
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err, CloudError::NotEnabled);
        assert!(!cloud.enabled());
        assert!(!cloud.vision_ready());
    }

    #[tokio::test]
    async fn unauthorized_maps_to_invalid_key() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"message": "invalid key"}})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let cloud = CloudAi::new(CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-bad".into(),
            ..CloudConfig::default()
        });
        cloud.set_base_url(&format!("http://{addr}/v1"));
        let err = cloud
            .complete(
                &[ChatTurn {
                    role: "user".into(),
                    content: "x".into(),
                }],
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CloudError::InvalidKey));
    }

    #[test]
    fn cloud_document_never_contains_the_key() {
        let doc = cloud_document(&CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-secret".into(),
            ..CloudConfig::default()
        });
        let text = doc.to_string();
        assert!(!text.contains("sk-secret"));
        assert_eq!(doc["api_key_set"], serde_json::json!(true));
    }
}
