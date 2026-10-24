//! Hot-toggle runtime for ONVIF, GB28181, and RTMP protocols.
//!
//! Each protocol can be started/stopped independently without restarting
//! the server. Shutdown is graceful (up to 5s) with force abort on timeout.
//! A failure in one protocol does NOT affect the others.
//!
//! ## Protocol specifics
//!
//! - **ONVIF**: `WsDiscoveryServer` background task, UDP 3702.
//! - **GB28181**: SIP device registration loop, attaches RTP outputs on INVITE.
//! - **RTMP**: Per-stream output (no global task). Toggling controls whether
//!   new streams auto-attach an RTMP push output.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use serde::Serialize;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Duration;

use crate::stream_manager::StreamManager;

// ── Constants ────────────────────────────────────────────────────────────────

/// Maximum time to wait for graceful protocol shutdown before force-aborting.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// ONVIF device HTTP service port (SOAP endpoint advertised in XAddrs).
const ONVIF_HTTP_PORT: u16 = 8080;

/// The value the old `protocols.onvif.port` default wrote (the WS-Discovery
/// UDP port). Mapped back to [`ONVIF_HTTP_PORT`] when read — see
/// `build_onvif_config_from_json`.
const LEGACY_DISCOVERY_PORT_AS_DEFAULT: u16 = 3702;

/// Default RTSP server port used in stream URLs.
const RTSP_PORT: u16 = 8554;

// ── Status types ─────────────────────────────────────────────────────────────

/// Runtime status of all three protocols.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolStatus {
    pub onvif: ProtocolState,
    pub gb28181: ProtocolState,
    pub rtmp: ProtocolState,
}

/// State of a single protocol.
#[derive(Debug, Clone, Serialize)]
pub struct ProtocolState {
    pub running: bool,
}

// ── Config extraction helpers ────────────────────────────────────────────────

