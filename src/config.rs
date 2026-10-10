use serde::{Deserialize, Serialize};

// Re-export protocol config types from the web crate.
// These types live in `web::config` so the protocol REST API can derive
// JSON Schemas via `schemars` without a circular dependency.
use std::path::Path;
pub use web::config::{Gb28181Config, OnvifConfig, RecordingConfig, RtmpPushConfig, WebRtcConfig};

// ---------------------------------------------------------------------------
// Web
// ---------------------------------------------------------------------------

/// Web UI server configuration (TLS host and port).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebConfig {
    #[serde(default = "default_web_port")]
    pub port: u16,

    #[serde(default = "default_web_host")]
    pub host: String,

    /// Advertised hostname/IP for URLs returned to clients.
    /// If None, auto-detected at startup via UDP socket.
    #[serde(default)]
    pub advertised_host: Option<String>,

    /// Optional additional plain-HTTP listener for LAN access without TLS
    /// ceremony (SPEC appendix A). 0 = disabled (default). Session cookies
    /// issued over this listener omit the `Secure` flag; cookies issued
    /// over the TLS listener keep it.
    #[serde(default)]
    pub http_port: u16,
}

fn default_web_port() -> u16 {
    8443
}
fn default_web_host() -> String {
    "0.0.0.0".into()
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            port: 8443,
            host: "0.0.0.0".into(),
            advertised_host: None,
            http_port: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// RTSP
// ---------------------------------------------------------------------------

/// RTSP server configuration (listening port).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RtspConfig {
    #[serde(default = "default_rtsp_port")]
    pub server_port: u16,
}

fn default_rtsp_port() -> u16 {
    8554
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self { server_port: 8554 }
    }
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

/// Local video/audio capture device configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureConfig {
    #[serde(default = "default_video_device")]
    pub video_device: String,

    #[serde(default = "default_audio_device")]
    pub audio_device: String,
}

fn default_video_device() -> String {
    "/dev/video0".into()
}
fn default_audio_device() -> String {
    "default".into()
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            video_device: "/dev/video0".into(),
            audio_device: "default".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

/// Authentication rate-limiting configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_rate_limit_max")]
    pub rate_limit_max: usize,

    #[serde(default = "default_rate_limit_window")]
    pub rate_limit_window_secs: u64,
}

fn default_rate_limit_max() -> usize {
    20
}
fn default_rate_limit_window() -> u64 {
    60
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            rate_limit_max: 20,
            rate_limit_window_secs: 60,
        }
    }
}

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

/// OpenTelemetry tracing and log level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "default_otel_endpoint")]
    pub otel_endpoint: String,

    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Optional remote log shipping (Loki-compatible HTTP endpoint).
    /// When None (default), logs go to stdout only.
    #[serde(default)]
    pub logs: Option<RemoteLogConfig>,
}

fn default_otel_endpoint() -> String {
    "http://localhost:4317".into()
}
fn default_log_level() -> String {
    "info".into()
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            otel_endpoint: "http://localhost:4317".into(),
            log_level: "info".into(),
            logs: None,
        }
    }
}

/// Optional remote log shipping to a Loki-compatible HTTP endpoint.
///
/// When this section is present in config.toml, structured logs are shipped
/// to the configured endpoint in addition to stdout/stderr. The feature is
/// fail-open: if the endpoint is unreachable, a warning is logged and the
/// application continues with stdout-only logging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteLogConfig {
    /// Loki HTTP endpoint URL (e.g. http://loki:3100).
    #[serde(default)]
    pub endpoint: String,
    /// Number of log entries to batch per flush.
    #[serde(default = "default_remote_log_batch_size")]
    pub batch_size: usize,
    /// Flush interval in seconds.
    #[serde(default = "default_remote_log_flush_interval")]
    pub flush_interval_secs: u64,
    /// Additional labels attached to every log stream.
    #[serde(default)]
    pub labels: std::collections::HashMap<String, String>,
}

fn default_remote_log_batch_size() -> usize {
    100
}
fn default_remote_log_flush_interval() -> u64 {
    5
}

impl Default for RemoteLogConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            batch_size: 100,
            flush_interval_secs: 5,
            labels: std::collections::HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

/// SQLite database configuration with XDG-compliant default path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_database_path")]
    pub path: String,
}

fn default_database_path() -> String {
    // XDG default: ~/.local/share/mibee-eye/mibee_eye.db
    if let Some(data_dir) = dirs::data_dir() {
        data_dir
            .join("mibee-eye")
            .join("mibee_eye.db")
            .to_string_lossy()
            .to_string()
    } else {
        // Fallback to /tmp if XDG data dir is not available
        "/tmp/mibee-eye/mibee_eye.db".to_string()
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_database_path(),
        }
    }
}

// ---------------------------------------------------------------------------
// AppConfig — top-level configuration
// ---------------------------------------------------------------------------

