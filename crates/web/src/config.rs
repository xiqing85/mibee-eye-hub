//! Protocol configuration types owned by the web crate.
//!
//! These structs are used both for TOML config loading (via re-export from
//! the root crate's `config` module) and for JSON Schema generation in the
//! protocol config REST API (`routes/protocols.rs`). Keeping them here avoids
//! a circular dependency: the web crate needs the types for schema generation,
//! and the root crate needs them for `AppConfig`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// ONVIF Device
// ---------------------------------------------------------------------------

/// ONVIF device endpoint configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OnvifConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_onvif_device_name")]
    pub device_name: String,
    #[serde(default = "default_onvif_manufacturer")]
    pub manufacturer: String,
    #[serde(default = "default_onvif_model")]
    pub model: String,
    #[serde(default = "default_onvif_serial")]
    pub serial: String,
    #[serde(default = "default_onvif_firmware")]
    pub firmware_version: String,
    #[serde(default = "default_onvif_port")]
    pub port: u16,
    /// Expose the Pull-Point events service: AI motion alarms publish as
    /// `tns1:VideoSource/MotionAlarm` while an NVR holds a subscription.
    #[serde(default = "default_onvif_events_enabled")]
    pub events_enabled: bool,
    /// Serve the ver20 Media2 service alongside the legacy Media face
    /// (Profile-T entry path: `tr2` GetProfiles + its own
    /// `/onvif/media2_service` route). Requires an active stream at
    /// protocol start — like every Media field, restart-to-apply.
    #[serde(default = "default_onvif_media2_enabled")]
    pub media2_enabled: bool,
    /// Offer HTTP Digest transport auth (RFC 7616 MD5 subset) alongside
    /// WS-Security on the ONVIF SOAP listener — the Profile S route for
    /// non-TLS deployments. Default off (UsernameToken only).
    #[serde(default)]
    pub http_digest: bool,
    /// IPv4 allow-list for the ONVIF SOAP listener, as `a.b.c.d[/prefix]`
    /// strings (prefix 0–32, default 32 for bare addresses). Empty (the
    /// default) disables filtering entirely; when set, only listed
    /// networks may connect (peers are refused 403 before any SOAP
    /// processing). Restart-to-apply.
    #[serde(default)]
    pub ip_filter: Vec<String>,
}

fn default_onvif_events_enabled() -> bool {
    true
}

fn default_onvif_media2_enabled() -> bool {
    true
}

fn default_onvif_device_name() -> String {
    "mibee-eye".into()
}
fn default_onvif_manufacturer() -> String {
    "MiBee".into()
}
fn default_onvif_model() -> String {
    "Rec-01".into()
}
fn default_onvif_serial() -> String {
    "NC00000001".into()
}
fn default_onvif_firmware() -> String {
    "1.0.0".into()
}
/// Default ONVIF device service port.
///
/// This is the SOAP/HTTP port advertised in XAddrs. It was 3702, which is the
/// **WS-Discovery** port — the value every device got in its config document
/// while the service actually served 8080, because the runtime builder
/// hardcoded `ONVIF_HTTP_PORT` and never read this field. So the key was a
/// no-op and its default named the wrong protocol's port. Both are fixed
/// together; see `build_onvif_config_from_json` for the legacy-value mapping
/// that keeps existing deployments on 8080.
fn default_onvif_port() -> u16 {
    8080
}

impl Default for OnvifConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device_name: "mibee-eye".into(),
            manufacturer: "MiBee".into(),
            model: "Rec-01".into(),
            serial: "NC00000001".into(),
            firmware_version: "1.0.0".into(),
            // Must agree with `default_onvif_port()` and with the port the
            // runtime binds. These were three separate literals and only one
            // of them was real; `the_config_default_is_the_port_actually_served`
            // now fails if they drift apart again.
            port: default_onvif_port(),
            events_enabled: true,
            media2_enabled: true,
            http_digest: false,
            ip_filter: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// GB/T 28181 Device
// ---------------------------------------------------------------------------