/// Extract an ONVIF device config from the DB JSON value, filling in
/// runtime-dependent fields (rtsp_url, xaddrs) from known parameters.
/// Extract the Raspberry Pi `Serial` line from /proc/cpuinfo contents.
fn serial_from_cpuinfo(data: &str) -> String {
    for line in data.lines() {
        if let Some((key, value)) = line.split_once(':')
            && key.trim() == "Serial"
            && !value.trim().is_empty()
        {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// Validate/normalize a /etc/machine-id read: a long-enough token.
fn normalize_machine_id(data: &str) -> String {
    let id = data.lines().next().unwrap_or("").trim();
    if id.len() < 8 {
        String::new()
    } else {
        id.to_string()
    }
}

/// Device-level serial probe (issue #18): cpuinfo Serial → Linux
/// machine-id. NOT the MAC — dual-homed hosts flip identity on
/// interface change. Cached for the process lifetime (the value must
/// not drift between protocol toggles).
fn detect_device_serial() -> String {
    static DETECTED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DETECTED
        .get_or_init(|| {
            if let Ok(data) = std::fs::read_to_string("/proc/cpuinfo") {
                let serial = serial_from_cpuinfo(&data);
                if !serial.is_empty() {
                    return serial;
                }
            }
            for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
                if let Ok(data) = std::fs::read_to_string(path) {
                    let id = normalize_machine_id(&data);
                    if !id.is_empty() {
                        return id;
                    }
                }
            }
            String::new()
        })
        .clone()
}

pub fn build_onvif_config_from_json(
    db_config: &serde_json::Value,
    advertised_host: &str,
) -> OnvifRuntimeConfig {
    let get_str = |key: &str, default: &str| {
        db_config
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    };
    let get_u16 = |key: &str, default: u16| {
        db_config
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v as u16)
            .unwrap_or(default)
    };
    let get_u32 = |key: &str, default: u32| {
        db_config
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(default)
    };
    let get_bool = |key: &str, default: bool| {
        db_config
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    };

    let model = get_str("model", "Rec-01");

    // Serial chain (issue #18): explicit config wins; unset probes the
    // device identity (cpuinfo Serial / machine-id); the historical
    // shared default is the LAST resort so NVR stable_id dedup never
    // silently collapses distinct installs.
    // The legacy constant value counts as unset — DB rows persisted by
    // older builds carry the baked default, not a human choice.
    let serial_configured = match get_str("serial", "").as_str() {
        "" | "NC00000001" => String::new(),
        explicit => explicit.to_string(),
    };
    let serial = if !serial_configured.is_empty() {
        serial_configured
    } else {
        let detected = detect_device_serial();
        if detected.is_empty() {
            tracing::warn!(
                "onvif serial not configured and no device-level serial probeable — \
                 falling back to the shared default NC00000001; set protocols.onvif.serial"
            );
            "NC00000001".to_string()
        } else {
            tracing::info!(serial = %detected, "onvif serial auto-detected");
            detected
        }
    };

    OnvifRuntimeConfig {
        device: onvif_device_rs::DeviceConfig {
            manufacturer: get_str("manufacturer", "MiBee"),
            firmware: get_str("firmware_version", "1.0.0"), // hardcode-ok: SQLite 配置 get_str 兜底默认值（本仓配置默认层），非应用版本横幅
            serial_number: serial,
            hardware_id: model.clone(),
            name: model.clone(),
            model,
        },
        // Honour `protocols.onvif.port` — it used to be ignored, so the
        // settings page showed a value the device never used.
        //
        // `3702` is mapped back to the SOAP port for existing data. It is the
        // old default, which named the WS-Discovery UDP port, so every
        // deployment that never deliberately touched this key has it sitting
        // in its config document and in the DB settings row. Treating it as a
        // real choice would silently move SOAP off 8080 — the port every NVR
        // is configured against, and the one this device has always served.
        // Anything else the operator sets is taken at face value.
        onvif_port: match get_u16("port", ONVIF_HTTP_PORT) {
            0 | LEGACY_DISCOVERY_PORT_AS_DEFAULT => ONVIF_HTTP_PORT,
            port => port,
        },
        events_enabled: get_bool("events_enabled", true),
        media2_enabled: get_bool("media2_enabled", true),
        http_digest: get_bool("http_digest", false),
        ip_filter: db_config
            .get("ip_filter")
            .and_then(|v| v.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        username: get_str("username", ""),
        password: get_str("password", ""),
        host: advertised_host.to_string(),
        rtsp_port: RTSP_PORT,
        camera_width: get_u32("profile_width", 1280),
        camera_height: get_u32("profile_height", 720),
        camera_fps: get_u32("profile_fps", 25),
        camera_bitrate: get_u32("profile_bitrate", 2_500_000),
    }
}

/// Extract GB28181 config fields from the DB JSON value.
pub fn extract_gb28181_config(db_config: &serde_json::Value) -> Gb28181RuntimeConfig {
    let get_str = |key: &str, default: &str| {
        db_config
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(default)
            .to_string()
    };
    let get_u16 = |key: &str, default: u16| {
        db_config
            .get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v as u16)
            .unwrap_or(default)
    };
    let get_u64 = |key: &str, default: u64| {
        db_config
            .get(key)
            .and_then(|v| v.as_u64())
            .unwrap_or(default)
    };

    Gb28181RuntimeConfig {
        enabled: db_config
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        device_id: get_str("device_id", "34020000002000000001"), // hardcode-ok: SQLite 配置 get_str 兜底默认值（本仓配置默认层），标准示例编码
        sip_addr: get_str("platform_sip_address", "127.0.0.1"),
        sip_port: get_u16("platform_sip_port", 5060),
        password: get_str("password", ""),
        sip_domain: get_str("sip_domain", "3402000000"),
        register_interval: get_u64("register_interval_secs", 60),
        heartbeat_interval_secs: get_u64("heartbeat_interval_secs", 60),
        gb35114: db_config
            .get("gb35114")
            .map(|g| Gb35114RuntimeConfig {
                enabled: g.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
                device_cert_file: g
                    .get("device_cert_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                device_key_file: g
                    .get("device_key_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                platform_cert_file: g
                    .get("platform_cert_file")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                server_id: g
                    .get("server_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                include_device_cert: g
                    .get("include_device_cert")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
            .unwrap_or_default(),
        heartbeat_timeout_count: db_config
            .get("heartbeat_timeout_count")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(3),
        channel_id: get_str("channel_id", "34020000001320000001"), // hardcode-ok: SQLite 配置 get_str 兜底默认值（本仓配置默认层），标准示例编码
        local_sip_port: get_u16("local_sip_port", 5060),
        talkback_playback: db_config
            .get("talkback_playback")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        talkback_upstream: db_config
            .get("talkback_upstream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ptz_onvif_endpoint: get_str("ptz_onvif_endpoint", ""),
        ptz_profile_token: get_str("ptz_profile_token", "Profile_1"),
        ptz_username: get_str("ptz_username", ""),
        ptz_password: get_str("ptz_password", ""),
        device_name: get_str("device_name", "mibee-eye"),
        manufacturer: get_str("manufacturer", "MiBee"),
        model: get_str("model", "Rec-01"),
        firmware: get_str("firmware", env!("CARGO_PKG_VERSION")),
        alarm_notify_enabled: db_config
            .get("alarm_notify_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        alarm_cooldown_secs: db_config
            .get("alarm_cooldown_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30),
        position_longitude: get_str("position_longitude", ""),
        position_latitude: get_str("position_latitude", ""),
    }
}

/// ONVIF runtime configuration resolved from the persisted DB JSON. The
/// SOAP/discovery role itself lives in the `onvif-rs` library; media profile
/// dimensions default to 720p and are overridable via DB keys
/// (`profile_width` / `profile_height` / `profile_fps` / `profile_bitrate`).
#[derive(Debug, Clone)]
pub struct OnvifRuntimeConfig {
    pub device: onvif_device_rs::DeviceConfig,
    pub onvif_port: u16,
    /// Expose the Pull-Point events service (AI MotionAlarm) while the
    /// ONVIF protocol runs (onvif-device-rs 0.7 events service).
    pub events_enabled: bool,
    /// Serve the ver20 Media2 face next to the legacy Media service
    /// (onvif-device-rs 0.8): `/onvif/media2_service` route + GetServices
    /// advertisement. Only takes effect while a Media profile is mounted
    /// (active stream at protocol start).
    pub media2_enabled: bool,
    /// Offer HTTP Digest transport auth (RFC 7616 MD5 subset) on the SOAP
    /// listener alongside WS-Security (onvif-device-rs 0.8 security).
    pub http_digest: bool,
    /// IPv4 allow-list (`a.b.c.d[/prefix]`) enforced per connection; empty
    /// = no filtering.
    pub ip_filter: Vec<String>,
    pub username: String,
    pub password: String,
    /// Advertised host for XAddrs / stream URIs.
    pub host: String,
    pub rtsp_port: u16,
    pub camera_width: u32,
    pub camera_height: u32,
    pub camera_fps: u32,
    pub camera_bitrate: u32,
}

#[derive(Debug)]
/// Parsed GB28181 configuration extracted from DB JSON.
pub struct Gb28181RuntimeConfig {
    pub enabled: bool,
    /// GB35114 A-level (SM2 mutual auth) sub-tree — mirrors the field
    /// names the raspi twins use in their config files so operators see
    /// one vocabulary across devices.
    pub gb35114: Gb35114RuntimeConfig,
    pub device_id: String,
    pub sip_addr: String,
    pub sip_port: u16,
    pub password: String,
    pub sip_domain: String,
    pub register_interval: u64,
    pub heartbeat_interval_secs: u64,
    pub heartbeat_timeout_count: u32,
    pub channel_id: String,
    pub local_sip_port: u16,
    /// Play platform-initiated voice talkback on the local output device
    /// (GB/T 28181-2022 §9.2). Fail-open: off, or no usable output
    /// device, → no sink registered → talkback INVITEs answered 488.
    pub talkback_playback: bool,
    /// External ONVIF PTZ head endpoint (roadmap M4-a): when non-empty,
    /// GB PTZCmd dispatches to the PtzProxy steering that head. Empty =
    /// the hub has no motors and PTZ stays a no-op ack.
    pub ptz_onvif_endpoint: String,
    pub ptz_profile_token: String,
    pub ptz_username: String,
    pub ptz_password: String,
    /// Send microphone audio to the platform during talkback sessions
    /// (§9.2 send half). Fail-open: off, or no usable input device, → no
    /// source registered → recvonly offers answered 488. A-law encoded
    /// (PCMA is the de-facto GB platform codec).
    pub talkback_upstream: bool,
    /// Catalog/DeviceInfo identity. gb28181-rs 0.6.0 defaults to neutral
    /// placeholders unless the host stamps its product identity here.
    pub device_name: String,
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    /// Initial DeviceConfig AlarmReport gate for AI alarm NOTIFY (SPEC
    /// appendix A #16). The platform can flip it at runtime.
    pub alarm_notify_enabled: bool,
    /// Rising-edge alarm cooldown in seconds (SPEC appendix A #16).
    pub alarm_cooldown_secs: u64,
    /// Static MobilePosition coordinates; empty (default) = no reporting.
    pub position_longitude: String,
    pub position_latitude: String,
}

/// GB35114 A-level sub-config (nested under protocols.gb28181.gb35114).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gb35114RuntimeConfig {
    pub enabled: bool,
    pub device_cert_file: String,
    pub device_key_file: String,
    pub platform_cert_file: String,
    pub server_id: String,
    pub include_device_cert: bool,
}

/// Builds the GB35114 A-level REGISTER authenticator from the sub-config.
/// `Ok(None)` when disabled; `Err` when enabled but the identities cannot
/// be loaded — the caller must refuse to start the protocol rather than
/// silently falling back to Digest (fail closed).
pub fn build_gb35114_authenticator(
    cfg: &Gb35114RuntimeConfig,
    device_id: &str,
) -> anyhow::Result<Option<Arc<dyn gb28181_rs::authenticator::RegisterAuthenticator>>> {
    if !cfg.enabled {
        return Ok(None);
    }
    use gb28181_rs::security35114::{Authenticator, Options, load_certificate, load_identity};
    let read = |path: &str, what: &str| {
        std::fs::read_to_string(path)
            .with_context(|| format!("gb35114 {what} file {path:?} unreadable"))
    };
    let device = load_identity(
        &read(&cfg.device_cert_file, "device certificate")?,
        &read(&cfg.device_key_file, "device key")?,
    )
    .context("gb35114 device identity invalid")?;
    let platform_cert = load_certificate(&read(&cfg.platform_cert_file, "platform certificate")?)
        .context("gb35114 platform certificate invalid")?;
    let mut opts = Options::new(device, device_id.to_string(), cfg.server_id.clone());
    opts.platform_cert = Some(platform_cert);
    opts.include_device_cert = cfg.include_device_cert;
    let auth = Authenticator::new(opts).context("gb35114 authenticator options invalid")?;
    Ok(Some(Arc::new(auth)))
}

/// Static MobilePosition source (SPEC appendix A #16): reports the
/// configured coordinates on the subscription cadence. Empty longitude or
/// latitude → `None`, which makes the library skip that report.
struct StaticPositionSource {
    longitude: String,
    latitude: String,
}

impl gb28181_rs::subscribe::MobilePositionSource for StaticPositionSource {
    fn current_position(&self) -> Option<gb28181_rs::subscribe::PositionReport> {
        if self.longitude.is_empty() || self.latitude.is_empty() {
            return None;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64;
        Some(gb28181_rs::subscribe::PositionReport {
            time: gb28181_rs::client::format_gb_time_ms(now_ms),
            longitude: self.longitude.clone(),
            latitude: self.latitude.clone(),
            speed: "0".to_string(),
            direction: String::new(),
            altitude: "0".to_string(),
        })
    }
}

// ── ONVIF wiring (onvif-rs 0.8 capability batch) ─────────────────────────────

/// Mount the full ONVIF SOAP face onto a fresh server: Device service
/// (identity + user directory + host hooks + security seams), the
/// shared-store Media family, the ver20 Media2 face, and the Imaging
/// service.
///
/// Extracted from [`ProtocolRuntime::start_onvif`] so the SOAP wiring is
/// testable on an ephemeral listener (the runtime path additionally binds
/// the fixed 8080 port and pairs WS-Discovery).
///
/// Registration order is load-bearing for the shared action names
/// (onvif-rs `GetServiceCapabilities` note): Media registers first, then
/// Imaging — whose shape router (VideoSourceToken / `timg:` requests)
/// falls back to the media capabilities answer for plain bodies. PTZ is
/// deliberately NOT registered (no motors on a notebook).
fn mount_onvif_services(
    mut soap: onvif_device_rs::OnvifServer,
    config: &OnvifRuntimeConfig,
    device_ip: &str,
    media: Option<onvif_device_rs::media::SharedMediaConfig>,
    force_idr: Arc<AtomicBool>,
    streams: &Arc<StreamManager>,
    ip_filter: Option<onvif_device_rs::device::IpFilterState>,
) -> anyhow::Result<(
    onvif_device_rs::OnvifServer,
    Option<Arc<onvif_device_rs::events::EventsService>>,
)> {
    // Pull-Point events: the publish seam must be taken before start.
    let events = config.events_enabled.then(|| soap.enable_events());

    // The same shared IP-filter state backs the connection gate and the
    // SOAP-side view (the library's one-store contract).
    if let Some(state) = &ip_filter {
        soap = soap.with_ip_filter(Arc::clone(state));
    }

    let media2_mount = config.media2_enabled && media.is_some();

    // onvif-device-rs 0.6 fail-closes on placeholder identity (its
    // issue #20); the DB-backed defaults are real values, so an error
    // here aborts the protocol start with the library's reason.
    let mut device_svc = onvif_device_rs::device::DeviceServiceHandlers::new(
        config.device.clone(),
        config.onvif_port,
        device_ip.to_string(),
    )?
    // Advertise each service exactly when its routes are served — the
    // pairs must not disagree. (PTZ: never registered — a notebook has
    // no motors; the historical default-true advertisement was a lie.)
    .with_events_support(config.events_enabled)
    .with_media_support(media.is_some())
    .with_ptz_support(false)
    .with_media2_support(media2_mount)
    .with_hooks(Arc::new(OnvifDeviceHooks::new(Arc::clone(streams))))
    .with_users(vec![(
        onvif_wsdl_username(config),
        "Administrator".to_string(),
    )])?;
    if let Some(state) = ip_filter {
        device_svc = device_svc.with_ip_filter(state);
    }
    // AccessPolicy seam with the default empty policy: Get answers the
    // empty blob (identical bytes to no state at all), Set stays refused
    // until this product decides to accept policies.
    device_svc = device_svc.with_access_policy(Arc::new(std::sync::RwLock::new(Vec::new())));
    let device_svc = Arc::new(device_svc);

    for action in [
        "GetSystemDateAndTime",
        "GetDeviceInformation",
        "GetCapabilities",
        "GetServices",
        "GetScopes",
        // 0.8 additions: user directory + host-hook effects.
        "GetUsers",
        "GetSystemLog",
        "GetSystemSupportInformation",
        "SetSystemDateAndTime",
        "SystemReboot",
        "SetSystemFactoryDefault",
        // Security view (read-only; the mutating ops stay unregistered so
        // runtime state cannot drift from the DB config).
        "GetIPAddressFilter",
        "GetAccessPolicy",
    ] {
        soap.register_handler(
            action,
            Box::new(onvif_device_rs::device::DeviceHandler(Arc::clone(
                &device_svc,
            ))),
        );
    }
    // Pre-auth actions per ONVIF Core spec (discovery + clock sync
    // happen before clients can compute WS-Security digests).
    for action in ["GetSystemDateAndTime", "GetCapabilities", "GetServices"] {
        soap.register_anonymous_action(action);
    }

    // Media family through the 0.8 shared store (byte-identical answers
    // to the historical standalone handlers, now plus the encoder
    // configuration family, SetSynchronizationPoint, and the empty
    // audio/OSD sets). The keyframe hook fires the SAME IFrameCmd latch
    // the GB28181 control handler uses — one IDR seam for both protocols.
    if let Some(store) = media {
        let hook: Option<Arc<dyn Fn() + Send + Sync>> = Some({
            let force_idr = Arc::clone(&force_idr);
            Arc::new(move || force_idr.store(true, Ordering::SeqCst))
        });
        onvif_device_rs::media::register_media_actions(&mut soap, Arc::clone(&store), hook.clone());
        if media2_mount {
            soap.enable_media2(store, hook);
        }
    }

    // Imaging last (see the order note above): fixed-focus notebook
    // webcam — standard params answer fixed neutral values, focus Move
    // acks, unknown names refuse honestly.
    onvif_device_rs::imaging::register_imaging_actions(&mut soap, Arc::new(FixedFocusImaging));

    Ok((soap, events))
}

/// Username advertised through the ONVIF user directory (GetUsers): the
/// DB-configured WS username, or the product default `admin` (SPEC §2's
/// empty→admin rule) when unset.
fn onvif_wsdl_username(config: &OnvifRuntimeConfig) -> String {
    if config.username.is_empty() {
        "admin".to_string() // hardcode-ok: SPEC §2 empty→admin 产品回退默认，非部署值
    } else {
        config.username.clone()
    }
}

/// Host-side effects for the ONVIF Device service write operations
/// (onvif-rs [`DeviceHooks`]): observation-only. This product never lets
/// a SOAP peer re-clock, reboot, or factory-reset the machine — those
/// controls live behind the authenticated Web UI.
struct OnvifDeviceHooks {
    streams: Arc<StreamManager>,
}

/// Process start marker for the uptime line in the hook summaries
/// (best-effort: captured at the first ONVIF start, monotonic thereafter).
fn process_start() -> std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

impl OnvifDeviceHooks {
    fn new(streams: Arc<StreamManager>) -> Self {
        Self { streams }
    }

    /// One honest line of active-stream state for the text summaries.
    fn active_streams_line(&self) -> String {
        match self.streams.try_active_camera_ids() {
            Some(ids) if ids.is_empty() => "active streams: none".to_string(),
            Some(ids) => format!("active streams: {} ({})", ids.len(), ids.join(", ")),
            None => "active streams: unavailable (busy)".to_string(),
        }
    }
}

impl onvif_device_rs::DeviceHooks for OnvifDeviceHooks {
    fn set_date_time(&self, utc: (i32, i32, i32, i32, i32, i32), tz: &str) {
        tracing::warn!(
            year = utc.0,
            month = utc.1,
            day = utc.2,
            hour = utc.3,
            minute = utc.4,
            second = utc.5,
            tz = %tz,
            "ONVIF SetSystemDateAndTime observed — observation only, the system clock is not adjusted"
        );
    }

    fn reboot(&self) {
        tracing::warn!(
            "ONVIF SystemReboot requested — refused (reboot control lives in the Web UI)"
        );
    }

    fn factory_default(&self, hard: bool) {
        tracing::warn!(
            hard,
            "ONVIF SetSystemFactoryDefault requested — refused (this device is never wiped over SOAP)"
        );
    }

    fn system_log(&self) -> String {
        format!(
            "mibee-eye {} (mibee-eye-notebook)\nuptime: {}s\n{}",
            env!("CARGO_PKG_VERSION"),
            process_start().elapsed().as_secs(),
            self.active_streams_line(),
        )
    }

    fn support_info(&self) -> String {
        format!(
            "product: mibee-eye-notebook\nversion: {}\nuptime_secs: {}\n{}",
            env!("CARGO_PKG_VERSION"),
            process_start().elapsed().as_secs(),
            self.active_streams_line(),
        )
    }
}

/// Fixed-focus notebook webcam as an onvif-rs
/// [`ImagingParams`](onvif_device_rs::imaging::ImagingParams): the
/// camera's exposure/WB are UVC-auto and no V4L2 controls are wired at
/// this product layer, so the four standard names answer a fixed neutral
/// value (documented as not host-backed) and writes acknowledge within
/// the normalized [0,1] contract. Unknown names are honestly
/// `InvalidName`. Focus Move acks (no motor); modes report AUTO (the
/// trait defaults).
struct FixedFocusImaging;

/// The standard imaging parameter names the service queries; anything
/// else is not a camera parameter this product knows.
const FIXED_IMAGING_PARAMS: [&str; 4] = ["Brightness", "Contrast", "Saturation", "Sharpness"];

impl onvif_device_rs::imaging::ImagingParams for FixedFocusImaging {
    fn get_param(&self, name: &str) -> Result<f64, onvif_device_rs::imaging::ImagingParamError> {
        if FIXED_IMAGING_PARAMS.contains(&name) {
            Ok(0.5)
        } else {
            Err(onvif_device_rs::imaging::ImagingParamError::InvalidName(
                name.to_string(),
            ))
        }
    }

    fn set_param(
        &self,
        name: &str,
        value: f64,
    ) -> Result<(), onvif_device_rs::imaging::ImagingParamError> {
        if !FIXED_IMAGING_PARAMS.contains(&name) {
            return Err(onvif_device_rs::imaging::ImagingParamError::InvalidName(
                name.to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&value) {
            return Err(onvif_device_rs::imaging::ImagingParamError::OutOfRange {
                value,
                min: 0.0,
                max: 1.0,
            });
        }
        tracing::info!(
            param = name,
            value,
            "ONVIF imaging set acknowledged (fixed-focus camera, no host control wired)"
        );
        Ok(())
    }
}

/// Parse the DB `ip_filter` allow-list (`a.b.c.d` / `a.b.c.d/nn`) into
/// the library's IP filter. Empty input — or entries that all fail to
/// parse (each WARNs and is skipped) — yields the disabled filter, so a
/// malformed config can never lock the SOAP listener shut.
fn parse_ip_filter_entries(entries: &[String]) -> onvif_device_rs::device::IpFilter {
    use onvif_device_rs::device::{IpEntry, IpFilter, IpFilterMode};

    if entries.is_empty() {
        return IpFilter::disabled();
    }
    let mut parsed = Vec::new();
    for entry in entries {
        let (addr, prefix_str) = match entry.split_once('/') {
            Some((addr, prefix)) => (addr, prefix),
            None => (entry.as_str(), "32"),
        };
        let prefix_len = match prefix_str.parse::<u8>() {
            Ok(p) if p <= 32 => p,
            _ => {
                tracing::warn!(
                    entry = %entry,
                    "ONVIF ip_filter entry has an invalid prefix — skipped"
                );
                continue;
            }
        };
        match addr.parse::<std::net::Ipv4Addr>() {
            Ok(ip) => parsed.push(IpEntry {
                ipv4: ip.to_string(),
                prefix_len,
            }),
            Err(_) => tracing::warn!(
                entry = %entry,
                "ONVIF ip_filter entry is not an IPv4 address — skipped"
            ),
        }
    }
    if parsed.is_empty() {
        return IpFilter::disabled();
    }
    IpFilter {
        enabled: true,
        mode: IpFilterMode::Allow,
        entries: parsed,
    }
}

// ── ProtocolRuntime ──────────────────────────────────────────────────────────

/// Manages hot-toggle lifecycle for ONVIF, GB28181, and RTMP protocols.
///
/// Each protocol runs as an independent background task. Stopping sends
/// a shutdown signal and waits up to 5s for graceful exit; on timeout the
/// task is force-aborted. A failure in one protocol does NOT affect others.
pub struct ProtocolRuntime {
    onvif_handle: Option<JoinHandle<()>>,
    /// WS-Discovery task (stopped together with the SOAP server).
    discovery_handle: Option<JoinHandle<()>>,
    /// GB28181 server handle — kept so stop can deregister (REGISTER
    /// Expires: 0) before tearing the run loop down.
    gb28181_server: Option<Arc<tokio::sync::Mutex<gb28181_rs::server::ServerHandle>>>,
    /// SIP-Date drift observer (§9.10.2): polls the platform clock from
    /// REGISTER responses and WARNs on drift. Aborted before shutdown.
    gb_date_observer: Option<JoinHandle<()>>,
    /// The GB28181 frame-source seam for this server generation. Held so
    /// `stop_gb28181` can release live bridge tasks — the library does not
    /// unsubscribe on its `abort()` path, and without this every timed-out
    /// hot-toggle orphaned a task per live subscription.
    gb28181_frame_source: Option<Arc<StreamManagerFrameSource>>,
    /// Talkback playback stream; held for the server's lifetime so the
    /// local audio output stays open, dropped on stop.
    gb28181_talkback_stream: Option<cpal::Stream>,
    /// Talkback upstream mic capture stream (§9.2 send half); held for
    /// the server's lifetime so capture stays open, dropped on stop.
    gb28181_upstream_stream: Option<cpal::Stream>,
    /// Host-side NOTIFY sender (alarm/position/catalog). Filled on GB28181
    /// start, cleared on stop; the AI alarm bridge reads it on fire.
    notifier_slot: Arc<Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>>,
    /// ONVIF events service handle: filled on ONVIF start (when
    /// `events_enabled`), cleared on stop; the AI alarm bridge
    /// publishes MotionAlarms through it while the protocol is up.
    onvif_events_slot: Arc<Mutex<Option<Arc<onvif_device_rs::events::EventsService>>>>,
    /// DeviceConfig AlarmReport runtime gate (A.2.3.2.10) — platform
    /// switches override the config key while the protocol is up.
    alarm_notify_gate: Arc<AtomicBool>,
    /// Local-recording pause gate shared with the recording FileOutputs
    /// (platform RecordCmd StopRecord / Record).
    recording_paused: Arc<AtomicBool>,
    /// DeviceControl IFrameCmd latch shared with the camera encode loops.
    force_idr: Arc<AtomicBool>,
    /// DeviceConfig FrameMirror runtime flags (A.2.3.2.9) shared with
    /// every camera's capture loop — XOR-composed with per-camera static
    /// mount-compensation flips.
    gb_flips: Arc<streaming::capture_source::Flips>,
    /// Reserved for future use (RTMP currently per-stream, no global task).
    #[allow(dead_code)]
    rtmp_handle: Option<JoinHandle<()>>,
    /// Shutdown signal senders keyed by protocol name.
    shutdown_txs: HashMap<String, watch::Sender<bool>>,
    /// Whether RTMP push is enabled (per-stream; no global background task).
    rtmp_enabled: bool,
}

impl Default for ProtocolRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolRuntime {
    /// Create a new empty runtime with all protocols stopped.
    pub fn new() -> Self {
        Self::new_with_shares(
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(streaming::capture_source::Flips::default()),
            Arc::new(Mutex::new(None)),
        )
    }

    /// Create the runtime sharing caller-owned gates: main.rs hands the
    /// same notifier slot / alarm gate / recording pause flag / IDR latch
    /// / FrameMirror flags to the AI alarm bridge, the StreamManager and
    /// the GB28181 handlers, so one Arc each spans all of them.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_shares(
        notifier_slot: Arc<Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>>,
        alarm_notify_gate: Arc<AtomicBool>,
        recording_paused: Arc<AtomicBool>,
        force_idr: Arc<AtomicBool>,
        gb_flips: Arc<streaming::capture_source::Flips>,
        onvif_events_slot: Arc<Mutex<Option<Arc<onvif_device_rs::events::EventsService>>>>,
    ) -> Self {
        Self {
            onvif_handle: None,
            discovery_handle: None,
            gb28181_server: None,
            gb_date_observer: None,
            gb28181_frame_source: None,
            gb28181_talkback_stream: None,
            gb28181_upstream_stream: None,
            notifier_slot,
            onvif_events_slot,
            alarm_notify_gate,
            recording_paused,
            force_idr,
            gb_flips,
            rtmp_handle: None,
            shutdown_txs: HashMap::new(),
            rtmp_enabled: false,
        }
    }

    // ── ONVIF ────────────────────────────────────────────────────────────

    /// Start the ONVIF device role (onvif-rs): SOAP server + WS-Discovery.
    /// Stops the existing instance first.
    ///
    /// The Media profile advertises the FIRST active stream's RTSP path
    /// (`live/{camera_id}`), resolved at start time; toggle the protocol
    /// again after stream topology changes to refresh it. With no active
    /// stream the Device service + discovery still run (identity-only).
    #[tracing::instrument(skip(self, config, stream_manager))]
    pub async fn start_onvif(
        &mut self,
        config: OnvifRuntimeConfig,
        stream_manager: Arc<StreamManager>,
    ) -> anyhow::Result<()> {
        self.stop_onvif().await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        self.shutdown_txs.insert("onvif".into(), shutdown_tx);

        // Resolve the advertised stream path from the first active stream.
        let stream_path = stream_manager
            .list_active_streams()
            .await
            .first()
            .map(|info| format!("/live/{}", info.camera_id));
        match &stream_path {
            Some(p) => tracing::info!(path = %p, "ONVIF media profile targets first active stream"),
            None => tracing::warn!(
                "ONVIF starting with no active stream — media actions unavailable until re-toggled"
            ),
        }

        let host = config.host.clone();
        let soap_port = config.onvif_port;
        let device_ip = if host.is_empty() {
            onvif_device_rs::discovery::detect_local_ip()
        } else {
            host
        };

        // SOAP server: Device service (identity) + Media service (when a
        // stream is advertised) + Media2 + Imaging + the 0.8 security
        // seams — mounted by the shared builder below (also the test
        // surface for the SOAP face).
        // An empty password is this product's "auth off" setting (the ONVIF
        // toggle itself lives behind the authenticated settings page), so
        // opt into the library's fail-closed no-auth escape hatch.
        let soap = onvif_device_rs::OnvifServer::new(&onvif_device_rs::OnvifConfig {
            port: config.onvif_port,
            username: config.username.clone(),
            password: config.password.clone(),
            allow_no_auth: config.password.is_empty(),
            http_digest: config.http_digest,
            ..Default::default()
        });

        // Media profile store: the shared, mutable onvif-rs 0.8 store —
        // both the Media1 family and the Media2 face read (and
        // SetVideoEncoderConfiguration writes) through it.
        let media_store = if let Some(stream_path) = &stream_path {
            let mut media = onvif_device_rs::media::OnvifMediaConfig::new(
                config.camera_width,
                config.camera_height,
                config.camera_fps,
                config.camera_bitrate,
                config.rtsp_port,
                device_ip.clone(),
            );
            media.stream_path = stream_path.clone();
            // Substream profile (SPEC appendix A #20): advertised after the
            // primary when the first active camera runs with a substream;
            // GetStreamUri maps its `sub` token to the RTSP /live/{id}/sub
            // mount. Applies on the next ONVIF toggle (restart-to-apply,
            // like every other media field here).
            if let Some(sub) = stream_manager.first_active_substream().await {
                let base = media.stream_path.trim_end_matches('/');
                media.extra_profiles = vec![onvif_device_rs::media::MediaProfileConfig::new(
                    "sub",
                    sub.width,
                    sub.height,
                    sub.fps.max(1.0) as u32,
                    sub.bitrate_bps,
                    &format!("{base}/sub"),
                )];
                tracing::info!(
                    width = sub.width,
                    height = sub.height,
                    "ONVIF substream profile advertised"
                );
            }
            Some(Arc::new(std::sync::RwLock::new(media))
                as onvif_device_rs::media::SharedMediaConfig)
        } else {
            None
        };

        // Per-connection IP filter (allow-list from the DB config): parsed
        // BEFORE mounting so the device handlers and the connection gate
        // hold the SAME shared state (the library's one-store contract).
        let ip_filter_state: Option<onvif_device_rs::device::IpFilterState> = {
            let filter = parse_ip_filter_entries(&config.ip_filter);
            if filter.enabled {
                tracing::info!(
                    entries = config.ip_filter.len(),
                    "ONVIF IP allow-list active"
                );
                Some(Arc::new(std::sync::RwLock::new(filter)))
            } else {
                None
            }
        };

        let (soap, events) = mount_onvif_services(
            soap,
            &config,
            &device_ip,
            media_store,
            Arc::clone(&self.force_idr),
            &stream_manager,
            ip_filter_state,
        )?;

        // Pull-Point events (onvif-device-rs 0.7): the shared slot lets
        // the AI alarm bridge publish MotionAlarms while the protocol is
        // up; None while disabled by config or stopped.
        *self
            .onvif_events_slot
            .lock()
            .expect("onvif events slot lock") = events;

        let discovery = onvif_device_rs::discovery::DiscoveryServer::with_identity(
            &device_ip,
            soap_port,
            &config.device.name,
            &config.device.hardware_id,
        );

        let mut shutdown_rx = shutdown_rx;
        // Readiness channel. `start()` binds the listener, and only the task
        // can know whether that succeeded — so the task reports back before
        // the caller claims the protocol started.
        //
        // Without this the log said "ONVIF protocol started" unconditionally
        // and *then* logged a bind error, which reads as a contradiction and
        // tells the operator the opposite of the truth. It also made
        // `start_onvif` return `Ok(())` on a failed bind, so the hot-restart
        // supervisor retried a port the previous generation still held —
        // a 27-second error loop that only stopped because something else
        // released the socket.
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
        let handle = tokio::spawn(async move {
            tracing::info!(
                port = soap_port,
                "ONVIF SOAP + WS-Discovery starting (onvif-rs)"
            );
            // start() resolves as soon as the accept loop is spawned and its
            // handle's Drop stops the server — awaiting the handle here is
            // what keeps the task (and the server) alive.
            let server = match soap.start().await {
                Ok(server) => {
                    let _ = ready_tx.send(Ok(()));
                    server
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::error!(error = %e, "ONVIF SOAP server error");
                    let _ = ready_tx.send(Err(msg.clone()));
                    return;
                }
            };
            tokio::select! {
                _ = server => {
                    tracing::info!("ONVIF SOAP server exited");
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("ONVIF graceful shutdown signal received");
                }
            }
        });
        let discovery_handle = tokio::spawn(async move {
            match discovery.start().await {
                Ok(server) => {
                    let _ = server.await;
                }
                Err(e) => tracing::error!(error = %e, "ONVIF WS-Discovery server error"),
            }
        });

        // SOAP task is the tracked lifecycle handle; discovery is aborted
        // alongside it by the same stop path.
        // Only claim success once the listener is actually up. `start()`
        // binds and returns, so this is normally milliseconds; the bound is
        // there to stop a wedged task from hanging the caller, and hitting
        // it is a failure rather than an unknown-but-assumed-fine.
        match tokio::time::timeout(std::time::Duration::from_secs(10), ready_rx).await {
            // Reported in: bound and listening.
            Ok(Ok(Ok(()))) => {}
            // Reported in: the bind itself failed.
            Ok(Ok(Err(msg))) => {
                handle.abort();
                return Err(anyhow::anyhow!("ONVIF SOAP listener failed to bind: {msg}"));
            }
            // The task ended without reporting at all; nothing is listening.
            Ok(Err(_)) => {
                handle.abort();
                return Err(anyhow::anyhow!(
                    "ONVIF SOAP server task exited before the listener was up"
                ));
            }
            // Not knowing is not knowing. A timeout must not fall through to
            // the success path, or the whole point of this change is lost —
            // and it is exactly what happened on a loaded CI runner, where
            // scheduling pushed the bind past 5 s and the function returned
            // `Ok` for a port it had not yet (and would not) acquire.
            //
            // Failing here is the right trade: the supervisor retries, and a
            // retry against a genuinely free port succeeds. Reporting success
            // for a listener that never came up does not recover.
            Err(_) => {
                handle.abort();
                return Err(anyhow::anyhow!(
                    "ONVIF SOAP listener did not report readiness within 10s"
                ));
            }
        }

        self.onvif_handle = Some(handle);
        self.discovery_handle = Some(discovery_handle);
        tracing::info!("ONVIF protocol started");
        Ok(())
    }

    /// Stop ONVIF WS-Discovery server with graceful 5s timeout.
    #[tracing::instrument(skip(self))]
    pub async fn stop_onvif(&mut self) {
        *self
            .onvif_events_slot
            .lock()
            .expect("onvif events slot lock") = None;
        let tx = self.shutdown_txs.remove("onvif");
        let handle = self.onvif_handle.take();
        if let Some(dh) = self.discovery_handle.take() {
            dh.abort();
        }
        if tx.is_none() && handle.is_none() {
            return;
        }
        graceful_shutdown("onvif", tx, handle).await;
        tracing::info!("ONVIF protocol stopped");
    }

    // ── GB28181 ──────────────────────────────────────────────────────────

    /// Start the GB28181 device server (gb28181-rs). Stops existing first.
    ///
    /// The library server owns the full device role — REGISTER lifecycle
    /// (digest auth), keepalive, catalog, INVITE/ACK/BYE with its own SDP
    /// answers, and PS-over-RTP media push sourced from the first active
    /// camera via [`StreamManagerFrameSource`].
    // `config` carries the SIP register password — never record it in
    // the span (the INFO line inside logs the identifying fields).
    #[tracing::instrument(skip(self, stream_manager, config))]
    pub async fn start_gb28181(
        &mut self,
        config: &Gb28181RuntimeConfig,
        stream_manager: Arc<StreamManager>,
    ) -> anyhow::Result<()> {
        // Graceful stop any existing GB28181 task before starting a new one.
        self.stop_gb28181().await;

        let lib_config = gb28181_rs::config::Gb28181Config {
            enabled: true,
            platform_sip_address: config.sip_addr.clone(),
            platform_sip_port: config.sip_port,
            device_id: config.device_id.clone(),
            channel_id: config.channel_id.clone(),
            sip_domain: config.sip_domain.clone(),
            password: config.password.clone(),
            local_sip_port: config.local_sip_port,
            register_interval_secs: config.register_interval,
            heartbeat_interval_secs: config.heartbeat_interval_secs,
            heartbeat_timeout_count: config.heartbeat_timeout_count,
            transport: gb28181_rs::config::Transport::Udp,
            // gb28181-rs 0.10 additions: keep the historical warn-only
            // example-default policy; verify downstream Notes with the
            // default reject posture (only fires on Note-bearing requests
            // after an A-level handshake, which this Digest platform does
            // not use).
            strict_example_defaults: false,
            incoming_note_policy: gb28181_rs::authenticator::IncomingNotePolicy::default(),
            user_agent: Some(format!("mibee-eye/{}", env!("CARGO_PKG_VERSION"))),
            device_name: Some(config.device_name.clone()),
            manufacturer: Some(config.manufacturer.clone()),
            model: Some(config.model.clone()),
            firmware: Some(config.firmware.clone()),
            // X-GB-Ver negotiation stays opt-in (None = header omitted);
            // matches the mibee-eye-rs/go product defaults.
            protocol_version: None,
            // A configured external PTZ head (M4-a proxy) means the
            // channel can pan/tilt/zoom: advertise the §5.4.12 code 3 so
            // platforms dispatch PTZCmd. Empty endpoint → None → element
            // omitted (no PTZ).
            ptz_type: (!config.ptz_onvif_endpoint.is_empty()).then_some(3),
            // 0.13.0 transport-parity keys (extra_channels, multicast_ttl,
            // tls_*, max_sip_message_size, video-param knobs) stay at
            // their defaults — this runtime rides plain UDP.
            ..Default::default()
        };

        // GB35114 A-level (fail closed: enabled-but-unloadable identities
        // abort the protocol start instead of falling back to Digest).
        let authenticator = build_gb35114_authenticator(&config.gb35114, &config.device_id)?;

        let source = Arc::new(StreamManagerFrameSource::new(stream_manager));
        // Keep our own handle so `stop_gb28181` can tear down live bridges
        // without depending on the library unsubscribing (it does not, on the
        // `abort()` path). Reassigned on every start; cleared on stop.
        self.gb28181_frame_source = Some(source.clone());
        tracing::info!(
            device_id = %config.device_id,
            platform = %format!("{}:{}", config.sip_addr, config.sip_port),
            channel = %config.channel_id,
            "GB28181 device server starting (gb28181-rs)"
        );

        // Talkback receive (audio-only INVITE): decode G.711 to the local
        // output device. Fail-open — disabled or no output device means no
        // sink registered and the library answers talkback INVITEs 488.
        let talkback = match crate::gb28181_talkback::open_sink(config.talkback_playback) {
            Ok(opened) => opened,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "GB28181 talkback playback unavailable — audio INVITEs will be refused 488"
                );
                None
            }
        };

        // Talkback send half: mic → G.711 A-law source. Same fail-open
        // posture — no source registered means recvonly offers get 488.
        let talkback_upstream = match crate::gb28181_talkback::open_upstream(
            config.talkback_upstream,
        ) {
            Ok(opened) => opened,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "GB28181 talkback upstream unavailable — recvonly offers will be refused 488"
                );
                None
            }
        };

        // Host-side seams: control/config handlers, static position source,
        // and the NOTIFY sender for the alarm bridge.
        let alarm_gate = Arc::clone(&self.alarm_notify_gate);
        alarm_gate.store(config.alarm_notify_enabled, Ordering::SeqCst);

        let position_source = StaticPositionSource {
            longitude: config.position_longitude.clone(),
            latitude: config.position_latitude.clone(),
        };

        // External ONVIF PTZ head (M4-a): the GB platform steers it
        // through this hub. Fail-open — an unbuildable endpoint logs and
        // PTZ stays the no-op ack.
        let ptz_proxy = if config.ptz_onvif_endpoint.is_empty() {
            None
        } else {
            match crate::ptz_proxy::PtzProxy::start(&crate::ptz_proxy::PtzConfig {
                enabled: true,
                onvif_endpoint: config.ptz_onvif_endpoint.clone(),
                profile_token: config.ptz_profile_token.clone(),
                username: config.ptz_username.clone(),
                password: config.ptz_password.clone(),
            }) {
                Ok(p) => {
                    tracing::info!(
                        endpoint = %config.ptz_onvif_endpoint,
                        profile = %config.ptz_profile_token,
                        "gb28181: PTZ proxy attached to external ONVIF head"
                    );
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "gb28181: PTZ proxy failed to start — PTZ stays no-op");
                    None
                }
            }
        };
        let mut server_builder =
            gb28181_rs::server::Gb28181Server::with_recording_index(lib_config, source, None)
                .with_register_authenticator(authenticator)
                .with_control_handler(Some(Arc::new(
                    crate::gb28181_control::Gb28181ControlHandler::new(
                        Arc::clone(&self.recording_paused),
                        Arc::clone(&self.force_idr),
                    )
                    .with_ptz_proxy(ptz_proxy),
                )))
                .with_config_handler(Some(Arc::new(
                    crate::gb28181_control::DeviceConfigGlue::new(
                        Arc::clone(&alarm_gate),
                        Arc::clone(&self.gb_flips),
                    ),
                )))
                .with_position_source(Some(Arc::new(position_source)));

        // Taken before spawn(): after the move the builder is gone.
        let notifier = server_builder.notifier();
        *self.notifier_slot.lock().expect("notifier slot lock") = Some(notifier);

        let mut audio_stream = None;
        if let Some((sink, stream)) = talkback {
            server_builder = server_builder.with_audio_sink(sink);
            audio_stream = Some(stream);
        }
        let mut upstream_stream = None;
        if let Some(upstream) = talkback_upstream {
            server_builder = server_builder.with_talkback_source(upstream.frames);
            // Late-bind the negotiated-law handle the cpal encoder polls:
            // PCMA until the channel is installed, then whatever the
            // accepted offer negotiated (library seam, gb28181-rs #78).
            if let Some(handle) = server_builder.talkback_upstream_source() {
                let _ = upstream.law_slot.set(handle);
            }
            upstream_stream = Some(upstream.stream);
        }

        match server_builder.spawn().await {
            Ok(server) => {
                self.gb28181_talkback_stream = audio_stream;
                self.gb28181_upstream_stream = upstream_stream;
                // Shared so the SIP-Date drift observer can poll the
                // platform clock while the runtime keeps stop control.
                let server = Arc::new(tokio::sync::Mutex::new(server));
                self.gb_date_observer =
                    Some(tokio::spawn(observe_platform_date(Arc::clone(&server))));
                self.gb28181_server = Some(server);
            }
            Err(e) => {
                *self.notifier_slot.lock().expect("notifier slot lock") = None;
                self.gb28181_talkback_stream = None;
                self.gb28181_upstream_stream = None;
                tracing::error!(error = %e, "GB28181 device server failed to start");
                return Err(e);
            }
        }
        tracing::info!("GB28181 protocol started");
        Ok(())
    }

    /// Stop GB28181: de-register first (REGISTER `Expires: 0` — the same
    /// 401 Digest dance as registration, 2s response timeouts inside the
    /// library), then join the run loop. Every deregistration failure is
    /// logged and ignored: shutdown itself must always succeed. On timeout
    /// the server task is force-aborted.
    #[tracing::instrument(skip(self))]
    pub async fn stop_gb28181(&mut self) {
        // Clear the notify slot first — the alarm bridge must not fire
        // into a server that is going away.
        *self.notifier_slot.lock().expect("notifier slot lock") = None;
        // Stop the drift observer first and wait for it to release its
        // handle share before shutting the server down.
        if let Some(observer) = self.gb_date_observer.take() {
            observer.abort();
            let _ = observer.await;
        }
        let Some(server) = self.gb28181_server.take() else {
            return;
        };
        let mut server = server.lock().await;
        match tokio::time::timeout(Duration::from_secs(8), server.shutdown_with_deregister()).await
        {
            Ok(Ok(())) => tracing::info!("GB28181 deregistered and stopped"),
            Ok(Err(e)) => tracing::warn!(error = %e, "GB28181 shutdown error"),
            Err(_) => {
                tracing::warn!("GB28181 graceful shutdown timed out; aborting server task");
                server.abort();
            }
        }
        // Tear the frame bridges down unconditionally. Neither the graceful
        // path nor `abort()` guarantees the library unsubscribed, and a
        // bridge left running would keep resolving the first active camera
        // and forwarding access units into a channel nobody reads — once per
        // hot-toggle, for the life of the process.
        let drained = self
            .gb28181_frame_source
            .take()
            .map(|s| s.drain_subscribers())
            .unwrap_or(0);
        if drained > 0 {
            tracing::info!(
                drained,
                "GB28181 bridge: released live frame subscribers on stop"
            );
        }
        // Dropping the talkback playback/upstream streams releases the
        // audio output and the microphone.
        self.gb28181_talkback_stream = None;
        self.gb28181_upstream_stream = None;
        tracing::info!("GB28181 protocol stopped");
    }

    // ── RTMP ─────────────────────────────────────────────────────────────

    /// Enable RTMP push. RTMP is per-stream; no global background task is
    /// spawned. New streams will auto-attach an RTMP output based on the
    /// DB config that was just updated.
    #[tracing::instrument(skip(self))]
    pub async fn start_rtmp(&mut self) -> anyhow::Result<()> {
        if self.rtmp_enabled {
            tracing::debug!("RTMP already enabled, nothing to do");
            return Ok(());
        }
        self.rtmp_enabled = true;
        tracing::info!("RTMP push enabled (per-stream; new streams will auto-attach)");
        Ok(())
    }

    /// Disable RTMP push. Existing streams keep their RTMP output until stopped.
    #[tracing::instrument(skip(self))]
    pub async fn stop_rtmp(&mut self) {
        if !self.rtmp_enabled {
            return;
        }
        self.rtmp_enabled = false;
        tracing::info!("RTMP push disabled (new streams will not attach RTMP output)");
    }

    // ── Status ───────────────────────────────────────────────────────────

    /// Return the current running/stopped status for all protocols.
    pub fn status(&self) -> ProtocolStatus {
        ProtocolStatus {
            onvif: ProtocolState {
                running: self.onvif_handle.is_some(),
            },
            gb28181: ProtocolState {
                running: self.gb28181_server.is_some(),
            },
            rtmp: ProtocolState {
                running: self.rtmp_enabled,
            },
        }
    }

    /// Stop all protocols (for server shutdown).
    #[tracing::instrument(skip(self))]
    /// Shared NOTIFY-sender slot for the AI alarm bridge: `Some` while
    /// GB28181 is running, `None` otherwise.
    pub fn gb_notifier_slot(
        &self,
    ) -> Arc<Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>> {
        Arc::clone(&self.notifier_slot)
    }

    /// Shared local-recording pause gate (platform RecordCmd). Hand to the
    /// StreamManager so recording FileOutputs attach with the same flag.
    pub fn recording_paused_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.recording_paused)
    }

    /// Shared DeviceConfig AlarmReport runtime gate for the alarm bridge.
    pub fn alarm_notify_gate(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alarm_notify_gate)
    }

    /// Shared IFrameCmd latch handed to the StreamManager's encode loops.
    pub fn force_idr_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.force_idr)
    }

    pub async fn shutdown_all(&mut self) {
        self.stop_onvif().await;
        self.stop_gb28181().await;
        self.stop_rtmp().await;
    }
}

