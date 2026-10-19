//! Hardware capability introspection endpoints.
//!
//! `GET /api/capabilities` returns the probed host hardware snapshot plus the
//! recommended encoder profiles, letting the Web UI present adaptive
//! resolution / quality options to the user.

use axum::Json;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;
use std::sync::Arc;

use security::middleware::AuthenticatedUser;
use streaming::ai::AiEngine;
use streaming::capability::{EncoderProfile, SystemCapabilities, probe, recommended_profiles};

// ---------------------------------------------------------------------------
// Response model
// ---------------------------------------------------------------------------

/// Response body for `GET /api/capabilities`.
#[derive(Debug, Serialize)]
pub struct CapabilitiesResponse {
    /// Probed host hardware + available encoder backends.
    pub system: SystemCapabilities,
    /// Suggested encoder profiles (one per resolution tier the host can drive).
    pub recommended_profiles: Vec<EncoderProfile>,
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// GET /api/capabilities — the SPEC v1 capability superset (§3.1) plus the
/// host hardware probe as extension fields (`system`, `recommended_profiles`).
///
/// Probing is cheap (a handful of sysfs/`/proc` reads) but not free, so the
/// result is cached for the process lifetime via a [`std::sync::OnceLock`].
/// Resolved LLM resource tier (#30-E) as an Extension-safe newtype.
#[derive(Debug, Clone)]
pub struct LlmTier(pub String);

/// Configured wake word (#32) — newtype so its `Arc<...>` Extension
/// cannot collide with `Arc<String>` advertised-host.
pub struct WakeWord(pub String);

/// The engine handles `GET /api/capabilities` reports on, bundled into a
/// single Extension.
///
/// **Why a struct and not one `Extension` per engine:** axum handlers top out
/// at 16 extractors, and this handler was already sitting exactly on that
/// ceiling with 15 engines + `AuthenticatedUser` — so adding the one engine
/// that was missing (face) was a compile error, not a one-line change. (The
/// same ceiling has bitten this file before; see the 2026-10-10 away-mode
/// note in the workspace log.) Grouping the handles keeps the handler at two
/// extractors and leaves room for the next engine.
#[derive(Clone)]
pub struct CapHandles {
    pub ai: Arc<AiEngine>,
    pub audio_ai: Arc<streaming::audio_ai::AudioAiEngine>,
    pub ocr: Arc<streaming::ocr::OcrEngine>,
    pub voice: Arc<streaming::voice::VoiceEngine>,
    pub chat: Arc<streaming::llm::ChatEngine>,
    pub llm_tier: Arc<LlmTier>,
    pub resource: Arc<streaming::feature_gate::ResourceProfile>,
    pub decision: Arc<streaming::decision::DecisionEngine>,
    pub meeting: Arc<streaming::meeting::MeetingEngine>,
    pub vlm: Arc<streaming::vlm::VlmEngine>,
    pub face: Arc<streaming::face::FaceEngine>,
    pub stream_manager: Arc<crate::stream_manager::StreamManager>,
    pub convlog: Arc<crate::conversations::ConversationLog>,
    pub agent: Arc<crate::agent::ToolRegistry>,
    pub agent_config: crate::agent::AgentConfig,
    pub away: Arc<crate::away::AwayEngine>,
}

#[tracing::instrument(skip_all)]
pub async fn get_capabilities(
    Extension(h): Extension<CapHandles>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    let CapHandles {
        ai,
        audio_ai,
        ocr,
        voice,
        chat,
        llm_tier,
        resource,
        decision,
        meeting,
        vlm,
        face,
        stream_manager,
        convlog,
        agent,
        agent_config,
        away,
    } = h;
    static CACHE: std::sync::OnceLock<CapabilitiesResponse> = std::sync::OnceLock::new();
    let cached = CACHE.get_or_init(|| {
        let system = probe();
        let recommended = recommended_profiles(&system);
        CapabilitiesResponse {
            system,
            recommended_profiles: recommended,
        }
    });
    let mut events = vec!["camera_added", "camera_offlined"];
    let ai_hot_swap = ai.is_active() && ai_has_factory(&ai);
    if ai.is_active() {
        events.push("ai_detection");
    }
    if ai_hot_swap {
        events.push("ai_model_changed");
    }
    // The alarm bridge fires on AI rising edges (visual) or voted sound
    // classes (audio) and fans out to the SSE hub, the GB Alarm NOTIFY
    // and the ONVIF MotionAlarm — each channel no-ops until its consumer
    // is up/subscribed, so the event itself only requires either engine.
    if ai.is_active() || audio_ai.is_active() {
        events.push("alarm");
    }
    // Zone events ride the tracker inside the AI worker.
    if ai.is_active() {
        events.push("zone_event");
    }
    if voice.is_active() {
        events.push("voice_transcript");
    }
    // Dialogue turn records (SPEC §3.4): one SSE event per finished turn.
    if convlog.is_enabled() {
        events.push("conversation");
    }
    // Agent tool-calling steps (SPEC §3.5 `agent_step`): emitted while
    // the agent loop can engage at all.
    let tool_count = if agent_config.enabled {
        agent.cached_specs().len()
    } else {
        0
    };
    if agent_config.enabled && tool_count > 0 {
        events.push("agent_step");
    }
    // Away mode (SPEC §3.6): records + armed-state sync events ride the
    // bus whenever the watch can arm at all.
    if away.is_available() {
        events.push("away_event");
        events.push("away_state");
    }
    // Meeting lifecycle (SPEC appendix A #27).
    if meeting.is_active() {
        events.push("meeting_state");
    }
    // VLM alarm-frame descriptions (SPEC appendix A #23) ride the visual
    // alarm pipeline asynchronously.
    if vlm.is_active() {
        events.push("alarm_description");
    }
    // Model download progress (SPEC §4.9 model_task) rides the bus.
    events.push("model_task");
    // Real-time mic level for the voice waveform (SPEC §6 audio_level):
    // fires while any audio consumer keeps the monitor running.
    if voice.is_active() || audio_ai.is_active() || meeting.is_active() {
        events.push("audio_level");
    }
    // Voice-turn LLM replies (SPEC appendix A #22). Both emitters live on
    // the voice bridge (`src/main.rs`), so the voice engine is the real gate.
    //
    // This was simply missing from the advertised list, which made the event
    // unreachable from every client: `static/js/sse.js` registers listeners
    // *only* for names in `capabilities.events`, so a `chat_reply` payload
    // arriving on the wire had no handler. The frontend's conversations-off
    // fallback in `chat.js` has been dead code for the same reason.
    if voice.is_active() {
        events.push("chat_reply");
    }
    // Laya typed-decision triage over voice transcripts (SPEC appendix A #26)
    // — same omission, same dead-on-arrival shape.
    if decision.is_active() {
        events.push("voice_decision");
    }
    let superset = serde_json::json!({
        "spec_version": "1",
        "device": {
            "name": "mibee-eye",
            "model": "notebook",
            "vendor": "MiBee Studio",
        },
        "auth": {"model": "session", "setup": true},
        // Build provenance (mibee-rec#1): version + git sha + dirty flag
        // stamped at compile time — additive wire key, old frontends ignore.
        "build": build_document(),
        "multi_camera": true,
        "camera_management": true,
        "camera_control": true,
        "imaging": false,
        "ai": ai.is_active(),
        // Always-on sound-event detection + voice-presence signal
        // (SPEC appendix A notebook dialect; fail-open like `ai`).
        "audio_ai": audio_ai.is_active(),
        // Persistent hearing records (`GET/DELETE /api/audio/records`,
        // SPEC appendix A #24): anything the audio engines recognize
        // lands as a queryable text record.
        "audio_records": audio_records_capable(audio_ai.is_active(), voice.is_active()),
        // User-drawn intrusion/tripwire zones (`GET/PUT
        // /api/cameras/{id}/zones`); events require AI tracking.
        "zones": ai.is_active(),
        // On-device text recognition (PP-OCRv5, `POST /api/ocr`).
        "ocr": ocr.is_active(),
        // Wake word + offline transcription (SSE `voice_transcript`).
        "voice": voice.is_active(),
        // Voiceprint speaker profiles (`/api/voice/speakers`, SPEC
        // appendix A #25): enroll, verify-gate wake words, tag records.
        "voice_speakers": voice_speakers_capable(voice.is_active(), voice.speaker_capable()),
        // Local LLM dialogue (`POST /api/chat`).
        "chat": chat.is_active(),
        // Dialogue turn records (SPEC §3.4) — the human-readable log.
        "conversations": convlog.is_enabled(),
        // Tool/skill framework (SPEC §3.5): built-ins + MCP plugin
        // servers; `count` mirrors the current registry listing.
        "tools": tools_document(agent_config.enabled, tool_count),
        // Away mode watch surface (SPEC §3.6, appendix A #44):
        // `available` = armable (AI detection active), `voice` = the
        // greet-and-ask legs can run.
        "away": away_document(away.is_available(), away.voice_capable()),
        // Resource tier the LLM booted into (#30-E).
        "llm_tier": llm_tier.0.as_str(),
        // Boot-time feature admission snapshot (appendix A #40):
        // mode/budget plus per-feature cost & decision. Serialised from
        // the planner's own struct — the wire shape is its contract.
        "resource": serde_json::to_value(resource.as_ref()).unwrap_or_default(),
        // Laya typed-decision triage over voice transcripts (SSE
        // `voice_decision`, SPEC appendix A #26).
        "decision": decision.is_active(),
        // On-demand meeting mode (record → diarize → transcribe, SPEC
        // appendix A #27).
        "meeting": meeting.is_active(),
        // VLM alarm-frame descriptions (SSE `alarm_description`).
        "vlm": vlm.is_active(),
        "ai_models": ai_hot_swap,
        "ai_upload": ai_hot_swap && ai.config().allow_upload,
        // Model manager (SPEC §4.9): full-catalog download/switch surface.
        "model_manager": true,
        // Online AI via OpenRouter (SPEC §4.10) — the surface exists; the
        // routing-on state is GET /api/cloud's `provider != off`.
        "cloud_ai": true,
        "ptz": false,
        "hls": false,
        // Recording is config-only on this device (protocols.recording);
        // there is no per-camera record endpoint yet.
        "recording": false,
        // SPEC v1 §5.2: burned into every video output pre-encode; config
        // lives in protocols.watermark (applies at next stream start).
        "watermark": true,
        "devices": true,
        "mjpeg": true,
        "mse": true,
        // Low-resolution substream (SPEC appendix A #20): advertised
        // while any active stream runs with a substream.
        "substream": stream_manager.any_substream_active().await,
        "webrtc": false,
        "events": events,
        "config_apply": {"default": "immediate", "sections": {"scene": "immediate"},
                         "auto": true},
        "restart": true,
        // Face recognition (SPEC appendix A #33): YuNet detect + SFace
        // embeddings, `/api/faces`. Honesty matters more than coverage here —
        // this used to be hardcoded `true`, so a stock install (where
        // `[face]` defaults to off and the models are deployment assets)
        // rendered the "Enrolled faces" card and clicking enroll returned
        // 501. An over-promising capability is worse than an absent one,
        // because the frontend gates every card on it.
        "face": face.is_active(),
        "observability": observability_document(),
        // Device-specific extension: the host hardware probe.
        "system": cached.system,
        "recommended_profiles": cached.recommended_profiles,
    });
    (StatusCode::OK, Json(superset)).into_response()
}

/// Whether the engine can load models at runtime (hot-switch + upload).
fn ai_has_factory(ai: &AiEngine) -> bool {
    ai.can_load_models()
}

/// The `observability` capability object (SPEC §3.1/§3.3). Built outside
/// the big superset literal — one more nesting level there trips the
/// serde_json macro recursion limit.
fn observability_document() -> serde_json::Value {
    serde_json::json!({
        "metrics": true,
        "logs": true,
        "requests": true,
        // Conversation model call chains (SPEC §3.3) and per-model
        // Prometheus families (appendix A #39) — additive keys, absence
        // means false for older frontends.
        "traces": true,
        "model_metrics": true,
    })
}

/// The `build` provenance object (mibee-rec#1): the running binary's
/// version, git sha and dirty flag, embedded by this crate's `build.rs`.
/// Lets a deployment be verified against an expected commit without file
/// mtime archaeology; `sha: "unknown"` honestly marks a git-less build.
fn build_document() -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "sha": env!("MITBEE_BUILD_SHA"),
        "dirty": env!("MITBEE_BUILD_DIRTY") == "1",
    })
}

