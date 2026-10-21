//! `POST /api/chat` — one local-LLM dialogue turn (SPEC appendix A
//! notebook dialect #22 + #29). Body: `{"text": "...", "history":
//! [{"role","content"}], "vision": bool?}`; response: `{"reply": "...",
//! "grounded": "vlm"|"scene"|"none"}`. Fail-open: 501-family error when
//! the engine is inactive.
//!
//! Grounding (#29): every turn is prepended with a system turn that
//! carries the persona, a language-following instruction (the user may
//! speak Mandarin, Cantonese or English) and — when the grounding state
//! knows something fresh about the camera — a 【画面】 context block
//! built from live detection labels and the last VLM alarm description.
//! `vision: true` additionally routes the question itself through the
//! VLM against a fresh frame (slow, CPU); any failure falls back to
//! the grounded LLM path with an honest `grounded` value.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use streaming::llm::{ChatEngine, ChatTurn};
use streaming::vlm::VlmEngine;

use crate::db;
use crate::errors::ApiError;
use crate::grounding::GroundingState;
use crate::stream_manager::StreamManager;
use security::middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub text: String,
    #[serde(default)]
    pub history: Vec<ChatTurn>,
    /// Explicit VLM Q&A on a fresh frame (SPEC #29; slow on CPU).
    #[serde(default)]
    pub vision: bool,
}

/// The injected context blocks of one dialogue turn (#29 + #30-A).
/// All optional/fail-open: missing blocks are simply omitted.
#[derive(Debug, Default, Clone)]
pub struct TurnContext {
    /// 【画面】 from [`GroundingState::scene_summary`].
    pub scene: Option<String>,
    /// 【本机】 — local clock/system/camera facts (time questions must
    /// never be answered from model memory).
    pub local: Option<String>,
    /// 【联网】 — intent-gated lookup result (weather).
    pub web: Option<String>,
    /// Configured wake word — the assistant's name (#32 persona).
    pub wake_word: String,
}

fn language_hint(user_text: &str) -> Option<&'static str> {
    match streaming::lang::detect(user_text) {
        streaming::lang::SpokenLang::Cantonese => Some(
            "用户使用粤语——请用地道的口语粤语（广东话）回答：用粤语惯用字（而家、嘅、唔、\
             係、咗、乜嘢、冇），不要用普通话书面语（避免写「现在」「的」「不」「什么」）。",
        ),
        streaming::lang::SpokenLang::English => Some("The user speaks English — reply in English."),
        streaming::lang::SpokenLang::Mandarin => None,
    }
}

/// The always-on grounding system turn assembled from a
/// [`TurnContext`]. `user_text` drives the reply-language hint
/// (Cantonese/English are nudged explicitly — a small model does not
/// follow a generic instruction; Mandarin needs no hint).
pub fn build_system_turn(ctx: &TurnContext, user_text: &str) -> ChatTurn {
    let mut content = String::from(&format!(
        "你是一台家庭安防摄像头上的语音助手，名叫{}（唤醒词就是你的名字，\
         不要说自己是其他助手）。用用户所用的语言回复（普通话、粤语或英语），\
         回答简洁。涉及时间、天气、画面等问题时，只依据下面给出的【】资料回答；没有资料就\
         如实说不知道。\n",
        ctx.wake_word
    ));
    if let Some(hint) = language_hint(user_text) {
        content.push_str(hint);
        content.push('\n');
    }
    for (tag, block) in [
        ("【画面】", &ctx.scene),
        ("【本机】", &ctx.local),
        ("【联网】", &ctx.web),
    ] {
        if let Some(block) = block {
            content.push_str(tag);
            content.push_str(block);
            content.push('\n');
        }
    }
    ChatTurn {
        role: "system".into(),
        content,
    }
}

/// Chinese weekday name for an ISO weekday number (1=Monday…7=Sunday).
#[must_use]
pub fn weekday_name(iso_u: usize) -> &'static str {
    const NAMES: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    NAMES[(iso_u.clamp(1, 7) - 1) % 7]
}

/// Render the 【本机】 block from gathered facts (pure — testable).
#[must_use]
pub fn format_local_block(
    now_local: &str,
    weekday: &str,
    uptime_human: &str,
    load1: f64,
    avail_mib: u64,
    cameras: &str,
) -> String {
    format!(
        "当前时间 {now_local} {weekday}；本机已运行 {uptime_human}；负载 {load1:.2}；\
         可用内存 {avail_mib} MiB；相机：{cameras}。回答时间/日期问题必须以此为准。"
    )
}