// Top-level application configuration loaded from `config.toml`.
/// Each sub-section has sensible defaults; only the fields that differ from
/// defaults need to be specified in the TOML file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub web: WebConfig,

    #[serde(default)]
    pub rtsp: RtspConfig,
    #[serde(default)]
    pub capture: CaptureConfig,

    #[serde(default)]
    pub security: SecurityConfig,

    #[serde(default)]
    pub observability: ObservabilityConfig,

    #[serde(default)]
    pub onvif: OnvifConfig,

    #[serde(default)]
    pub gb28181: Gb28181Config,

    #[serde(default)]
    pub rtmp_push: RtmpPushConfig,

    #[serde(default)]
    pub recording: RecordingConfig,

    /// Video watermark bootstrap default (`[watermark]`; SPEC v1 §5.2).
    /// Runtime config lives in the DB (`protocols.watermark`) — this only
    /// seeds it on first run.
    #[serde(default)]
    pub watermark: web::config::WatermarkConfig,

    #[serde(default)]
    pub webrtc: WebRtcConfig,

    #[serde(default)]
    pub database: DatabaseConfig,

    /// On-device AI detection (`[ai]` section, SPEC v1 §5 device-specific
    /// config). Off by default; see `streaming::ai` for semantics.
    #[serde(default)]
    pub ai: streaming::ai::AiConfig,

    /// On-device audio intelligence (`[audio_ai]` section): sound events
    /// from the always-on microphone monitor. Off by default — continuous
    /// listening is opt-in; see `streaming::audio_ai`.
    #[serde(default)]
    pub audio_ai: streaming::audio_ai::AudioAiConfig,

    /// On-device OCR (`[ocr]` section, PP-OCRv5). Off by default.
    #[serde(default)]
    pub ocr: streaming::ocr::OcrConfig,

    /// Voice interaction (`[voice]` section: wake word + offline ASR).
    /// Off by default; requires a `voice`-feature build.
    #[serde(default)]
    pub voice: streaming::voice::VoiceConfig,

    /// Local LLM dialogue (`[llm]` section, llama.cpp/Qwen3 GGUF). Off by
    /// default; requires an `llm`-feature build.
    #[serde(default)]
    pub llm: streaming::llm::LlmConfig,

    /// TTS playback (`[tts]` section, sherpa-onnx CLI subprocess). Off by
    /// default; the binary is a deployment asset.
    #[serde(default)]
    pub tts: streaming::tts::TtsConfig,

    /// Decision triage (`[decision]` section, Laya typed decisions via
    /// ONNX Runtime). Off by default; rides the `ai` feature's `ort`.
    #[serde(default)]
    pub decision: streaming::decision::DecisionConfig,

    /// Alarm-image description (`[vlm]` section, Qwen3-VL GGUF via
    /// llama.cpp mtmd). Off by default; workstation-class only.
    #[serde(default)]
    pub vlm: streaming::vlm::VlmConfig,

    /// Meeting mode (`[meeting]` section: on-demand recording +
    /// diarization + per-segment ASR, SPEC appendix A #27). Off by
    /// default; processing requires a `voice`-feature build.
    #[serde(default)]
    pub meeting: streaming::meeting::MeetingConfig,
    /// Dialogue task tools (weather lookup; SPEC appendix A #30-A).
    #[serde(default)]
    pub tools: streaming::tools::ToolsConfig,
    /// Face recognition (#33) — off by default.
    #[serde(default)]
    pub face: streaming::face::FaceConfig,
    /// Resource-adaptive capability tiering (#30-E).
    #[serde(default)]
    pub resources: streaming::tools::ResourcesConfig,
    /// Model manager (SPEC §4.9): where the model catalog installs files.
    #[serde(default)]
    pub models: ModelsConfig,
    /// Desktop integration (SPEC appendix A #42): tray + notifications.
    #[serde(default)]
    pub desktop: DesktopConfig,
    /// Conversation records (SPEC §3.4): privacy master switch.
    #[serde(default)]
    pub conversations: ConversationsConfig,
    /// Agent tool/skill framework (SPEC §3.5 / appendix A #43): the
    /// tool-calling loop + MCP stdio plugin servers.
    #[serde(default)]
    pub agent: web::agent::AgentConfig,
    /// Away mode (SPEC §3.6 / appendix A #44): the armed watch loop —
    /// cadence, cooldowns, greetings, listen window, snapshot dir.
    #[serde(default)]
    pub away: web::away::AwayConfig,
}

/// `[models]` — the download root for the model manager (SPEC §4.9).
/// Engine paths are cwd-relative strings, so the default matches the
/// deployment's `models/` directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsConfig {
    pub dir: String,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            dir: "models".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Desktop integration & conversation records
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

/// Desktop integration (`[desktop]`, SPEC appendix A #42): tray icon +
/// desktop notifications on hosts with a desktop session. All fail-open
/// — a headless server skips the feature entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopConfig {
    /// Show a StatusNotifierItem tray icon when a session bus exists.
    #[serde(default = "default_true")]
    pub tray: bool,
    /// Desktop notifications for alarm rising edges.
    #[serde(default = "default_true")]
    pub notifications: bool,
    /// Also notify on completed voice replies (default off — the reply
    /// is already spoken aloud via TTS).
    #[serde(default)]
    pub notify_conversations: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            tray: true,
            notifications: true,
            notify_conversations: false,
        }
    }
}