/// The `tools` capability object (SPEC §3.5) — same nesting-trick as
/// [`observability_document`].
fn tools_document(enabled: bool, count: usize) -> serde_json::Value {
    serde_json::json!({ "enabled": enabled, "count": count })
}

/// The `away` capability object (SPEC §3.6) — same nesting-trick as
/// [`observability_document`].
fn away_document(available: bool, voice: bool) -> serde_json::Value {
    serde_json::json!({ "available": available, "voice": voice })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory pool for the ConversationLog fixture — capabilities only
    /// reads its enabled flag, so migrations never run here.
    async fn create_test_pool() -> sqlx::SqlitePool {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        pool
    }

    #[test]
    fn capabilities_response_is_serialisable() {
        let system = probe();
        let resp = CapabilitiesResponse {
            system,
            recommended_profiles: Vec::new(),
        };
        let json = serde_json::to_value(&resp).expect("must serialise");
        assert!(json.get("system").is_some());
    }

    /// Build provenance (mibee-rec#1): the wire shape is version + a
    /// short git hash (or the honest `"unknown"`) + a boolean dirty flag.
    #[test]
    fn build_provenance_is_advertised() {
        let doc = super::build_document();
        assert_eq!(doc["version"], env!("CARGO_PKG_VERSION"));
        let sha = doc["sha"].as_str().expect("sha is a string");
        assert!(
            sha == "unknown" || (sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit())),
            "unexpected build sha: {sha}"
        );
        assert!(doc["dirty"].is_boolean());
    }

    /// The capability superset is hand-built JSON — a check that the
    /// watermark key (SPEC v1 §5.2) stays present and true.
    #[tokio::test]
    async fn capabilities_advertise_watermark() {
        let ai = Arc::new(AiEngine::from_parts(
            streaming::ai::AiConfig::default(),
            None,
        ));
        let audio_ai = Arc::new(streaming::audio_ai::AudioAiEngine::from_config(
            &streaming::audio_ai::AudioAiConfig::default(),
        ));
        let res = get_capabilities(
            Extension(CapHandles {
                ai,
                audio_ai,
                ocr: Arc::new(streaming::ocr::OcrEngine::from_config(
                    &streaming::ocr::OcrConfig::default(),
                )),
                voice: Arc::new(streaming::voice::VoiceEngine::from_config(
                    &streaming::voice::VoiceConfig::default(),
                )),
                chat: Arc::new(streaming::llm::ChatEngine::from_config(
                    &streaming::llm::LlmConfig::default(),
                )),
                llm_tier: Arc::new(LlmTier("manual".into())),
                resource: Arc::new(streaming::feature_gate::ResourceProfile::unrestricted()),
                decision: Arc::new(streaming::decision::DecisionEngine::from_config(
                    &streaming::decision::DecisionConfig::default(),
                )),
                meeting: Arc::new(streaming::meeting::MeetingEngine::from_config(
                    &streaming::meeting::MeetingConfig::default(),
                    &streaming::voice::VoiceConfig::default(),
                )),
                vlm: Arc::new(streaming::vlm::VlmEngine::from_config(
                    &streaming::vlm::VlmConfig::default(),
                )),
                face: Arc::new(streaming::face::FaceEngine::from_config(
                    &streaming::face::FaceConfig::default(),
                )),
                stream_manager: Arc::new(crate::stream_manager::StreamManager::new()),
                convlog: Arc::new(crate::conversations::ConversationLog::new(
                    create_test_pool().await,
                    Arc::new(crate::routes::events::new_event_bus()),
                    true,
                )),
                agent: crate::server::AppRouterState::agent_for_tests().0,
                agent_config: crate::server::AppRouterState::agent_for_tests().1,
                away: crate::server::AppRouterState::away_for_tests(),
            }),
            Extension(security::middleware::AuthenticatedUser("admin".to_string())),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["watermark"], serde_json::json!(true));
        // Conversation records (SPEC §3.4) advertise when enabled.
        assert_eq!(json["conversations"], serde_json::json!(true));
        assert!(
            json["events"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("conversation"))
        );
        // Tool/skill framework (SPEC §3.5): the fixture registry carries
        // the built-ins, so the capability object + agent_step event ride
        // along.
        assert_eq!(json["tools"]["enabled"], serde_json::json!(true));
        assert!(json["tools"]["count"].as_u64().unwrap_or(0) >= 2);
        assert!(
            json["events"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("agent_step"))
        );
        // Away mode (SPEC §3.6): fixture AI is inactive → not armable,
        // voice legs off, no away SSE events advertised.
        assert_eq!(json["away"]["available"], serde_json::json!(false));
        assert_eq!(json["away"]["voice"], serde_json::json!(false));
        assert!(
            !json["events"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("away_event"))
        );
        // No active streams → substream capability false (SPEC appendix
        // A #20: it follows an active substream pipeline, not the config).
        assert_eq!(json["substream"], serde_json::json!(false));
        // Neither audio engine active (default engines: no models in
        // tests, fail-open) → no hearing-records capability.
        assert_eq!(json["audio_records"], serde_json::json!(false));
        // Boot-time admission snapshot rides along (#40) — even the
        // unrestricted fixture carries the object with its mode. The
        // named-tier trio (named-tier addendum) is additive so old
        // clients parsing resource.mode keep working.
        assert_eq!(json["resource"]["mode"], serde_json::json!("all"));
        assert_eq!(json["resource"]["tier"], serde_json::json!("full"));
        assert_eq!(
            json["resource"]["preset_source"],
            serde_json::json!("config")
        );
        assert!(json["resource"]["profile"].is_object());
        assert!(
            json["resource"]["features"].is_array() || json["resource"]["decisions"].is_array()
        );
    }

    /// Regression: `capabilities.face` was a hardcoded `true` while `[face]`
    /// defaults to off and both models are deployment assets. A stock install
    /// therefore rendered the "Enrolled faces" card and clicking enroll
    /// returned 501. The flag must follow the engine's real state.
    #[tokio::test]
    async fn face_capability_follows_the_engine_not_a_constant() {
        let res = capabilities_json(Arc::new(streaming::face::FaceEngine::from_config(
            &streaming::face::FaceConfig::default(),
        )))
        .await;
        assert_eq!(
            res["face"],
            serde_json::json!(false),
            "an unconfigured face engine must not advertise the capability"
        );
    }

    /// The gating itself: with no voice/decision models loaded, the
    /// voice-bridge events must be absent (advertising them unconditionally
    /// would be its own lie — the reverse of the bug this fixed).
    #[tokio::test]
    async fn voice_events_stay_gated_off_when_the_engines_are_inactive() {
        let res = capabilities_json(Arc::new(streaming::face::FaceEngine::from_config(
            &streaming::face::FaceConfig::default(),
        )))
        .await;
        let events = res["events"].as_array().expect("events array");
        for gated in ["chat_reply", "voice_decision", "voice_transcript"] {
            assert!(
                !events.contains(&serde_json::json!(gated)),
                "`{gated}` must not be advertised while its engine is inactive"
            );
        }
    }

    /// The capabilities handler sits exactly on axum's 16-extractor ceiling —
    /// which is why `face` could not simply be appended as a 17th. Bundling
    /// them into `CapHandles` must not change what is advertised.
    #[tokio::test]
    async fn every_gated_capability_is_reachable_through_cap_handles() {
        let res = capabilities_json(Arc::new(streaming::face::FaceEngine::from_config(
            &streaming::face::FaceConfig::default(),
        )))
        .await;
        for key in [
            "ai",
            "audio_ai",
            "ocr",
            "voice",
            "chat",
            "vlm",
            "decision",
            "meeting",
            "face",
            "conversations",
            "away",
            "resource",
            "substream",
            "tools",
        ] {
            assert!(
                res.get(key).is_some(),
                "capability `{key}` disappeared in the CapHandles refactor"
            );
        }
    }

    /// Every SSE wire name the server can emit must be advertised here.
    ///
    /// `static/js/sse.js` registers listeners *only* for names present in
    /// `capabilities.events`, so an event that is emitted but not advertised is
    /// dead on arrival — the payload arrives on the wire and nothing handles it.
    /// That is not hypothetical: `chat_reply` and `voice_decision` were both
    /// emitted by the voice bridge and absent from this list, which also made the
    /// conversations-off fallback toast in `chat.js` unreachable.
    ///
    /// This test scans the single function that serialises `CameraEvent` into SSE
    /// (`routes/events::event_to_sse`) for its wire names and requires each one to
    /// appear here. It fails loudly the next time someone adds an event arm and
    /// forgets the advertisement — which is exactly how the original bug shipped.
    #[test]
    fn every_emitted_sse_event_is_advertised_in_capabilities() {
        let events_src = include_str!("events.rs");
        // Only the production half of this file. The needles below are built from
        // fragments so that this test's own source cannot match them — a scraper
        // that finds its own literal passes vacuously, which is exactly what a
        // first attempt at this test did.
        let caps_src = include_str!("capabilities.rs")
            .split_once("#[cfg(test)]")
            .map_or(include_str!("capabilities.rs"), |(prod, _)| prod);
        let push_needle: String = ["events.push(", "\""].concat();
        let seed_needle: String = ["let mut events = vec", "!", "["].concat();

        // The wire names the SSE layer actually produces.
        let mut emitted: Vec<&str> = Vec::new();
        let mut rest = events_src;
        while let Some(idx) = rest.find(".event(\"") {
            rest = &rest[idx + ".event(\"".len()..];
            let end = rest.find('"').expect("unterminated event name");
            emitted.push(&rest[..end]);
            rest = &rest[end..];
        }
        emitted.sort_unstable();
        emitted.dedup();
        assert!(
            emitted.len() >= 12,
            "expected to scrape the full SSE surface, only found {emitted:?} — the \
         scraper drifted from the real shape and this test is now vacuous"
        );

        // Scrape the advertisement side from the *event-building* statements only.
        // A naive `caps_src.contains("\"chat_reply\"")` is vacuous: the name also
        // occurs in the explanatory comments above, so deleting the real
        // `events.push("chat_reply")` would leave this test green.
        //
        // Two forms build the list: the seed `vec![...]` holding the always-on
        // camera events, and `events.push("...")` for the gated ones.
        let mut advertised: Vec<String> = Vec::new();
        let mut rest = caps_src;
        while let Some(idx) = rest.find(&push_needle) {
            rest = &rest[idx + push_needle.len()..];
            let end = rest.find('"').expect("unterminated event name");
            advertised.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        if let Some(idx) = caps_src.find(&seed_needle) {
            let after = &caps_src[idx + seed_needle.len()..];
            let list_end = after.find(']').expect("unterminated seed event list");
            for element in after[..list_end].split(',') {
                let name = element.trim();
                let name = name
                    .strip_prefix('"')
                    .and_then(|p| p.strip_suffix('"'))
                    .unwrap_or_default();
                if !name.is_empty() {
                    advertised.push(name.to_string());
                }
            }
        }
        // Some events are advertised by capability *object* rather than a literal
        // push (their gate is the object, and the name is implied).
        let implied = ["conversation", "agent_step", "away_event"];
        let missing: Vec<&str> = emitted
            .iter()
            .copied()
            .filter(|n| !advertised.iter().any(|a| a == n) && !implied.contains(n))
            .collect();
        assert!(
            !advertised.is_empty(),
            "the advertisement scraper found nothing — it has drifted from the \
         real `events.push(...)` shape and this test is now vacuous"
        );
        assert!(
            missing.is_empty(),
            "these SSE events are emitted by event_to_sse but absent from \
         capabilities.events, so no client can subscribe to them: {missing:?}"
        );
    }

    /// Build the capability response against a chosen face engine.
    async fn capabilities_json(face: Arc<streaming::face::FaceEngine>) -> serde_json::Value {
        let ai = Arc::new(AiEngine::from_parts(
            streaming::ai::AiConfig::default(),
            None,
        ));
        let audio_ai = Arc::new(streaming::audio_ai::AudioAiEngine::from_config(
            &streaming::audio_ai::AudioAiConfig::default(),
        ));
        let res = get_capabilities(
            Extension(CapHandles {
                ai,
                audio_ai,
                ocr: Arc::new(streaming::ocr::OcrEngine::from_config(
                    &streaming::ocr::OcrConfig::default(),
                )),
                voice: Arc::new(streaming::voice::VoiceEngine::from_config(
                    &streaming::voice::VoiceConfig::default(),
                )),
                chat: Arc::new(streaming::llm::ChatEngine::from_config(
                    &streaming::llm::LlmConfig::default(),
                )),
                llm_tier: Arc::new(LlmTier("manual".into())),
                resource: Arc::new(streaming::feature_gate::ResourceProfile::unrestricted()),
                decision: Arc::new(streaming::decision::DecisionEngine::from_config(
                    &streaming::decision::DecisionConfig::default(),
                )),
                meeting: Arc::new(streaming::meeting::MeetingEngine::from_config(
                    &streaming::meeting::MeetingConfig::default(),
                    &streaming::voice::VoiceConfig::default(),
                )),
                vlm: Arc::new(streaming::vlm::VlmEngine::from_config(
                    &streaming::vlm::VlmConfig::default(),
                )),
                face,
                stream_manager: Arc::new(crate::stream_manager::StreamManager::new()),
                convlog: Arc::new(crate::conversations::ConversationLog::new(
                    create_test_pool().await,
                    Arc::new(crate::routes::events::new_event_bus()),
                    true,
                )),
                agent: crate::server::AppRouterState::agent_for_tests().0,
                agent_config: crate::server::AppRouterState::agent_for_tests().1,
                away: crate::server::AppRouterState::away_for_tests(),
            }),
            Extension(security::middleware::AuthenticatedUser("admin".to_string())),
        )
        .await
        .into_response();
        let body = axum::body::to_bytes(res.into_body(), 256 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}

/// SPEC appendix A #24: the hearing-records surface (`GET/DELETE
/// /api/audio/records`) exists when either audio engine is active — the
/// device can hear through sound events or the voice loop alike.
fn audio_records_capable(audio_ai_active: bool, voice_active: bool) -> bool {
    audio_ai_active || voice_active
}

/// Voiceprint features need BOTH the wake-word loop and the speaker
/// embedding model (a missing CAM++ file keeps the capability off while
/// the rest of voice keeps working).
fn voice_speakers_capable(voice_active: bool, speaker_model_loaded: bool) -> bool {
    voice_active && speaker_model_loaded
}

#[cfg(test)]
mod voice_speakers_tests {
    use super::voice_speakers_capable;

    #[test]
    fn truth_table() {
        assert!(!voice_speakers_capable(false, false));
        assert!(!voice_speakers_capable(true, false), "model file required");
        assert!(!voice_speakers_capable(false, true), "voice loop required");
        assert!(voice_speakers_capable(true, true));
    }
}

#[cfg(test)]
mod audio_records_tests {
    use super::audio_records_capable;

    #[test]
    fn audio_records_capability_follows_any_active_audio_engine() {
        assert!(!audio_records_capable(false, false));
        assert!(audio_records_capable(true, false));
        assert!(audio_records_capable(false, true));
        assert!(audio_records_capable(true, true));
    }
}