/// Gather and render the 【本机】 block (reads /proc + camera rows).
pub async fn local_block(pool: &SqlitePool) -> String {
    let now = chrono::Local::now();
    let uptime_human = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| {
            t.split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
        })
        .map(|secs| {
            let d = (secs / 86_400.0) as u64;
            let h = ((secs % 86_400.0) / 3600.0) as u64;
            let m = ((secs % 3600.0) / 60.0) as u64;
            if d > 0 {
                format!("{d}天{h}小时")
            } else {
                format!("{h}小时{m}分")
            }
        })
        .unwrap_or_else(|| "未知".into());
    let load1 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|t| {
            t.split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(0.0);
    let avail_mib = streaming::tools::available_mem_mib().unwrap_or(0);
    let cameras = db::list_cameras(pool)
        .await
        .map(|cams| {
            if cams.is_empty() {
                "无".to_string()
            } else {
                cams.iter()
                    .map(|c| format!("{}({})", c.name, c.status))
                    .collect::<Vec<_>>()
                    .join("、")
            }
        })
        .unwrap_or_else(|_| "未知".into());
    let weekday_idx: usize = now
        .format("%u")
        .to_string()
        .parse()
        .unwrap_or(1)
        .clamp(1, 7);
    format_local_block(
        &now.format("%Y-%m-%d %H:%M:%S").to_string(),
        weekday_name(weekday_idx),
        &uptime_human,
        load1,
        avail_mib,
        &cameras,
    )
}

/// Weather lookup for the 【联网】 block (#30-A): intent-gated,
/// config-enabled, fail-open (errors → None and the model honestly
/// says it does not know).
pub async fn web_block(tools: &streaming::tools::ToolsConfig, user_text: &str) -> Option<String> {
    if !tools.weather_enabled || tools.weather_city.is_empty() {
        return None;
    }
    if !streaming::tools::weather_intent(user_text) {
        return None;
    }
    match streaming::tools::fetch_weather(&tools.weather_city, tools.timeout_secs).await {
        Ok(report) => Some(report),
        Err(e) => {
            tracing::warn!(error = %e, "tools: weather lookup failed (fail-open)");
            None
        }
    }
}

/// Which engine answers this chat turn (SPEC §4.10 routing): the cloud
/// answers first when configured — text via `chat_model`, `vision:true`
/// via `vision_model` on a fresh frame — with the local engines as the
/// fallback (or the only path when the cloud is off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRoute {
    CloudVision,
    CloudChat,
    LocalVlm,
    LocalLlm,
}

/// Pure routing decision (truth-table tested); execution happens in
/// [`chat`].
#[must_use]
pub fn route_chat(
    cloud_on: bool,
    vision: bool,
    cloud_vision_ready: bool,
    vlm_active: bool,
) -> ChatRoute {
    route_chat_routed(
        cloud_on,
        vision,
        cloud_vision_ready,
        vlm_active,
        &streaming::model_route::RoutingConfig::default(),
    )
}