/// Conversation records (`[conversations]`, SPEC appendix A #41):
/// privacy master switch for the dialogue-turn log. Disabled → records
/// nothing and advertises no `conversations` capability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ConversationsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl AppConfig {
    /// Load configuration from a TOML file.
    ///
    /// Reads the file at `path`, parses it as TOML, and returns the deserialized
    /// `AppConfig`. Missing sections/fields fall back to their respective defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Validate the configuration at startup.
    ///
    /// Checks:
    /// - Ports must be > 1024 (web.port, rtsp.server_port, gb28181.platform_sip_port)
    /// - Port conflicts (web.port must not equal rtsp.server_port)
    /// - security.rate_limit_max must be > 0
    /// - gb28181.register_interval_secs must be > 0 (if enabled)
    /// - rtmp_push.reconnect_interval_secs must be > 0 (if enabled)
    /// - rtmp_push.max_reconnect_attempts must be > 0 (if enabled)
    /// - observability.log_level must be one of: trace, debug, info, warn, error
    /// - recording.path must not be empty
    /// - recording.segment_duration_secs must be > 0
    pub fn validate(&self) -> anyhow::Result<()> {
        // Web port
        if self.web.port <= 1024 {
            anyhow::bail!("web.port: must be > 1024, got {}", self.web.port);
        }
        // RTSP port
        if self.rtsp.server_port <= 1024 {
            anyhow::bail!(
                "rtsp.server_port: must be > 1024, got {}",
                self.rtsp.server_port
            );
        }
        // Port conflict
        if self.web.port == self.rtsp.server_port {
            anyhow::bail!(
                "web.port ({}) must not equal rtsp.server_port ({})",
                self.web.port,
                self.rtsp.server_port
            );
        }
        // GB28181 SIP port (if enabled)
        if self.gb28181.enabled && self.gb28181.platform_sip_port <= 1024 {
            anyhow::bail!(
                "gb28181.platform_sip_port: must be > 1024, got {}",
                self.gb28181.platform_sip_port
            );
        }
        // Rate limit
        if self.security.rate_limit_max == 0 {
            anyhow::bail!("security.rate_limit_max: must be > 0, got 0");
        }
        // GB28181 register interval
        if self.gb28181.enabled && self.gb28181.register_interval_secs == 0 {
            anyhow::bail!("gb28181.register_interval_secs: must be > 0, got 0");
        }
        // RTMP push reconnect interval
        if self.rtmp_push.enabled && self.rtmp_push.reconnect_interval_secs == 0 {
            anyhow::bail!("rtmp_push.reconnect_interval_secs: must be > 0, got 0");
        }
        // RTMP push max reconnect attempts
        if self.rtmp_push.enabled && self.rtmp_push.max_reconnect_attempts == 0 {
            anyhow::bail!("rtmp_push.max_reconnect_attempts: must be > 0, got 0");
        }
        // Recording path
        if self.recording.path.trim().is_empty() {
            anyhow::bail!("recording.path: must not be empty");
        }
        // Recording segment duration
        if self.recording.segment_duration_secs == 0 {
            anyhow::bail!("recording.segment_duration_secs: must be > 0, got 0");
        }
        // Log level validation
        match self.observability.log_level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => anyhow::bail!(
                "observability.log_level: must be one of trace/debug/info/warn/error, got {}",
                other
            ),
        }
        // AI section
        if self.ai.enabled {
            if self.ai.model_path.trim().is_empty() {
                anyhow::bail!("ai.model_path: must not be empty when ai.enabled");
            }
            if self.ai.interval_ms == 0 {
                anyhow::bail!("ai.interval_ms: must be > 0, got 0");
            }
            if !(0.0..=1.0).contains(&self.ai.confidence_threshold) {
                anyhow::bail!(
                    "ai.confidence_threshold: must be within [0, 1], got {}",
                    self.ai.confidence_threshold
                );
            }
        }
        // Audio AI section
        if self.audio_ai.enabled {
            if !(0.01..=1.0).contains(&self.audio_ai.threshold) {
                anyhow::bail!(
                    "audio_ai.threshold: must be within (0, 1], got {}",
                    self.audio_ai.threshold
                );
            }
            if self.audio_ai.classes.is_empty() {
                anyhow::bail!("audio_ai.classes: must not be empty when audio_ai.enabled");
            }
            if self.audio_ai.cooldown_secs == 0 {
                anyhow::bail!("audio_ai.cooldown_secs: must be > 0, got 0");
            }
        }
        Ok(())
    }
}

/// Apply `scene.*` settings-bag rows (persisted by PUT /api/config,
/// SPEC appendix A #31) over the TOML-loaded config at boot — web edits
/// take precedence over file defaults. Unknown/malformed rows are
/// skipped with a warning rather than failing boot (the API validated
/// them on write; a hand-edited DB row must not brick startup).
pub fn overlay_scene_from_rows(config: &mut AppConfig, rows: &[(String, String)]) {
    for (key, value) in rows {
        let applied = match key.as_str() {
            "scene.voice.follow_up_window_secs" => value
                .parse::<f32>()
                .ok()
                .filter(|v| (0.0..=120.0).contains(v))
                .map(|v| config.voice.follow_up_window_secs = v),
            "scene.tools.weather_enabled" => value.parse::<bool>().ok().map(|v| {
                config.tools.weather_enabled = v;
            }),
            "scene.voice.wake_word" => {
                if streaming::voice::wake_word_to_keyword_line(value).is_ok() {
                    config.voice.wake_word = value.clone();
                }
                Some(())
            }
            "scene.tools.weather_city" => {
                config.tools.weather_city = value.clone();
                Some(())
            }
            "scene.tools.weather_timeout_secs" => value.parse::<u64>().ok().map(|v| {
                config.tools.timeout_secs = v;
            }),
            "scene.away.voice_control" => value
                .parse::<bool>()
                .ok()
                .map(|v| config.away.voice_control = v),
            "scene.away.summary_on_disarm" => value
                .parse::<bool>()
                .ok()
                .map(|v| config.away.summary_on_disarm = v),
            "scene.away.deterrence" => value
                .parse::<bool>()
                .ok()
                .map(|v| config.away.deterrence = v),
            "scene.away.deterrence_text" => {
                config.away.deterrence_text = value.clone();
                Some(())
            }
            "scene.away.reask_text" => {
                config.away.reask_text = value.clone();
                Some(())
            }
            "scene.away.schedule_arm" => {
                if web::away::valid_schedule_time(value) {
                    config.away.schedule_arm = value.clone();
                }
                Some(())
            }
            "scene.away.schedule_disarm" => {
                if web::away::valid_schedule_time(value) {
                    config.away.schedule_disarm = value.clone();
                }
                Some(())
            }
            "scene.away.schedule_weekdays_only" => value
                .parse::<bool>()
                .ok()
                .map(|v| config.away.schedule_weekdays_only = v),
            "scene.away.push_url" => {
                if value.is_empty() || value.starts_with("http://") || value.starts_with("https://")
                {
                    config.away.push_url = value.clone();
                }
                Some(())
            }
            "scene.away.audio_link" => value
                .parse::<bool>()
                .ok()
                .map(|v| config.away.audio_link = v),
            _ => None,
        };
        if applied.is_none() && key.starts_with("scene.") {
            tracing::warn!(key = %key, value = %value, "ignoring invalid scene overlay row");
        }
    }
}

