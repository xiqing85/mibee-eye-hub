use clap::Parser;

use protocols::rtsp_server::{RtspServer, RtspServerConfig};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use web::protocol_runtime::ProtocolRuntime;

#[derive(Parser, Debug)]
#[command(
    name = "mibee-eye",
    about = "MiBee Eye — Professional laptop surveillance agent"
)]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Path to SQLite database
    #[arg(short = 'd', long, default_value = "mibee_eye.db")]
    db_path: PathBuf,

    /// Reset password for a user (prompts for credentials, does not start the server)
    #[arg(long)]
    reset_password: bool,

    /// Run the audio-event pipeline on a WAV file and print the JSON
    /// result (deterministic deployment verification; no server started).
    #[arg(long)]
    selftest_audio: Option<PathBuf>,

    /// Run OCR on an image file and print the JSON result.
    #[arg(long)]
    selftest_ocr: Option<PathBuf>,

    /// Run the wake-word + transcription pipeline on a WAV file and print
    /// the JSON result (deterministic deployment verification).
    #[arg(long)]
    selftest_voice: Option<PathBuf>,

    /// Run one greedy LLM completion and print the JSON result.
    #[arg(long)]
    selftest_llm: Option<String>,

    /// Synthesize one utterance via the TTS subprocess and report the WAV.
    #[arg(long)]
    selftest_tts: Option<String>,
    /// Describe one JPEG via the VLM (`--selftest-vlm <path>`).
    #[arg(long)]
    selftest_vlm: Option<String>,
    /// Diarize a multi-speaker WAV (`--selftest-meeting <wav>`; `voice`
    /// builds only — segmentation only, no ASR).
    #[arg(long)]
    selftest_meeting: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Process-level setup (OpenMP cap + rustls provider) lives in the
    // runtime tree now (design-capability-split M1 slice 1).
    mibee_eye::runtime::bootstrap::init_process_env();

    let args = Args::parse();

    // Handle --reset-password before starting the server
    if args.reset_password {
        return reset_password_cli(&args).await;
    }
    if let Some(path) = &args.selftest_audio {
        return selftest_cli("audio", || {
            streaming::audio_ai::selftest_wav(&path.to_string_lossy())
        });
    }
    if let Some(path) = &args.selftest_ocr {
        return selftest_cli("ocr", || {
            streaming::ocr::selftest_image(&path.to_string_lossy())
        });
    }
    if let Some(path) = &args.selftest_voice {
        return selftest_cli("voice", || {
            streaming::voice::selftest_voice(&path.to_string_lossy())
        });
    }
    if let Some(prompt) = &args.selftest_llm {
        return selftest_cli("llm", || streaming::llm::selftest_llm(prompt));
    }
    if let Some(path) = &args.selftest_vlm {
        return selftest_cli("vlm", || streaming::vlm::selftest_vlm(path));
    }
    if let Some(path) = &args.selftest_meeting {
        return selftest_cli("meeting", || {
            streaming::meeting::selftest_meeting(&path.to_string_lossy())
        });
    }
    if let Some(text) = &args.selftest_tts {
        return selftest_cli("tts", || streaming::tts::selftest_tts(text));
    }

    let mut config = mibee_eye::config::AppConfig::load(&args.config)?;
    config.validate()?;

    // Resolve advertised host: use configured value or auto-detect LAN IP
    let advertised_host = match &config.web.advertised_host {
        Some(host) if !host.is_empty() => host.clone(),
        _ => get_first_non_loopback_ipv4().unwrap_or_else(|| "127.0.0.1".to_string()),
    };
    tracing::info!(advertised_host = %advertised_host, "resolved advertised host");

    // Initialise rate limit config from security settings
    security::rate_limit::init_rate_limit_config(
        config.security.rate_limit_max,
        config.security.rate_limit_window_secs,
    );

    // Initialise tracing (subscriber, optional OTLP export + Loki log shipping)
    let loki_endpoint = config
        .observability
        .logs
        .as_ref()
        .map(|l| l.endpoint.clone());
    let loki_labels = config
        .observability
        .logs
        .as_ref()
        .map(|l| l.labels.clone())
        .unwrap_or_default();
    observability::init_tracing(
        &config.observability.log_level,
        false,
        if config.observability.otel_endpoint.is_empty() {
            None
        } else {
            Some(config.observability.otel_endpoint.clone())
        },
        loki_endpoint,
        loki_labels,
    )?;

    // Build provenance (mibee-rec#1): one authoritative startup line mapping
    // this binary to its source commit — no more archaeology on file mtimes.
    // Must live after init_tracing or it is silently dropped (known gotcha).
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        sha = env!("MITBEE_BUILD_SHA"),
        dirty = env!("MITBEE_BUILD_DIRTY") == "1",
        "mibee-eye starting"
    );

    // Feature-level resource admission (SPEC appendix A #40), before any
    // engine is built. The LLM tier model resolves first (#30-E) so the
    // gate charges the file that will actually load.
    let mem = streaming::tools::memory_mib().unwrap_or((0, 0));
    // Named boot tier (#40 named-tier addendum): "auto" (default) derives
    // from the hardware profile; a pinned name selects the preset
    // directly. Unknown pins warn and fall back to hardware derivation —
    // a typo must not fail the boot.
    let hardware = streaming::feature_gate::HardwareProfile::detect();
    let (tier, tier_source) = match streaming::feature_gate::Tier::from_name(&config.resources.tier)
    {
        Some(t) => (t, streaming::feature_gate::TierSource::Config),
        None if config.resources.tier.is_empty() || config.resources.tier == "auto" => (
            streaming::feature_gate::resolve_tier(&hardware, mem.0),
            streaming::feature_gate::TierSource::Hardware,
        ),
        None => {
            tracing::warn!(
                tier = %config.resources.tier,
                "resources.tier: unknown preset name (want minimal|basic|standard|full|auto); \
                 falling back to hardware auto tier"
            );
            (
                streaming::feature_gate::resolve_tier(&hardware, mem.0),
                streaming::feature_gate::TierSource::Hardware,
            )
        }
    };
    let (llm_model, llm_tier) = streaming::tools::resolve_llm_tier(
        &config.llm.model_path,
        &config.llm.model_path_mid,
        &config.llm.model_path_lite,
        config.resources.auto_tier,
        mem.1,
        hardware.physical_cores,
        &config.resources.tier,
    );
    let tier_ctx = streaming::feature_gate::TierContext {
        tier,
        source: tier_source,
        profile: hardware,
    };
    if llm_model != config.llm.model_path {
        config.llm.model_path = llm_model;
    }
    let feature_inputs = {
        use streaming::feature_gate::FeatureInput;
        let tts_paths = [
            &config.tts.model,
            &config.tts.yue_model,
            &config.tts.en_model,
        ]
        .into_iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<String>>();
        let mut voice_paths = vec![
            config.voice.kws_encoder.clone(),
            config.voice.kws_decoder.clone(),
            config.voice.kws_joiner.clone(),
            config.voice.paraformer_model.clone(),
        ];
        if config.voice.speaker_verify {
            voice_paths.push(config.voice.speaker_embedding_model.clone());
        }
        vec![
            FeatureInput {
                name: "ai",
                enabled: config.ai.enabled,
                model_paths: vec![config.ai.model_path.clone()],
                overhead_mib: 40,
                depends_on: None,
            },
            FeatureInput {
                name: "audio_ai",
                enabled: config.audio_ai.enabled,
                model_paths: vec![
                    config.audio_ai.model_path.clone(),
                    config.audio_ai.vad_model_path.clone(),
                ],
                overhead_mib: 40,
                depends_on: None,
            },
            FeatureInput {
                name: "voice",
                enabled: config.voice.enabled,
                model_paths: voice_paths,
                overhead_mib: 60,
                depends_on: None,
            },
            FeatureInput {
                name: "llm",
                enabled: config.llm.enabled,
                model_paths: vec![config.llm.model_path.clone()],
                overhead_mib: 80,
                depends_on: None,
            },
            FeatureInput {
                name: "tts",
                enabled: config.tts.enabled,
                model_paths: tts_paths,
                overhead_mib: 40,
                depends_on: None,
            },
            FeatureInput {
                name: "decision",
                enabled: config.decision.enabled,
                model_paths: vec![
                    config.decision.model_path.clone(),
                    config.decision.tokenizer_path.clone(),
                ],
                overhead_mib: 40,
                depends_on: Some("voice"),
            },
            FeatureInput {
                name: "face",
                enabled: config.face.enabled,
                model_paths: vec![
                    config.face.detect_model.clone(),
                    config.face.recog_model.clone(),
                ],
                overhead_mib: 40,
                depends_on: None,
            },
            FeatureInput {
                name: "ocr",
                enabled: config.ocr.enabled,
                model_paths: vec![config.ocr.det_path.clone(), config.ocr.rec_path.clone()],
                overhead_mib: 40,
                depends_on: None,
            },
            FeatureInput {
                name: "meeting",
                enabled: config.meeting.enabled,
                model_paths: vec![
                    config.meeting.segmentation_model.clone(),
                    config.meeting.punctuation_model.clone(),
                ],
                overhead_mib: 30,
                depends_on: Some("voice"),
            },
            FeatureInput {
                name: "vlm",
                enabled: config.vlm.enabled,
                model_paths: vec![
                    config.vlm.model_path.clone(),
                    config.vlm.mmproj_path.clone(),
                ],
                overhead_mib: 80,
                depends_on: None,
            },
        ]
    };
    let resource_profile = Arc::new(streaming::feature_gate::plan(
        &feature_inputs,
        &config.resources.feature_gate,
        &tier_ctx,
        mem.0,
        mem.1,
        config.resources.reserve_mib,
    ));
    // Apply the plan: a non-admitted enabled feature is turned off in the
    // config copy every engine reads — the engines' own fail-open logs
    // then explain it exactly like a config-disabled feature. `disables()`
    // covers both shed classes (off_budget, off_tier) so the loop also
    // disables tier-excluded features; off_config/dependency/unknown
    // reasons are informational here (config stays as the user set it).
    let mut accounted_mib: u64 = 0;
    for d in &resource_profile.features {
        if d.admitted {
            accounted_mib = accounted_mib.saturating_add(d.cost_mib);
            continue;
        }
        if !streaming::feature_gate::disables(d.reason) {
            continue;
        }
        // `accounted_mib` is what earlier, higher-priority features already
        // consumed. Without it this line reads as a contradiction — the
        // observed pair was `cost_mib: 1784, budget_mib: 6388`, which looks
        // like it should have fit, and an operator debugging a missing VLM
        // would reasonably conclude the gate is broken. The admission test is
        // `accounted + cost <= budget`, so that is the arithmetic to show.
        match d.reason {
            streaming::feature_gate::REASON_OFF_BUDGET => tracing::warn!(
                feature = d.name,
                cost_mib = d.cost_mib,
                accounted_mib,
                budget_mib = resource_profile.budget_mib,
                "resource gate: feature shed by boot memory budget \
                 (earlier features already account for {accounted_mib} MiB)"
            ),
            streaming::feature_gate::REASON_OFF_TIER => tracing::warn!(
                feature = d.name,
                tier = %resource_profile.tier,
                tier_source = %resource_profile.preset_source,
                "resource gate: feature outside the boot tier preset \
                 (excluded features cost nothing against the budget)"
            ),
            _ => {}
        }
        match d.name {
            "ai" => config.ai.enabled = false,
            "audio_ai" => config.audio_ai.enabled = false,
            "voice" => config.voice.enabled = false,
            "llm" => config.llm.enabled = false,
            "tts" => config.tts.enabled = false,
            "decision" => config.decision.enabled = false,
            "face" => config.face.enabled = false,
            "ocr" => config.ocr.enabled = false,
            "meeting" => config.meeting.enabled = false,
            "vlm" => config.vlm.enabled = false,
            _ => {}
        }
    }
    tracing::info!(
        mode = %resource_profile.mode,
        tier = %resource_profile.tier,
        tier_source = %resource_profile.preset_source,
        hardware_cores = resource_profile.profile.physical_cores,
        hardware_simd = resource_profile.profile.simd.name(),
        hardware_battery = resource_profile.profile.battery,
        available_mib = resource_profile.available_mib,
        total_mib = resource_profile.total_mib,
        budget_mib = resource_profile.budget_mib,
        llm_tier,
        admitted = resource_profile
            .features
            .iter()
            .filter(|d| d.admitted)
            .count(),
        shed = resource_profile
            .features
            .iter()
            .filter(|d| !d.admitted && d.reason != streaming::feature_gate::REASON_OFF_CONFIG)
            .count(),
        "resource gate: boot feature admission resolved"
    );

    // Initialise database - create SqlitePool for web CRUD and Connection for security calls
    let db_path_str = args.db_path.to_string_lossy().to_string();
    let pool = web::db::init_pool(&db_path_str).await?;
    let auth_db_conn = web::db::init_auth_db(&db_path_str)?;
    let auth_db = Arc::new(tokio::sync::Mutex::new(auth_db_conn));
    // Scene overlay (#31): web-persisted scene.* rows take precedence
    // over TOML at boot, before any engine reads the values.
    if let Ok(rows) = web::db::list_settings(&pool).await {
        let n = rows.iter().filter(|(k, _)| k.starts_with("scene.")).count();
        if n > 0 {
            tracing::info!(
                rows = n,
                "applying scene config overlay from settings store"
            );
        }
        mibee_eye::config::overlay_scene_from_rows(&mut config, &rows);
        let n_models = rows.iter().filter(|(k, _)| k.starts_with("model.")).count();
        if n_models > 0 {
            tracing::info!(
                rows = n_models,
                "applying model selection overlay from settings store"
            );
        }
        mibee_eye::config::overlay_models_from_rows(&mut config, &rows);
    }
    // Custom wake word (#32): regenerate the keywords override next to
    // the DB and point the KWS at it. Default word keeps the shipped
    // model file (zero behavior change for existing deployments).
    if config.voice.wake_word != streaming::voice::DEFAULT_WAKE_WORD {
        match streaming::voice::wake_word_to_keyword_line(&config.voice.wake_word) {
            Ok(line) => {
                let kw_path = args.db_path.with_file_name("kws-keywords.txt");
                match std::fs::write(&kw_path, format!("{line}\n")) {
                    Ok(()) => {
                        tracing::info!(
                            wake_word = %config.voice.wake_word,
                            path = %kw_path.display(),
                            "voice: custom wake word active"
                        );
                        config.voice.keywords_file = kw_path.to_string_lossy().into_owned();
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "failed to write wake-word override; keeping default keywords"
                    ),
                }
            }
            Err(msg) => tracing::warn!(%msg, "invalid persisted wake word; keeping default"),
        }
    }
    // One shared tools handle (#31): the voice bridge, the web routes and
    // the scene config API all see hot updates.
    let shared_tools: web::server::SharedTools =
        Arc::new(std::sync::RwLock::new(config.tools.clone()));
    // Seed protocol_configs table from config.toml on first run (when empty).
    // Subsequent runs use the persisted values; users' Web UI edits survive restart.
    if web::db::protocol_configs_is_empty(&pool).await? {
        tracing::info!("seeding protocol_configs from config.toml (first run)");
        web::db::set_protocol_config(&pool, "onvif", &serde_json::to_value(&config.onvif)?).await?;
        web::db::set_protocol_config(&pool, "gb28181", &serde_json::to_value(&config.gb28181)?)
            .await?;
        web::db::set_protocol_config(
            &pool,
            "rtmp_push",
            &serde_json::to_value(&config.rtmp_push)?,
        )
        .await?;
        web::db::set_protocol_config(
            &pool,
            "recording",
            &serde_json::to_value(&config.recording)?,
        )
        .await?;
        web::db::set_protocol_config(
            &pool,
            "watermark",
            &serde_json::to_value(&config.watermark)?,
        )
        .await?;
        web::db::set_protocol_config(&pool, "webrtc", &serde_json::to_value(&config.webrtc)?)
            .await?;
    } else {
        tracing::debug!("protocol_configs table already populated; keeping persisted values");
    }
    let discovered = web::db::auto_discover_cameras(&pool).await?;
    if discovered > 0 {
        tracing::info!(count = discovered, "auto-discovered cameras on startup");
    }

    // Collect protocol JoinHandles for graceful shutdown
    let mut protocol_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Start RTSP server in background task
    let rtsp_config = RtspServerConfig {
        port: config.rtsp.server_port,
        ..Default::default()
    };
    let rtsp_server = Arc::new(RtspServer::new(rtsp_config));
    let rtsp_server_clone = rtsp_server.clone();
    let rtsp_handle = tokio::spawn(async move {
        if let Err(e) = rtsp_server_clone.run().await {
            tracing::error!(error = %e, "RTSP server error");
        }
    });
    protocol_handles.push(rtsp_handle);
    tracing::info!(port = config.rtsp.server_port, "RTSP server started");

    // Protocol startup is now handled by ProtocolRuntime (below, after
    // StreamManager is created and streams are auto-started).
    // This allows hot-toggling ONVIF/GB28181/RTMP without server restart.

    // SSE event bus — created early so the AI engine bridge (below) and the
    // hot-plug monitor share one bus with the web server.
    let event_tx = Arc::new(web::routes::events::new_event_bus());

    // Conversation records (SPEC §3.4): the shared dialogue-turn sink for
    // the HTTP chat route, the voice bridge and the record list endpoint.
    let conversations_log = Arc::new(web::conversations::ConversationLog::new(
        pool.clone(),
        Arc::clone(&event_tx),
        config.conversations.enabled,
    ));

    // Agent tool/skill registry (SPEC §3.5): built-ins + MCP stdio plugin
    // servers. Built before the voice bridge captures it; the stream
    // manager attaches later (camera.snapshot). A warmup task connects
    // the MCP servers so the capabilities count settles.
    let agent_registry = Arc::new(web::agent::ToolRegistry::new(
        Arc::clone(&shared_tools),
        &config.agent,
    ));

    // Desktop integration (SPEC appendix A #42): tray icon + desktop
    // notifications — no-ops without a desktop session (fail-open).
    let desktop = mibee_eye::desktop::Desktop::new(&config.desktop);
    mibee_eye::desktop::spawn_tray(
        &config.desktop,
        &mibee_eye::desktop::web_ui_url(config.web.http_port, config.web.port),
    );

    // AI detection engine (fail-open: a missing model / ONNX Runtime library
    // leaves it inactive; the service runs on without AI).
    // The runtime model choice (SPEC §4.6 activate) persists as a db
    // setting; overlay it over the TOML boot default.
    if config.ai.enabled
        && let Ok(Some(v)) = web::db::get_setting(&pool, "ai.model").await
        && streaming::ai::registry::valid_model_id(&v)
    {
        config.ai.model = v;
    }
    let ai_engine = Arc::new(streaming::ai::AiEngine::from_config(&config.ai));
    if !ai_engine.is_active() {
        tracing::info!(reason = %ai_engine.inactive_reason(), "ai: detection disabled");
    }

    // Shared gate Arcs: one each spans the AI alarm bridge, the protocol
    // runtime (GB28181 handlers), and the StreamManager's recording outputs.
    let onvif_events_slot: Arc<
        std::sync::Mutex<Option<Arc<onvif_device_rs::events::EventsService>>>,
    > = Arc::new(std::sync::Mutex::new(None));
    let notifier_slot: Arc<std::sync::Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let alarm_notify_gate = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let recording_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let force_idr = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gb_flips = Arc::new(streaming::capture_source::Flips::default());

    // Alarm bridge config: cooldown from the gb28181 db subtree (SPEC
    // appendix A #16); TOML boot default as fallback.
    let gb_cfg_json = web::db::get_protocol_config(&pool, "gb28181")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| serde_json::to_value(&config.gb28181).unwrap_or_default());
    let alarm_cooldown_secs = gb_cfg_json
        .get("alarm_cooldown_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);
    if let Some(gate) = gb_cfg_json
        .get("alarm_notify_enabled")
        .and_then(|v| v.as_bool())
    {
        alarm_notify_gate.store(gate, std::sync::atomic::Ordering::SeqCst);
    }

    // VLM alarm-frame descriptions (SPEC appendix A #23). Created before
    // the alarm bridge so the hook below can clone it.
    let vlm_engine = Arc::new(streaming::vlm::VlmEngine::from_config(&config.vlm));
    // Live scene grounding (SPEC appendix A #29): fed by the AI loop and
    // the VLM alarm descriptions, read by the chat routes and the voice
    // auto-reply — one shared instance keeps every answer equally fresh.
    let grounding_state = Arc::new(web::grounding::GroundingState::new());
    let face_engine = Arc::new(streaming::face::FaceEngine::from_config(&config.face));
    if face_engine.is_active() {
        match web::db::load_face_templates(&pool).await {
            Ok(templates) => {
                let n = templates.len();
                face_engine.with_registry(|r| r.load(templates));
                tracing::info!(enrolled = n, "face: registry loaded");
            }
            Err(e) => tracing::warn!(error = %e, "face: registry load failed (starting empty)"),
        }
    }
    if !vlm_engine.is_active() {
        tracing::info!(reason = %vlm_engine.inactive_reason(), "vlm: descriptions disabled");
    }
    let decision_engine = Arc::new(streaming::decision::DecisionEngine::from_config(
        &config.decision,
    ));
    if !decision_engine.is_active() {
        tracing::info!(reason = %decision_engine.inactive_reason(), "decision: triage disabled");
    }

    // Bridge AI detection events into the SSE event bus (SPEC v1 §6), and
    // fire alarms on detection rising edges (SPEC §6 `alarm` + §9.5.2
    // Alarm NOTIFY when GB28181 is up and the AlarmReport gate allows).
    // (M1 slice 3: the block itself lives in runtime::ai_bridge now.)
    mibee_eye::runtime::ai_bridge::AiBridge {
        events: ai_engine.subscribe_events(),
        vlm: Arc::clone(&vlm_engine),
        grounding: Arc::clone(&grounding_state),
        face: Arc::clone(&face_engine),
        bridge_tx: event_tx.clone(),
        alarm_cooldown_secs,
        notifier_slot: Arc::clone(&notifier_slot),
        alarm_notify_gate: Arc::clone(&alarm_notify_gate),
        onvif_events_slot: Arc::clone(&onvif_events_slot),
        desktop: Arc::clone(&desktop),
    }
    .spawn();

    // Model manager (SPEC §4.9): the download engine + the boot-time
    // default selection per capability (engine paths matched against the
    // catalog; the detection capability defaults to the engine's active
    // registry model). Progress rides the SSE bus as `model_task` events.
    let mut model_defaults = mibee_eye::config::default_model_selection(&config);
    if ai_engine.is_active() {
        model_defaults.insert("ai".to_string(), ai_engine.active_model().to_string());
    }
    let model_manager = Arc::new(web::routes::models_api::ModelManager::new(
        std::path::PathBuf::from(&config.models.dir),
        model_defaults.clone(),
    ));
    {
        let mut task_rx = model_manager.downloads.subscribe();
        let event_tx_for_models = event_tx.clone();
        tokio::spawn(async move {
            while let Ok(snapshot) = task_rx.recv().await {
                let _ = event_tx_for_models
                    .send(web::routes::events::CameraEvent::ModelTask { task: snapshot });
            }
        });
    }
    // Online AI (SPEC §4.10): config lives in its own table; the live
    // handle hot-swaps on PUT /api/cloud and routes voice + HTTP chat.
    let cloud_ai = Arc::new(web::cloud::CloudAi::new(
        web::db::get_cloud_config(&pool).await.unwrap_or_default(),
    ));
    if cloud_ai.enabled() {
        tracing::info!(
            provider = %cloud_ai.config().provider,
            "cloud: online AI routing active"
        );
    }
    // Audio AI engine (sound events + voice presence). Fail-open: no
    // microphone, missing models, or a missing ONNX Runtime library leave
    // it inactive.
    let audio_ai_engine = Arc::new(streaming::audio_ai::AudioAiEngine::from_config(
        &config.audio_ai,
    ));
    if !audio_ai_engine.is_active() {
        tracing::info!(
            reason = %audio_ai_engine.inactive_reason(),
            "audio_ai: sound events disabled"
        );
    }
    let voice_engine = Arc::new(streaming::voice::VoiceEngine::from_config(&config.voice));
    // The LLM resource tier (#30-E) was resolved before the feature gate
    // (top of main) and written back into config.llm — ChatEngine reads
    // the already-tiered path; llm_tier is threaded to capabilities.
    let chat_engine = Arc::new(streaming::llm::ChatEngine::from_config(&config.llm));
    if !chat_engine.is_active() {
        tracing::info!(reason = %chat_engine.inactive_reason(), "llm: chat disabled");
    }
    let tts_engine = Arc::new(streaming::tts::TtsEngine::from_config(&config.tts));
    if !tts_engine.is_active() {
        tracing::info!(reason = %tts_engine.inactive_reason(), "tts: playback disabled");
    }

    // Away mode (SPEC v1 §3.6): armed watch over the AI detection
    // stream. Voice legs need TTS + the wake-word engine (listening).
    let away_engine = Arc::new(web::away::AwayEngine::new(
        config.away.clone(),
        Arc::clone(&ai_engine),
        tts_engine.is_active() && voice_engine.is_active(),
    ));
    if !away_engine.is_available() {
        tracing::info!(reason = %away_engine.unavailable_reason(), "away: not armable");
    }
    // Armed state survives restarts (settings bag) — a security mode
    // must not silently disarm on a service bounce.
    if let Ok(Some(v)) = web::db::get_setting(&pool, "away.active").await
        && v == "true"
        && away_engine.is_available()
    {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        away_engine.arm(now_ms);
        tracing::info!("away: armed state restored from persistence");
    }
    mibee_eye::away_monitor::spawn(mibee_eye::away_monitor::AwayMonitor {
        away: Arc::clone(&away_engine),
        ai: Arc::clone(&ai_engine),
        voice: Arc::clone(&voice_engine),
        tts: Arc::clone(&tts_engine),
        face: Arc::clone(&face_engine),
        vlm: Arc::clone(&vlm_engine),
        desktop: Arc::clone(&desktop),
        pool: pool.clone(),
        event_tx: Arc::clone(&event_tx),
    });
    // Schedule watcher (#44 amendment): scene.away.schedule_arm/disarm
    // boundary-minute edges; no-op while both times are empty.
    web::away::spawn_scheduler(
        Arc::clone(&away_engine),
        pool.clone(),
        Arc::clone(&event_tx),
        Some(Arc::clone(&chat_engine)),
    );
    if !voice_engine.is_active() {
        tracing::info!(reason = %voice_engine.inactive_reason(), "voice: interaction disabled");
    }
    // Persisted voiceprint profiles feed the verify gate + record tagging
    // (SPEC appendix A #25). Corrupt rows are skipped inside the loader.
    if voice_engine.speaker_capable() {
        match web::db::load_voice_speaker_embeddings(&pool).await {
            Ok(profiles) => {
                let loaded = voice_engine.load_speakers(&profiles);
                tracing::info!(
                    loaded,
                    total = profiles.len(),
                    "voice: speaker profiles registered"
                );
            }
            Err(e) => tracing::warn!(error = %e, "voice: speaker profile load failed"),
        }
    }
    // Meeting mode (SPEC appendix A #27): rides the voice models for ASR,
    // shares the 16 kHz monitor for recording, and votes cluster names
    // through the voiceprint registry above.
    let meeting_engine = Arc::new(streaming::meeting::MeetingEngine::from_config(
        &config.meeting,
        &config.voice,
    ));
    if !meeting_engine.is_active() {
        tracing::info!(reason = %meeting_engine.inactive_reason(), "meeting: mode disabled");
    } else {
        let voice_for_lookup = Arc::clone(&voice_engine);
        meeting_engine.set_speaker_lookup(std::sync::Arc::new(move |samples| {
            voice_for_lookup.resolve_speaker_blocking(samples)
        }));
    }
    let mut _audio_monitor = None;
    if audio_ai_engine.is_active() || voice_engine.is_active() || meeting_engine.is_active() {
        // Boot-time races (audio service still registering the source, a
        // restart racing the previous instance's device release) used to
        // kill sound for the whole run on a single lost open — retry with a
        // bound instead. The full error chain is logged: the top-level
        // context alone says "i16", the cause says why.
        let mut opened = None;
        for attempt in 1..=10 {
            match capture::audio_monitor::AudioMonitor::open(&config.audio_ai.device) {
                Ok(mut monitor) => match monitor.start() {
                    Ok((rx, _tx)) => {
                        opened = Some((monitor, rx));
                        break;
                    }
                    Err(e) => tracing::warn!(
                        attempt,
                        error = format!("{e:#}"),
                        "audio monitor: start failed"
                    ),
                },
                Err(e) => tracing::warn!(
                    attempt,
                    error = format!("{e:#}"),
                    "audio monitor: open failed"
                ),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        match opened {
            Some((monitor, rx)) => {
                // Real-time voice waveform (SPEC §6 audio_level): a level
                // tap next to the engine workers — smoothed RMS at ≤10 Hz.
                {
                    let mut level_rx = rx.resubscribe();
                    let events = event_tx.clone();
                    tokio::spawn(async move {
                        let mut meter = streaming::audio_level::LevelMeter::new();
                        loop {
                            let chunk = match level_rx.recv().await {
                                Ok(c) => c,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            };
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            if let Some(level) = meter.update(&chunk.samples, now) {
                                let _ = events.send(web::routes::events::CameraEvent::AudioLevel {
                                    level,
                                    timestamp_ms: now,
                                });
                            }
                        }
                    });
                }
                if audio_ai_engine.is_active() {
                    audio_ai_engine.spawn_worker(rx.resubscribe());
                }
                if voice_engine.is_active() {
                    voice_engine.spawn_worker(rx.resubscribe());
                }
                if meeting_engine.is_active() {
                    meeting_engine.spawn_worker(rx.resubscribe());
                    // Auto-stopped sessions (max duration) ride the same
                    // pipeline as manual stops: diarize → transcribe →
                    // persist → meeting_state SSE.
                    if let Some(mut finished) = meeting_engine.finished_receiver() {
                        let pool = pool.clone();
                        let events = event_tx.clone();
                        let meeting = Arc::clone(&meeting_engine);
                        tokio::spawn(async move {
                            while let Some(rec) = finished.recv().await {
                                tracing::info!(meeting = rec.id, "meeting: auto-stopped");
                                web::routes::meetings::process_finished(
                                    pool.clone(),
                                    Arc::clone(&meeting),
                                    events.clone(),
                                    rec,
                                )
                                .await;
                            }
                        });
                    }
                }
                _audio_monitor = Some(monitor);
            }
            None => tracing::warn!(
                "audio monitor: giving up after retries (sound events + wake word paused)"
            ),
        }
    }
    // Shared MP4-segment registry (#30-C media_ref): the FileOutputs
    // publish their current segment here; hearing bridges resolve the
    // covering file; the StreamManager below adopts the same Arc.
    let segment_registry: Arc<std::sync::Mutex<HashMap<String, streaming::output::SegmentSlot>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Sound events ride the same alarm pipeline as visual detections: SSE
    // `alarm` with source:"audio" (+ class/score) and the GB Alarm NOTIFY
    // with the same pinned 2022 standard triple; the description carries
    // the detected class. The microphone is device-level hardware, so the
    // event is attributed to camera "0".
    {
        let mut sound_events = audio_ai_engine.subscribe_events();
        let bridge_tx = event_tx.clone();
        let notifier_slot = Arc::clone(&notifier_slot);
        let alarm_notify_gate = Arc::clone(&alarm_notify_gate);
        let records_pool = pool.clone();
        let grounding_sound = Arc::clone(&grounding_state);
        let sound_pool_for_scene = pool.clone();
        // Away audio link (#44 amendment): sound alarms become away
        // activity records while armed.
        let away_for_sound = Arc::clone(&away_engine);
        let streams_for_sound = Arc::clone(&segment_registry);
        let desktop_sound_alarm = Arc::clone(&desktop);
        tokio::spawn(async move {
            loop {
                match sound_events.recv().await {
                    Ok(ev) => {
                        // Persistent hearing record (SPEC appendix A #24) —
                        // fail-open: a DB hiccup never touches the alarm
                        // pipeline. Scene = what the camera saw when the
                        // sound fired (#30-C).
                        let camera = web::db::list_cameras(&sound_pool_for_scene)
                            .await
                            .ok()
                            .and_then(|cams| cams.first().map(|c| c.id.clone()));
                        let scene = camera
                            .as_deref()
                            .and_then(|id| grounding_sound.scene_summary(id, ev.timestamp_ms))
                            .unwrap_or_default();
                        // Video dimension (#30-C): the MP4 segment covering
                        // this sound, when the camera is recording.
                        let media_ref = camera
                            .as_deref()
                            .and_then(|id| {
                                web::stream_manager::StreamManager::segment_covering(
                                    &streams_for_sound,
                                    id,
                                    ev.timestamp_ms,
                                )
                            })
                            .unwrap_or_default();
                        if let Err(e) = web::db::insert_hearing_record(
                            &records_pool,
                            "sound",
                            &ev.class,
                            Some(ev.score as f64),
                            "",
                            "",
                            &scene,
                            &media_ref,
                            ev.timestamp_ms as i64,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "hearing record: insert failed");
                        }
                        let _ = bridge_tx.send(web::routes::events::CameraEvent::Alarm {
                            camera_id: "0".to_string(),
                            targets: 1,
                            timestamp_ms: ev.timestamp_ms,
                            source: "audio".to_string(),
                            class: Some(ev.class.clone()),
                            score: Some(ev.score),
                        });
                        // Desktop notification (SPEC appendix A #42).
                        desktop_sound_alarm.notify_alarm("audio", 1, Some(&ev.class));
                        // Away audio link (#44): one activity record per
                        // cooldown while armed (observe_audio gates the
                        // audio_link setting + cooldown internally).
                        if away_for_sound.is_armed()
                            && let Some(labels) =
                                away_for_sound.observe_audio(&ev.class, ev.score, ev.timestamp_ms)
                        {
                            mibee_eye::away_monitor::record_audio_event(
                                &away_for_sound,
                                &sound_pool_for_scene,
                                &bridge_tx,
                                &labels,
                            )
                            .await;
                        }
                        if alarm_notify_gate.load(std::sync::atomic::Ordering::SeqCst) {
                            let notifier =
                                notifier_slot.lock().expect("gb notifier slot lock").clone();
                            if let Some(n) = notifier {
                                let description = format!(
                                    "audio event: {} ({:.2})",
                                    if ev.label_zh.is_empty() {
                                        &ev.class
                                    } else {
                                        &ev.label_zh
                                    },
                                    ev.score
                                );
                                let sent = n.send_alarm(
                                    "4",
                                    "5",
                                    &gb28181_rs::client::format_gb_time_ms(ev.timestamp_ms),
                                    "2",
                                    &description,
                                );
                                if !sent {
                                    tracing::debug!("alarm notify: no active subscription");
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "audio_ai: SSE bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // OCR engine (fail-open: missing models leave it inactive).
    let ocr_engine = Arc::new(streaming::ocr::OcrEngine::from_config(&config.ocr));
    if !ocr_engine.is_active() {
        tracing::info!(reason = %ocr_engine.inactive_reason(), "ocr: disabled");
    }

    // Shared zone map (user-drawn intrusion/tripwire zones, db-mirrored)
    // feeding the zone-event engine below.
    let shared_zones = web::zones::new_shared();
    web::zones::load_from_db(&pool, &shared_zones).await;

    // Zone-event engine: tracked detections × user zones → intrusion /
    // loiter / line-cross events → SSE `zone_event` + GB Alarm NOTIFY
    // (same pinned triple; description names the zone).
    {
        let mut track_events = ai_engine.subscribe_tracks();
        let bridge_tx = event_tx.clone();
        let zones_ref = shared_zones.clone();
        let grounding_zones = Arc::clone(&grounding_state);
        let notifier_slot = Arc::clone(&notifier_slot);
        let alarm_notify_gate = Arc::clone(&alarm_notify_gate);
        let desktop_zone_alarm = Arc::clone(&desktop);
        tokio::spawn(async move {
            let mut engines: HashMap<String, streaming::ai::zones::ZoneEngine> = HashMap::new();
            loop {
                match track_events.recv().await {
                    Ok(ev) => {
                        let zones = zones_ref.read().clone();
                        if zones.is_empty() {
                            continue;
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let engine = engines.entry(ev.camera_id.clone()).or_default();
                        for ze in engine.update(
                            &ev.camera_id,
                            zones.get(&ev.camera_id).map_or(&[], Vec::as_slice),
                            &ev.tracks,
                            now_ms,
                        ) {
                            let kind = match ze.event {
                                streaming::ai::zones::ZoneEventKind::Intrusion => "intrusion",
                                streaming::ai::zones::ZoneEventKind::Loiter => "loiter",
                                streaming::ai::zones::ZoneEventKind::LineCross {
                                    forward: true,
                                } => "line_cross_forward",
                                streaming::ai::zones::ZoneEventKind::LineCross {
                                    forward: false,
                                } => "line_cross_backward",
                            };
                            tracing::info!(
                                camera = %ze.camera_id,
                                zone = %ze.zone,
                                kind,
                                track = ze.track_id,
                                "zones: event"
                            );
                            grounding_zones.record_zone_event(
                                &ze.camera_id,
                                ze.timestamp_ms,
                                &ze.zone,
                            );
                            let _ = bridge_tx.send(web::routes::events::CameraEvent::ZoneEvent {
                                camera_id: ze.camera_id.clone(),
                                zone: ze.zone.clone(),
                                event: kind.to_string(),
                                track_id: ze.track_id,
                                label: ze.label.clone(),
                                timestamp_ms: ze.timestamp_ms,
                            });
                            // Desktop notification (SPEC appendix A #42).
                            desktop_zone_alarm.notify_alarm("zone", 1, Some(&ze.zone));
                            if alarm_notify_gate.load(std::sync::atomic::Ordering::SeqCst) {
                                let notifier =
                                    notifier_slot.lock().expect("gb notifier slot lock").clone();
                                if let Some(n) = notifier {
                                    let description = format!("zone event: {} ({kind})", ze.zone);
                                    let sent = n.send_alarm(
                                        "4",
                                        "5",
                                        &gb28181_rs::client::format_gb_time_ms(ze.timestamp_ms),
                                        "2",
                                        &description,
                                    );
                                    if !sent {
                                        tracing::debug!("alarm notify: no active subscription");
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "zones: bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Voice interactions → SSE `voice_transcript`; when the local LLM is
    // active, non-empty transcripts also get one auto-reply (SSE
    // `chat_reply`) — the TTS playback stage will consume these.
    // (M1 slice 2: the block itself lives in runtime::voice_bridge now.)
    mibee_eye::runtime::voice_bridge::VoiceBridge {
        events: voice_engine.subscribe_events(),
        bridge_tx: event_tx.clone(),
        chat: chat_engine.clone(),
        cloud: cloud_ai.clone(),
        decision: decision_engine.clone(),
        convlog: Arc::clone(&conversations_log),
        desktop: Arc::clone(&desktop),
        grounding: Arc::clone(&grounding_state),
        streams: Arc::clone(&segment_registry),
        tools: Arc::clone(&shared_tools),
        agent_registry: Arc::clone(&agent_registry),
        agent_config: config.agent.clone(),
        wake_word: config.voice.wake_word.clone(),
        voice_engine: Arc::clone(&voice_engine),
        away: Arc::clone(&away_engine),
        tts: Arc::clone(&tts_engine),
        pool: pool.clone(),
        interrupt_enabled: config.voice.interrupt_enable,
        interrupt_confidence: config.voice.interrupt_confidence,
    }
    .spawn();

    // Create StreamManager early so protocol handlers (ONVIF/GB28181) can
    // reference it for runtime output attachment.
    // Open a second DB connection for StreamManager (WAL mode supports
    // concurrent readers; this lets StreamManager read protocol configs at
    // stream-creation time without contending with the web server's lock).
    let streamer_db = pool.clone();
    let stream_manager = Arc::new(
        web::stream_manager::StreamManager::with_host_and_db(advertised_host.clone(), streamer_db)
            .with_recording_pause_flag(Arc::clone(&recording_paused))
            .with_force_idr_flag(Arc::clone(&force_idr))
            .with_gb_flips(Arc::clone(&gb_flips))
            .with_ai(ai_engine.clone())
            .with_segment_slots(Arc::clone(&segment_registry)),
    );
    // Agent tool/skill registry (SPEC §3.5): built-ins + MCP stdio plugin
    // servers. A warmup task connects the servers so the capabilities
    // count settles; unreachable servers simply contribute no tools.
    agent_registry.attach_streams(stream_manager.clone());
    {
        let warmup = Arc::clone(&agent_registry);
        tokio::spawn(async move { warmup.warmup().await });
    }

    // Auto-start streams for cameras with DB status "running" (resume across restart).
    // This ensures the in-memory StreamManager state matches the persistent DB state.
    {
        let running_cameras = {
            let all = web::db::list_cameras(&pool).await?;
            all.into_iter()
                .filter(|c| c.status == "running")
                .collect::<Vec<_>>()
        };
        let count = running_cameras.len();
        if count > 0 {
            tracing::info!(
                count = count,
                "found cameras with status 'running', auto-starting"
            );
        }
        for camera in running_cameras {
            tracing::info!(camera_id = %camera.id, name = %camera.name, "auto-starting stream");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "0".to_string());
            match stream_manager
                .create_stream(
                    camera.id.clone(),
                    &camera.camera_type,
                    &camera.config,
                    Some(&rtsp_server),
                )
                .await
            {
                Ok(info) => {
                    tracing::info!(camera_id = %camera.id, rtsp_url = ?info.rtsp_url, "stream auto-started");
                }
                Err(e) => {
                    tracing::warn!(
                        camera_id = %camera.id,
                        error = %e,
                        "failed to auto-start stream; marking as stopped"
                    );
                    // Update DB status to "stopped" since stream cannot start.
                    let _ = web::db::update_camera(
                        &pool,
                        &web::db::CameraRow {
                            status: "stopped".to_string(),
                            updated_at: now,
                            ..camera
                        },
                    )
                    .await;
                }
            }
        }
        if count > 0 {
            tracing::info!(count = count, "auto-start complete");
        }
    }

    // ── ProtocolRuntime: hot-toggle ONVIF / GB28181 / RTMP ───────────────
    //
    // Check DB for enabled protocols and start them. This replaces the old
    // inline ONVIF/GB28181 startup code and enables hot-toggling via Web UI.
    let mut protocol_runtime = ProtocolRuntime::new_with_shares(
        Arc::clone(&notifier_slot),
        Arc::clone(&alarm_notify_gate),
        Arc::clone(&recording_paused),
        Arc::clone(&force_idr),
        Arc::clone(&gb_flips),
        Arc::clone(&onvif_events_slot),
    );

    // ONVIF
    {
        let cfg = web::db::get_protocol_config(&pool, "onvif")
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| serde_json::to_value(&config.onvif).unwrap_or_default());
        let enabled = cfg
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if enabled {
            let onvif_config =
                web::protocol_runtime::build_onvif_config_from_json(&cfg, &advertised_host);
            if let Err(e) = protocol_runtime
                .start_onvif(onvif_config, stream_manager.clone())
                .await
            {
                tracing::warn!(error = %e, "failed to start ONVIF at startup");
            }
        }
    }

    // GB28181
    {
        let cfg = web::db::get_protocol_config(&pool, "gb28181")
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| serde_json::to_value(&config.gb28181).unwrap_or_default());
        let enabled = cfg
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if enabled {
            let gb_config = web::protocol_runtime::extract_gb28181_config(&cfg);
            if let Err(e) = protocol_runtime
                .start_gb28181(&gb_config, stream_manager.clone())
                .await
            {
                tracing::warn!(error = %e, "failed to start GB28181 at startup");
            }
        }
    }

    // RTMP (per-stream; just track enabled state)
    {
        let cfg = web::db::get_protocol_config(&pool, "rtmp_push")
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| serde_json::to_value(&config.rtmp_push).unwrap_or_default());
        let enabled = cfg
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if enabled && let Err(e) = protocol_runtime.start_rtmp().await {
            tracing::warn!(error = %e, "failed to enable RTMP at startup");
        }
    }

    let protocol_runtime = Arc::new(Mutex::new(protocol_runtime));

    // ── Hot-plug monitor: auto-discover plugged cameras, mark unplugged ──
    //
    // Listens to kernel netlink uevents for the video4linux subsystem.
    // On ADD: re-runs auto-discovery (does NOT auto-start the stream).
    // On REMOVE: marks the camera offline in DB and gracefully stops its
    //   active stream (flushing any in-progress recording segment).
    let hotplug_tx = event_tx.clone();
    let hotplug_db = pool.clone();
    let hotplug_stream_manager = stream_manager.clone();
    let hotplug_handle = tokio::spawn(async move {
        // Diagnostic escape hatch: MIBEE_DISABLE_HOTPLUG=1 skips the netlink
        // monitor entirely (used to bisect runtime starvation).
        if std::env::var_os("MIBEE_DISABLE_HOTPLUG").is_some_and(|v| v != "0") {
            tracing::warn!("hot-plug monitor disabled via MIBEE_DISABLE_HOTPLUG");
            return;
        }
        let monitor = match capture::hotplug::HotplugMonitor::new() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "failed to start hot-plug monitor; camera plug/unplug will not be detected");
                return;
            }
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            monitor.run(tx).await;
        });

        while let Some(event) = rx.recv().await {
            match event {
                capture::hotplug::HotplugEvent::Added { device_index, .. } => {
                    tracing::info!(device_index, "camera plugged in, running auto-discovery");
                    // Re-run discovery. Does NOT auto-start (user decision).
                    match web::db::auto_discover_cameras(&hotplug_db).await {
                        Ok(n) if n > 0 => {
                            // Find the newly-added camera to broadcast event.
                            let cameras =
                                web::db::list_cameras(&hotplug_db).await.unwrap_or_default();
                            for cam in cameras.iter().filter(|c| {
                                c.camera_type == "usb"
                                    && c.config.get("device_index").and_then(|v| v.as_u64())
                                        == Some(device_index as u64)
                            }) {
                                let _ = hotplug_tx.send(
                                    web::routes::events::CameraEvent::CameraAdded {
                                        camera_id: cam.id.clone(),
                                        device_index,
                                        name: cam.name.clone(),
                                    },
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "auto-discovery after hot-plug failed")
                        }
                    }
                }
                capture::hotplug::HotplugEvent::Removed { device_index } => {
                    tracing::info!(device_index, "camera unplugged, marking offline");
                    match web::db::mark_cameras_offline_by_device_index(
                        &hotplug_db,
                        device_index as i64,
                    )
                    .await
                    {
                        Ok(camera_ids) => {
                            for cam_id in &camera_ids {
                                // Gracefully stop the active stream (flushes FileOutput).
                                if hotplug_stream_manager.has_stream(cam_id).await {
                                    tracing::info!(camera_id = %cam_id, "stopping stream for unplugged camera");
                                    if let Err(e) = hotplug_stream_manager.stop_stream(cam_id).await
                                    {
                                        tracing::warn!(camera_id = %cam_id, error = %e, "failed to stop stream for unplugged camera");
                                    }
                                }
                                // Broadcast SSE event.
                                let _ = hotplug_tx.send(
                                    web::routes::events::CameraEvent::CameraOfflined {
                                        camera_id: cam_id.clone(),
                                        device_index,
                                    },
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, device_index, "failed to mark camera offline");
                        }
                    }
                }
            }
        }
    });
    protocol_handles.push(hotplug_handle);
    // Periodic session cleanup — runs every 5 minutes to purge expired auth sessions
    let cleanup_db_path = db_path_str.clone();
    let cleanup_handle = tokio::spawn(async move {
        let interval = tokio::time::Duration::from_secs(300);
        loop {
            tokio::time::sleep(interval).await;
            let path = cleanup_db_path.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || {
                let conn = web::db::init_auth_db(&path)?;
                let removed = security::auth::cleanup_expired_sessions(&conn)?;
                if removed > 0 {
                    tracing::info!(expired_sessions = removed, "Cleaned up expired sessions");
                } else {
                    tracing::debug!("Session cleanup: no expired sessions");
                }
                Ok::<(), anyhow::Error>(())
            })
            .await
            {
                tracing::warn!(error = %e, "Session cleanup task failed");
            }
        }
    });
    protocol_handles.push(cleanup_handle);

    // Shutdown coordination signal
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Restart endpoint channel (#32) — same graceful path as SIGTERM.
    let restart_tx = shutdown_tx.clone();

    // Signal handler task: listen for SIGINT (ctrl-c) and SIGTERM
    let signal_handle = tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        let terminate = async {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("Failed to register SIGTERM handler");
            sig.recv().await;
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => tracing::info!("Received SIGINT, shutting down"),
            _ = terminate => tracing::info!("Received SIGTERM, shutting down"),
        }

        // Signal the web server to stop. Second signal is a no-op (watch already true).
        let _ = shutdown_tx.send(true);
    });

    println!(
        "mibee-eye server starting on {}:{}...",
        config.web.host, config.web.port
    );
    // Create StreamManager and run server (blocks until shutdown)
    // (StreamManager was created earlier so protocol handlers can reference it.)

    // Build protocol config store for REST API
    let mut protocol_configs = HashMap::new();
    protocol_configs.insert("onvif".into(), serde_json::to_value(&config.onvif).unwrap());
    protocol_configs.insert(
        "gb28181".into(),
        serde_json::to_value(&config.gb28181).unwrap(),
    );
    protocol_configs.insert(
        "rtmp_push".into(),
        serde_json::to_value(&config.rtmp_push).unwrap(),
    );
    let protocol_configs = Arc::new(tokio::sync::Mutex::new(protocol_configs));

    // Clone for post-server-shutdown cleanup
    let protocol_runtime_for_shutdown = protocol_runtime.clone();

    web::server::run_with_shutdown(
        &config.web.host,
        config.web.port,
        config.web.http_port,
        pool,
        auth_db,
        stream_manager.clone(),
        rtsp_server,
        protocol_configs,
        protocol_runtime,
        shutdown_rx,
        advertised_host.clone(),
        event_tx,
        ai_engine,
        audio_ai_engine,
        shared_zones,
        ocr_engine,
        voice_engine,
        face_engine,
        chat_engine,
        vlm_engine,
        decision_engine,
        meeting_engine,
        grounding_state,
        Arc::clone(&shared_tools),
        Arc::new(web::routes::capabilities::LlmTier(llm_tier.to_string())),
        Arc::clone(&resource_profile),
        Arc::new(web::routes::capabilities::WakeWord(
            config.voice.wake_word.clone(),
        )),
        restart_tx,
        model_manager,
        cloud_ai.clone(),
        conversations_log,
        Arc::clone(&agent_registry),
        config.agent.clone(),
        Arc::clone(&away_engine),
    )
    .await?;

    // Abort the signal handler task
    signal_handle.abort();
    tracing::info!("shutdown: stopping active streams");

    // Stop all active streams
    stream_manager.shutdown_all().await;
    tracing::info!("shutdown: streams stopped");

    // Graceful shutdown of all protocols via ProtocolRuntime
    {
        let mut rt = protocol_runtime_for_shutdown.lock().await;
        rt.shutdown_all().await;
    }
    tracing::info!("shutdown: protocols stopped");

    // Abort remaining background tasks (RTSP server, session cleanup)
    for handle in protocol_handles {
        handle.abort();
    }
    tracing::info!("All protocol tasks shut down");
    // Skip destructors AND atexit handlers.
    //
    // Why: tearing down the dynamically loaded ONNX Runtime and its OpenMP
    // thread pool segfaults once the graceful path above has finished. That
    // is not a theory — it produced SIGTERM-time core dumps on 2026-10-10 and
    // the self-test CLI had been working around it all along for the same
    // reason. Everything that matters has already been shut down explicitly
    // (streams, protocols, GB deregistration), so the destructors left to run
    // only cover memory that the kernel is about to reclaim anyway.
    //
    // What skipping them costs, stated precisely so nobody has to guess:
    //
    //   Data:      nothing. Verified empirically — a writer that commits with
    //               `synchronous=FULL` and then calls `_exit` with its
    //               connection still open leaves a populated `-wal` behind,
    //               and the next open recovers every row. SQLite durability is
    //               per-commit, not per-close.
    //   Buffers:   explicit. `tracing_subscriber::fmt` writes to stdout (not
    //               stderr), and stdout is flushed below, so the final
    //               "All protocol tasks shut down" line is not lost.
    //   Checkpoint: the `-wal` is left for the next open to replay. It grows
    //               only by the work done since the last checkpoint, so the
    //               steady state is unchanged.
    //   Exit code: still 0.
    //
    // The one thing that is genuinely lost is any `Drop` impl that has a
    // side effect. Nothing in this shutdown path relies on one, which is why
    // this is safe today — and why a future `Drop`-based cleanup would be a
    // silent regression. Use the explicit stop calls above instead.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    unsafe { libc::_exit(0) };
    #[allow(unreachable_code)]
    Ok(())
}

/// Run one self-test subcommand, print the JSON report. Exit code 0 on a
/// completed run (events or not), 2 when the pipeline is unavailable.
fn selftest_cli<F>(name: &str, run: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<serde_json::Value>,
{
    match run() {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report)?);
            // Skip destructors AND atexit handlers: tearing down the
            // dynamically loaded ONNX Runtime + OpenMP threads segfaults
            // on some hosts after the report is complete.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            unsafe { libc::_exit(0) };
        }
        Err(e) => {
            eprintln!("selftest[{name}] failed: {e:#}");
            std::process::exit(2);
        }
    }
}

/// Handle the `--reset-password` CLI flag.
///
/// Prompts for username, current password, and new password, then calls
/// [`security::auth::reset_password`] and prints the result.
async fn reset_password_cli(args: &Args) -> anyhow::Result<()> {
    let db_path = args.db_path.to_string_lossy().to_string();
    let conn = web::db::init_auth_db(&db_path)?;

    let mut input = String::new();

    eprint!("Username: ");
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    let username = input.trim().to_string();

    eprint!("Current password: ");
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    let old_password = input.trim().to_string();

    eprint!("New password: ");
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    let new_password = input.trim().to_string();

    eprint!("Confirm new password: ");
    input.clear();
    std::io::stdin().read_line(&mut input)?;
    let confirm = input.trim().to_string();

    if new_password != confirm {
        eprintln!("Error: passwords do not match");
        std::process::exit(1);
    }

    match security::auth::reset_password(&conn, &username, &old_password, &new_password) {
        Ok(()) => {
            println!("Password reset successfully for user '{}'", username);
            println!("All existing sessions have been invalidated — please log in again.");
            Ok(())
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

/// Get the first non-loopback IPv4 address via interface enumeration.
/// Falls back to "127.0.0.1" if no suitable interface is found.
fn get_first_non_loopback_ipv4() -> Option<String> {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut ip = None;
        let mut ptr = ifap;
        while !ptr.is_null() {
            let ifa = &*ptr;
            if let Some(addr) = ifa.ifa_addr.as_ref()
                && addr.sa_family as libc::c_uint == libc::AF_INET as libc::c_uint
            {
                let sin = addr as *const libc::sockaddr as *const libc::sockaddr_in;
                let ip_addr = Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr));
                if !ip_addr.is_loopback() && !ip_addr.is_unspecified() {
                    ip = Some(ip_addr.to_string());
                    break;
                }
            }
            ptr = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        ip
    }
}

#[cfg(test)]
mod build_provenance_tests {
    /// The startup banner's sha is stamped by `build.rs`: a short git hex
    /// hash, or the honest `"unknown"` when built outside a repository.
    #[test]
    fn build_sha_is_git_hex_or_unknown() {
        let sha = env!("MITBEE_BUILD_SHA");
        assert!(
            sha == "unknown" || (sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit())),
            "unexpected build sha: {sha}"
        );
    }
}