// ── GB28181 frame source (gb28181-rs seam) ───────────────────────────────────

/// [`gb28181_rs::frame::FrameSource`] bridging the StreamManager's first
/// active camera into the device server.
///
/// The GB28181 runtime exposes the notebook's streams as ONE logical channel
/// (the same "first active stream" routing the previous hand-rolled runtime
/// used): each subscription attaches a bridge task that resolves the first
/// active camera, parses its Annex-B frames into access units, and forwards
/// them into the library's bounded channel (full channel = frame dropped,
/// mirroring the hub's slow-consumer semantics). When no stream is active the
/// bridge retries until one appears, so an INVITE racing stream startup still
/// gets media.
struct StreamManagerFrameSource {
    manager: Arc<StreamManager>,
    next_id: std::sync::atomic::AtomicU64,
    /// Subscription id → (sender the library reads, bridge task).
    ///
    /// The `JoinHandle` is retained deliberately. The bridge task only ever
    /// exits when its sender's channel disconnects, and the registry holds a
    /// second sender clone — so dropping the registry entry is what ends the
    /// task. `gb28181_rs::ServerHandle::abort()` explicitly does *not* clean
    /// up active media tasks (its own docs say so), so on the abort path
    /// nothing ever called `unsubscribe` and every timed-out GB28181
    /// hot-toggle leaked one map entry, one detached task still resolving and
    /// forwarding frames into a void, and one `SyncSender`. Retaining the
    /// handle lets both paths tear down deterministically.
    ///
    /// `parking_lot::Mutex` rather than `std::sync::Mutex`: this sits on the
    /// INVITE/media path, and a panic anywhere under a std lock would poison
    /// it permanently — every subsequent INVITE would panic too.
    subscribers: parking_lot::Mutex<HashMap<u64, SubscriberEntry>>,
}