/// Apply persisted `model.<capability>` selections (written by
/// `POST /api/models/{cap}/{id}/activate`, SPEC §4.9) over the TOML
/// config at boot. Unknown ids or malformed rows are skipped with a
/// warning — a hand-edited DB row must not brick startup.
pub fn overlay_models_from_rows(config: &mut AppConfig, rows: &[(String, String)]) {
    for (key, value) in rows {
        let Some(cap) = key.strip_prefix("model.") else {
            continue;
        };
        let Some(model) = streaming::models::find(cap, value) else {
            tracing::warn!(key = %key, value = %value, "ignoring unknown model selection row");
            continue;
        };
        apply_model_selection(config, cap, model);
    }
}

fn file_path(
    models_dir: &str,
    model: &streaming::models::CatalogModel,
    role: &str,
) -> Option<String> {
    let fl = model.files.iter().find(|f| f.role == role)?;
    let joined = format!("{}/{}/{}", models_dir, model.dir, fl.path);
    Some(joined.replace("/./", "/"))
}

fn dict_dir(models_dir: &str, model: &streaming::models::CatalogModel) -> String {
    // The dict role ships many files under one directory (jieba); the
    // engine wants that directory itself.
    model
        .files
        .iter()
        .find(|f| f.role == "dict")
        .and_then(|f| f.path.rsplit_once('/'))
        .map(|(dir, _)| format!("{}/{}/{}", models_dir, model.dir, dir))
        .unwrap_or_default()
}

fn rule_fsts(models_dir: &str, model: &streaming::models::CatalogModel) -> String {
    let fsts: Vec<String> = model
        .files
        .iter()
        .filter(|f| f.role == "fst")
        .map(|f| format!("{}/{}/{}", models_dir, model.dir, f.path))
        .collect();
    fsts.join(",")
}

fn apply_model_selection(
    config: &mut AppConfig,
    cap: &str,
    model: &streaming::models::CatalogModel,
) {
    let dir = config.models.dir.clone();
    match cap {
        "llm" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.llm.model_path = p;
            }
        }
        "vlm" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.vlm.model_path = p;
            }
            if let Some(p) = file_path(&dir, model, "mmproj") {
                config.vlm.mmproj_path = p;
            }
        }
        "voice.asr" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.voice.paraformer_model = p;
            }
            if let Some(p) = file_path(&dir, model, "tokens") {
                config.voice.paraformer_tokens = p;
            }
        }
        "tts.zh" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.tts.model = p;
            }
            if let Some(p) = file_path(&dir, model, "lexicon") {
                config.tts.lexicon = p;
            }
            if let Some(p) = file_path(&dir, model, "tokens") {
                config.tts.tokens = p;
            }
            let d = dict_dir(&dir, model);
            if !d.is_empty() {
                config.tts.dict_dir = d;
            }
            let fsts = rule_fsts(&dir, model);
            if !fsts.is_empty() {
                config.tts.rule_fsts = fsts;
            }
        }
        "tts.yue" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.tts.yue_model = p;
            }
            if let Some(p) = file_path(&dir, model, "lexicon") {
                config.tts.yue_lexicon = p;
            }
            config.tts.yue_dict_dir = dict_dir(&dir, model);
        }
        "tts.en" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.tts.en_model = p;
            }
            config.tts.en_lexicon = file_path(&dir, model, "lexicon").unwrap_or_default();
        }
        "face.detect" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.face.detect_model = p;
            }
        }
        "face.recog" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.face.recog_model = p;
            }
        }
        "ocr" => {
            if let Some(p) = file_path(&dir, model, "det") {
                config.ocr.det_path = p;
            }
            if let Some(p) = file_path(&dir, model, "rec") {
                config.ocr.rec_path = p;
            }
            if let Some(p) = file_path(&dir, model, "dict") {
                config.ocr.dict_path = p;
            }
        }
        "decision" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.decision.model_path = p;
            }
            if let Some(p) = file_path(&dir, model, "tokenizer") {
                config.decision.tokenizer_path = p;
            }
            if let Some(p) = file_path(&dir, model, "config") {
                config.decision.config_path = p;
            }
        }
        "speaker" => {
            if let Some(p) = file_path(&dir, model, "model") {
                config.voice.speaker_embedding_model = p;
            }
        }
        // The detection capability is immediate-class: the §4.6 registry
        // setting (ai.model) governs; no boot overlay needed.
        _ => {}
    }
}