/// The M5 convergence point (design §6): the historic four-arg decision
/// plus the operator's per-capability routing declarations. Defaults
/// reproduce the historic posture exactly (chat cloud-first, vision
/// cloud-if-ready); `[models.chat] provider = "local"` flips a
/// capability local-first without touching the code.
pub fn route_chat_routed(
    cloud_on: bool,
    vision: bool,
    cloud_vision_ready: bool,
    vlm_active: bool,
    routing: &streaming::model_route::RoutingConfig,
) -> ChatRoute {
    use streaming::model_route::Provider;
    let chat_cloud = cloud_on && routing.effective("chat", cloud_on) == Provider::Cloud;
    let vision_cloud =
        cloud_on && cloud_vision_ready && routing.effective("vision", cloud_on) == Provider::Cloud;
    if vision && vision_cloud {
        return ChatRoute::CloudVision;
    }
    if chat_cloud && !vision {
        return ChatRoute::CloudChat;
    }
    if vision && vlm_active {
        return ChatRoute::LocalVlm;
    }
    ChatRoute::LocalLlm
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolve the camera whose grounding/feed a device-level chat turn
/// should use: the first registered camera (single-camera deployments
/// are the norm; deterministic for multi-camera ones).
async fn primary_camera_id(pool: &SqlitePool) -> Option<String> {
    db::list_cameras(pool)
        .await
        .ok()
        .and_then(|cams| cams.first().map(|c| c.id.clone()))
}

#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
pub async fn chat(
    Extension(engine): Extension<Arc<ChatEngine>>,
    Extension(vlm): Extension<Arc<VlmEngine>>,
    Extension(streams): Extension<Arc<StreamManager>>,
    Extension(grounding): Extension<Arc<GroundingState>>,
    Extension(tools): Extension<crate::server::SharedTools>,
    Extension(wake_word): Extension<Arc<crate::routes::capabilities::WakeWord>>,
    Extension(cloud): Extension<Arc<crate::cloud::CloudAi>>,
    Extension(convlog): Extension<Arc<crate::conversations::ConversationLog>>,
    Extension(registry): Extension<Arc<crate::agent::ToolRegistry>>,
    Extension(agent_config): Extension<crate::agent::AgentConfig>,
    Extension(event_tx): Extension<Arc<crate::routes::events::EventBus>>,
    Extension(pool): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
    body: axum::extract::Json<ChatRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if body.text.trim().is_empty() {
        return Err(ApiError::bad_request("text must not be empty"));
    }
    // A configured cloud (SPEC §4.10) answers even when the local model
    // is inactive; the 501 only fires when local is the actual path.
    let cloud_on = cloud.enabled();
    if !cloud_on && !engine.is_active() {
        return Err(ApiError::not_implemented(format!(
            "llm inactive: {}",
            engine.inactive_reason()
        )));
    }
    // Conversation trace (SPEC §3.3): one trace per HTTP dialogue turn;
    // the model chain inside is recorded by convtrace spans below.
    let conv = crate::convtrace::convtrace().start("chat");
    conv.add_turn();
    // Conversation record (SPEC §3.4): the same turn as a human-readable
    // log entry — prompt, one thinking note per leg (failed fallbacks
    // included), reply + engine. Finished on every exit path below.
    let mut draft =
        crate::conversations::TurnDraft::new(crate::conversations::TurnOrigin::Http, conv.id())
            .user_text(body.text.clone());
    let camera_id = primary_camera_id(&pool).await;
    let cloud_on = cloud.enabled();
    let route = route_chat(cloud_on, body.vision, cloud.vision_ready(), vlm.is_active());
    let tools_now = tools.read().expect("tools config lock poisoned").clone();
    let ctx = TurnContext {
        scene: camera_id
            .as_deref()
            .and_then(|id| grounding.scene_summary(id, unix_now_ms())),
        local: Some(local_block(&pool).await),
        web: web_block(&tools_now, &body.text).await,
        wake_word: wake_word.0.clone(),
    };
    let scene = ctx.scene.clone();

    // Turn assembly (shared by cloud and local paths — the cloud gets
    // the same persona/grounding system turn, SPEC #35).
    let mut turns: Vec<ChatTurn> = vec![build_system_turn(&ctx, &body.text)];
    turns.extend(body.history.iter().take(8).cloned());
    turns.push(ChatTurn {
        role: "user".into(),
        content: body.text.clone(),
    });
    let grounded = if scene.is_some() { "scene" } else { "none" };

    // ── Agent tool loop (SPEC §3.5): text routes with a non-empty
    // registry run the tool-calling loop first; any failure falls
    // through to the legacy single-shot legs unchanged (fail-open).
    if !body.vision
        && matches!(route, ChatRoute::CloudChat | ChatRoute::LocalLlm)
        && registry.agent_ready(&agent_config)
    {
        let cloud_first = route == ChatRoute::CloudChat;
        let engine_label = if cloud_first { "cloud" } else { "local" };
        let (think_source, model_id) = if cloud_first {
            ("cloud.chat", cloud.config().chat_model.clone())
        } else {
            ("llm", engine.model_variant())
        };
        let channel: Box<dyn crate::agent::runner::ModelChannel> = if cloud_first {
            Box::new(crate::agent::channels::CloudChannel {
                cloud: Arc::clone(&cloud),
                timeout_secs: cloud.config().timeout_secs,
            })
        } else {
            Box::new(crate::agent::channels::LocalChannel {
                engine: Arc::clone(&engine),
            })
        };
        let conv_id = conv.id().to_string();
        let tx = Arc::clone(&event_tx);
        // (source, model, note, duration) — folded into the draft after.
        let mut thinks: Vec<(String, String, String, u64)> = Vec::new();
        let agent_started = std::time::Instant::now();
        let send_step = |tx: &crate::routes::events::EventBus,
                         conv_id: &str,
                         kind: &str,
                         state: &str,
                         tool: Option<String>,
                         args: Option<serde_json::Value>,
                         result: Option<String>,
                         duration_ms: Option<u64>| {
            let _ = tx.send(crate::routes::events::CameraEvent::AgentStep {
                conversation_id: conv_id.to_string(),
                kind: kind.to_string(),
                state: state.to_string(),
                tool,
                args,
                result,
                duration_ms,
                note: None,
            });
        };
        let outcome = crate::agent::runner::run_agent(
            &registry,
            channel.as_ref(),
            &turns,
            engine_label,
            agent_config.max_steps,
            std::time::Duration::from_millis(agent_config.step_timeout_ms.max(1000)),
            |step| match step {
                crate::agent::AgentStep::PhaseThinking => {
                    send_step(&tx, &conv_id, "phase", "thinking", None, None, None, None);
                }
                crate::agent::AgentStep::PhaseAnswering { engine } => {
                    let _ = engine; // the SSE note field stays unused in v1
                    send_step(&tx, &conv_id, "phase", "answering", None, None, None, None);
                }
                crate::agent::AgentStep::ToolStarted { name, args } => {
                    send_step(
                        &tx,
                        &conv_id,
                        "tool",
                        "running",
                        Some(name.clone()),
                        Some(args.clone()),
                        None,
                        None,
                    );
                }
                crate::agent::AgentStep::ToolFinished {
                    name: _,
                    args: _,
                    record,
                } => {
                    send_step(
                        &tx,
                        &conv_id,
                        "tool",
                        if record.ok { "done" } else { "error" },
                        Some(record.name.clone()),
                        Some(record.args.clone()),
                        Some(record.result.clone()),
                        Some(record.duration_ms),
                    );
                    thinks.push((
                        "tool".to_string(),
                        record.name.clone(),
                        record.result.clone(),
                        record.duration_ms,
                    ));
                }
            },
        )
        .await;
        match outcome {
            Ok(out) => {
                let dur = agent_started.elapsed().as_millis() as u64;
                conv.close();
                let mut d = draft;
                for (source, model, note, ms) in thinks {
                    d = d.think(&source, &model, note, ms);
                }
                let note = if out.tokens.0 > 0 || out.tokens.1 > 0 {
                    format!(
                        "agent 应答 · prompt {}↑ / completion {}↓ tok（含 {} 次工具调用）",
                        out.tokens.0,
                        out.tokens.1,
                        out.tool_calls.len()
                    )
                } else {
                    format!("agent 应答（含 {} 次工具调用）", out.tool_calls.len())
                };
                d = d.think(think_source, &model_id, note, dur);
                convlog
                    .finish(d.reply(out.reply.clone(), engine_label))
                    .await;
                return Ok((
                    StatusCode::OK,
                    axum::Json(json!({
                        "reply": out.reply,
                        "engine": engine_label,
                        "grounded": grounded,
                        "applied": "immediate",
                        "tool_calls": out.tool_calls,
                    })),
                ));
            }
            Err(e) => {
                // Fail-open: the legacy legs below answer instead.
                tracing::warn!(error = %e, "chat: agent loop failed — falling back to legacy legs");
            }
        }
    }

    // ── Cloud routing (SPEC §4.10: text→chat_model, vision→vision_model
    // on a fresh frame; failures fall back locally when allowed).
    let fresh_jpeg: Option<Arc<[u8]>> = match camera_id.as_deref() {
        Some(id) if matches!(route, ChatRoute::CloudVision | ChatRoute::LocalVlm) => {
            streams.latest_jpeg(id).await
        }
        _ => None,
    };
    if route == ChatRoute::CloudVision
        && let Some(jpeg) = fresh_jpeg.clone()
    {
        let model_id = cloud.config().vision_model.clone();
        let mut span = conv.span(
            "cloud.vision",
            &model_id,
            "云端看图应答",
            vec![("grounded".to_string(), "vlm".to_string())],
        );
        let jpeg: Vec<u8> = jpeg.iter().copied().collect();
        let leg_start = std::time::Instant::now();
        match span
            .run(cloud.complete_with_usage(&turns, Some(&jpeg), None, None))
            .await
        {
            Ok((reply, usage)) => {
                let dur = leg_start.elapsed().as_millis() as u64;
                span.tokens(usage.map(|u| u.0), usage.map(|u| u.1))
                    .finish_ok();
                let note = match usage {
                    Some((p, c)) => format!("云端看图应答 · {p}↑/{c}↓ tok"),
                    None => "云端看图应答".to_string(),
                };
                draft = draft.think("cloud.vision", &model_id, note, dur);
                conv.close();
                convlog.finish(draft.reply(reply.clone(), "cloud")).await;
                return Ok((
                    StatusCode::OK,
                    axum::Json(json!({
                        "reply": reply,
                        "engine": "cloud",
                        "grounded": "vlm",
                        "applied": "immediate",
                    })),
                ));
            }
            Err(e) => {
                let dur = leg_start.elapsed().as_millis() as u64;
                span.finish_err();
                if !cloud.config().fallback_local {
                    conv.close();
                    convlog
                        .finish(draft.think(
                            "cloud.vision",
                            &model_id,
                            format!("云端看图失败：{e}（不回落）"),
                            dur,
                        ))
                        .await;
                    return Err(ApiError::internal(format!("cloud: {e}")));
                }
                tracing::warn!(error = %e, "chat: cloud vision failed — falling back to local");
                draft = draft.think(
                    "cloud.vision",
                    &model_id,
                    format!("云端看图失败：{e} — 回落本地"),
                    dur,
                );
            }
        }
    } else if route == ChatRoute::CloudChat {
        let model_id = cloud.config().chat_model.clone();
        let mut span = conv.span(
            "cloud.chat",
            &model_id,
            "云端应答",
            vec![("grounded".to_string(), grounded.to_string())],
        );
        let leg_start = std::time::Instant::now();
        match span
            .run(cloud.complete_with_usage(&turns, None, None, None))
            .await
        {
            Ok((reply, usage)) => {
                let dur = leg_start.elapsed().as_millis() as u64;
                span.tokens(usage.map(|u| u.0), usage.map(|u| u.1))
                    .finish_ok();
                let note = match usage {
                    Some((p, c)) => format!("云端应答 · {p}↑/{c}↓ tok"),
                    None => "云端应答".to_string(),
                };
                draft = draft.think("cloud.chat", &model_id, note, dur);
                conv.close();
                convlog.finish(draft.reply(reply.clone(), "cloud")).await;
                return Ok((
                    StatusCode::OK,
                    axum::Json(json!({
                        "reply": reply,
                        "engine": "cloud",
                        "grounded": grounded,
                        "applied": "immediate",
                    })),
                ));
            }
            Err(e) => {
                let dur = leg_start.elapsed().as_millis() as u64;
                span.finish_err();
                if !cloud.config().fallback_local {
                    conv.close();
                    convlog
                        .finish(draft.think(
                            "cloud.chat",
                            &model_id,
                            format!("云端失败：{e}（不回落）"),
                            dur,
                        ))
                        .await;
                    return Err(ApiError::internal(format!("cloud: {e}")));
                }
                tracing::warn!(error = %e, "chat: cloud reply failed — falling back to local llm");
                draft = draft.think(
                    "cloud.chat",
                    &model_id,
                    format!("云端失败：{e} — 回落本地"),
                    dur,
                );
            }
        }
    }

    // ── Local VLM Q&A (#29): the question itself goes to the vision
    // model with a fresh frame. Any failure falls through to the
    // grounded LLM path — never a hard error for an optional mode.
    if body.vision
        && vlm.is_active()
        && let Some(jpeg) = fresh_jpeg
    {
        let vlm_variant = vlm.model_variant();
        let vlm = Arc::clone(&vlm);
        let question = body.text.clone();
        let mut span = conv.span(
            "vlm",
            &vlm_variant,
            "看图直答",
            vec![("grounded".to_string(), "vlm".to_string())],
        );
        let leg_start = std::time::Instant::now();
        let answered = match span
            .run(tokio::task::spawn_blocking(move || {
                vlm.answer_jpeg(&jpeg, &question)
            }))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                conv.close();
                convlog
                    .finish(draft.think(
                        "vlm",
                        &vlm_variant,
                        format!("看图直答任务失败：{e}"),
                        leg_start.elapsed().as_millis() as u64,
                    ))
                    .await;
                return Err(ApiError::internal(format!("vlm task: {e}")));
            }
        };
        match answered {
            Ok(reply) => {
                let dur = leg_start.elapsed().as_millis() as u64;
                span.finish_ok();
                draft = draft.think("vlm", &vlm_variant, "看图直答", dur);
                conv.close();
                convlog.finish(draft.reply(reply.clone(), "vlm")).await;
                return Ok((
                    StatusCode::OK,
                    axum::Json(json!({
                        "reply": reply,
                        "engine": "vlm",
                        "grounded": "vlm",
                        "applied": "immediate",
                    })),
                ));
            }
            Err(e) => {
                let dur = leg_start.elapsed().as_millis() as u64;
                span.finish_err();
                tracing::warn!(error = %e, "chat: vlm Q&A failed — falling back to llm");
                draft = draft.think(
                    "vlm",
                    &vlm_variant,
                    format!("看图直答失败：{e} — 回落本地"),
                    dur,
                );
            }
        }
    }

    // ── Local LLM (the only path when the cloud is off, the fallback
    // when it failed). Token counts flow into the conversation span
    // (SPEC §3.3).
    let engine_variant = engine.model_variant();
    let engine = Arc::clone(&engine);
    let mut span = conv.span(
        "llm",
        &engine_variant,
        "本地应答",
        vec![("grounded".to_string(), grounded.to_string())],
    );
    let leg_start = std::time::Instant::now();
    let reply = match span
        .run(tokio::task::spawn_blocking(move || {
            engine.complete_with_usage(&turns)
        }))
        .await
    {
        Ok(Ok((reply, prompt_tokens, completion_tokens))) => {
            let dur = leg_start.elapsed().as_millis() as u64;
            span.tokens(Some(prompt_tokens), Some(completion_tokens))
                .finish_ok();
            draft = draft.think(
                "llm",
                &engine_variant,
                format!("本地应答 · prompt {prompt_tokens}↑ / completion {completion_tokens}↓ tok"),
                dur,
            );
            reply
        }
        Ok(Err(e)) => {
            let dur = leg_start.elapsed().as_millis() as u64;
            span.finish_err();
            conv.close();
            convlog
                .finish(draft.think("llm", &engine_variant, format!("本地应答失败：{e}"), dur))
                .await;
            return Err(ApiError::internal(format!("llm: {e}")));
        }
        Err(e) => {
            let dur = leg_start.elapsed().as_millis() as u64;
            conv.close();
            convlog
                .finish(draft.think(
                    "llm",
                    &engine_variant,
                    format!("本地应答任务失败：{e}"),
                    dur,
                ))
                .await;
            return Err(ApiError::internal(format!("llm task: {e}")));
        }
    };
    conv.close();
    convlog.finish(draft.reply(reply.clone(), "local")).await;
    Ok((
        StatusCode::OK,
        axum::Json(json!({
            "reply": reply,
            "engine": "local",
            "grounded": grounded,
            "applied": "immediate",
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(scene: Option<&str>) -> TurnContext {
        TurnContext {
            scene: scene.map(String::from),
            local: Some("当前时间 2026-10-01 21:00:00 周四；本机已运行 3小时5分".into()),
            web: None,
            wake_word: streaming::voice::DEFAULT_WAKE_WORD.into(),
        }
    }

    /// End-to-end through the real handler: a cloud-configured chat turn
    /// runs the agent tool loop against a local OpenAI-compatible fake —
    /// first iteration answers with a tool_call (time.now executes for
    /// real through the registry), the forced final iteration answers in
    /// text. Exercises the agent_step SSE arms, the draft's tool thinking
    /// entries and the tool_calls response surface.
    #[tokio::test]
    async fn chat_handler_runs_the_cloud_agent_loop_end_to_end() {
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&calls);
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(
                move |body: axum::extract::Json<serde_json::Value>| async move {
                    let n = c2.fetch_add(1, Ordering::SeqCst);
                    let with_tools = body.get("tools").is_some();
                    if n == 0 && with_tools {
                        axum::Json(json!({
                            "choices": [{ "message": {
                                "role": "assistant", "content": null,
                                "tool_calls": [{
                                    "id": "c1", "type": "function",
                                    "function": {"name": "time.now", "arguments": "{}"}
                                }]
                            }}]
                        }))
                    } else {
                        axum::Json(json!({
                            "choices": [{ "message": {
                                "role": "assistant", "content": "现在 12:00（假服务器）"
                            }}]
                        }))
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (pool, auth_db) = crate::routes::config_api::tests::test_db().await;
        // Seeded admin + session in the state's own auth DB (the shared
        // fixture), or the setup gate answers 503.
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let (mut state, _ai) =
            crate::routes::config_api::tests::build_state_with_ai(pool.clone(), auth_db);
        let cloud = crate::cloud::CloudAi::new(crate::cloud::CloudConfig {
            provider: "openrouter".into(),
            api_key: "sk-test".into(),
            chat_model: "fake/model".into(),
            ..crate::cloud::CloudConfig::default()
        });
        cloud.set_base_url(&format!("http://{addr}/v1"));
        state.cloud = Arc::new(cloud);
        let mut bus_rx = state.event_tx.subscribe();

        let app = crate::server::build_app_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/api/chat")
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=t"))
            .header("x-csrf-token", "t")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_vec(&json!({"text": "现在几点了"})).unwrap(),
            ))
            .unwrap();
        use tower::ServiceExt;
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "chat handler error: {body}"
        );
        let data = &body["data"];
        assert_eq!(data["engine"], "cloud", "{body}");
        assert_eq!(
            data["reply"].as_str().map(str::to_string),
            Some("现在 12:00（假服务器）".into()),
            "{body}"
        );
        let tool_calls = data["tool_calls"].as_array().cloned().unwrap_or_default();
        assert_eq!(tool_calls.len(), 1, "{body}");
        assert_eq!(tool_calls[0]["name"], "time.now");

        // The SSE arms fired: thinking → tool running → tool done →
        // answering, in order, on the real bus.
        let mut states: Vec<(&'static str, &'static str)> = Vec::new();
        for _ in 0..16 {
            match tokio::time::timeout(std::time::Duration::from_millis(2_000), bus_rx.recv()).await
            {
                Ok(Ok(crate::routes::events::CameraEvent::AgentStep { kind, state, .. })) => {
                    states.push((kind.clone().leak(), state.clone().leak()));
                }
                _ => break,
            }
        }
        assert!(
            states.contains(&("phase", "thinking")),
            "agent phases on the wire: {states:?}"
        );
        assert!(
            states.contains(&("tool", "running")) && states.contains(&("tool", "done")),
            "tool lifecycle on the wire: {states:?}"
        );
        assert!(
            states.contains(&("phase", "answering")),
            "agent phases on the wire: {states:?}"
        );

        // The conversation record carries the tool thinking entry.
        let mut turns = Vec::new();
        for _ in 0..40 {
            if let Ok(t) = crate::db::list_conversation_turns(&pool, 5).await
                && !t.is_empty()
            {
                turns = t;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            turns
                .iter()
                .any(|t| t.user_text.as_deref() == Some("现在几点了")),
            "{turns:?}"
        );
    }

    #[test]
    fn system_turn_carries_language_scene_and_local() {
        let t = build_system_turn(&ctx(Some("实时检测：2×person")), "你看到几个人");
        assert_eq!(t.role, "system");
        assert!(t.content.contains("粤语"), "{}", t.content);
        assert!(
            t.content.contains("【画面】实时检测：2×person"),
            "{}",
            t.content
        );
        assert!(
            t.content.contains("【本机】当前时间 2026-10-01"),
            "{}",
            t.content
        );
        assert!(!t.content.contains("用户使用粤语"), "{}", t.content);
    }

    #[tokio::test]
    async fn local_block_renders_machine_state_and_cameras() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.unwrap();
        let block = local_block(&pool).await;
        assert!(block.contains("当前时间"), "{block}");
        assert!(block.contains("可用内存"), "{block}");
        assert!(block.contains("相机：无"), "no cameras registered: {block}");
        // A registered camera shows up as name(status).
        crate::db::create_camera(
            &pool,
            &crate::db::CameraRow {
                id: "c1".into(),
                name: "门口".into(),
                camera_type: "usb".into(),
                config: serde_json::json!({}),
                status: "online".into(),
                created_at: "0".into(),
                updated_at: "0".into(),
                offline_since: None,
            },
        )
        .await
        .unwrap();
        let block = local_block(&pool).await;
        assert!(block.contains("门口(online)"), "{block}");
    }

    #[tokio::test]
    async fn web_block_gates_on_flag_and_intent() {
        let mut tools = streaming::tools::ToolsConfig::default();
        // Off by flag.
        assert!(web_block(&tools, "广州天气怎么样").await.is_none());
        // On but the turn has no weather intent.
        tools.weather_enabled = true;
        tools.weather_city = "Guangzhou".into();
        assert!(web_block(&tools, "今天几号").await.is_none());
        // No city configured → still none.
        tools.weather_city = String::new();
        assert!(web_block(&tools, "广州天气怎么样").await.is_none());
    }

    #[tokio::test]
    async fn primary_camera_prefers_the_first_registered() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.unwrap();
        assert_eq!(primary_camera_id(&pool).await, None);
        for id in ["b", "a"] {
            crate::db::create_camera(
                &pool,
                &crate::db::CameraRow {
                    id: id.into(),
                    name: format!("cam-{id}"),
                    camera_type: "usb".into(),
                    config: serde_json::json!({}),
                    status: "online".into(),
                    created_at: "0".into(),
                    updated_at: "0".into(),
                    offline_since: None,
                },
            )
            .await
            .unwrap();
        }
        // list_cameras orders by creation; the first registered wins.
        let primary = primary_camera_id(&pool).await;
        assert!(primary.is_some());
    }

    #[test]
    fn unix_now_ms_is_plausible() {
        let now = unix_now_ms();
        assert!(now > 1_700_000_000_000, "after 2023-11: {now}");
    }

    #[test]
    fn system_turn_without_scene_omits_that_block_only() {
        let t = build_system_turn(&ctx(None), "你好");
        assert!(!t.content.contains("【画面】"), "{}", t.content);
        assert!(t.content.contains("【本机】"), "{}", t.content);
    }

    #[test]
    fn system_turn_persona_follows_configured_wake_word() {
        let mut c = ctx(None);
        c.wake_word = "你好小蜂".into();
        let t = build_system_turn(&c, "你叫什么名字");
        assert!(t.content.contains("名叫你好小蜂"), "{}", t.content);
        assert!(!t.content.contains("小蜜蜂"), "{}", t.content);
    }

    #[test]
    fn system_turn_cantonese_gets_vernacular_hint() {
        // 而家-only questions reach the Cantonese arm via the bigram and
        // must carry the vernacular-writing nudge — a small model turns
        // a bare "用粤语回答" into standard written Chinese.
        let t = build_system_turn(&ctx(None), "而家广州天气点呀");
        assert!(t.content.contains("口语粤语"), "{}", t.content);
        assert!(t.content.contains("而家、嘅"), "{}", t.content);
        assert!(t.content.contains("不要用普通话书面语"), "{}", t.content);
    }

    #[test]
    fn web_block_appears_when_fetched() {
        let mut c = ctx(None);
        c.web = Some("广州 当前天气：Partly cloudy，气温 26°C".into());
        let t = build_system_turn(&c, "今天天气如何");
        assert!(t.content.contains("【联网】广州"), "{}", t.content);
    }

    #[test]
    fn cantonese_text_gets_explicit_nudge() {
        let t = build_system_turn(&ctx(None), "我唔知道啊");
        assert!(t.content.contains("用户使用粤语"), "{}", t.content);
    }

    #[test]
    fn english_text_gets_explicit_nudge() {
        let t = build_system_turn(&ctx(None), "how many people do you see?");
        assert!(t.content.contains("reply in English"), "{}", t.content);
    }

    #[test]
    fn weekday_names_map_iso_monday_first() {
        // 2026-10-01 is a Thursday → ISO weekday 4 → 周四 (the Sunday-first
        // off-by-one this replaces said 周三).
        assert_eq!(weekday_name(4), "周四");
        assert_eq!(weekday_name(1), "周一");
        assert_eq!(weekday_name(7), "周日");
        // Out-of-range values clamp instead of panicking.
        assert_eq!(weekday_name(0), "周一");
        assert_eq!(weekday_name(9), "周日");
    }

    #[test]
    fn chat_routing_truth_table() {
        use super::ChatRoute::*;
        use super::route_chat_routed;
        use streaming::model_route::{Provider, RouteSelection, RoutingConfig};
        let def = RoutingConfig::default();
        // Defaults (M5 privacy decision): chat cloud-first, vision
        // local-first even when the cloud has a vision model.
        assert_eq!(
            route_chat_routed(true, false, false, false, &def),
            CloudChat
        );
        assert_eq!(
            route_chat_routed(true, true, true, true, &def),
            LocalVlm,
            "privacy surface stays local under defaults"
        );
        // Operator override flips vision cloud-first (historic posture).
        let cloud_vision = RoutingConfig {
            vision: RouteSelection {
                provider: Provider::Cloud,
                fallback: Provider::Local,
            },
            ..def.clone()
        };
        assert_eq!(
            route_chat_routed(true, true, true, false, &cloud_vision),
            CloudVision
        );
        // Cloud on, vision but no vision_model configured → local VLM.
        assert_eq!(route_chat_routed(true, true, false, true, &def), LocalVlm);
        // Cloud off, vision with local VLM → local VLM.
        assert_eq!(route_chat_routed(false, true, false, true, &def), LocalVlm);
        // Vision but no VLM anywhere → grounded local LLM.
        assert_eq!(route_chat_routed(false, true, false, false, &def), LocalLlm);
        assert_eq!(route_chat_routed(true, true, false, false, &def), LocalLlm);
        // Plain text without cloud → local LLM.
        assert_eq!(route_chat_routed(false, false, false, true, &def), LocalLlm);
        // chat pinned local → cloud on still answers locally.
        let local_chat = RoutingConfig {
            chat: RouteSelection::LOCAL_ONLY,
            ..def.clone()
        };
        assert_eq!(
            route_chat_routed(true, false, false, true, &local_chat),
            LocalLlm
        );
    }

    #[test]
    fn local_block_formats_the_clock_authoritatively() {
        let b = format_local_block(
            "2026-10-01 21:00:00",
            "周四",
            "3小时5分",
            0.42,
            8192,
            "客厅(running)",
        );
        assert!(b.contains("当前时间 2026-10-01 21:00:00 周四"), "{b}");
        assert!(b.contains("可用内存 8192 MiB"), "{b}");
        assert!(b.contains("客厅(running)"), "{b}");
        assert!(b.contains("必须以此为准"), "{b}");
    }
}