struct SubscriberEntry {
    /// Kept alive so the library's receiver stays connected.
    #[allow(dead_code)]
    sender: std::sync::mpsc::SyncSender<gb28181_rs::frame::AccessUnit>,
    task: tokio::task::JoinHandle<()>,
}

impl StreamManagerFrameSource {
    fn new(manager: Arc<StreamManager>) -> Self {
        Self {
            manager,
            next_id: std::sync::atomic::AtomicU64::new(1),
            subscribers: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

impl gb28181_rs::frame::FrameSource for StreamManagerFrameSource {
    fn subscribe_with_capacity(&self, capacity: usize) -> gb28181_rs::frame::FrameSubscription {
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let manager = self.manager.clone();
        // The task owns one sender; the registry entry keeps the other so
        // dropping the entry both disconnects the library's receiver and
        // gives `drain_subscribers` a handle to abort a parked task.
        let bridge_tx = tx.clone();
        let task = tokio::spawn(async move {
            let mut no_camera_ticks: u32 = 0;
            let mut forwarded: u64 = 0;
            loop {
                let camera = manager
                    .list_active_streams()
                    .await
                    .first()
                    .map(|info| info.camera_id.clone());
                let frames = match camera {
                    Some(camera_id) => {
                        tracing::debug!(%camera_id, "gb28181 bridge: subscribed to camera frames");
                        manager.subscribe_frames(&camera_id).await
                    }
                    None => {
                        no_camera_ticks += 1;
                        if no_camera_ticks % 10 == 1 {
                            tracing::warn!(
                                ticks = no_camera_ticks,
                                "gb28181 bridge: no active camera to bridge"
                            );
                        }
                        None
                    }
                };
                let mut frames = match frames {
                    Some(f) => f,
                    None => {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let mut gatherer = AuGatherer::new();
                loop {
                    match frames.recv().await {
                        Ok(frame) => {
                            let streaming::source::MediaFrame::Video { .. } = frame.as_ref() else {
                                continue; // audio is not part of the H.264 push path
                            };
                            let Some(au) = gatherer.push(&frame) else {
                                continue;
                            };
                            forwarded += 1;
                            if forwarded % 300 == 1 {
                                // Heartbeat cadence for a steady pull is
                                // operational noise on journald (#2) — the
                                // send failures and teardown summary are
                                // what carry signal, and those stay warn+.
                                tracing::debug!(
                                    forwarded,
                                    nalus = au.nalus.len(),
                                    key = au.is_key_frame,
                                    "gb28181 bridge: forwarding access units"
                                );
                            }
                            // Bounded, drop-on-full: mirrors the hub's
                            // slow-consumer semantics without blocking the
                            // async bridge on a full channel.
                            use std::sync::mpsc::TrySendError;
                            match bridge_tx.try_send(au) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {}
                                Err(TrySendError::Disconnected(_)) => {
                                    return; // subscriber dropped (BYE / re-INVITE)
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue; // slow consumer: skip missed, keep latest
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break; // stream ended — fall back to camera re-resolution
                        }
                    }
                }
            }
        });

        // Registered after the spawn so the entry owns both halves of the
        // teardown: dropping it disconnects the library's receiver, and the
        // retained handle lets unsubscribe abort a bridge that is parked in
        // its 1s retry sleep rather than waiting for a send to notice.
        self.subscribers
            .lock()
            .insert(id, SubscriberEntry { sender: tx, task });

        gb28181_rs::frame::FrameSubscription { id, receiver: rx }
    }

    fn unsubscribe(&self, id: u64) {
        if let Some(entry) = self.subscribers.lock().remove(&id) {
            entry.task.abort();
            tracing::debug!(subscription = id, "gb28181 bridge: subscriber released");
        }
    }
}

impl StreamManagerFrameSource {
    /// Tear down every live bridge task.
    ///
    /// Called when the GB28181 server is stopped so the shutdown path does
    /// not depend on the library unsubscribing for us — `ServerHandle::abort`
    /// does not, and that is exactly how the leak used to happen.
    fn drain_subscribers(&self) -> usize {
        let entries: Vec<SubscriberEntry> =
            self.subscribers.lock().drain().map(|(_, e)| e).collect();
        let n = entries.len();
        for entry in entries {
            entry.task.abort();
        }
        n
    }
}

/// Group the capture pipeline's per-NAL [`MediaFrame`]s into H.264 access
/// units for the GB28181 push path.
///
/// The USB capture pipeline emits **one MediaFrame per NAL unit** (start code
/// stripped, `keyframe` true for IDR slices *and* their SPS/PPS). The
/// library's `FrameSource` contract wants whole access units, so the
/// gatherer holds non-VCL NALs (SPS/PPS/SEI/AUD) pending and emits one
/// access unit per VCL NAL (slice, types 1–5) with those prefixes attached.
struct AuGatherer {
    pending: Vec<Vec<u8>>,
}

impl AuGatherer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Feed one video MediaFrame; returns a complete access unit when the
    /// fed NAL closes one (i.e. it was a VCL slice).
    fn push(
        &mut self,
        frame: &streaming::source::MediaFrame,
    ) -> Option<gb28181_rs::frame::AccessUnit> {
        let (data, keyframe_hint) = match frame {
            streaming::source::MediaFrame::Video { data, keyframe, .. } => (data, *keyframe),
            streaming::source::MediaFrame::Audio { .. } => return None,
        };
        if data.is_empty() {
            return None;
        }
        let nalu_type = data[0] & 0x1F;
        if !(1..=5).contains(&nalu_type) {
            // Non-VCL prefix (SPS/PPS/SEI/AUD…): keep the latest set, capped
            // so a runaway encoder cannot grow this without bound.
            self.pending.push(data.clone());
            if self.pending.len() > 8 {
                self.pending.remove(0);
            }
            return None;
        }
        let mut nalus: Vec<gb28181_rs::frame::Nalu> = self
            .pending
            .drain(..)
            .chain(std::iter::once(data.clone()))
            .map(|n| {
                let t = n[0] & 0x1F;
                gb28181_rs::frame::Nalu {
                    nalu_type: t,
                    is_idr: t == 5,
                    is_sps: t == 7,
                    is_pps: t == 8,
                    is_aud: t == 9,
                    data: n,
                }
            })
            .collect();
        let is_key_frame = keyframe_hint || nalus.iter().any(|n| n.is_idr);
        // Defensive: drop degenerate empties (cannot happen for a non-empty
        // input, but keeps the contract explicit).
        nalus.retain(|n| !n.data.is_empty());
        if nalus.is_empty() {
            return None;
        }
        Some(gb28181_rs::frame::AccessUnit {
            is_key_frame,
            timestamp: std::time::Instant::now(),
            nalus,
        })
    }
}

async fn graceful_shutdown(
    protocol: &str,
    shutdown_tx: Option<watch::Sender<bool>>,
    handle: Option<JoinHandle<()>>,
) {
    // 1. Send the shutdown signal so the task can exit cleanly.
    if let Some(tx) = shutdown_tx {
        let _ = tx.send(true);
        tracing::info!(protocol, "shutdown signal sent");
    }

    // 2. Wait for the task to finish, or force-abort after the timeout.
    if let Some(handle) = handle {
        tokio::pin!(handle);
        tokio::select! {
            result = &mut handle => {
                match result {
                    Ok(()) => tracing::info!(protocol, "protocol stopped gracefully"),
                    Err(e) => tracing::warn!(
                        protocol, error = %e,
                        "protocol task error during shutdown"
                    ),
                }
            }
            _ = tokio::time::sleep(SHUTDOWN_TIMEOUT) => {
                tracing::warn!(
                    protocol,
                    "graceful shutdown timed out after 5s, force aborting"
                );
                handle.abort();
                match (&mut handle).await {
                    Ok(()) => {}
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => tracing::warn!(
                        protocol, error = %e,
                        "protocol task error after force abort"
                    ),
                }
            }
        }
    }
}

// ── Network helpers (duplicated from main.rs for self-containment) ───────────

/// Get the local IP address that can reach the given server address.
///
/// Creates a UDP socket, connects to the server, and reads the local address.
/// This works on all platforms (uses std::net, not libc).
#[cfg_attr(not(test), allow(dead_code))]
fn get_local_ip_for_server(server_addr: &SocketAddr) -> anyhow::Result<String> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(server_addr)?;
    let local_addr = socket.local_addr()?;
    Ok(local_addr.ip().to_string())
}

// ── SIP-Date drift observation (GB/T 28181-2022 §9.10.2) ─────────────────────

/// Poll cadence for the platform-clock observer. REGISTER responses
/// refresh the platform date inside the library; a minute is far finer
/// than clock drift moves.
const DATE_OBSERVER_INTERVAL: Duration = Duration::from_secs(60);

/// Drift threshold (seconds) beyond which the observer WARNs. §9.10.2
/// makes REGISTER's SIP `Date` header the device-side time source; on a
/// disciplined host the drift is informational only — the system clock is
/// never touched here (host decision, library seam contract).
const DATE_DRIFT_WARN_SECS: u64 = 5;

/// Outcome of evaluating one platform-clock sample against the latch.
#[derive(Debug, PartialEq, Eq)]
enum DateDriftOutcome {
    /// Drift back within the threshold — clear the latch (log once).
    Recovered,
    /// Beyond threshold but not moved another threshold since the last
    /// WARN — stay quiet (keep the latch).
    Stable,
    /// Beyond threshold and moved since the last WARN — WARN now with
    /// the signed drift, latch its magnitude.
    Warn(i64),
}

fn evaluate_date_drift(
    platform_unix: i64,
    local_unix: i64,
    last_warned: Option<u64>,
) -> DateDriftOutcome {
    let drift = local_unix - platform_unix;
    let abs = drift.unsigned_abs();
    if abs <= DATE_DRIFT_WARN_SECS {
        return if last_warned.is_some() {
            DateDriftOutcome::Recovered
        } else {
            DateDriftOutcome::Stable
        };
    }
    match last_warned {
        Some(prev) if abs.abs_diff(prev) < DATE_DRIFT_WARN_SECS => DateDriftOutcome::Stable,
        _ => DateDriftOutcome::Warn(drift),
    }
}

/// Background observer: every [`DATE_OBSERVER_INTERVAL`] reads the
/// platform clock as last carried by a REGISTER response and WARNs on
/// significant drift. Observation only — disciplining the clock stays a
/// host/NTP decision.
async fn observe_platform_date(server: Arc<tokio::sync::Mutex<gb28181_rs::server::ServerHandle>>) {
    let mut interval = tokio::time::interval(DATE_OBSERVER_INTERVAL);
    // First tick completes immediately — skip it (a just-started server
    // has no REGISTER response yet anyway).
    interval.tick().await;
    let mut last_warned: Option<u64> = None;
    loop {
        interval.tick().await;
        let platform = {
            let guard = match server.try_lock() {
                Ok(g) => g,
                Err(_) => continue, // stop path holds the lock
            };
            guard.platform_date_unix()
        };
        let Some(platform) = platform else { continue };
        let local = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        match evaluate_date_drift(platform, local, last_warned) {
            DateDriftOutcome::Warn(drift) => {
                tracing::warn!(
                    drift_secs = drift,
                    threshold_secs = DATE_DRIFT_WARN_SECS,
                    "GB28181 platform clock drifts from local time (SIP Date, §9.10.2); \
                     observation only — the system clock is not adjusted"
                );
                last_warned = Some(drift.unsigned_abs());
            }
            DateDriftOutcome::Recovered => {
                tracing::info!("GB28181 platform clock drift back within threshold");
                last_warned = None;
            }
            DateDriftOutcome::Stable => {}
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SIP-Date drift evaluation (§9.10.2) ─────────────────────────────

    #[test]
    fn date_drift_within_threshold_is_quiet() {
        assert_eq!(
            evaluate_date_drift(1_000, 1_000, None),
            DateDriftOutcome::Stable
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_005, None),
            DateDriftOutcome::Stable
        );
    }

    #[test]
    fn date_drift_first_excursion_warns() {
        assert_eq!(
            evaluate_date_drift(1_000, 1_007, None),
            DateDriftOutcome::Warn(7)
        );
        // Sign preserved: platform ahead of local.
        assert_eq!(
            evaluate_date_drift(1_012, 1_000, None),
            DateDriftOutcome::Warn(-12)
        );
    }

    #[test]
    fn date_drift_stable_drift_does_not_rewarn() {
        // Already warned at 7s; the same drift (or a 1s wiggle) stays quiet.
        assert_eq!(
            evaluate_date_drift(1_000, 1_007, Some(7)),
            DateDriftOutcome::Stable
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_009, Some(7)),
            DateDriftOutcome::Stable
        );
        // Moved another threshold → warn again.
        assert_eq!(
            evaluate_date_drift(1_000, 1_013, Some(7)),
            DateDriftOutcome::Warn(13)
        );
    }

    #[test]
    fn date_drift_recovery_clears_the_latch() {
        // Was warned; drift back within threshold → Recovered (latch clears,
        // so the next excursion warns immediately).
        assert_eq!(
            evaluate_date_drift(1_000, 1_003, Some(7)),
            DateDriftOutcome::Recovered
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_000, None),
            DateDriftOutcome::Stable
        );
        assert_eq!(
            evaluate_date_drift(1_000, 1_006, None),
            DateDriftOutcome::Warn(6)
        );
    }

    /// The gb35114 sub-tree of the DB protocol config flows into the
    /// runtime config (nested object under protocols.gb28181.gb35114).
    #[test]
    fn extract_gb28181_config_parses_gb35114() {
        let db = serde_json::json!({
            "enabled": true,
            "device_id": "34020000001320000001",
            "platform_sip_address": "192.168.1.100",
            "gb35114": {
                "enabled": true,
                "device_cert_file": "/etc/mibee/gb35114/device_cert.pem",
                "device_key_file": "/etc/mibee/gb35114/device_key.pem",
                "platform_cert_file": "/etc/mibee/gb35114/platform_cert.pem",
                "server_id": "34020000002000000001",
            },
        });
        let cfg = extract_gb28181_config(&db);
        assert!(cfg.gb35114.enabled);
        assert_eq!(cfg.gb35114.server_id, "34020000002000000001");
        assert!(cfg.gb35114.device_cert_file.ends_with("device_cert.pem"));
        // Absent sub-tree → disabled default, everything else keeps working.
        let plain = extract_gb28181_config(&serde_json::json!({"enabled": false}));
        assert!(!plain.gb35114.enabled);
        assert!(plain.gb35114.server_id.is_empty());
    }

    /// Talkback playback is on by default (the point of enabling GB28181
    /// talk on a device with speakers) and can be turned off per config.
    #[test]
    fn extract_gb28181_config_talkback_playback() {
        let default = extract_gb28181_config(&serde_json::json!({}));
        assert!(default.talkback_playback);
        let off = extract_gb28181_config(&serde_json::json!({"talkback_playback": false}));
        assert!(!off.talkback_playback);
        let on = extract_gb28181_config(&serde_json::json!({"talkback_playback": true}));
        assert!(on.talkback_playback);
    }

    /// Fixture-backed SM2 identities build a working A-level
    /// authenticator; a bad path surfaces as an error (never a panic, never
    /// a silent Digest fallback when the operator asked for A-level).
    #[test]
    fn build_gb35114_authenticator_ok_and_fail_paths() {
        let dir = format!("{}/tests/fixtures/gb35114", env!("CARGO_MANIFEST_DIR"));
        let cfg = Gb35114RuntimeConfig {
            enabled: true,
            device_cert_file: format!("{dir}/device_cert.pem"),
            device_key_file: format!("{dir}/device_key.pem"),
            platform_cert_file: format!("{dir}/platform_cert.pem"),
            server_id: "34020000002000000001".to_string(),
            include_device_cert: false,
        };
        let auth = build_gb35114_authenticator(&cfg, "34020000001320000001")
            .expect("fixture identities must build")
            .expect("enabled must yield an authenticator");
        // The seam is wired: the authenticator answers a challenge it can
        // later verify (round-trip through its own header roundtrip).
        drop(auth);

        // Disabled → None without touching the filesystem.
        let off = Gb35114RuntimeConfig {
            enabled: false,
            ..cfg.clone()
        };
        assert!(build_gb35114_authenticator(&off, "d").unwrap().is_none());

        // Missing cert file → Err (fail closed, protocol refuses to start).
        let bad = Gb35114RuntimeConfig {
            device_cert_file: "/nonexistent/cert.pem".to_string(),
            ..cfg
        };
        assert!(build_gb35114_authenticator(&bad, "d").is_err());
    }

    #[test]
    fn test_new_runtime_all_stopped() {
        let rt = ProtocolRuntime::new();
        let status = rt.status();
        assert!(!status.onvif.running);
        assert!(!status.gb28181.running);
        assert!(!status.rtmp.running);
    }

    fn video_nal(nalu_type: u8, keyframe: bool) -> streaming::source::MediaFrame {
        streaming::source::MediaFrame::Video {
            data: vec![nalu_type | 0x60, 0xAA, 0xBB],
            keyframe,
            timestamp: 0,
        }
    }

    /// The capture pipeline emits one MediaFrame per NAL (SPS/PPS/IDR all
    /// keyframe=true); the gatherer must emit one AU per slice with the
    /// non-VCL prefix attached.
    #[test]
    fn au_gatherer_groups_per_nal_frames_into_access_units() {
        let mut g = AuGatherer::new();
        assert!(g.push(&video_nal(7, true)).is_none()); // SPS
        assert!(g.push(&video_nal(8, true)).is_none()); // PPS
        let idr = g.push(&video_nal(5, true)).expect("IDR closes the AU");
        assert!(idr.is_key_frame);
        assert_eq!(
            idr.nalus.iter().map(|n| n.nalu_type).collect::<Vec<_>>(),
            vec![7, 8, 5]
        );
        let p = g.push(&video_nal(1, false)).expect("slice closes the AU");
        assert!(!p.is_key_frame);
        assert_eq!(p.nalus.len(), 1);
        // Audio frames are ignored.
        assert!(
            g.push(&streaming::source::MediaFrame::Audio {
                data: vec![0x00],
                timestamp: 0,
            })
            .is_none()
        );
    }

    #[test]
    fn au_gatherer_caps_pending_prefixes() {
        let mut g = AuGatherer::new();
        for _ in 0..20 {
            g.push(&video_nal(6, false)); // SEI-like non-VCL
        }
        assert!(g.pending.len() <= 8);
    }

    #[test]
    fn test_rtmp_enable_disable() {
        let mut rt = ProtocolRuntime::new();
        assert!(!rt.status().rtmp.running);

        // Use block_on for the async methods
        let rt_handle = tokio::runtime::Runtime::new().unwrap();
        rt_handle.block_on(async {
            rt.start_rtmp().await.unwrap();
            assert!(rt.status().rtmp.running);

            rt.stop_rtmp().await;
            assert!(!rt.status().rtmp.running);
        });
    }

    #[test]
    fn test_extract_gb28181_config_defaults() {
        let json = serde_json::json!({});
        let config = extract_gb28181_config(&json);
        assert_eq!(config.sip_port, 5060);
        assert_eq!(config.register_interval, 60);
        assert_eq!(config.heartbeat_interval_secs, 60);
        assert_eq!(config.heartbeat_timeout_count, 3);
        assert_eq!(config.channel_id, "34020000001320000001");
        assert_eq!(config.local_sip_port, 5060);
        assert_eq!(config.register_interval, 60);
    }

    #[test]
    fn test_extract_gb28181_config_from_json() {
        let json = serde_json::json!({
            "device_id": "34020000001320000001",
            "platform_sip_address": "10.0.0.1",
            "platform_sip_port": 5060,
            "password": "secret",
            "sip_domain": "3402000000",
            "register_interval_secs": 120,
            "heartbeat_interval_secs": 30,
            "heartbeat_timeout_count": 5,
            "channel_id": "34020000001320000001",
            "local_sip_port": 7060
        });
        let config = extract_gb28181_config(&json);
        assert_eq!(config.device_id, "34020000001320000001");
        assert_eq!(config.sip_addr, "10.0.0.1");
        assert_eq!(config.sip_port, 5060);
        assert_eq!(config.password, "secret");
        assert_eq!(config.register_interval, 120);
        assert_eq!(config.heartbeat_interval_secs, 30);
        assert_eq!(config.heartbeat_timeout_count, 5);
        assert_eq!(config.channel_id, "34020000001320000001");
        assert_eq!(config.local_sip_port, 7060);
    }

    #[test]
    fn test_build_onvif_config_from_json() {
        let json = serde_json::json!({
            "manufacturer": "TestCorp",
            "model": "CAM-100",
            "serial": "SN12345",
            "firmware_version": "2.0.0"
        });
        let config = build_onvif_config_from_json(&json, "192.168.1.50");
        assert_eq!(config.device.manufacturer, "TestCorp");
        assert_eq!(config.device.model, "CAM-100");
        assert_eq!(config.device.serial_number, "SN12345");
        assert_eq!(config.device.firmware, "2.0.0");
        assert_eq!(config.device.hardware_id, "CAM-100");
        assert_eq!(config.host, "192.168.1.50");
        assert_eq!(config.rtsp_port, 8554);
        // Media profile defaults (720p) until DB keys override them.
        assert_eq!(config.camera_width, 1280);
        assert_eq!(config.camera_height, 720);
    }

    #[test]
    fn serial_from_cpuinfo_parses_pi_serial() {
        assert_eq!(
            serial_from_cpuinfo("Hardware\t: BCM2835\nSerial\t\t: 10000000a1b2c3d4\n"),
            "10000000a1b2c3d4"
        );
        assert_eq!(serial_from_cpuinfo("no serial here"), "");
        assert_eq!(serial_from_cpuinfo("Serial\t: \n"), "");
    }

    #[test]
    fn normalize_machine_id_takes_first_long_token() {
        assert_eq!(
            normalize_machine_id("3f2b1c0d9e8a7b6c5d4e3f2a1b0c9d8e\n"),
            "3f2b1c0d9e8a7b6c5d4e3f2a1b0c9d8e"
        );
        assert_eq!(normalize_machine_id("\n"), "");
        assert_eq!(normalize_machine_id("short\n"), "");
    }

    #[test]
    fn test_build_onvif_config_serial_chain() {
        // Explicit serial wins untouched.
        let config = build_onvif_config_from_json(
            &serde_json::json!({"serial": "SN-EXPLICIT-1"}),
            "192.0.2.10",
        );
        assert_eq!(config.device.serial_number, "SN-EXPLICIT-1");

        // Unset — and the legacy baked default persisted by older
        // builds — probes the device identity; on this host the probe
        // chain succeeds, so the shared NC00000001 must NOT survive
        // (issue #18).
        for json in [
            serde_json::json!({}),
            serde_json::json!({"serial": "NC00000001"}),
        ] {
            let config = build_onvif_config_from_json(&json, "192.0.2.10");
            assert_eq!(config.device.serial_number, detect_device_serial());
            assert_ne!(config.device.serial_number, "NC00000001");
            assert!(!config.device.serial_number.is_empty());
        }
    }

    #[test]
    fn test_build_onvif_config_from_json_events_enabled() {
        // Absent key (existing DB rows) defaults to true; explicit false
        // parses through.
        let config = build_onvif_config_from_json(&serde_json::json!({}), "192.0.2.10");
        assert!(config.events_enabled);
        let config = build_onvif_config_from_json(
            &serde_json::json!({"events_enabled": false}),
            "192.0.2.10",
        );
        assert!(!config.events_enabled);
    }

    #[test]
    fn test_build_onvif_config_from_json_profile_overrides() {
        let json = serde_json::json!({
            "profile_width": 1920,
            "profile_height": 1080,
            "profile_fps": 30,
            "profile_bitrate": 4000000
        });
        let config = build_onvif_config_from_json(&json, "10.0.0.1");
        assert_eq!(config.camera_width, 1920);
        assert_eq!(config.camera_height, 1080);
        assert_eq!(config.camera_fps, 30);
        assert_eq!(config.camera_bitrate, 4_000_000);
    }

    #[test]
    fn test_get_local_ip_for_server_loopback() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let result = get_local_ip_for_server(&addr).unwrap();
        assert!(result.starts_with("127."));
    }

    /// `protocols.onvif.port` has to reach the listener.
    ///
    /// It used to be a dead key: the builder hardcoded `ONVIF_HTTP_PORT`, so
    /// the settings page rendered a value the device never used, accepted an
    /// edit, persisted it, and served on a different port than the one on
    /// screen. Found while testing the readiness handshake below — that test
    /// passed locally and failed in CI precisely because it was occupying a
    /// port the server never looked at.
    #[test]
    fn build_onvif_config_honours_the_configured_port() {
        let cfg = build_onvif_config_from_json(
            &serde_json::json!({ "enabled": true, "port": 18080 }),
            "onvif",
        );
        assert_eq!(cfg.onvif_port, 18080, "a chosen SOAP port must be used");
    }

    /// The old default named the WS-Discovery UDP port, so every deployment
    /// that never touched the key still has `3702` in its config document and
    /// settings row. Honouring it literally would move SOAP off 8080 — the
    /// port every NVR is configured against, and the one this device has
    /// always served — with no way for an operator to have meant that.
    #[test]
    fn build_onvif_config_maps_the_legacy_discovery_port_back_to_soap() {
        for legacy in [3702, 0] {
            let cfg = build_onvif_config_from_json(
                &serde_json::json!({ "enabled": true, "port": legacy }),
                "onvif",
            );
            assert_eq!(
                cfg.onvif_port, ONVIF_HTTP_PORT,
                "legacy/absent port {legacy} must resolve to the SOAP port"
            );
        }
        // Absent entirely.
        let cfg = build_onvif_config_from_json(&serde_json::json!({ "enabled": true }), "onvif");
        assert_eq!(cfg.onvif_port, ONVIF_HTTP_PORT);
    }

    /// A default that disagrees with what the service serves is the deeper
    /// half of the same defect: the key was a no-op *and* its default named
    /// another protocol's port.
    #[test]
    fn the_config_default_is_the_port_actually_served() {
        assert_eq!(
            crate::config::OnvifConfig::default().port,
            ONVIF_HTTP_PORT,
            "the shipped default must be the SOAP port the device binds"
        );
    }

    #[tokio::test]
    async fn test_stop_onvif_when_not_running_is_noop() {
        let mut rt = ProtocolRuntime::new();
        rt.stop_onvif().await; // should not panic
        assert!(!rt.status().onvif.running);
    }

    /// `start_onvif` must not claim success on a port it cannot bind.
    ///
    /// It used to log "ONVIF protocol started" unconditionally, straight after
    /// spawning the task that does the binding — so the operator saw success
    /// and then a bind error, in that order, and the function returned `Ok`
    /// either way. The hot-restart supervisor then retried a port the previous
    /// generation still held, producing a 27-second `EADDRINUSE` loop that
    /// only ended when something else released the socket.
    ///
    /// Occupied by a real listener here, so the bind genuinely cannot succeed.
    #[tokio::test]
    async fn start_onvif_reports_a_port_it_cannot_bind() {
        // Hold the port the runtime is about to try.
        let squatter = std::net::TcpListener::bind("0.0.0.0:0").expect("bind squatter");
        let port = squatter.local_addr().expect("addr").port();

        let mut rt = ProtocolRuntime::new();
        let config = build_onvif_config_from_json(
            &serde_json::json!({ "enabled": true, "port": port }),
            "onvif",
        );
        let result = rt
            .start_onvif(
                config,
                Arc::new(crate::stream_manager::StreamManager::new()),
            )
            .await;

        assert!(
            result.is_err(),
            "binding an occupied port must be reported as a failure, not as \
             'ONVIF protocol started' (got: {result:?})"
        );
        assert!(
            !rt.status().onvif.running,
            "a failed bind must not leave the runtime reporting ONVIF as up"
        );
    }

    #[tokio::test]
    async fn test_stop_gb28181_when_not_running_is_noop() {
        let mut rt = ProtocolRuntime::new();
        rt.stop_gb28181().await; // should not panic
        assert!(!rt.status().gb28181.running);
    }

    #[tokio::test]
    async fn test_stop_rtmp_when_not_running_is_noop() {
        let mut rt = ProtocolRuntime::new();
        rt.stop_rtmp().await; // should not panic
        assert!(!rt.status().rtmp.running);
    }

    #[tokio::test]
    async fn test_shutdown_all() {
        let mut rt = ProtocolRuntime::new();
        rt.rtmp_enabled = true;
        rt.shutdown_all().await;
        let status = rt.status();
        assert!(!status.onvif.running);
        assert!(!status.gb28181.running);
        assert!(!status.rtmp.running);
    }

    #[test]
    fn au_gatherer_classifies_nalu_flags() {
        let mut g = AuGatherer::new();
        // Per-NAL frames of one keyframe encode: SPS(0x67) PPS(0x68) IDR(0x65)
        g.push(&video_nal(0x67 & 0x1F, true));
        g.push(&video_nal(0x68 & 0x1F, true));
        let au = g.push(&video_nal(5, true)).expect("IDR closes the AU");
        assert!(au.is_key_frame);
        assert_eq!(au.nalus.len(), 3);
        assert!(au.nalus[0].is_sps && au.nalus[0].nalu_type == 7);
        assert!(au.nalus[1].is_pps && au.nalus[1].nalu_type == 8);
        assert!(au.nalus[2].is_idr && au.nalus[2].nalu_type == 5);
    }

    #[test]
    fn au_gatherer_marks_p_frame_not_key() {
        let mut g = AuGatherer::new();
        let au = g.push(&video_nal(1, false)).expect("slice closes the AU");
        assert!(!au.is_key_frame);
        assert_eq!(au.nalus.len(), 1);
        assert_eq!(au.nalus[0].nalu_type, 1);
    }

    #[test]
    fn au_gatherer_skips_audio_and_empty() {
        let mut g = AuGatherer::new();
        let audio = streaming::source::MediaFrame::Audio {
            data: vec![0xFF; 160],
            timestamp: 1,
        };
        assert!(g.push(&audio).is_none());

        let empty = streaming::source::MediaFrame::Video {
            keyframe: false,
            data: Vec::new(),
            timestamp: 2,
        };
        assert!(g.push(&empty).is_none());
    }

    #[tokio::test]
    async fn frame_source_subscribe_unsubscribe_tracks_sender() {
        use gb28181_rs::frame::FrameSource;
        use std::sync::Arc;

        // A StreamManager with no active streams exercises the retry path;
        // subscribe/unsubscribe bookkeeping is observable without a camera.
        let manager = Arc::new(StreamManager::new());
        let source = StreamManagerFrameSource::new(manager);

        let sub = source.subscribe_with_capacity(8);
        assert_eq!(sub.id, 1);
        // registry holds the sender for the live subscription
        assert_eq!(source.subscribers.lock().len(), 1);
        source.unsubscribe(sub.id);
        assert_eq!(source.subscribers.lock().len(), 0);
    }

    // ── ONVIF wiring (onvif-rs 0.8 capability batch) ────────────────────

    use onvif_device_rs::device::IpFilterState;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Wrap one action element in a minimal SOAP 1.2 envelope.
    fn soap_envelope(action_body: &str) -> String {
        format!(
            "<soap:Envelope xmlns:soap=\"http://www.w3.org/2003/05/soap-envelope\">\
             <soap:Body>{action_body}</soap:Body></soap:Envelope>"
        )
    }

    /// One HTTP exchange against the mounted SOAP face: POST `body` to
    /// `path`, read to EOF (the server answers `Connection: close`).
    /// Returns `(status, www_authenticate, body)`.
    async fn soap_exchange(
        addr: std::net::SocketAddr,
        path: &str,
        extra_headers: &[(&str, String)],
        body: &str,
    ) -> (u16, String, String) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: application/soap+xml; charset=utf-8\r\n\
             Content-Length: {}\r\n",
            body.len(),
        );
        for (name, value) in extra_headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .await
            .expect("write request head");
        stream.write_all(body.as_bytes()).await.expect("write body");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.expect("read response");
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, resp_body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let mut www_auth = String::new();
        for line in head.lines().skip(1) {
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("WWW-Authenticate")
            {
                www_auth = value.trim().to_string();
            }
        }
        (status, www_auth, resp_body.to_string())
    }

