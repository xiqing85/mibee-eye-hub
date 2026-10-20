//! Web-server assembly — the former inline `main()` call site, moved
//! verbatim (roadmap #6, M1 final slice).
//!
//! [`WebServerDeps`] is the explicit dependency surface of the unified
//! web API: the M2 form bins construct exactly this struct (with their
//! own engine subsets) and hand it to [`serve`].

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, watch};

use web::protocol_runtime::ProtocolRuntime;
use web::routes::capabilities::LlmTier;
use web::routes::capabilities::WakeWord;
use web::routes::events::EventBus;
use web::routes::models_api::ModelManager;
use web::stream_manager::StreamManager;

use protocols::rtsp_server::RtspServer;

use rusqlite::Connection;

/// Everything `web::server::run_with_shutdown` needs, as one named
/// struct — the M2 assembly face.
pub struct WebServerDeps {
    pub host: String,
    pub port: u16,
    pub http_port: u16,
    pub db: sqlx::SqlitePool,
    pub auth_db: Arc<Mutex<Connection>>,
    pub stream_manager: Arc<StreamManager>,
    pub rtsp_server: Arc<RtspServer>,
    pub protocol_configs: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    pub protocol_runtime: Arc<Mutex<ProtocolRuntime>>,
    pub shutdown_rx: watch::Receiver<bool>,
    pub advertised_host: String,
    pub event_tx: Arc<EventBus>,
    pub ai: Arc<streaming::ai::AiEngine>,
    pub audio_ai: Arc<streaming::audio_ai::AudioAiEngine>,
    pub zones: web::zones::SharedZones,
    pub ocr: Arc<streaming::ocr::OcrEngine>,
    pub voice: Arc<streaming::voice::VoiceEngine>,
    pub face: Arc<streaming::face::FaceEngine>,
    pub chat: Arc<streaming::llm::ChatEngine>,
    pub vlm: Arc<streaming::vlm::VlmEngine>,
    pub decision: Arc<streaming::decision::DecisionEngine>,
    pub meeting: Arc<streaming::meeting::MeetingEngine>,
    pub grounding: Arc<web::grounding::GroundingState>,
    pub tools: web::server::SharedTools,
    pub llm_tier: Arc<LlmTier>,
    pub resource: Arc<streaming::feature_gate::ResourceProfile>,
    pub wake_word: Arc<WakeWord>,
    pub restart_tx: watch::Sender<bool>,
    pub models: Arc<ModelManager>,
    pub cloud: Arc<web::cloud::CloudAi>,
    pub conversations: Arc<web::conversations::ConversationLog>,
    pub agent: Arc<web::agent::ToolRegistry>,
    pub agent_config: web::agent::AgentConfig,
    pub models_routing: streaming::model_route::RoutingConfig,
    pub plugins_loaded: usize,
    pub away: Arc<web::away::AwayEngine>,
}

/// Run the unified web API until the shutdown watch fires. Blocks.
pub async fn serve(deps: WebServerDeps) -> anyhow::Result<()> {
    let WebServerDeps {
        host,
        port,
        http_port,
        db,
        auth_db,
        stream_manager,
        rtsp_server,
        protocol_configs,
        protocol_runtime,
        shutdown_rx,
        advertised_host,
        event_tx,
        ai,
        audio_ai,
        zones,
        ocr,
        voice,
        face,
        chat,
        vlm,
        decision,
        meeting,
        grounding,
        tools,
        llm_tier,
        resource,
        wake_word,
        restart_tx,
        models,
        cloud,
        conversations,
        agent,
        agent_config,
        models_routing,
        plugins_loaded,
        away,
    } = deps;
    web::server::run_with_shutdown(
        &host,
        port,
        http_port,
        db,
        auth_db,
        stream_manager,
        rtsp_server,
        protocol_configs,
        protocol_runtime,
        shutdown_rx,
        advertised_host,
        event_tx,
        ai,
        audio_ai,
        zones,
        ocr,
        voice,
        face,
        chat,
        vlm,
        decision,
        meeting,
        grounding,
        tools,
        llm_tier,
        resource,
        wake_word,
        restart_tx,
        models,
        cloud,
        conversations,
        agent,
        agent_config,
        models_routing,
        plugins_loaded,
        away,
    )
    .await
}