/// GB28181 device registration configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Gb28181Config {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_gb28181_sip_address")]
    pub platform_sip_address: String,
    #[serde(default = "default_gb28181_sip_port")]
    pub platform_sip_port: u16,
    #[serde(default = "default_gb28181_device_id")]
    pub device_id: String,
    #[serde(default = "default_gb28181_username")]
    pub username: String,
    #[serde(default = "default_gb28181_password")]
    pub password: String,
    #[serde(default = "default_gb28181_sip_domain")]
    pub sip_domain: String,
    #[serde(default = "default_gb28181_register_interval")]
    pub register_interval_secs: u64,
    #[serde(default = "default_gb28181_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_gb28181_heartbeat_timeout")]
    pub heartbeat_timeout_count: u32,
    #[serde(default = "default_gb28181_channel_id")]
    pub channel_id: String,
    #[serde(default = "default_gb28181_local_sip_port")]
    pub local_sip_port: u16,
    /// Play platform-initiated voice talkback (audio-only INVITE,
    /// GB/T 28181-2022 §9.2) on the local output device. Fail-open: with
    /// this off, or with no usable output device, talkback INVITEs are
    /// refused with 488.
    #[serde(default = "default_gb28181_talkback_playback")]
    pub talkback_playback: bool,
    #[serde(default)]
    pub gb35114: Gb35114Config,
}

/// GB 35114 A-level (SM2 certificate) REGISTER authentication settings.
/// Certificate paths are read by the device at protocol start; enabling
/// with missing/invalid files fails closed (GB28181 refuses to start).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Gb35114Config {
    #[serde(default)]
    pub enabled: bool,
    /// Path to the device certificate (PEM, SM2).
    #[serde(default)]
    pub device_cert_file: String,
    /// Path to the device private key (PEM, SM2).
    #[serde(default)]
    pub device_key_file: String,
    /// Path to the platform certificate (PEM, SM2) used to verify the platform.
    #[serde(default)]
    pub platform_cert_file: String,
    /// Platform server ID (20-digit) for the signed REGISTER exchange.
    #[serde(default = "default_gb35114_server_id")]
    pub server_id: String,
}

fn default_gb35114_server_id() -> String {
    "34020000002000000001".into()
}

fn default_gb28181_sip_address() -> String {
    "192.168.1.100".into()
}
fn default_gb28181_sip_port() -> u16 {
    5060
}
fn default_gb28181_device_id() -> String {
    "34020000002000000001".into()
}
fn default_gb28181_username() -> String {
    String::new()
}
fn default_gb28181_password() -> String {
    String::new()
}
fn default_gb28181_sip_domain() -> String {
    "3402000000".into()
}
fn default_gb28181_register_interval() -> u64 {
    60
}
fn default_gb28181_heartbeat_interval() -> u64 {
    60
}
fn default_gb28181_heartbeat_timeout() -> u32 {
    3
}
fn default_gb28181_channel_id() -> String {
    "34020000001320000001".into()
}
fn default_gb28181_local_sip_port() -> u16 {
    5060
}
fn default_gb28181_talkback_playback() -> bool {
    true
}

impl Default for Gb28181Config {
    fn default() -> Self {
        Self {
            enabled: false,
            platform_sip_address: "192.168.1.100".into(),
            platform_sip_port: 5060,
            device_id: "34020000002000000001".into(),
            username: String::new(),
            password: String::new(),
            sip_domain: "3402000000".into(),
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            channel_id: default_gb28181_channel_id(),
            local_sip_port: 5060,
            talkback_playback: default_gb28181_talkback_playback(),
            gb35114: Gb35114Config::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// RTMP Push Client
// ---------------------------------------------------------------------------

/// RTMP push client configuration (pushes local stream to external ingest).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RtmpPushConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_rtmp_push_url")]
    pub push_url: String,
    #[serde(default = "default_rtmp_app_name")]
    pub app_name: String,
    #[serde(default = "default_rtmp_stream_name")]
    pub stream_name: String,
    #[serde(default = "default_rtmp_reconnect_interval")]
    pub reconnect_interval_secs: u64,
    #[serde(default = "default_rtmp_max_reconnect")]
    pub max_reconnect_attempts: u32,
}

fn default_rtmp_push_url() -> String {
    "rtmp://192.168.1.100:1935/live".into()
}
fn default_rtmp_app_name() -> String {
    "live".into()
}
fn default_rtmp_stream_name() -> String {
    "stream1".into()
}
fn default_rtmp_reconnect_interval() -> u64 {
    5
}
fn default_rtmp_max_reconnect() -> u32 {
    10
}

impl Default for RtmpPushConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            push_url: "rtmp://192.168.1.100:1935/live".into(),
            app_name: "live".into(),
            stream_name: "stream1".into(),
            reconnect_interval_secs: 5,
            max_reconnect_attempts: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Local recording configuration.
///
/// When enabled, captured H.264 frames are muxed into rolling MP4 segments
/// in `path`. Oldest segments are auto-pruned when total size exceeds
/// `max_capacity_mb`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordingConfig {
    /// Master enable toggle. Individual streams can opt out via Web UI.
    #[serde(default)]
    pub enabled: bool,

    /// Directory where MP4 segment files are written. Must be writable.
    #[serde(default = "default_recording_path")]
    pub path: String,

    /// MP4 segment duration in seconds. Each file covers this much video.
    #[serde(default = "default_segment_duration_secs")]
    pub segment_duration_secs: u64,

    /// Max total capacity in megabytes. 0 = unlimited (no pruning).
    #[serde(default = "default_max_capacity_mb")]
    pub max_capacity_mb: u64,
}

fn default_recording_path() -> String {
    "./recordings".into()
}

fn default_segment_duration_secs() -> u64 {
    900 // 15 minutes
}

fn default_max_capacity_mb() -> u64 {
    10_240 // 10 GB
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_recording_path(),
            segment_duration_secs: default_segment_duration_secs(),
            max_capacity_mb: default_max_capacity_mb(),
        }
    }
}