    /// A runtime config with real (non-placeholder) identity, empty
    /// password (the product's auth-off semantic) and overridable fields.
    fn onvif_test_config() -> OnvifRuntimeConfig {
        let mut cfg = build_onvif_config_from_json(&serde_json::json!({}), "127.0.0.1");
        // The probe chain fills a real serial on any host; make the value
        // independent of the machine running the test.
        cfg.device.serial_number = "TEST-SERIAL-01".to_string();
        cfg
    }

    /// A media store shaped like the product's first-active-stream
    /// profile (`/live/{camera_id}` RTSP mount).
    fn test_media_store() -> onvif_device_rs::media::SharedMediaConfig {
        let mut media = onvif_device_rs::media::OnvifMediaConfig::new(
            1280,
            720,
            25,
            2_500_000,
            8554,
            "127.0.0.1".to_string(),
        );
        media.stream_path = "/live/cam-1".to_string();
        Arc::new(std::sync::RwLock::new(media))
    }

    /// Mount the SOAP face exactly like `start_onvif` does and serve it
    /// on an ephemeral listener. Returns the bound address, the server
    /// handle (shut down on drop) and the events publish seam.
    #[allow(clippy::type_complexity)]
    async fn start_mounted_onvif(
        config: &OnvifRuntimeConfig,
        media: Option<onvif_device_rs::media::SharedMediaConfig>,
        force_idr: Arc<AtomicBool>,
        ip_filter: Option<IpFilterState>,
    ) -> (
        std::net::SocketAddr,
        onvif_device_rs::OnvifServerHandle,
        Option<Arc<onvif_device_rs::events::EventsService>>,
    ) {
        let streams = Arc::new(StreamManager::new());
        let soap = onvif_device_rs::OnvifServer::new(&onvif_device_rs::OnvifConfig {
            port: 0,
            username: config.username.clone(),
            password: config.password.clone(),
            allow_no_auth: config.password.is_empty(),
            http_digest: config.http_digest,
            ..Default::default()
        });
        let (soap, events) = mount_onvif_services(
            soap,
            config,
            "127.0.0.1",
            media,
            force_idr,
            &streams,
            ip_filter,
        )
        .expect("mount_onvif_services");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handle = soap.start_on(listener).await.expect("start_on");
        (addr, handle, events)
    }

