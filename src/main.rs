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

/// VLM description scheduler state: single-flight flag + last-start
/// watermark (floor below). See the alarm hook in main for the rationale.
static VLM_DESC_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static VLM_DESC_LAST_START: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Minimum spacing between description starts — a description takes
/// 35–60 s of near-full CPU; this keeps headroom for everything else.
const VLM_DESC_MIN_INTERVAL_MS: u64 = 120_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Cap the OpenMP pool before any ORT session spawns it (hosts like
    // .40 have no swap — an oversubscribed spin-waiting pool is an
    // OOM/latency hazard). Halve the logical CPUs, clamped to [1, 4];
    // a user-provided OMP_NUM_THREADS always wins.
    if std::env::var_os("OMP_NUM_THREADS").is_none() {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let omp = (logical / 2).clamp(1, 4);
        // SAFETY: single-threaded startup, before any worker exists.
        unsafe { std::env::set_var("OMP_NUM_THREADS", omp.to_string()) };
    }

    // Install rustls crypto provider (required for rustls 0.23+)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

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

    // Feature-level resource admission (SPEC appendix A #40), before any
    // engine is built. The LLM tier model resolves first (#30-E) so the
    // gate charges the file that will actually load.
    let mem = streaming::tools::memory_mib().unwrap_or((0, 0));
    let (llm_model, llm_tier) = streaming::tools::resolve_llm_tier(
        &config.llm.model_path,
        &config.llm.model_path_mid,
        &config.llm.model_path_lite,
        config.resources.auto_tier,
        mem.1,
    );
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
        mem.0,
        mem.1,
        config.resources.reserve_mib,
    ));
    // Apply the plan: a non-admitted enabled feature is turned off in the
    // config copy every engine reads — the engines' own fail-open logs
    // then explain it exactly like a config-disabled feature.
    for d in &resource_profile.features {
        if d.admitted || d.reason != streaming::feature_gate::REASON_OFF_BUDGET {
            continue;
        }
        tracing::warn!(
            feature = d.name,
            cost_mib = d.cost_mib,
            budget_mib = resource_profile.budget_mib,
            "resource gate: feature shed by boot memory budget"
        );
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
    {
        let vlm_engine = Arc::clone(&vlm_engine);
        let grounding_feed = Arc::clone(&grounding_state);
        let face_for_ai = Arc::clone(&face_engine);
        let mut ai_events = ai_engine.subscribe_events();
        let bridge_tx = event_tx.clone();
        let mut alarm_bridge =
            web::alarm::AlarmBridge::new(std::time::Duration::from_secs(alarm_cooldown_secs));
        let notifier_slot = Arc::clone(&notifier_slot);
        let alarm_notify_gate = Arc::clone(&alarm_notify_gate);
        let onvif_events_slot = Arc::clone(&onvif_events_slot);
        let desktop_ai_alarm = Arc::clone(&desktop);
        tokio::spawn(async move {
            loop {
                match ai_events.recv().await {
                    Ok(ev) => {
                        let targets = ev.detections.len();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let frame_w = ev
                            .jpeg
                            .as_deref()
                            .and_then(web::grounding::jpeg_dimensions)
                            .map_or(0, |(w, _)| w);
                        grounding_feed.record_detections(
                            &ev.camera_id,
                            now_ms,
                            frame_w,
                            &ev.detections,
                        );
                        // Face recognition (#33): names + enrollment feed
                        // on the same frame the detection ran on.
                        if face_for_ai.is_active()
                            && let Some(jpeg) = ev.jpeg.clone()
                        {
                            let face = Arc::clone(&face_for_ai);
                            let cam_id = ev.camera_id.clone();
                            let feed = Arc::clone(&grounding_feed);
                            tokio::task::spawn_blocking(move || {
                                let hits = face.match_jpeg(&jpeg);
                                if !hits.is_empty() {
                                    let mut named: std::collections::BTreeMap<String, usize> =
                                        std::collections::BTreeMap::new();
                                    let mut unknown = 0usize;
                                    for h in &hits {
                                        match &h.name {
                                            Some(n) => {
                                                *named.entry(n.clone()).or_default() += 1;
                                            }
                                            None => unknown += 1,
                                        }
                                    }
                                    let mut text: Vec<String> = named
                                        .into_iter()
                                        .map(|(n, c)| format!("{n}×{c}"))
                                        .collect();
                                    if unknown > 0 {
                                        text.push(format!("未识别×{unknown}"));
                                    }
                                    let ts = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    feed.record_face_labels(&cam_id, ts, &text.join("、"));
                                }
                                // Enrollment feed: any face in frame
                                // advances the pending session.
                                let in_session = face.with_registry(|r| {
                                    r.enrollment_status(face.enroll_frames()).is_some()
                                });
                                if in_session {
                                    let needed = face.enroll_frames();
                                    if let Ok(emb) = face.embed_jpeg(&jpeg) {
                                        face.with_registry(|r| {
                                            r.feed_enroll(emb, needed);
                                        });
                                    }
                                }
                            });
                        }
                        let _ = bridge_tx.send(web::routes::events::CameraEvent::AiDetection {
                            camera_id: ev.camera_id.clone(),
                            detections: ev.detections,
                            frame_number: ev.frame_number,
                        });

                        if let Some(sig) = alarm_bridge.observe(
                            &ev.camera_id,
                            targets,
                            std::time::Instant::now(),
                            now_ms,
                        ) {
                            let _ = bridge_tx.send(web::routes::events::CameraEvent::Alarm {
                                camera_id: sig.camera_id.clone(),
                                targets: sig.targets,
                                timestamp_ms: sig.timestamp_ms,
                                source: "ai".to_string(),
                                class: None,
                                score: None,
                            });
                            // Desktop notification rides the same accepted
                            // edge (SPEC appendix A #42; no-op headless).
                            desktop_ai_alarm.notify_alarm("ai", sig.targets, None);
                            // VLM description of the triggering frame (SPEC
                            // appendix A #23): the alarm never waits for it —
                            // the description arrives as its own SSE event.
                            // Single-flight + a floor between starts: a
                            // flickering false-positive detection fires rising
                            // edges every cooldown (30 s) while one
                            // description takes 35–60 s — without the guard
                            // they stack into concurrent contexts that
                            // saturate the CPU (and can OOM-abort the
                            // process). New triggers during a run or inside
                            // the floor are dropped, not queued.
                            if vlm_engine.is_active()
                                && let Some(jpeg) = ev.jpeg.clone()
                            {
                                if VLM_DESC_IN_FLIGHT
                                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                                {
                                    // Another description is still running —
                                    // drop this frame (never queue).
                                    tracing::debug!(
                                        "vlm: description in flight — alarm frame skipped"
                                    );
                                } else if now_ms.saturating_sub(
                                    VLM_DESC_LAST_START.load(std::sync::atomic::Ordering::SeqCst),
                                ) < VLM_DESC_MIN_INTERVAL_MS
                                {
                                    // Release OUR acquisition only (a run we
                                    // just prevented, not someone else's).
                                    VLM_DESC_IN_FLIGHT
                                        .store(false, std::sync::atomic::Ordering::SeqCst);
                                    tracing::debug!(
                                        "vlm: description throttled — alarm frame skipped"
                                    );
                                } else {
                                    VLM_DESC_LAST_START
                                        .store(now_ms, std::sync::atomic::Ordering::SeqCst);
                                    let vlm = Arc::clone(&vlm_engine);
                                    let tx = bridge_tx.clone();
                                    let camera_id = sig.camera_id.clone();
                                    let alarm_ts = sig.timestamp_ms;
                                    let grounding_feed = Arc::clone(&grounding_feed);
                                    tokio::task::spawn_blocking(move || {
                                        let started = std::time::Instant::now();
                                        let result = vlm.describe_jpeg(&jpeg);
                                        VLM_DESC_IN_FLIGHT
                                            .store(false, std::sync::atomic::Ordering::SeqCst);
                                        match result {
                                            Ok(description) => {
                                                grounding_feed.record_vlm_description(
                                                    &camera_id,
                                                    alarm_ts,
                                                    &description,
                                                );
                                                let _ = tx.send(
                                                    web::routes::events::CameraEvent::AlarmDescription {
                                                        camera_id,
                                                        alarm_timestamp_ms: alarm_ts,
                                                        description,
                                                        elapsed_s: (started
                                                            .elapsed()
                                                            .as_secs_f64()
                                                            * 100.0)
                                                            .round()
                                                            / 100.0,
                                                    },
                                                );
                                            }
                                            Err(e) => tracing::warn!(
                                                error = %e,
                                                "vlm: description failed"
                                            ),
                                        }
                                    });
                                }
                            }
                            // ONVIF MotionAlarm rides the same accepted
                            // edge (no NVR subscribed = no-op).
                            if let Some(events) = onvif_events_slot
                                .lock()
                                .expect("onvif events slot lock")
                                .clone()
                            {
                                events.publish_event(web::onvif_alarm::motion_alarm_event(
                                    &sig.camera_id,
                                    sig.targets,
                                ));
                            }
                            if alarm_notify_gate.load(std::sync::atomic::Ordering::SeqCst) {
                                let notifier =
                                    notifier_slot.lock().expect("gb notifier slot lock").clone();
                                if let Some(n) = notifier {
                                    // 2022 standard table: priority 4, video
                                    // alarm (method 5), motion target (type 2).
                                    let sent = n.send_alarm(
                                        "4",
                                        "5",
                                        &gb28181_rs::client::format_gb_time_ms(now_ms),
                                        "2",
                                        "motion target detected",
                                    );
                                    if !sent {
                                        tracing::debug!("alarm notify: no active subscription");
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "ai: SSE bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

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
    {
        let mut voice_events = voice_engine.subscribe_events();
        let bridge_tx = event_tx.clone();
        let chat_for_voice = chat_engine.clone();
        let cloud_for_voice = cloud_ai.clone();
        let decision_for_voice = decision_engine.clone();
        let convlog_for_voice = Arc::clone(&conversations_log);
        let desktop_for_voice = Arc::clone(&desktop);
        let grounding_for_voice = Arc::clone(&grounding_state);
        let streams_for_voice = Arc::clone(&segment_registry);
        let tools_for_voice = Arc::clone(&shared_tools);
        let agent_registry_for_voice = Arc::clone(&agent_registry);
        let agent_config_for_voice = config.agent.clone();
        let wake_word_for_voice = config.voice.wake_word.clone();
        let voice_engine_for_arming = Arc::clone(&voice_engine);
        // Away voice control (#44): short utterances arm/disarm the
        // watch ahead of the decision/LLM pipeline.
        let away_for_voice = Arc::clone(&away_engine);
        let tts_for_away_cmd = Arc::clone(&tts_engine);
        // Conversation session (#30-B): the last 120 s of turns give
        // follow-ups ("再说详细点") their context. The slot also owns the
        // conversation trace (SPEC §3.3): all turns inside the window land
        // on one `origin:"voice"` trace; expiry closes it.
        type ConversationSlot = Arc<
            std::sync::Mutex<
                Option<(
                    std::time::Instant,
                    Vec<streaming::llm::ChatTurn>,
                    web::convtrace::ConvHandle,
                )>,
            >,
        >;
        let conversation: ConversationSlot = Arc::new(std::sync::Mutex::new(None));
        let records_pool = pool.clone();
        let grounding_voice = Arc::clone(&grounding_state);
        // Active answer round (SPEC appendix A #45): at most one at a
        // time — this slot is what turns a transcript arriving mid-round
        // into an interruption decision instead of a racing second round.
        // `cancel` stops the streaming generation at the next token and
        // kills playback; `spoken_prefix` accumulates the sentences that
        // actually finished playing (the interrupted turn's honest
        // reply); `round_id` lets a finished round clear the slot only
        // when it still owns it.
        struct ActiveRound {
            cancel: Arc<std::sync::atomic::AtomicBool>,
            round_id: u64,
        }
        let active_round: Arc<std::sync::Mutex<Option<ActiveRound>>> =
            Arc::new(std::sync::Mutex::new(None));
        let round_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let interrupt_enabled = config.voice.interrupt_enable;
        let interrupt_confidence = config.voice.interrupt_confidence;
        tokio::spawn(async move {
            loop {
                match voice_events.recv().await {
                    Ok(ev) => {
                        // Persistent hearing record (SPEC appendix A #24)
                        // with the correlated scene (#30-C).
                        let camera = web::db::list_cameras(&records_pool)
                            .await
                            .ok()
                            .and_then(|cams| cams.first().map(|c| c.id.clone()));
                        let scene = camera
                            .as_deref()
                            .and_then(|id| grounding_voice.scene_summary(id, ev.timestamp_ms))
                            .unwrap_or_default();
                        let media_ref = camera
                            .as_deref()
                            .and_then(|id| {
                                web::stream_manager::StreamManager::segment_covering(
                                    &streams_for_voice,
                                    id,
                                    ev.timestamp_ms,
                                )
                            })
                            .unwrap_or_default();
                        if let Err(e) = web::db::insert_hearing_record(
                            &records_pool,
                            "voice",
                            &ev.transcript,
                            None,
                            &ev.keyword,
                            &ev.speaker,
                            &scene,
                            &media_ref,
                            ev.timestamp_ms as i64,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "hearing record: insert failed");
                        }
                        let _ = bridge_tx.send(web::routes::events::CameraEvent::VoiceTranscript {
                            keyword: ev.keyword,
                            transcript: ev.transcript.clone(),
                            speaker: ev.speaker.clone(),
                            scene: scene.clone(),
                            follow_up: ev.follow_up,
                            timestamp_ms: ev.timestamp_ms,
                        });
                        // Conversation trace (SPEC §3.3): join the 120 s
                        // slot's trace or start a fresh one; a stale slot
                        // closes its trace first.
                        let conv = {
                            let mut slot = conversation.lock().expect("conversation slot lock");
                            let fresh = slot
                                .as_ref()
                                .is_some_and(|(at, _, _)| at.elapsed().as_secs() < 120);
                            if !fresh && let Some((_, _, old)) = slot.take() {
                                old.close();
                            }
                            match slot.as_ref() {
                                Some((_, _, handle)) => {
                                    handle.add_turn();
                                    handle.clone()
                                }
                                None => {
                                    let handle = web::convtrace::convtrace().start("voice");
                                    handle.add_turn();
                                    *slot = Some((
                                        std::time::Instant::now(),
                                        Vec::new(),
                                        handle.clone(),
                                    ));
                                    handle
                                }
                            }
                        };
                        // Conversation record (SPEC §3.4): one turn per
                        // utterance — finished on every exit path below
                        // (reply, ignore decision, or failure). The draft
                        // collects the internal "thinking" entries as the
                        // legs run.
                        let mut draft = web::conversations::TurnDraft::new(
                            web::conversations::TurnOrigin::Voice,
                            conv.id(),
                        )
                        .user_text(ev.transcript.clone());
                        // Interruption gate (SPEC appendix A #45): a
                        // transcript while a round is running is an
                        // interruption candidate, not a second racing
                        // round. Stop words are rule-matched (0ms);
                        // everything else goes to the decision engine as
                        // supersede/backchannel with a conservative
                        // fail-open (ignore).
                        if interrupt_enabled
                            && active_round.lock().expect("active round lock").is_some()
                        {
                            if ev.transcript.trim().is_empty() {
                                // VAD noise blip mid-round — not worth a
                                // record; the round keeps thinking.
                                conv.close();
                                continue;
                            }
                            let interrupt_started = std::time::Instant::now();
                            let action = if streaming::decision::stop_words_match(&ev.transcript) {
                                streaming::decision::InterruptAction::Stop
                            } else {
                                let mut action = streaming::decision::InterruptAction::Backchannel;
                                if decision_for_voice.is_active() {
                                    let state_text = format!(
                                        "语音助手正在回答上一个问题时，用户又说：「{}」",
                                        ev.transcript
                                    );
                                    let engine = decision_for_voice.clone();
                                    if let Ok(d) = tokio::task::spawn_blocking(move || {
                                        engine.decide_choice(
                                            &state_text,
                                            "这句话是新的问题还是附和？",
                                            &[
                                                (
                                                    "supersede".into(),
                                                    "用户提出了新的问题或改变了话题，需要重新回答"
                                                        .into(),
                                                ),
                                                (
                                                    "backchannel".into(),
                                                    "附和、确认或无意义的声音（嗯、好、哦）".into(),
                                                ),
                                            ],
                                        )
                                    })
                                    .await
                                        && let Some(d) = d
                                    {
                                        action = streaming::decision::interrupt_policy(
                                            &d.label,
                                            d.confidence,
                                            interrupt_confidence,
                                        );
                                    }
                                }
                                action
                            };
                            match action {
                                streaming::decision::InterruptAction::Stop => {
                                    if let Some(active) =
                                        active_round.lock().expect("active round lock").take()
                                    {
                                        active
                                            .cancel
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    tracing::info!(
                                        transcript = %ev.transcript,
                                        "voice: round interrupted (stop word)"
                                    );
                                    convlog_for_voice
                                        .finish(draft.think(
                                            "interrupt",
                                            "rule",
                                            "停词 — 已取消当前回答",
                                            interrupt_started.elapsed().as_millis() as u64,
                                        ))
                                        .await;
                                    conv.close();
                                    continue;
                                }
                                streaming::decision::InterruptAction::Backchannel => {
                                    tracing::info!(
                                        transcript = %ev.transcript,
                                        "voice: mid-round utterance ignored (backchannel)"
                                    );
                                    // The VAD segment machinery re-arms the
                                    // follow-up window to now+2s on every
                                    // segment — restore the round-long
                                    // thinking window so one backchannel
                                    // blip does not end wake-free
                                    // listening for the rest of the round.
                                    voice_engine_for_arming.arm_listen(180_000);
                                    convlog_for_voice
                                        .finish(draft.think(
                                            "interrupt",
                                            "laya",
                                            "附和或低置信 — 忽略，当前回答继续",
                                            interrupt_started.elapsed().as_millis() as u64,
                                        ))
                                        .await;
                                    conv.close();
                                    continue;
                                }
                                streaming::decision::InterruptAction::Supersede => {
                                    if let Some(active) =
                                        active_round.lock().expect("active round lock").take()
                                    {
                                        active
                                            .cancel
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    tracing::info!(
                                        transcript = %ev.transcript,
                                        "voice: round superseded by a new question"
                                    );
                                    // Fall through: this utterance becomes
                                    // the new round below (the dying round
                                    // records itself as interrupted).
                                    draft = draft.think(
                                        "interrupt",
                                        "laya",
                                        "新问题 — 已取消上一轮回答",
                                        interrupt_started.elapsed().as_millis() as u64,
                                    );
                                }
                            }
                        }
                        // Away voice control (#44 amendment): "我要出门了"
                        // "我回来了" arm/disarm the watch directly — a
                        // built-in intent, answered without the LLM.
                        let away_cmd_started = std::time::Instant::now();
                        if let Some(want_active) = web::away::match_voice_command(&ev.transcript)
                            && away_for_voice.settings_snapshot().voice_control
                        {
                            let result = web::away::set_active(
                                &away_for_voice,
                                &records_pool,
                                &bridge_tx,
                                want_active,
                                Some(&chat_for_voice),
                            )
                            .await;
                            let (reply, note) = match &result {
                                Ok(None) if want_active => (
                                    "好的，离家模式已布防，路上注意安全。".to_string(),
                                    "已布防".to_string(),
                                ),
                                Ok(summary) => match summary {
                                    Some(s) => (
                                        format!("欢迎回家！离家模式已撤防。{s}"),
                                        format!("已撤防 · {s}"),
                                    ),
                                    None => (
                                        "欢迎回家！离家模式已撤防。".to_string(),
                                        "已撤防（无事件）".to_string(),
                                    ),
                                },
                                Err(reason) => (
                                    format!("抱歉，当前无法布防：{reason}"),
                                    format!("布防失败：{reason}"),
                                ),
                            };
                            let _ = bridge_tx.send(web::routes::events::CameraEvent::ChatReply {
                                source: "voice".into(),
                                reply: reply.clone(),
                                grounded: "none".into(),
                                timestamp_ms: unix_now_ms(),
                            });
                            if tts_for_away_cmd.is_active() {
                                voice_engine_for_arming.begin_playback_mute();
                                let tts = Arc::clone(&tts_for_away_cmd);
                                let text = reply.clone();
                                if let Err(e) =
                                    tokio::task::spawn_blocking(move || tts.speak(&text)).await
                                {
                                    tracing::warn!(error = %e, "away: command speak failed");
                                }
                                voice_engine_for_arming.end_playback_mute();
                            }
                            voice_engine_for_arming.arm_follow_up();
                            desktop_for_voice.notify_conversation(&reply);
                            convlog_for_voice
                                .finish(
                                    draft
                                        .think(
                                            "away.command",
                                            "builtin",
                                            note,
                                            away_cmd_started.elapsed().as_millis() as u64,
                                        )
                                        .reply(reply, "builtin"),
                                )
                                .await;
                            continue;
                        }
                        // Decision triage (SPEC appendix A #26): one Laya
                        // typed decision classifies the transcript before
                        // any local-LLM tokens are spent. `ignore` skips
                        // the auto-reply; inactive/low-confidence fails
                        // open to the previous behavior (always answer).
                        let mut skip_reply = false;
                        if decision_for_voice.is_active() && !ev.transcript.trim().is_empty() {
                            let state_text = format!("用户对家庭摄像头说：「{}」", ev.transcript);
                            let engine = decision_for_voice.clone();
                            let transcript_for_decision = ev.transcript.clone();
                            let tx_for_decision = bridge_tx.clone();
                            let ts = ev.timestamp_ms;
                            let mut decision_span =
                                conv.span("decision", "laya", "意图决策", vec![]);
                            let decision_started = std::time::Instant::now();
                            let decision = match decision_span
                                .run(tokio::task::spawn_blocking(move || {
                                    engine
                                        .decide_choice(
                                            &state_text,
                                            "这句话属于哪一类意图？",
                                            &[
                                                (
                                                    "answer".into(),
                                                    "用户在提问或聊天（普通话、粤语、英语都算），需要回答"
                                                        .into(),
                                                ),
                                                ("device".into(), "用户想控制设备或查询状态".into()),
                                                (
                                                    "ignore".into(),
                                                    "环境噪声、误唤醒或无意义内容".into(),
                                                ),
                                            ],
                                        )
                                        .map(|d| (d, transcript_for_decision, ts))
                                }))
                                .await
                            {
                                Ok(v) => {
                                    decision_span.finish_ok();
                                    v
                                }
                                Err(_) => {
                                    decision_span.finish_err();
                                    None
                                }
                            };
                            if let Some((d, transcript, ts)) = decision {
                                tracing::info!(
                                    choice = %d.label,
                                    confidence = d.confidence,
                                    "decision: voice triage"
                                );
                                skip_reply = streaming::decision::triage_skips_reply(
                                    &d.label,
                                    &ev.transcript,
                                );
                                let mut decision_note =
                                    format!("意图={}（置信度 {:.2}）", d.label, d.confidence);
                                if skip_reply {
                                    decision_note.push_str(" → 不回复");
                                }
                                draft = draft.think(
                                    "decision",
                                    "laya",
                                    decision_note,
                                    decision_started.elapsed().as_millis() as u64,
                                );
                                let _ = tx_for_decision.send(
                                    web::routes::events::CameraEvent::VoiceDecision {
                                        camera_id: "0".to_string(),
                                        transcript,
                                        choice: d.label.clone(),
                                        confidence: d.confidence,
                                        act_probability: d.act_probability,
                                        timestamp_ms: ts,
                                    },
                                );
                            }
                        }
                        if !skip_reply
                            && chat_for_voice.is_active()
                            && !ev.transcript.trim().is_empty()
                        {
                            // Scene grounding (SPEC #29) + local/web
                            // context (#30-A): same blocks the HTTP chat
                            // route injects — the spoken answer knows what
                            // the camera sees, the local clock, and (when
                            // asked + enabled) the weather, and follows the
                            // user's language.
                            let scene = web::db::list_cameras(&records_pool)
                                .await
                                .ok()
                                .and_then(|cams| cams.first().map(|c| c.id.clone()))
                                .and_then(|id| {
                                    grounding_for_voice.scene_summary(&id, unix_now_ms())
                                });
                            let ctx = web::routes::chat::TurnContext {
                                scene,
                                local: Some(web::routes::chat::local_block(&records_pool).await),
                                web: {
                                    let t = tools_for_voice
                                        .read()
                                        .expect("tools config lock poisoned")
                                        .clone();
                                    web::routes::chat::web_block(&t, &ev.transcript).await
                                },
                                wake_word: wake_word_for_voice.clone(),
                            };
                            let grounded =
                                if ctx.scene.is_some() { "scene" } else { "none" }.to_string();
                            // Session history: keep the last 8 turns if the
                            // conversation is younger than 120 s.
                            let history_turns = {
                                let slot = conversation.lock().expect("conversation slot lock");
                                slot.as_ref()
                                    .map_or_else(Vec::new, |(_, turns, _)| turns.clone())
                            };
                            let mut turns =
                                vec![web::routes::chat::build_system_turn(&ctx, &ev.transcript)];
                            turns.extend(history_turns.iter().take(8).cloned());
                            turns.push(streaming::llm::ChatTurn {
                                role: "user".into(),
                                content: ev.transcript.clone(),
                            });
                            let engine = chat_for_voice.clone();
                            let cloud = cloud_for_voice.clone();
                            let agent_registry = Arc::clone(&agent_registry_for_voice);
                            let agent_cfg = agent_config_for_voice.clone();
                            let agent_tx = bridge_tx.clone();
                            let tx = bridge_tx.clone();
                            let tts_for_reply = tts_engine.clone();
                            // Sentence-streaming pipeline (2026-10-12 P0):
                            // the answer block itself drives TTS for the
                            // local leg — clones captured by the answer
                            // future below (the wrapper keeps its own).
                            let tts_for_pipeline = tts_engine.clone();
                            let voice_for_pipeline = Arc::clone(&voice_engine_for_arming);
                            let grounded = grounded.clone();
                            let conversation_for_reply = Arc::clone(&conversation);
                            let voice_engine_for_reply = Arc::clone(&voice_engine_for_arming);
                            let user_turn = ev.transcript.clone();
                            let conv_for_answer = conv.clone();
                            let conv_for_reply = conv.clone();
                            let convlog_for_reply = Arc::clone(&convlog_for_voice);
                            let desktop_for_conversation = Arc::clone(&desktop_for_voice);
                            // Active-round slot (#45): this round owns the
                            // mic's thinking-phase wake-free window and the
                            // interrupt cancel flag; the wrapper clears the
                            // slot when it still holds round_id.
                            let round_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let spoken_prefix = Arc::new(std::sync::Mutex::new(String::new()));
                            let round_id =
                                round_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            {
                                let mut slot = active_round.lock().expect("active round lock");
                                *slot = Some(ActiveRound {
                                    cancel: Arc::clone(&round_cancel),
                                    round_id,
                                });
                            }
                            if interrupt_enabled {
                                // Wake-free listening for the whole round
                                // (thinking + speaking): VAD segments land
                                // in the #45 gate above. Replaced by the
                                // normal 12 s follow-up window when the
                                // wrapper finishes the round.
                                voice_engine_for_arming.arm_listen(180_000);
                            }
                            let active_round_for_reply = Arc::clone(&active_round);
                            let round_id_for_reply = round_id;
                            // Answer path (SPEC §4.10): the cloud answers
                            // first when configured — same persona/grounding
                            // system turn — and the local model is the
                            // fallback (or the only path when cloud is off).
                            // Each model call opens a conversation span
                            // (SPEC §3.3) — the fallback keeps the failed
                            // cloud span visible in the chain. The record
                            // draft rides along (SPEC §3.4): success
                            // returns it with the reply; failure returns it
                            // with the error thinking note appended.
                            let answer = async move {
                                let mut draft = draft;
                                // Agent tool loop (SPEC §3.5) — same
                                // fail-open contract as the HTTP route:
                                // any error falls through to the legacy
                                // single-shot legs below. Voice turns
                                // carry the (prefill-heavy) tool table
                                // only on tool-shaped utterances
                                // (`voice_tool_gate`) — chit-chat keeps
                                // the fast single-shot path.
                                let voice_tools = !agent_cfg.voice_tool_gate
                                    || streaming::tools::tool_intent(&ev.transcript);
                                if agent_registry.agent_ready(&agent_cfg) && voice_tools {
                                    let cloud_first = cloud.enabled();
                                    let engine_label = if cloud_first { "cloud" } else { "local" };
                                    let (think_source, model_id) = if cloud_first {
                                        ("cloud.chat", cloud.config().chat_model.clone())
                                    } else {
                                        ("llm", engine.model_variant())
                                    };
                                    let channel: Box<dyn web::agent::runner::ModelChannel> =
                                        if cloud_first {
                                            Box::new(web::agent::channels::CloudChannel {
                                                cloud: cloud.clone(),
                                                timeout_secs: cloud.config().timeout_secs,
                                            })
                                        } else {
                                            Box::new(web::agent::channels::LocalChannel {
                                                engine: engine.clone(),
                                            })
                                        };
                                    let conv_id = conv_for_answer.id().to_string();
                                    let agent_started = std::time::Instant::now();
                                    let tool_thinks =
                                        std::sync::Mutex::new(
                                            Vec::<(String, String, String, u64)>::new(),
                                        );
                                    let outcome = web::agent::runner::run_agent(
                                        &agent_registry,
                                        channel.as_ref(),
                                        &turns,
                                        engine_label,
                                        agent_cfg.max_steps,
                                        std::time::Duration::from_millis(
                                            agent_cfg.step_timeout_ms.max(1000),
                                        ),
                                        |step| {
                                            let event = match &step {
                                                web::agent::AgentStep::PhaseThinking => {
                                                    web::routes::events::CameraEvent::AgentStep {
                                                        conversation_id: conv_id.clone(),
                                                        kind: "phase".into(),
                                                        state: "thinking".into(),
                                                        tool: None,
                                                        args: None,
                                                        result: None,
                                                        duration_ms: None,
                                                        note: None,
                                                    }
                                                }
                                                web::agent::AgentStep::PhaseAnswering {
                                                    ..
                                                } => web::routes::events::CameraEvent::AgentStep {
                                                    conversation_id: conv_id.clone(),
                                                    kind: "phase".into(),
                                                    state: "answering".into(),
                                                    tool: None,
                                                    args: None,
                                                    result: None,
                                                    duration_ms: None,
                                                    note: None,
                                                },
                                                web::agent::AgentStep::ToolStarted {
                                                    name,
                                                    args,
                                                } => web::routes::events::CameraEvent::AgentStep {
                                                    conversation_id: conv_id.clone(),
                                                    kind: "tool".into(),
                                                    state: "running".into(),
                                                    tool: Some(name.clone()),
                                                    args: Some(args.clone()),
                                                    result: None,
                                                    duration_ms: None,
                                                    note: None,
                                                },
                                                web::agent::AgentStep::ToolFinished {
                                                    record,
                                                    ..
                                                } => web::routes::events::CameraEvent::AgentStep {
                                                    conversation_id: conv_id.clone(),
                                                    kind: "tool".into(),
                                                    state: if record.ok {
                                                        "done".into()
                                                    } else {
                                                        "error".into()
                                                    },
                                                    tool: Some(record.name.clone()),
                                                    args: Some(record.args.clone()),
                                                    result: Some(record.result.clone()),
                                                    duration_ms: Some(record.duration_ms),
                                                    note: None,
                                                },
                                            };
                                            let _ = agent_tx.send(event);
                                            if let web::agent::AgentStep::ToolFinished {
                                                record,
                                                ..
                                            } = step
                                            {
                                                tool_thinks
                                                    .lock()
                                                    .expect("agent thinks lock")
                                                    .push((
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
                                            for (source, model, note, ms) in std::mem::take(
                                                &mut *tool_thinks
                                                    .lock()
                                                    .expect("agent thinks lock"),
                                            ) {
                                                draft = draft.think(&source, &model, note, ms);
                                            }
                                            let note = if out.tokens.0 > 0 || out.tokens.1 > 0 {
                                                format!(
                                                    "agent 应答 · prompt {}↑ / completion {}↓ tok（含 {} 次工具调用）",
                                                    out.tokens.0,
                                                    out.tokens.1,
                                                    out.tool_calls.len()
                                                )
                                            } else {
                                                format!(
                                                    "agent 应答（含 {} 次工具调用）",
                                                    out.tool_calls.len()
                                                )
                                            };
                                            draft = draft.think(think_source, &model_id, note, dur);
                                            return Ok((
                                                draft,
                                                Some(out.reply),
                                                engine_label,
                                                false,
                                                false,
                                            ));
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "voice: agent loop failed — falling back to legacy legs");
                                        }
                                    }
                                }
                                if cloud.enabled() {
                                    let model_id = cloud.config().chat_model.clone();
                                    let mut span = conv_for_answer.span(
                                        "cloud.chat",
                                        &model_id,
                                        "云端应答",
                                        vec![],
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
                                                Some((p, c)) => {
                                                    format!("云端应答 · {p}↑/{c}↓ tok")
                                                }
                                                None => "云端应答".to_string(),
                                            };
                                            draft = draft.think("cloud.chat", &model_id, note, dur);
                                            return Ok((draft, Some(reply), "cloud", false, false));
                                        }
                                        Err(e) => {
                                            let dur = leg_start.elapsed().as_millis() as u64;
                                            span.finish_err();
                                            tracing::warn!(error = %e, "cloud: voice reply failed — falling back to local llm");
                                            if !cloud.config().fallback_local {
                                                draft = draft.think(
                                                    "cloud.chat",
                                                    &model_id,
                                                    format!("云端失败：{e}（不回落）"),
                                                    dur,
                                                );
                                                return Err(draft);
                                            }
                                            draft = draft.think(
                                                "cloud.chat",
                                                &model_id,
                                                format!("云端失败：{e} — 回落本地"),
                                                dur,
                                            );
                                        }
                                    }
                                }
                                let variant = engine.model_variant();
                                let mut span =
                                    conv_for_answer.span("llm", &variant, "本地应答", vec![]);
                                let engine = engine.clone();
                                let turns = turns.clone();
                                let leg_start = std::time::Instant::now();
                                // Sentence-streaming pipeline (2026-10-12
                                // P0): with TTS live, the local leg
                                // streams generation deltas into a speaker
                                // task that synthesizes/plays per sentence
                                // and keeps ONE synthesis ahead of
                                // playback — first audio lands after the
                                // FIRST sentence instead of after the
                                // whole reply, and the playback self-mute
                                // latches only around actual playback
                                // (synthesis is silent), roughly halving
                                // the deaf window. `spoken` tells the
                                // wrapper this leg already spoke.
                                let tts_stream = tts_for_pipeline.is_active();
                                let mut generator = None;
                                let mut speaker: Option<
                                    tokio::task::JoinHandle<(usize, std::time::Duration)>,
                                > = None;
                                if tts_stream {
                                    let (tx_s, mut rx_s) = tokio::sync::mpsc::channel::<String>(8);
                                    let cancel_gen = Arc::clone(&round_cancel);
                                    generator = Some(tokio::task::spawn_blocking({
                                        let engine = engine.clone();
                                        let turns = turns.clone();
                                        let tx_s = tx_s.clone();
                                        move || {
                                            engine.complete_streaming_with_usage(
                                                &turns,
                                                Some(&mut |delta: &str| {
                                                    // #45: a set cancel flag
                                                    // stops generation at the
                                                    // next token; the partial
                                                    // reply is kept.
                                                    if cancel_gen
                                                        .load(std::sync::atomic::Ordering::SeqCst)
                                                    {
                                                        return false;
                                                    }
                                                    tx_s.blocking_send(delta.to_string()).is_ok()
                                                }),
                                            )
                                        }
                                    }));
                                    let cancel_speak = Arc::clone(&round_cancel);
                                    let prefix_cell = Arc::clone(&spoken_prefix);
                                    speaker = Some(tokio::spawn({
                                        let tts = Arc::clone(&tts_for_pipeline);
                                        let voice = Arc::clone(&voice_for_pipeline);
                                        let conv = conv_for_answer.clone();
                                        async move {
                                            let mut filter =
                                                streaming::sentence::ThinkFilter::new();
                                            let mut splitter =
                                                streaming::sentence::SentenceSplitter::new();
                                            let speak_span = conv.span(
                                                "tts",
                                                "vits",
                                                "语音播报（流水线）",
                                                vec![],
                                            );
                                            let started = std::time::Instant::now();
                                            let mut first = true;
                                            let mut sentences = 0_usize;
                                            let mut synth_ahead: Option<(
                                                String,
                                                tokio::task::JoinHandle<
                                                    anyhow::Result<std::path::PathBuf>,
                                                >,
                                            )> = None;
                                            while let Some(delta) = rx_s.recv().await {
                                                if cancel_speak
                                                    .load(std::sync::atomic::Ordering::SeqCst)
                                                {
                                                    break;
                                                }
                                                let visible = filter.push(&delta);
                                                for sentence in splitter.push(&visible) {
                                                    let t = Arc::clone(&tts);
                                                    let synth_text = sentence.clone();
                                                    let next =
                                                        tokio::task::spawn_blocking(move || {
                                                            t.synthesize(&synth_text)
                                                        });
                                                    if let Some((played, handle)) =
                                                        synth_ahead.take()
                                                    {
                                                        speak_pending_sentence(
                                                            &tts,
                                                            &voice,
                                                            handle,
                                                            &played,
                                                            &mut first,
                                                            &mut sentences,
                                                            &cancel_speak,
                                                            &prefix_cell,
                                                        )
                                                        .await;
                                                    }
                                                    synth_ahead = Some((sentence, next));
                                                }
                                            }
                                            if !cancel_speak
                                                .load(std::sync::atomic::Ordering::SeqCst)
                                            {
                                                if let Some(tail) = splitter.flush() {
                                                    let t = Arc::clone(&tts);
                                                    let synth_text = tail.clone();
                                                    let next =
                                                        tokio::task::spawn_blocking(move || {
                                                            t.synthesize(&synth_text)
                                                        });
                                                    if let Some((played, handle)) =
                                                        synth_ahead.take()
                                                    {
                                                        speak_pending_sentence(
                                                            &tts,
                                                            &voice,
                                                            handle,
                                                            &played,
                                                            &mut first,
                                                            &mut sentences,
                                                            &cancel_speak,
                                                            &prefix_cell,
                                                        )
                                                        .await;
                                                    }
                                                    synth_ahead = Some((tail, next));
                                                }
                                                if let Some((played, handle)) = synth_ahead.take() {
                                                    speak_pending_sentence(
                                                        &tts,
                                                        &voice,
                                                        handle,
                                                        &played,
                                                        &mut first,
                                                        &mut sentences,
                                                        &cancel_speak,
                                                        &prefix_cell,
                                                    )
                                                    .await;
                                                }
                                            }
                                            if !first {
                                                voice.end_playback_mute();
                                            }
                                            if sentences > 0 {
                                                speak_span.finish_ok();
                                            } else {
                                                speak_span.finish_err();
                                            }
                                            (sentences, started.elapsed())
                                        }
                                    }));
                                }
                                let gen_result = match generator {
                                    Some(g) => span.run(g).await,
                                    None => {
                                        let e = engine.clone();
                                        let t = turns.clone();
                                        span.run(tokio::task::spawn_blocking(move || {
                                            e.complete_with_usage(&t)
                                        }))
                                        .await
                                    }
                                };
                                // Finish the generation span/elapsed BEFORE
                                // awaiting the speaker — the LLM thinking
                                // entry must not absorb playback time (the
                                // pipeline overlaps the two by design).
                                let gen_elapsed_ms = leg_start.elapsed().as_millis() as u64;
                                match &gen_result {
                                    Ok(Ok((_, p, c))) => {
                                        span.tokens(Some(*p), Some(*c)).finish_ok();
                                    }
                                    _ => {
                                        span.finish_err();
                                    }
                                }
                                let mut spoken = 0_usize;
                                let mut speak_ms = 0_u64;
                                if let Some(sp) = speaker {
                                    match sp.await {
                                        Ok((n, dur)) => {
                                            spoken = n;
                                            speak_ms = dur.as_millis() as u64;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "voice: tts speaker task failed"
                                            );
                                        }
                                    }
                                }
                                match gen_result {
                                    Ok(Ok((reply, prompt_tokens, completion_tokens))) => {
                                        draft = draft.think(
                                            "llm",
                                            &variant,
                                            format!(
                                                "本地应答 · prompt {prompt_tokens}↑ / completion {completion_tokens}↓ tok"
                                            ),
                                            gen_elapsed_ms,
                                        );
                                        if spoken > 0 {
                                            draft = draft.think(
                                                "tts",
                                                "vits",
                                                format!("已播报 · {spoken} 句（流水线）"),
                                                speak_ms,
                                            );
                                        }
                                        // #45: a cancelled round reports
                                        // the actually-spoken prefix (None
                                        // when nothing got out) plus the
                                        // interrupted marker.
                                        let cancelled =
                                            round_cancel.load(std::sync::atomic::Ordering::SeqCst);
                                        let final_reply = if cancelled {
                                            let prefix = spoken_prefix
                                                .lock()
                                                .expect("spoken prefix lock")
                                                .trim()
                                                .to_string();
                                            if prefix.is_empty() {
                                                None
                                            } else {
                                                Some(prefix)
                                            }
                                        } else {
                                            Some(reply)
                                        };
                                        if cancelled {
                                            draft = draft.think(
                                                "interrupt",
                                                "rule",
                                                "回答被打断 — 仅保留已播报部分",
                                                0,
                                            );
                                        }
                                        Ok((draft, final_reply, "local", spoken > 0, cancelled))
                                    }
                                    Ok(Err(e)) => {
                                        draft = draft.think(
                                            "llm",
                                            &variant,
                                            format!("本地应答失败：{e}"),
                                            gen_elapsed_ms,
                                        );
                                        Err(draft)
                                    }
                                    Err(e) => {
                                        draft = draft.think(
                                            "llm",
                                            &variant,
                                            format!("本地应答任务失败：{e}"),
                                            gen_elapsed_ms,
                                        );
                                        Err(draft)
                                    }
                                }
                            };
                            tokio::spawn(async move {
                                let outcome = answer.await;
                                // Release the #45 active-round slot when this
                                // round still owns it (a superseding round
                                // already took it otherwise) — only AFTER the
                                // round finished: clearing earlier would
                                // empty the slot mid-round and deafen the
                                // interrupt gate.
                                {
                                    let mut slot =
                                        active_round_for_reply.lock().expect("active round lock");
                                    if slot
                                        .as_ref()
                                        .is_some_and(|r| r.round_id == round_id_for_reply)
                                    {
                                        slot.take();
                                    }
                                }
                                match outcome {
                                    Ok((mut draft, reply, engine_label, spoken, cancelled)) => {
                                        // Extend the conversation session with
                                        // this Q/A pair (trimmed to the last 4
                                        // pairs = 8 turns). An interrupted
                                        // round contributes its SPOKEN prefix
                                        // — the model's unspoken tail never
                                        // happened as far as the dialogue is
                                        // concerned.
                                        {
                                            let mut slot = conversation_for_reply
                                                .lock()
                                                .expect("conversation slot lock");
                                            let entry = slot.get_or_insert_with(|| {
                                                (
                                                    std::time::Instant::now(),
                                                    Vec::new(),
                                                    conv_for_reply.clone(),
                                                )
                                            });
                                            entry.0 = std::time::Instant::now();
                                            entry.1.push(streaming::llm::ChatTurn {
                                                role: "user".into(),
                                                content: user_turn,
                                            });
                                            if let Some(reply) = &reply {
                                                entry.1.push(streaming::llm::ChatTurn {
                                                    role: "assistant".into(),
                                                    content: reply.clone(),
                                                });
                                            }
                                            let keep = entry.1.len().saturating_sub(8);
                                            entry.1.drain(..keep);
                                        }
                                        if let Some(reply) = &reply {
                                            let _ = tx.send(
                                                web::routes::events::CameraEvent::ChatReply {
                                                    source: "voice".into(),
                                                    reply: reply.clone(),
                                                    grounded,
                                                    timestamp_ms: unix_now_ms(),
                                                },
                                            );
                                        }
                                        // Playback self-mute: while this reply
                                        // is on the speaker the wake-word
                                        // worker stays deaf — a spoken 小蜜蜂
                                        // in the reply would re-wake the
                                        // device into a self-talk loop.
                                        // The sentence-streaming local leg
                                        // already spoke (and managed the
                                        // mute itself); this one-shot path
                                        // covers the cloud/agent legs.
                                        if tts_for_reply.is_active() && !spoken && !cancelled {
                                            // One-shot path (cloud/agent legs);
                                            // `reply` is Some here — the local
                                            // pipeline leg set spoken=true or
                                            // was cancelled into None.
                                            let reply = reply.clone().unwrap_or_default();
                                            let tts_model = match streaming::lang::detect(&reply) {
                                                streaming::lang::SpokenLang::Cantonese => "tts.yue",
                                                streaming::lang::SpokenLang::English => "tts.en",
                                                streaming::lang::SpokenLang::Mandarin => "tts.zh",
                                            };
                                            let mut span = conv_for_reply.span(
                                                tts_model,
                                                "vits",
                                                "语音播报",
                                                vec![],
                                            );
                                            voice_engine_for_reply.begin_playback_mute();
                                            let tts_started = std::time::Instant::now();
                                            match span
                                                .run(async { tts_for_reply.speak(&reply) })
                                                .await
                                            {
                                                Ok(_) => {
                                                    span.finish_ok();
                                                    draft = draft.think(
                                                        tts_model,
                                                        "vits",
                                                        "已播报",
                                                        tts_started.elapsed().as_millis() as u64,
                                                    );
                                                }
                                                Err(e) => {
                                                    span.finish_err();
                                                    tracing::warn!(error = %e, "tts: speak failed");
                                                    draft = draft.think(
                                                        tts_model,
                                                        "vits",
                                                        format!("播报失败：{e}"),
                                                        tts_started.elapsed().as_millis() as u64,
                                                    );
                                                }
                                            }
                                            voice_engine_for_reply.end_playback_mute();
                                        }
                                        // Continuous dialogue (#30-B): arm the
                                        // VAD follow-up window only after the
                                        // reply finished playing — earlier would
                                        // let the mic capture our own TTS. Also
                                        // replaces the round-long #45 listening
                                        // window with the normal 12 s one.
                                        voice_engine_for_reply.arm_follow_up();
                                        // Desktop notification for the finished
                                        // reply (opt-in, SPEC appendix A #42).
                                        if let Some(reply) = &reply {
                                            desktop_for_conversation.notify_conversation(reply);
                                        }
                                        // Record the completed turn (SPEC §3.4)
                                        // — persist + SSE `conversation`.
                                        // Interrupted turns carry the spoken
                                        // prefix (or no reply at all) plus the
                                        // interrupted marker (#45).
                                        let mut draft = match &reply {
                                            Some(r) => draft.reply(r.clone(), engine_label),
                                            None => draft,
                                        };
                                        if cancelled {
                                            draft = draft.interrupted();
                                        }
                                        convlog_for_reply.finish(draft).await;
                                    }
                                    Err(draft) => {
                                        tracing::warn!("voice auto-reply failed");
                                        conv_for_reply.close();
                                        // The failed legs are already thinking
                                        // notes on the draft — record the
                                        // no-reply turn (SPEC §3.4).
                                        convlog_for_reply.finish(draft).await;
                                    }
                                }
                            });
                        } else {
                            // No reply path ran (decision said ignore, or
                            // the chat engine is inactive): the trace — at
                            // most a decision span — ends here. The record
                            // keeps the decision note — no-reply turns are
                            // honestly logged (SPEC §3.4).
                            convlog_for_voice.finish(draft).await;
                            conv.close();
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "voice: SSE bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

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
    // Skip destructors AND atexit handlers — same rationale as the
    // self-test CLI: tearing down the dynamically loaded ONNX Runtime +
    // OpenMP threads segfaults after the graceful path is done (observed
    // as SIGTERM-time core-dumps 2026-10-10). Everything that matters
    // (streams, protocols, GB registration refresh) has been shut down
    // above; the process exit code stays 0.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    unsafe { libc::_exit(0) };
    #[allow(unreachable_code)]
    Ok(())
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Voice pipeline P0 (2026-10-12): finish one in-flight sentence
/// synthesis — wait for its WAV, latch the playback self-mute on the
/// FIRST actual playback (synthesis is silent and stays audible-to-KWS),
/// then play it to completion. The next sentence's synthesis runs
/// concurrently (the caller started it before awaiting this).
#[allow(clippy::too_many_arguments)]
async fn speak_pending_sentence(
    tts: &Arc<streaming::tts::TtsEngine>,
    voice: &Arc<streaming::voice::VoiceEngine>,
    pending: tokio::task::JoinHandle<anyhow::Result<std::path::PathBuf>>,
    sentence: &str,
    first: &mut bool,
    sentences: &mut usize,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    spoken_prefix: &Arc<std::sync::Mutex<String>>,
) {
    let Ok(Ok(wav)) = pending.await else {
        return;
    };
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if *first {
        voice.begin_playback_mute();
        *first = false;
    }
    // #45: poll the player so an interruption kills it mid-sentence.
    let played = match tts.spawn_play(&wav) {
        Ok(Some(mut child)) => loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) => {
                    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        let _ = child.kill();
                        let _ = child.wait();
                        break false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(_) => break false,
            }
        },
        // No player configured: "played" by definition (selftest mode).
        Ok(None) => true,
        Err(e) => {
            tracing::warn!(error = %e, "tts: play spawn failed");
            false
        }
    };
    if played {
        *sentences += 1;
        let mut prefix = spoken_prefix.lock().expect("spoken prefix lock");
        prefix.push_str(sentence);
        prefix.push(' ');
    }
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