// ---------------------------------------------------------------------------
// WebRTC
// ---------------------------------------------------------------------------

/// WebRTC streaming configuration.
///
/// When enabled, exposes WHIP/WHEP (ingest/egress) endpoints so browsers can
/// play a camera stream via RTCPeerConnection for sub-second latency. Disabled
/// by default — the MSE/fMP4 path (1-3s latency, no signalling required) is
/// the default transport; WebRTC is for cases that need the lowest possible
/// latency (e.g. live monitoring, future talk-back audio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct WebRtcConfig {
    /// Master enable toggle.
    #[serde(default)]
    pub enabled: bool,

    /// UDP port range start for ICE candidate gathering (host candidates).
    /// The server uses a small ephemeral range; 0 = let the OS choose.
    #[serde(default)]
    pub udp_port_min: u16,

    /// UDP port range end (inclusive).
    #[serde(default = "default_webrtc_udp_port_max")]
    pub udp_port_max: u16,

    /// Comma-separated list of STUN server URLs (e.g.
    /// `stun:stun.l.google.com:19302`). Empty = host candidates only (LAN).
    #[serde(default)]
    pub stun_servers: String,
}

fn default_webrtc_udp_port_max() -> u16 {
    0
}

// ---------------------------------------------------------------------------
// Watermark (SPEC v1 §5.2)
// ---------------------------------------------------------------------------

pub use streaming::watermark::Position as WatermarkPosition;
/// Video watermark configuration (`protocols.watermark`, SPEC v1 §5.2).
///
/// Defined in the streaming crate (single source of truth — the JsonSchema
/// derive drives the Web API's field/type/range/enum validation). Burned
/// into the pre-encode YUV frames of every camera stream like the
/// device-level flips — RTSP, MSE, recordings and snapshots all see it.
/// Read at stream start (like `protocols.recording`); a config change takes
/// effect on the next camera (re)start.
pub use streaming::watermark::WatermarkSettings as WatermarkConfig;

/// SPEC §5.2 semantic validation over a **merged** `protocols.watermark`
/// blob (schema validation covers types/ranges/enums; this covers
/// semantics): `enabled` requires something to render, and the timestamp
/// format must be in the strftime whitelist.
pub fn validate_watermark_blob(blob: &serde_json::Value) -> Result<(), String> {
    let cfg: WatermarkConfig =
        serde_json::from_value(blob.clone()).map_err(|e| format!("watermark: {e}"))?;
    if cfg.enabled && cfg.text.trim().is_empty() && !cfg.show_timestamp {
        return Err("watermark: enabled requires text or show_timestamp".into());
    }
    if !streaming::watermark::valid_timestamp_format(&cfg.timestamp_format) {
        return Err(format!(
            "watermark.timestamp_format only allows %Y %m %d %H %M %S %F %T %% and literals, got: {}",
            cfg.timestamp_format
        ));
    }
    Ok(())
}