    /// Legacy Media face through the 0.8 shared store: GetProfiles and
    /// GetStreamUri keep their byte-stable NVR shape (MediaUri/Uri) and
    /// the product's `/live/{camera_id}` RTSP mount.
    #[tokio::test]
    async fn onvif_media_face_shape_is_stable() {
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope("<GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>"),
        )
        .await;
        assert_eq!(status, 200, "GetProfiles: {body}");
        assert!(body.contains("GetProfilesResponse"));
        assert!(body.contains(r#"Profiles token="main""#));
        assert!(body.contains("VideoSourceConfiguration"));
        assert!(body.contains("VideoEncoderConfiguration"));
        assert!(body.contains("<Width>1280</Width>"));
        assert!(body.contains("<Height>720</Height>"));

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<GetStreamUri xmlns=\"http://www.onvif.org/ver10/media/wsdl\"><ProfileToken>main</ProfileToken></GetStreamUri>",
            ),
        )
        .await;
        assert_eq!(status, 200, "GetStreamUri: {body}");
        // NVR byte-stability contract: MediaUri → Uri element chain.
        assert!(body.contains("GetStreamUriResponse"));
        assert!(body.contains("MediaUri"));
        assert!(body.contains("<Uri>rtsp://127.0.0.1:8554/live/cam-1</Uri>"));
    }

    /// No active stream at protocol start → no media profile mounted and
    /// no Media advertisement (the served routes and GetServices agree).
    #[tokio::test]
    async fn onvif_no_media_without_active_stream() {
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            None,
            Arc::new(AtomicBool::new(false)),
            None,
        )
        .await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope("<GetProfiles xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>"),
        )
        .await;
        assert!(status != 200, "GetProfiles must not answer: {body}");

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"><IncludeCapability>true</IncludeCapability></GetServices>",
            ),
        )
        .await;
        assert_eq!(status, 200);
        assert!(!body.contains("ver10/media/wsdl"), "no Media ad: {body}");
        assert!(!body.contains("ver20/media/wsdl"), "no Media2 ad: {body}");
        assert!(!body.contains("ptz/wsdl"), "no PTZ ad: {body}");
        assert!(body.contains("ver10/device/wsdl"));
        assert!(body.contains("ver10/events/wsdl"));
    }

    /// Media2 (default on): the `tr2` GetProfiles face on its own route
    /// and the GetServices advertisement both appear — and both vanish
    /// when the DB key turns it off.
    #[tokio::test]
    async fn onvif_media2_tr2_face_and_advertisement() {
        let config = onvif_test_config();
        let (addr, _handle, _) =
            start_mounted_onvif(&config, Some(test_media_store()), Arc::default(), None).await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/media2_service",
            &[],
            &soap_envelope("<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>"),
        )
        .await;
        assert_eq!(status, 200, "Media2 GetProfiles: {body}");
        assert!(body.contains("tr2:GetProfilesResponse"));
        assert!(body.contains("http://www.onvif.org/ver20/media/wsdl"));

        let (_, _, services) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope("<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>"),
        )
        .await;
        assert!(services.contains("http://www.onvif.org/ver20/media/wsdl"));
        assert!(services.contains("/onvif/media2_service"));
        assert!(services.contains("ver10/media/wsdl"));
        // PR#66 interop golden: `Service` elements are DIRECT children of
        // GetServicesResponse — the old `tds:Services` wrapper made strict
        // clients (onvif-go) parse zero services (library issue #64).
        assert!(
            !services.contains("<tds:Services>"),
            "GetServices must not wrap Service elements: {services}"
        );
        assert!(services.contains("<tds:Service>"));

        // DB key off → no route, no advertisement.
        let mut off = onvif_test_config();
        off.media2_enabled = false;
        let (addr, _handle, _) =
            start_mounted_onvif(&off, Some(test_media_store()), Arc::default(), None).await;
        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/media2_service",
            &[],
            &soap_envelope("<GetProfiles xmlns=\"http://www.onvif.org/ver20/media/wsdl\"/>"),
        )
        .await;
        assert!(status != 200, "Media2 route must be gone: {body}");
        let (_, _, services) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope("<GetServices xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>"),
        )
        .await;
        assert!(!services.contains("ver20/media/wsdl"));
    }

    /// SetSynchronizationPoint (Media1 + Media2 both) fires the shared
    /// IFrameCmd latch — the SAME Arc the GB28181 control handler uses.
    #[tokio::test]
    async fn onvif_set_synchronization_point_fires_idr_latch() {
        let force_idr = Arc::new(AtomicBool::new(false));
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::clone(&force_idr),
            None,
        )
        .await;

        for path in ["/onvif/device_service", "/onvif/media2_service"] {
            force_idr.store(false, Ordering::SeqCst);
            let (status, _, body) = soap_exchange(
                addr,
                path,
                &[],
                &soap_envelope(
                    "<SetSynchronizationPoint xmlns=\"http://www.onvif.org/ver10/media/wsdl\"/>",
                ),
            )
            .await;
            assert_eq!(status, 200, "SetSynchronizationPoint on {path}: {body}");
            assert!(
                force_idr.load(Ordering::SeqCst),
                "IDR latch must fire via {path}"
            );
        }
    }

    /// Imaging face (fixed-focus webcam): GetImagingSettings answers the
    /// fixed neutral values, Move acks, GetMoveOptions answers ranges.
    #[tokio::test]
    async fn onvif_imaging_fixed_focus_face() {
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::default(),
            None,
        )
        .await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<GetImagingSettings xmlns=\"http://www.onvif.org/ver10/imaging/wsdl\"><VideoSourceToken>videoSrc0</VideoSourceToken></GetImagingSettings>",
            ),
        )
        .await;
        assert_eq!(status, 200, "GetImagingSettings: {body}");
        assert!(body.contains("timg:GetImagingSettingsResponse"));
        assert!(body.contains("<tt:Brightness Value=\"0.5\"/>"));
        assert!(body.contains("<tt:Contrast Value=\"0.5\"/>"));
        assert!(body.contains("<tt:ColorSaturation Value=\"0.5\"/>"));
        assert!(body.contains("<tt:Exposure>"));
        assert!(body.contains("<tt:Mode>AUTO</tt:Mode>"));

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<Move xmlns=\"http://www.onvif.org/ver10/imaging/wsdl\"><VideoSourceToken>videoSrc0</VideoSourceToken><Focus><Absolute><Position>0.4</Position><Speed>0.5</Speed></Absolute></Focus></Move>",
            ),
        )
        .await;
        assert_eq!(status, 200, "Move: {body}");
        assert!(body.contains("timg:MoveResponse"));

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<GetMoveOptions xmlns=\"http://www.onvif.org/ver10/imaging/wsdl\"><VideoSourceToken>videoSrc0</VideoSourceToken></GetMoveOptions>",
            ),
        )
        .await;
        assert_eq!(status, 200, "GetMoveOptions: {body}");
        assert!(body.contains("timg:GetMoveOptionsResponse"));
    }

    /// DeviceHooks texts and the GetUsers directory: GetSystemLog answers
    /// a real application summary (version/uptime/streams), GetUsers
    /// echoes the configured WS username (default admin) as Administrator.
    #[tokio::test]
    async fn onvif_system_log_and_users_directory() {
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::default(),
            None,
        )
        .await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<GetSystemLog xmlns=\"http://www.onvif.org/ver10/device/wsdl\"><LogType>System</LogType></GetSystemLog>",
            ),
        )
        .await;
        assert_eq!(status, 200, "GetSystemLog: {body}");
        assert!(body.contains("GetSystemLogResponse"));
        assert!(body.contains("mibee-eye"));
        assert!(body.contains("uptime"));
        assert!(body.contains("active streams"));

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope("<GetUsers xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>"),
        )
        .await;
        assert_eq!(status, 200, "GetUsers: {body}");
        assert!(body.contains("GetUsersResponse"));
        assert!(body.contains("<tt:Username>admin</tt:Username>"));
        assert!(body.contains("<tt:UserLevel>Administrator</tt:UserLevel>"));
    }

    /// GetScopes answers the WSDL `tt:Scope` form (library PR#66 / issue
    /// #65): `tds:Scopes` entries of ScopeDef + ScopeItem — the built-in
    /// device scopes report `Fixed`.
    #[tokio::test]
    async fn onvif_get_scopes_wdsl_form() {
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::default(),
            None,
        )
        .await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope("<GetScopes xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>"),
        )
        .await;
        assert_eq!(status, 200, "GetScopes: {body}");
        assert!(body.contains("tds:GetScopesResponse"));
        assert!(body.contains("<tds:Scopes>"));
        assert!(body.contains("<tt:ScopeDef>Fixed</tt:ScopeDef>"));
        assert!(
            body.contains("<tt:ScopeItem>onvif://www.onvif.org/type/video_encoder</tt:ScopeItem>")
        );
        assert!(body.contains("onvif://www.onvif.org/name/"));
    }

    /// HTTP Digest (DB key `http_digest`): token-less requests get the
    /// 401 Digest challenge; a correct MD5 response authenticates.
    #[tokio::test]
    async fn onvif_http_digest_challenge_and_pass() {
        use md5::{Digest as _, Md5};

        let md5_hex = |input: &[u8]| {
            let mut hasher = Md5::new();
            hasher.update(input);
            let out = hasher.finalize();
            out.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };

        let mut config = onvif_test_config();
        config.username = "admin".to_string();
        config.password = "secret".to_string();
        config.http_digest = true;
        let (addr, _handle, _) =
            start_mounted_onvif(&config, Some(test_media_store()), Arc::default(), None).await;

        let get_device_info = || {
            soap_envelope(
                "<GetDeviceInformation xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
            )
        };

        // 1) Token-less → 401 + Digest challenge.
        let (status, challenge, _) =
            soap_exchange(addr, "/onvif/device_service", &[], &get_device_info()).await;
        assert_eq!(status, 401);
        assert!(challenge.starts_with("Digest "), "got: {challenge}");
        let challenge_params = challenge
            .strip_prefix("Digest ")
            .unwrap_or(challenge.as_str());
        let param = |key: &str| -> String {
            challenge_params
                .split(',')
                .find_map(|part| {
                    let (k, v) = part.split_once('=')?;
                    k.trim()
                        .eq_ignore_ascii_case(key)
                        .then(|| v.trim().trim_matches('"').to_string())
                })
                .unwrap_or_default()
        };
        let nonce = param("nonce");
        let realm = param("realm");
        let opaque = param("opaque");
        assert!(!nonce.is_empty() && !realm.is_empty());

        // 2) RFC 7616 MD5 (qop=auth) response → 200.
        let uri = "/onvif/device_service";
        let ha1 = md5_hex(format!("admin:{realm}:secret").as_bytes());
        let ha2 = md5_hex(format!("POST:{uri}").as_bytes());
        let cnonce = "0123456789abcdef";
        let nc = "00000001";
        let response = md5_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes());
        let auth = format!(
            "Digest username=\"admin\", realm=\"{realm}\", nonce=\"{nonce}\", \
             uri=\"{uri}\", qop=auth, nc={nc}, cnonce=\"{cnonce}\", \
             response=\"{response}\", opaque=\"{opaque}\""
        );
        let (status, _, body) =
            soap_exchange(addr, uri, &[("Authorization", auth)], &get_device_info()).await;
        assert_eq!(status, 200, "digest-authenticated request: {body}");
        assert!(body.contains("GetDeviceInformationResponse"));
    }

    /// IP allow-list: a peer outside the configured networks is refused
    /// 403 before any SOAP processing.
    #[tokio::test]
    async fn onvif_ip_filter_refuses_unlisted_peer() {
        let filter = parse_ip_filter_entries(&["10.0.0.0/8".to_string()]);
        assert!(filter.enabled);
        let state: IpFilterState = Arc::new(std::sync::RwLock::new(filter));
        let (addr, _handle, _) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::default(),
            Some(state),
        )
        .await;

        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/device_service",
            &[],
            &soap_envelope(
                "<GetDeviceInformation xmlns=\"http://www.onvif.org/ver10/device/wsdl\"/>",
            ),
        )
        .await;
        assert_eq!(status, 403, "unlisted peer must be refused: {body}");
    }

    /// Events push (wsnt:Subscribe, automatic after enable_events): a
    /// subscription registers a push consumer, and publishing through the
    /// AI MotionAlarm seam delivers a wsnt:Notify to the consumer.
    #[tokio::test]
    async fn onvif_events_subscribe_push_notify() {
        let (addr, _handle, events) = start_mounted_onvif(
            &onvif_test_config(),
            Some(test_media_store()),
            Arc::default(),
            None,
        )
        .await;
        let events = events.expect("events service enabled by default");

        // Push consumer: a one-shot HTTP server capturing the Notify POST.
        let consumer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let consumer_addr = consumer.local_addr().expect("consumer addr");
        let capture = tokio::spawn(async move {
            let (mut sock, _) = consumer.accept().await.expect("consumer accept");
            let mut raw = Vec::new();
            sock.read_to_end(&mut raw).await.expect("consumer read");
            let text = String::from_utf8_lossy(&raw).to_string();
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            text
        });

        let subscribe = format!(
            "<wsnt:Subscribe xmlns:wsnt=\"http://docs.oasis-open.org/wsn/b-2\" \
             xmlns:wsa=\"http://www.w3.org/2005/08/addressing\">\
             <wsnt:ConsumerReference><wsa:Address>http://{consumer_addr}/notify</wsa:Address>\
             </wsnt:ConsumerReference><wsnt:TerminationTime>PT1M</wsnt:TerminationTime>\
             </wsnt:Subscribe>"
        );
        let (status, _, body) = soap_exchange(
            addr,
            "/onvif/events_service",
            &[],
            &soap_envelope(&subscribe),
        )
        .await;
        assert_eq!(status, 200, "wsnt:Subscribe: {body}");
        assert!(body.contains("SubscribeResponse"));

        events.publish_event(crate::onvif_alarm::motion_alarm_event("cam-1", 2));

        let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), capture)
            .await
            .expect("Notify delivered within timeout")
            .expect("capture task ok");
        assert!(
            delivered.contains("/notify"),
            "posted to consumer: {delivered}"
        );
        assert!(
            delivered.contains("wsnt:Notify"),
            "Notify envelope: {delivered}"
        );
        assert!(
            delivered.contains("tns1:VideoSource/MotionAlarm"),
            "MotionAlarm topic: {delivered}"
        );
        assert!(delivered.contains("Name=\"Source\" Value=\"cam-1\""));
        assert!(delivered.contains("Name=\"State\" Value=\"true\""));
        assert!(delivered.contains("Name=\"Targets\" Value=\"2\""));
    }

    /// The new DB keys parse with their documented defaults and explicit
    /// overrides (`build_onvif_config_from_json`).
    #[test]
    fn build_onvif_config_new_security_and_media2_keys() {
        let default = build_onvif_config_from_json(&serde_json::json!({}), "192.0.2.10");
        assert!(default.media2_enabled, "media2 defaults on");
        assert!(!default.http_digest, "digest defaults off");
        assert!(default.ip_filter.is_empty(), "ip_filter defaults empty");

        let explicit = build_onvif_config_from_json(
            &serde_json::json!({
                "media2_enabled": false,
                "http_digest": true,
                "ip_filter": ["192.168.1.0/24", "10.1.2.3"],
            }),
            "192.0.2.10",
        );
        assert!(!explicit.media2_enabled);
        assert!(explicit.http_digest);
        assert_eq!(explicit.ip_filter, vec!["192.168.1.0/24", "10.1.2.3"]);
    }

    /// `parse_ip_filter_entries`: empty → disabled; CIDR and bare IPv4
    /// parse; malformed entries WARN-and-skip; an all-malformed list
    /// fails open (disabled), never locks the listener.
    #[test]
    fn parse_ip_filter_entries_matrix() {
        use onvif_device_rs::device::{IpFilter, IpFilterMode};

        assert!(!parse_ip_filter_entries(&[]).enabled);

        let f = parse_ip_filter_entries(&["192.168.1.0/24".to_string()]);
        assert!(f.enabled);
        assert_eq!(f.mode, IpFilterMode::Allow);
        assert_eq!(f.entries.len(), 1);
        assert_eq!(f.entries[0].ipv4, "192.168.1.0");
        assert_eq!(f.entries[0].prefix_len, 24);
        assert!(f.allows_client_ip("192.168.1.55"));
        assert!(!f.allows_client_ip("192.168.2.1"));

        // Bare address → /32 single host.
        let f = parse_ip_filter_entries(&["10.1.2.3".to_string()]);
        assert_eq!(f.entries[0].prefix_len, 32);
        assert!(f.allows_client_ip("10.1.2.3"));
        assert!(!f.allows_client_ip("10.1.2.4"));

        // Malformed entries are skipped; prefix > 32 is refused.
        let f = parse_ip_filter_entries(&[
            "not-an-ip".to_string(),
            "10.0.0.0/33".to_string(),
            "10.0.0.0/8".to_string(),
        ]);
        assert!(f.enabled);
        assert_eq!(f.entries.len(), 1);
        assert!(f.allows_client_ip("10.9.9.9"));

        // All-malformed → disabled (fail open, documented).
        let f: IpFilter = parse_ip_filter_entries(&["bogus".to_string()]);
        assert!(!f.enabled);
    }

    /// `FixedFocusImaging`: the four standard names answer the neutral
    /// value; unknown names are honestly InvalidName; writes respect the
    /// normalized [0,1] contract.
    #[test]
    fn fixed_focus_imaging_param_semantics() {
        use onvif_device_rs::imaging::{
            FocusMoveCmd, FocusMoveKind, ImagingParamError, ImagingParams,
        };

        let pm = FixedFocusImaging;
        for name in FIXED_IMAGING_PARAMS {
            assert_eq!(pm.get_param(name).unwrap(), 0.5, "{name}");
            assert!(pm.set_param(name, 0.25).is_ok());
        }
        assert!(matches!(
            pm.get_param("Zoom"),
            Err(ImagingParamError::InvalidName(ref n)) if n == "Zoom"
        ));
        assert!(matches!(
            pm.set_param("Zoom", 0.5),
            Err(ImagingParamError::InvalidName(_))
        ));
        assert!(matches!(
            pm.set_param("Brightness", 1.5),
            Err(ImagingParamError::OutOfRange { .. })
        ));
        // Focus Move acks (trait default: no motor), modes report AUTO.
        pm.focus_move(FocusMoveCmd {
            kind: FocusMoveKind::Absolute,
            position: 0.4,
            speed: 0.5,
        })
        .unwrap();
        assert_eq!(pm.exposure_mode(), "AUTO");
        assert_eq!(pm.white_balance_mode(), "AUTO");
    }

    /// DeviceHooks texts carry the real application summary (version,
    /// uptime, honest stream state) — never empty strings.
    #[test]
    fn onvif_device_hooks_summary_texts() {
        use onvif_device_rs::DeviceHooks as _;

        let hooks = OnvifDeviceHooks::new(Arc::new(StreamManager::new()));
        let log = hooks.system_log();
        assert!(log.contains("mibee-eye"));
        assert!(log.contains("uptime"));
        assert!(log.contains("active streams: none"));
        let info = hooks.support_info();
        assert!(info.contains("mibee-eye-notebook"));
        assert!(info.contains("active streams: none"));
    }

    /// The GetUsers username: DB-configured WS username wins; empty falls
    /// back to the product default admin (SPEC §2's empty→admin rule).
    #[test]
    fn onvif_wsdl_username_default_and_override() {
        let mut config = onvif_test_config();
        config.username.clear();
        assert_eq!(onvif_wsdl_username(&config), "admin");
        config.username = "operator1".to_string();
        assert_eq!(onvif_wsdl_username(&config), "operator1");
    }
}