/// The boot-time default selection per capability: which catalog model
/// the TOML engine paths currently point at (before any web selection).
/// Serves `GET /api/models` until a web activation persists a row.
pub fn default_model_selection(config: &AppConfig) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let dir = &config.models.dir;
    let configured: Vec<(&str, String)> = vec![
        ("llm", config.llm.model_path.clone()),
        ("vlm", config.vlm.model_path.clone()),
        ("voice.asr", config.voice.paraformer_model.clone()),
        ("tts.zh", config.tts.model.clone()),
        ("tts.yue", config.tts.yue_model.clone()),
        ("tts.en", config.tts.en_model.clone()),
        ("face.detect", config.face.detect_model.clone()),
        ("face.recog", config.face.recog_model.clone()),
        ("ocr", config.ocr.det_path.clone()),
        ("decision", config.decision.model_path.clone()),
        ("speaker", config.voice.speaker_embedding_model.clone()),
    ];
    // The engine field each capability's default selection is read from.
    let primary_role = |cap: &str| match cap {
        "ocr" => "det",
        _ => "model",
    };
    for cap in streaming::models::catalog() {
        // Detection rides the registry; nothing to path-match here.
        if cap.apply == "immediate" {
            continue;
        }
        let Some((_, primary)) = configured.iter().find(|(c, _)| *c == cap.id) else {
            continue;
        };
        let role = primary_role(cap.id);
        for m in cap.models {
            if let Some(p) = file_path(dir, m, role)
                && p == *primary
            {
                out.insert(cap.id.to_string(), m.id.to_string());
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_overlay_maps_every_capability_to_engine_paths() {
        let mut cfg: AppConfig = toml::from_str("").expect("empty config");
        overlay_models_from_rows(
            &mut cfg,
            &[
                ("model.llm".into(), "qwen3-1.7b-q4_k_m".into()),
                ("model.vlm".into(), "qwen3-vl-4b-instruct".into()),
                ("model.voice.asr".into(), "paraformer-zh-small".into()),
                ("model.tts.en".into(), "melo-en".into()),
                ("model.face.detect".into(), "yunet-2023mar".into()),
                ("model.decision".into(), "laya-multilingual-int8".into()),
                ("model.llm".into(), "garbage-id".into()), // unknown → skipped
                ("scene.voice.wake_word".into(), "小蜜蜂".into()), // not a model row
            ],
        );
        assert_eq!(cfg.llm.model_path, "models/llm/Qwen3-1.7B-Q4_K_M.gguf");
        assert_eq!(
            cfg.vlm.model_path,
            "models/vlm/Qwen3VL-4B-Instruct-Q4_K_M.gguf"
        );
        assert_eq!(
            cfg.vlm.mmproj_path,
            "models/vlm/mmproj-Qwen3VL-4B-Instruct-Q8_0.gguf"
        );
        assert_eq!(
            cfg.voice.paraformer_model,
            "models/voice/paraformer-zh-small/model.int8.onnx"
        );
        assert_eq!(
            cfg.voice.paraformer_tokens,
            "models/voice/paraformer-zh-small/tokens.txt"
        );
        // melo-en reuses the zh melo dir: dict + fsts included.
        assert_eq!(cfg.tts.en_model, "models/voice/melo/model.onnx");
        assert_eq!(cfg.tts.en_lexicon, "models/voice/melo/lexicon.txt");
        assert_eq!(
            cfg.face.detect_model,
            "models/face/face_detection_yunet_2023mar.onnx"
        );
        assert_eq!(
            cfg.decision.tokenizer_path,
            "models/decision/tokenizer.json"
        );
        assert_eq!(cfg.decision.config_path, "models/decision/laya_config.json");
    }

    #[test]
    fn default_selection_matches_toml_paths() {
        let cfg: AppConfig = toml::from_str(
            "[llm]\nmodel_path = \"models/llm/Qwen3-4B-Instruct-2507-Q4_K_M.gguf\"\n[face]\ndetect_model = \"models/face/face_detection_yunet_2023mar.onnx\"\n",
        )
        .expect("parse");
        let sel = default_model_selection(&cfg);
        assert_eq!(sel.get("llt").map(String::as_str), None);
        assert_eq!(
            sel.get("llm").map(String::as_str),
            Some("qwen3-4b-instruct-2507-q4_k_m")
        );
        assert_eq!(
            sel.get("face.detect").map(String::as_str),
            Some("yunet-2023mar")
        );
        // Built-in defaults resolve too (VlmConfig's stock 2B paths).
        assert_eq!(
            sel.get("vlm").map(String::as_str),
            Some("qwen3-vl-2b-instruct")
        );
        // Nothing configured for Cantonese TTS (empty path) → no default.
        assert!(!sel.contains_key("tts.yue"));
    }

    #[test]
    fn scene_overlay_applies_rows_and_skips_garbage() {
        let mut cfg: AppConfig = toml::from_str("").expect("empty config");
        assert_eq!(cfg.voice.follow_up_window_secs, 0.0);
        overlay_scene_from_rows(
            &mut cfg,
            &[
                ("scene.voice.follow_up_window_secs".into(), "12.5".into()),
                ("scene.voice.wake_word".into(), "你好小蜂".into()),
                ("scene.tools.weather_enabled".into(), "true".into()),
                ("scene.tools.weather_city".into(), "Guangzhou".into()),
                ("scene.tools.weather_timeout_secs".into(), "8".into()),
                ("scene.voice.follow_up_window_secs".into(), "999".into()), // out of range
                ("scene.tools.weather_timeout_secs".into(), "abc".into()),  // malformed
                ("ui.theme".into(), "dark".into()),                         // not a scene row
            ],
        );
        assert_eq!(cfg.voice.follow_up_window_secs, 12.5);
        assert_eq!(cfg.voice.wake_word, "你好小蜂");
        assert!(cfg.tools.weather_enabled);
        assert_eq!(cfg.tools.weather_city, "Guangzhou");
        assert_eq!(cfg.tools.timeout_secs, 8);
    }

    #[test]
    fn test_ai_section_parses_and_defaults() {
        let cfg: AppConfig = toml::from_str("").expect("empty config");
        assert!(!cfg.ai.enabled, "AI is opt-in");
        assert_eq!(cfg.ai.model_path, "models/nanodet-m.onnx");
        assert_eq!(cfg.ai.interval_ms, 1000);
        assert!(cfg.validate().is_ok());

        let cfg: AppConfig = toml::from_str(
            "[ai]\nenabled = true\nmodel_path = \"/opt/models/nanodet-m.onnx\"\ninterval_ms = 500\nconfidence_threshold = 0.4\n",
        )
        .expect("parse [ai]");
        assert!(cfg.ai.enabled);
        assert_eq!(cfg.ai.model_path, "/opt/models/nanodet-m.onnx");
        assert_eq!(cfg.ai.interval_ms, 500);
        assert!((cfg.ai.confidence_threshold - 0.4).abs() < f32::EPSILON);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_ai_section_rejects_bad_values() {
        let cfg: AppConfig =
            toml::from_str("[ai]\nenabled = true\nmodel_path = \"\"\n").expect("parse");
        assert!(cfg.validate().is_err());

        let cfg: AppConfig =
            toml::from_str("[ai]\nenabled = true\ninterval_ms = 0\n").expect("parse");
        assert!(cfg.validate().is_err());

        let cfg: AppConfig =
            toml::from_str("[ai]\nenabled = true\nconfidence_threshold = 1.5\n").expect("parse");
        assert!(cfg.validate().is_err());
    }

    // --- Individual default tests ---

    #[test]
    fn test_web_config_default() {
        let cfg = WebConfig::default();
        assert_eq!(cfg.port, 8443);
        assert_eq!(cfg.host, "0.0.0.0");
    }

    #[test]
    fn test_rtsp_config_default() {
        let cfg = RtspConfig::default();
        assert_eq!(cfg.server_port, 8554);
    }

    #[test]
    fn test_capture_config_default() {
        let cfg = CaptureConfig::default();
        assert_eq!(cfg.video_device, "/dev/video0");
        assert_eq!(cfg.audio_device, "default");
    }

    #[test]
    fn test_security_config_default() {
        let cfg = SecurityConfig::default();
        assert_eq!(cfg.rate_limit_max, 20);
        assert_eq!(cfg.rate_limit_window_secs, 60);
    }

    #[test]
    fn test_observability_config_default() {
        let cfg = ObservabilityConfig::default();
        assert_eq!(cfg.otel_endpoint, "http://localhost:4317");
        assert_eq!(cfg.log_level, "info");
    }

    // --- New protocol config defaults ---

    #[test]
    fn test_onvif_config_default() {
        let cfg = OnvifConfig::default();
        assert!(!cfg.enabled, "ONVIF must default to disabled");
        assert_eq!(cfg.device_name, "mibee-eye");
        assert_eq!(cfg.manufacturer, "MiBee");
        assert_eq!(cfg.model, "Rec-01");
        assert_eq!(cfg.serial, "NC00000001");
        assert_eq!(cfg.firmware_version, "1.0.0");
    }

    #[test]
    fn test_gb28181_config_default() {
        let cfg = Gb28181Config::default();
        assert!(!cfg.enabled, "GB28181 must default to disabled");
        assert_eq!(cfg.platform_sip_address, "192.168.1.100");
        assert_eq!(cfg.platform_sip_port, 5060);
        assert_eq!(cfg.device_id, "34020000002000000001");
        assert_eq!(cfg.username, "");
        assert_eq!(cfg.password, "");
        assert_eq!(cfg.sip_domain, "3402000000");
        assert_eq!(cfg.register_interval_secs, 60);
    }

    #[test]
    fn test_rtmp_push_config_default() {
        let cfg = RtmpPushConfig::default();
        assert!(!cfg.enabled, "RTMP push must default to disabled");
        assert_eq!(cfg.push_url, "rtmp://192.168.1.100:1935/live");
        assert_eq!(cfg.app_name, "live");
        assert_eq!(cfg.stream_name, "stream1");
        assert_eq!(cfg.reconnect_interval_secs, 5);
        assert_eq!(cfg.max_reconnect_attempts, 10);
    }

    // --- AppConfig default ---

    #[test]
    fn test_app_config_default_aggregates() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.web.port, 8443);
        assert_eq!(cfg.web.host, "0.0.0.0");
        assert_eq!(cfg.rtsp.server_port, 8554);
        assert_eq!(cfg.capture.video_device, "/dev/video0");
        assert_eq!(cfg.capture.audio_device, "default");
        assert_eq!(cfg.security.rate_limit_max, 20);
        assert_eq!(cfg.security.rate_limit_window_secs, 60);
        assert_eq!(cfg.observability.otel_endpoint, "http://localhost:4317");
        assert_eq!(cfg.observability.log_level, "info");
        assert!(!cfg.onvif.enabled);
        assert_eq!(cfg.onvif.device_name, "mibee-eye");
        assert!(!cfg.gb28181.enabled);
        assert_eq!(cfg.gb28181.platform_sip_address, "192.168.1.100");
        assert!(!cfg.rtmp_push.enabled);
        assert_eq!(cfg.rtmp_push.push_url, "rtmp://192.168.1.100:1935/live");
    }

    // --- Serde round-trip ---

    #[test]
    fn test_app_config_toml_roundtrip() {
        let cfg = AppConfig::default();
        let toml_str = toml::to_string(&cfg).expect("serialize to TOML");
        let back: AppConfig = toml::from_str(&toml_str).expect("deserialize from TOML");
        assert_eq!(back, cfg);
    }

    #[test]
    fn test_app_config_json_roundtrip() {
        let cfg = AppConfig::default();
        let json = serde_json::to_string(&cfg).expect("serialize to JSON");
        let back: AppConfig = serde_json::from_str(&json).expect("deserialize from JSON");
        assert_eq!(back, cfg);
    }

    // --- Partial config deserialization (fallback to defaults) ---

    #[test]
    fn test_app_config_partial_toml_uses_defaults() {
        let toml_str = "\
[web]
port = 9090
";
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.web.port, 9090);
        // host should fall back to default
        assert_eq!(cfg.web.host, "0.0.0.0");
        // rtsp should fall back to default
        assert_eq!(cfg.rtsp.server_port, 8554);
    }

    // --- AppConfig::load ---

    #[test]
    fn test_app_config_load_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_nb_cam_config.toml");
        // Clean up if left over from a previous run
        let _ = std::fs::remove_file(&path);

        let cfg = AppConfig {
            web: WebConfig {
                port: 9090,
                host: "127.0.0.1".into(),
                advertised_host: None,
                http_port: 0,
            },
            ..AppConfig::default()
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        std::fs::write(&path, &toml_str).unwrap();

        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded.web.port, 9090);
        assert_eq!(loaded.web.host, "127.0.0.1");
        // Non-overridden fields keep defaults
        assert_eq!(loaded.rtsp.server_port, 8554);
        assert_eq!(loaded.capture.video_device, "/dev/video0");

        // Cleanup
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_app_config_load_missing_file_returns_error() {
        let path = Path::new("/tmp/nonexistent_cfg_xyz123.toml");
        let result = AppConfig::load(path);
        assert!(result.is_err(), "Loading a nonexistent file must fail");
    }

    #[test]
    fn test_app_config_load_invalid_toml_returns_error() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_nb_cam_invalid.toml");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "[[[invalid toml").unwrap();
        let result = AppConfig::load(&path);
        assert!(result.is_err(), "Invalid TOML must fail");
        std::fs::remove_file(&path).ok();
    }

    // --- Edge case tests ---
    //
    // These verify that empty/missing sections fall back to defaults, that
    // unusual-but-valid values deserialize without panicking, and that
    // validation-adjacent edge cases are handled gracefully.

    #[test]
    fn test_empty_gb28181_section_uses_defaults() {
        let toml_str = "[gb28181]\n";
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!cfg.gb28181.enabled);
        assert_eq!(cfg.gb28181.platform_sip_address, "192.168.1.100");
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
        assert_eq!(cfg.gb28181.device_id, "34020000002000000001");
        assert_eq!(cfg.gb28181.register_interval_secs, 60);
    }

    #[test]
    fn test_empty_onvif_section_uses_defaults() {
        let toml_str = "[onvif]\n";
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!cfg.onvif.enabled);
        assert_eq!(cfg.onvif.device_name, "mibee-eye");
        assert_eq!(cfg.onvif.manufacturer, "MiBee");
        assert_eq!(cfg.onvif.model, "Rec-01");
        assert_eq!(cfg.onvif.firmware_version, "1.0.0");
    }

    #[test]
    fn test_empty_rtmp_push_section_uses_defaults() {
        let toml_str = "[rtmp_push]\n";
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!cfg.rtmp_push.enabled);
        assert_eq!(cfg.rtmp_push.push_url, "rtmp://192.168.1.100:1935/live");
        assert_eq!(cfg.rtmp_push.app_name, "live");
        assert_eq!(cfg.rtmp_push.stream_name, "stream1");
        assert_eq!(cfg.rtmp_push.reconnect_interval_secs, 5);
        assert_eq!(cfg.rtmp_push.max_reconnect_attempts, 10);
    }

    #[test]
    fn test_gb28181_device_id_empty() {
        let toml_str = r#"
[gb28181]
device_id = ""
"#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.gb28181.device_id, "");
        // Other fields keep defaults
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
        assert_eq!(cfg.gb28181.register_interval_secs, 60);
    }

    #[test]
    fn test_gb28181_device_id_short() {
        let toml_str = r#"
[gb28181]
device_id = "123"
"#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.gb28181.device_id, "123");
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
    }

    #[test]
    fn test_gb28181_device_id_19_chars() {
        let toml_str = r#"
[gb28181]
device_id = "3402000000200000000"
"#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.gb28181.device_id.len(), 19);
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
    }

    #[test]
    fn test_port_zero_edge_case() {
        let toml_str = r#"
[web]
port = 0
"#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.web.port, 0);
        // host falls back to default since not specified
        assert_eq!(cfg.web.host, "0.0.0.0");
    }

    #[test]
    fn test_rtmp_push_url_empty_keeps_other_defaults() {
        let toml_str = r#"
[rtmp_push]
push_url = ""
"#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.rtmp_push.push_url, "");
        // Other fields keep their own defaults
        assert_eq!(cfg.rtmp_push.app_name, "live");
        assert_eq!(cfg.rtmp_push.stream_name, "stream1");
        assert_eq!(cfg.rtmp_push.reconnect_interval_secs, 5);
        assert_eq!(cfg.rtmp_push.max_reconnect_attempts, 10);
    }

    #[test]
    fn test_all_empty_sections_use_defaults() {
        let toml_str = r#"
[web]
[rtsp]
[capture]
[security]
[observability]
[onvif]
[gb28181]
[rtmp_push]
"#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.web.port, 8443, "web.port");
        assert_eq!(cfg.web.host, "0.0.0.0", "web.host");
        assert_eq!(cfg.rtsp.server_port, 8554, "rtsp.server_port");
        assert_eq!(
            cfg.capture.video_device, "/dev/video0",
            "capture.video_device"
        );
        assert_eq!(cfg.capture.audio_device, "default", "capture.audio_device");
        assert_eq!(cfg.security.rate_limit_max, 20, "security.rate_limit_max");
        assert_eq!(
            cfg.security.rate_limit_window_secs, 60,
            "security.rate_limit_window_secs"
        );
        assert_eq!(
            cfg.observability.otel_endpoint, "http://localhost:4317",
            "observability.otel_endpoint"
        );
        assert_eq!(
            cfg.observability.log_level, "info",
            "observability.log_level"
        );
        assert!(!cfg.onvif.enabled);
        assert_eq!(cfg.onvif.device_name, "mibee-eye", "onvif.device_name");
        assert!(!cfg.gb28181.enabled);
        assert_eq!(
            cfg.gb28181.device_id, "34020000002000000001",
            "gb28181.device_id"
        );
        assert!(!cfg.rtmp_push.enabled);
        assert_eq!(
            cfg.rtmp_push.push_url, "rtmp://192.168.1.100:1935/live",
            "rtmp_push.push_url"
        );
    }

    #[test]
    fn test_all_protocols_config_loads() {
        let toml_str = r#"
[onvif]
enabled = true

[gb28181]
enabled = true

[rtmp_push]
enabled = false
"#;
        let cfg: AppConfig = toml::from_str(toml_str).expect("valid TOML with protocols enabled");
        assert!(cfg.onvif.enabled, "ONVIF must be enabled");
        assert!(cfg.gb28181.enabled, "GB28181 must be enabled");
        assert!(!cfg.rtmp_push.enabled, "RTMP push must be disabled");

        // Verify other fields load with defaults
        assert_eq!(cfg.onvif.device_name, "mibee-eye");
        assert_eq!(cfg.gb28181.platform_sip_address, "192.168.1.100");
        assert_eq!(cfg.gb28181.platform_sip_port, 5060);
        assert_eq!(cfg.rtmp_push.push_url, "rtmp://192.168.1.100:1935/live");

        // Verify no port conflicts between protocols
        // ONVIF WS-Discovery uses UDP 3702 (hardcoded in protocols/src/onvif.rs)
        // RTSP server uses config.server_port (default 8554)
        // Web UI uses config.port (default 8443)
        // GB28181 SIP uses config.platform_sip_port (default 5060)
        assert_ne!(
            3702u16, cfg.rtsp.server_port,
            "ONVIF port 3702 conflicts with RTSP"
        );
        assert_ne!(3702u16, cfg.web.port, "ONVIF port 3702 conflicts with Web");
        assert_ne!(
            cfg.rtsp.server_port, cfg.web.port,
            "RTSP port conflicts with Web"
        );
    }

    #[test]
    fn test_main_builds_with_all_protocols() {
        // Verify all protocol config types are constructable with enabled state
        // This ensures main.rs can build with protocol imports
        let onvif = OnvifConfig {
            enabled: true,
            ..OnvifConfig::default()
        };
        assert!(onvif.enabled);
        assert_eq!(onvif.device_name, "mibee-eye");

        let gb28181 = Gb28181Config {
            enabled: true,
            ..Gb28181Config::default()
        };
        assert!(gb28181.enabled);
        assert_eq!(gb28181.platform_sip_address, "192.168.1.100");

        let rtmp_push = RtmpPushConfig {
            enabled: false,
            ..RtmpPushConfig::default()
        };
        assert!(!rtmp_push.enabled);

        // Verify AppConfig can hold all protocol configs (compile check)
        let cfg = AppConfig {
            onvif: OnvifConfig {
                enabled: true,
                ..OnvifConfig::default()
            },
            gb28181: Gb28181Config {
                enabled: true,
                ..Gb28181Config::default()
            },
            rtmp_push: RtmpPushConfig {
                enabled: false,
                ..RtmpPushConfig::default()
            },
            ..AppConfig::default()
        };
        assert!(cfg.onvif.enabled);
        assert!(cfg.gb28181.enabled);
        assert!(!cfg.rtmp_push.enabled);
    }

    // --- Validation tests ---

    #[test]
    fn test_validate_valid_config_passes() {
        let cfg = AppConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_web_port_low_rejected() {
        let cfg = AppConfig {
            web: WebConfig {
                port: 80,
                host: "0.0.0.0".into(),
                advertised_host: None,
                http_port: 0,
            },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("web.port"),
            "error should mention web.port, got: {err}"
        );
    }

    #[test]
    fn test_validate_rtsp_port_low_rejected() {
        let cfg = AppConfig {
            rtsp: RtspConfig { server_port: 554 },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("rtsp.server_port"),
            "error should mention rtsp.server_port, got: {err}"
        );
    }

    #[test]
    fn test_validate_port_conflict_rejected() {
        let cfg = AppConfig {
            web: WebConfig {
                port: 8554,
                host: "0.0.0.0".into(),
                advertised_host: None,
                http_port: 0,
            },
            rtsp: RtspConfig { server_port: 8554 },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("web.port"),
            "error should mention port conflict, got: {err}"
        );
        assert!(
            err.contains("rtsp.server_port"),
            "error should mention rtsp.server_port, got: {err}"
        );
    }

    #[test]
    fn test_validate_rate_limit_zero_rejected() {
        let cfg = AppConfig {
            security: SecurityConfig {
                rate_limit_max: 0,
                rate_limit_window_secs: 60,
            },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("rate_limit_max"),
            "error should mention rate_limit_max, got: {err}"
        );
    }

    #[test]
    fn test_validate_gb28181_interval_zero_rejected() {
        let cfg = AppConfig {
            gb28181: Gb28181Config {
                enabled: true,
                register_interval_secs: 0,
                ..Gb28181Config::default()
            },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("register_interval_secs"),
            "error should mention register_interval_secs, got: {err}"
        );
    }

    #[test]
    fn test_validate_rtmp_reconnect_interval_zero_rejected() {
        let cfg = AppConfig {
            rtmp_push: RtmpPushConfig {
                enabled: true,
                reconnect_interval_secs: 0,
                ..RtmpPushConfig::default()
            },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("reconnect_interval_secs"),
            "error should mention reconnect_interval_secs, got: {err}"
        );
    }

    #[test]
    fn test_validate_rtmp_max_reconnect_zero_rejected() {
        let cfg = AppConfig {
            rtmp_push: RtmpPushConfig {
                enabled: true,
                max_reconnect_attempts: 0,
                ..RtmpPushConfig::default()
            },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("max_reconnect_attempts"),
            "error should mention max_reconnect_attempts, got: {err}"
        );
    }

    #[test]
    fn test_validate_gb28181_sip_port_low_rejected() {
        let cfg = AppConfig {
            gb28181: Gb28181Config {
                enabled: true,
                platform_sip_port: 506,
                ..Gb28181Config::default()
            },
            ..AppConfig::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("platform_sip_port"),
            "error should mention platform_sip_port, got: {err}"
        );
    }
}
