//! Central manager for camera stream lifecycles.
//!
//! [`StreamManager`] tracks active streams in a [`HashMap`], creates appropriate
//! [`Source`] + [`Output`] + [`StreamHub`] pipelines for each camera, and
//! manages the start/stop lifecycle of each stream.
//!
//! # Resource limits
//!
//! The manager uses [`ResourceController`] to bound concurrent streams
//! (default max 16). Stream creation will block or fail when the limit
//! is reached.
//!
//! # Example
//!
//! ```ignore
//! let manager = StreamManager::new();
//!
//! // Start a USB camera on /dev/video0
//! let info = manager
//!     .create_stream(
//!         "cam-1".into(),
//!         "usb",
//!         &serde_json::json!({"device_index": 0}),
//!         None,
//!     )
//!     .await?;
//!
//! // Stop it
//! manager.stop_stream("cam-1").await?;
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use sqlx::SqlitePool;
use tokio::sync::{RwLock, broadcast, oneshot, watch};
use tracing::{info, warn};
use uuid::Uuid;

use protocols::rtsp_server::RtspServer;
use streaming::hub::{HubHandle, OutputId, StreamHub};
use streaming::output::{FileOutput, Output, RtmpOutput, RtspOutput};
use streaming::resource::ResourceController;
use streaming::source::Source;

// ── Constants ────────────────────────────────────────────────────────────────

/// Minimal SDP body for H.264 video over RTP.
const SDP_BODY: &str = concat!(
    "v=0\r\n",
    "o=- 0 0 IN IP4 0.0.0.0\r\n",
    "s=mibee-eye\r\n",
    "c=IN IP4 0.0.0.0\r\n",
    "t=0 0\r\n",
    "m=video 0 RTP/AVP 96\r\n",
    "a=rtpmap:96 H264/90000\r\n",
    "a=fmtp:96 packetization-mode=1\r\n",
    "a=control:track1\r\n",
);

/// Default RTSP port for stream URLs.
const RTSP_PORT: u16 = 8554;

/// Default maximum concurrent streams.
const DEFAULT_MAX_STREAMS: usize = 16;

// ── StreamStatus ─────────────────────────────────────────────────────────────

/// The operational status of a managed stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum StreamStatus {
    /// Stream is actively running.
    Running,
    /// Stream has been stopped (graceful shutdown).
    Stopped,
    /// Stream encountered an error.
    #[serde(rename = "error")]
    Error(String),
}

// ── StreamInfo ───────────────────────────────────────────────────────────────

/// Response type for stream info queries.
#[derive(Debug, Clone, Serialize)]
pub struct StreamInfo {
    /// Camera identifier.
    pub camera_id: String,
    /// RTSP URL for clients to connect to (if configured).
    pub rtsp_url: Option<String>,
    /// RTMP URL for clients to connect to (if configured).
    pub rtmp_url: Option<String>,
    /// Current stream status.
    pub status: StreamStatus,
}

// ── StreamHandle ─────────────────────────────────────────────────────────────

/// Latest-JPEG snapshot handle, shared between the capture source and the web
/// UI's snapshot endpoint. `None` until the first frame has been encoded.
pub type LatestJpeg = Arc<Mutex<Option<Arc<[u8]>>>>;

/// Cached (SPS, PPS) NAL pair harvested from the frame broadcast, so new MSE
/// clients can bootstrap an init segment without waiting for the next IDR.
pub type SpsPpsCache = Arc<Mutex<Option<(Vec<u8>, Vec<u8>)>>>;

/// Live capture-dimensions handle (width/height once the source starts).
type DimensionsHandle =
    Arc<parking_lot::Mutex<Option<streaming::capture_source::StreamDimensions>>>;

/// Typed capture-source construction result: the boxed source plus the
/// JPEG-tap handles only `VideoCaptureSource` exposes.
type SourceBundle = (
    Box<dyn Source>,
    Option<LatestJpeg>,
    Option<DimensionsHandle>,
    Option<broadcast::Sender<Arc<[u8]>>>,
    // Substream frame broadcast (SPEC appendix A #20); None = disabled.
    Option<broadcast::Sender<Arc<streaming::source::MediaFrame>>>,
    // Parsed substream settings (None = disabled/unsupported arm).
    Option<streaming::capture_source::SubstreamSettings>,
);

struct StreamHandle {
    /// Join handle for the spawned pipeline task.
    join_handle: Option<tokio::task::JoinHandle<()>>,
    /// Watch sender to signal stream stop.
    stop_tx: Option<watch::Sender<bool>>,
    /// RTSP URL for external access.
    rtsp_url: Option<String>,
    /// RTMP URL we are pushing to (if RTMP push enabled).
    rtmp_url: Option<String>,
    /// Handle for attaching outputs at runtime (None if stream not running
    /// or hub handle already extracted).
    hub_handle: Option<HubHandle>,
    /// Latest-JPEG snapshot handle (for the snapshot endpoint).
    latest_jpeg: Option<LatestJpeg>,
    /// JPEG preview broadcast sender (for the MJPEG live-preview endpoint).
    jpeg_tx: Option<broadcast::Sender<Arc<[u8]>>>,
    /// Cached (SPS, PPS) NAL units extracted from the broadcast, so new MSE
    /// clients can bootstrap an init segment without waiting for the next IDR.
    /// `None` until the first IDR has been observed.
    sps_pps_cache: Option<SpsPpsCache>,
    /// Substream (SPEC appendix A #20): frame broadcast sender, its own
    /// SPS/PPS harvester cache and the RTSP `/live/{id}/sub` URL. All
    /// `None` when the camera runs without a substream.
    sub_frames: Option<broadcast::Sender<Arc<streaming::source::MediaFrame>>>,
    sub_sps_pps_cache: Option<SpsPpsCache>,
    /// Substream geometry (w, h, fps, bitrate) for the ONVIF sub profile.
    sub_settings: Option<streaming::capture_source::SubstreamSettings>,
    /// RTSP mounts this stream registered, each paired with a handle to the
    /// server it lives on.
    ///
    /// `RtspServer::live_streams` is insert-only from the server's point of
    /// view — without an explicit unregister, a stopped camera's mount stayed
    /// DESCRIBE-able and an NVR could PLAY it and then hang waiting for frames
    /// that will never come (and, before the registry entry was fixed to own
    /// its parked receiver, every stop leaked a 300-slot broadcast ring).
    /// `stop_stream` drains this and unregisters each mount.
    rtsp_mounts: Vec<(RtspServer, String)>,
    /// Current status.
    status: StreamStatus,
}

// ── StreamManager ────────────────────────────────────────────────────────────

/// Central manager for camera stream lifecycles.
///
/// Tracks active streams, creates capture → encode → stream pipelines,
/// and enforces resource limits via [`ResourceController`].
pub struct StreamManager {
    /// Active streams keyed by camera ID.
    streams: RwLock<HashMap<String, StreamHandle>>,
    /// Resource controller for bounding concurrent streams.
    resource_controller: ResourceController,
    /// Hostname/IP advertised in stream URLs returned to clients.
    /// Resolved at startup from config or auto-detected LAN IP.
    advertised_host: String,
    /// Optional DB connection for reading protocol configs (RTMP push, etc.).
    /// When None, no protocol-driven outputs are auto-attached.
    db: Option<SqlitePool>,
    /// Shared local-recording pause gate (platform RecordCmd via GB28181).
    recording_pause_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Per-camera published MP4 segment slots (#30-C media_ref): the
    /// FileOutput of each recording stream writes its current segment
    /// path here; hearing records resolve the covering file.
    segment_slots: Arc<std::sync::Mutex<HashMap<String, streaming::output::SegmentSlot>>>,
    /// Shared IFrameCmd latch consumed by each camera's encode loop.
    force_idr_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Shared GB FrameMirror runtime flags (DeviceConfig A.2.3.2.9),
    /// XOR-composed with each camera's static mount flips per frame.
    gb_flips: Option<Arc<streaming::capture_source::Flips>>,
    /// AI detection engine. When present and active, one detection worker
    /// is spawned per started camera stream (it taps the JPEG preview
    /// broadcast and exits with the stream).
    ai: Option<Arc<streaming::ai::AiEngine>>,
}

impl StreamManager {
    /// Create a new stream manager with the default max (16) concurrent streams.
    #[tracing::instrument(skip_all)]
    pub fn new() -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            resource_controller: ResourceController::new(DEFAULT_MAX_STREAMS),
            advertised_host: "localhost".to_string(),
            db: None,
            recording_pause_flag: None,
            segment_slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            force_idr_flag: None,
            gb_flips: None,
            ai: None,
        }
    }

    /// Create a new stream manager with a custom maximum number of concurrent
    /// streams.
    pub fn with_max_streams(max_streams: usize) -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            resource_controller: ResourceController::new(max_streams),
            advertised_host: "localhost".to_string(),
            db: None,
            recording_pause_flag: None,
            segment_slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            force_idr_flag: None,
            gb_flips: None,
            ai: None,
        }
    }

    /// Create a new stream manager with an explicit advertised host.
    ///
    /// The `advertised_host` is used when constructing RTSP/RTMP/etc. URLs
    /// returned to clients so they can reach this machine from the network.
    /// Use this in production; [`new`](Self::new) defaults to `"localhost`.
    pub fn with_host(advertised_host: String) -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            resource_controller: ResourceController::new(DEFAULT_MAX_STREAMS),
            advertised_host,
            db: None,
            recording_pause_flag: None,
            segment_slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            force_idr_flag: None,
            gb_flips: None,
            ai: None,
        }
    }

    /// Create a new stream manager with advertised host AND DB connection.
    ///
    /// The DB is used to read protocol configs (e.g., RTMP push enable/url)
    /// at stream-creation time, so Web UI config changes take effect on the
    /// next stream start (no restart required for new streams).
    #[tracing::instrument(skip_all)]
    pub fn with_host_and_db(advertised_host: String, db: SqlitePool) -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            resource_controller: ResourceController::new(DEFAULT_MAX_STREAMS),
            advertised_host,
            db: Some(db),
            recording_pause_flag: None,
            segment_slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            force_idr_flag: None,
            gb_flips: None,
            ai: None,
        }
    }

    /// Attach the AI detection engine so started streams get a detection
    /// worker. No-op effect when the engine is inactive.
    #[must_use]
    /// Share the local-recording pause gate (platform RecordCmd): every
    /// FileOutput attached afterwards reads the same flag.
    pub fn with_recording_pause_flag(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.recording_pause_flag = Some(flag);
        self
    }

    /// Share the DeviceControl IFrameCmd latch: set by the GB28181 control
    /// handler, consumed by the next camera encode loop pass.
    pub fn with_force_idr_flag(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.force_idr_flag = Some(flag);
        self
    }

    /// Share the GB FrameMirror runtime flags (DeviceConfig A.2.3.2.9):
    /// every camera started afterwards composes them with its static
    /// mount-compensation flips.
    #[must_use]
    pub fn with_gb_flips(mut self, flips: Arc<streaming::capture_source::Flips>) -> Self {
        self.gb_flips = Some(flips);
        self
    }

    pub fn with_ai(mut self, ai: Arc<streaming::ai::AiEngine>) -> Self {
        self.ai = Some(ai);
        self
    }

    /// Return the number of available stream slots.
    pub fn available_permits(&self) -> usize {
        self.resource_controller.available_permits()
    }

    /// Return the maximum number of concurrent streams.
    pub fn max_streams(&self) -> usize {
        self.resource_controller.max_streams()
    }

    /// Start streaming from a camera.
    ///
    /// Creates the appropriate [`Source`] based on `camera_type`, sets up
    /// protocol outputs (e.g., RTSP via [`RtspOutput`]), constructs the
    /// pipeline through [`StreamHub`], and spawns it as a background task.
    ///
    /// # Parameters
    ///
    /// * `camera_id` — Unique identifier for the camera.
    /// * `camera_type` — Type of camera (`"usb"`).
    /// * `config` — JSON configuration with type-specific fields:
    ///   - `"usb"`: `{ "device_index": <u64> }`
    /// * `rtsp_server` — Optional [`RtspServer`] for publishing the stream
    ///   so RTSP clients can connect.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The camera type is unsupported.
    /// * The resource limit has been reached (use [`available_permits`](Self::available_permits)).
    /// * A stream for this `camera_id` already exists.
    /// * Source creation or pipeline setup fails.
    #[tracing::instrument(skip_all, fields(camera_id))]
    pub async fn create_stream(
        &self,
        camera_id: String,
        camera_type: &str,
        config: &serde_json::Value,
        rtsp_server: Option<&RtspServer>,
    ) -> Result<StreamInfo> {
        // ── 1. Guard: resource limit ───────────────────────────────────
        //
        // Acquire a permit *before* creating the source so we fail fast
        // when the system is at capacity.
        let _permit = self.resource_controller.try_acquire().ok_or_else(|| {
            anyhow::anyhow!(
                "all {} stream slots are exhausted; try again later",
                DEFAULT_MAX_STREAMS
            )
        })?;

        // ── 2. Guard: duplicate stream ─────────────────────────────────
        {
            let streams = self.streams.read().await;
            if streams.contains_key(&camera_id) {
                anyhow::bail!("stream already exists for camera {camera_id}");
            }
        }

        // ── 3. Create source based on camera type ───────────────────────
        //
        // We need to extract the JPEG-tap handles (latest_jpeg, jpeg sender)
        // *before* the source is boxed as `dyn Source`, since those methods
        // are specific to `VideoCaptureSource`. So we keep the typed value
        // around for handle extraction, then box it.
        let (source, latest_jpeg, dimensions, jpeg_tx, sub_frames, sub_settings): SourceBundle =
            match camera_type {
                "usb" => {
                    let device_index = config
                        .get("device_index")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            anyhow::anyhow!("USB camera config must include 'device_index'")
                        })?;
                    let mut vcs =
                        streaming::capture_source::VideoCaptureSource::new(device_index as usize);
                    if let Some(flag) = &self.force_idr_flag {
                        vcs = vcs.with_force_idr_flag(Arc::clone(flag));
                    }
                    if let Some(flips) = &self.gb_flips {
                        vcs = vcs.with_gb_flips(Arc::clone(flips));
                    }
                    // Adapt the encoder to the host: probe once (cached) and pick
                    // the recommended quality preset. A user-set override from the
                    // camera config (`quality_preset`) takes precedence when present.
                    let preset = config
                        .get("quality_preset")
                        .and_then(|v| v.as_str())
                        .and_then(parse_quality_preset)
                        .unwrap_or_else(|| streaming::capability::probe().recommended_quality);
                    vcs = vcs.with_quality_preset(preset);
                    // Honour an explicit target fps from the camera config so the
                    // encoder's GOP matches the real capture rate.
                    if let Some(fps) = config.get("target_fps").and_then(|v| v.as_f64())
                        && fps > 0.0
                    {
                        vcs = vcs.with_target_fps(fps as f32);
                    }
                    // Device-level flips from the camera config — permanent,
                    // baked into the encoded stream and snapshots.
                    let hflip = config
                        .get("hflip")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let vflip = config
                        .get("vflip")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    vcs = vcs.with_flips(hflip, vflip);
                    // Device-level rotation (SPEC v1 appendix A #19), same
                    // bake-in semantics; 90/270 swap the stream geometry —
                    // applied on this (re)start like the flips.
                    let rotation = config.get("rotation").and_then(|v| v.as_u64());
                    if let Some(rotation) = rotation
                        && !matches!(rotation, 0 | 90 | 180 | 270)
                    {
                        anyhow::bail!(
                            "camera config rotation must be 0, 90, 180 or 270, got {rotation}"
                        );
                    }
                    vcs = vcs.with_rotation(rotation.unwrap_or(0) as u32);
                    // Watermark (SPEC v1 §5.2): device-global `protocols.watermark`,
                    // read at use time like `protocols.recording` — a config change
                    // applies on the next stream (re)start. Burned pre-encode into
                    // every camera's frames.
                    if let Some(db) = &self.db
                        && let Ok(Some(cfg)) = crate::db::get_protocol_config(db, "watermark").await
                    {
                        match serde_json::from_value::<streaming::watermark::WatermarkSettings>(cfg)
                        {
                            Ok(wm_cfg) if wm_cfg.enabled => {
                                match streaming::watermark::Watermark::new(&wm_cfg) {
                                    Ok(wm) => {
                                        info!(%camera_id, position = ?wm_cfg.position, font_size = wm_cfg.font_size, "watermark attached");
                                        vcs = vcs.with_watermark(wm);
                                    }
                                    Err(e) => {
                                        warn!(%camera_id, error = %e, "watermark init failed; streaming without watermark")
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!(%camera_id, error = %e, "invalid protocols.watermark config in DB; streaming without watermark")
                            }
                        }
                    }
                    // Substream (SPEC appendix A #20): per-camera
                    // `config.substream`, read-at-use like the other keys —
                    // applies on stream (re)start.
                    let sub_settings = parse_substream_config(config)?;
                    if let Some(settings) = &sub_settings {
                        vcs = vcs.with_substream(settings.clone());
                    }
                    let latest = vcs.latest_jpeg_handle();
                    let dims = vcs.dimensions_handle();
                    let tx = vcs.jpeg_sender();
                    let sub_tx = vcs.sub_frames_sender();
                    (
                        Box::new(vcs),
                        Some(latest),
                        Some(dims),
                        tx,
                        sub_tx,
                        sub_settings,
                    )
                }
                other => anyhow::bail!("unsupported camera type: {other}"),
            };

        // ── 3b. Spawn the AI detection worker (when enabled) ────────
        //
        // The worker taps the JPEG preview broadcast — it never touches the
        // capture/encode path — and exits automatically when the stream
        // stops (broadcast closed), clearing its state entry.
        if let Some(ai) = &self.ai
            && ai.is_active()
            && let Some(jpeg_tx) = &jpeg_tx
        {
            ai.spawn_worker(camera_id.clone(), jpeg_tx.subscribe());
        }

        // ── 4. Set up pipeline ──────────────────────────────────────────
        let (stop_tx, stop_rx) = watch::channel(false);
        let rtsp_base = format!("rtsp://{}:{}", self.advertised_host, RTSP_PORT);

        let (
            run_handle,
            rtsp_url,
            rtmp_url,
            hub_handle,
            sps_pps_cache,
            sub_sps_pps_cache,
            rtsp_mounts,
        ) = {
            let mut hub = StreamHub::new(source, self.resource_controller.clone());
            let stream_url: Option<String>;
            // RTSP mounts registered below, released by `stop_stream`.
            let mut rtsp_mounts: Vec<(RtspServer, String)> = Vec::new();

            // Register with RTSP server if provided.
            if let Some(server) = rtsp_server {
                let stream_path = format!("live/{}", camera_id);
                let ssrc = Uuid::new_v4().as_u128() as u32;

                // Oneshot channel to receive SPS/PPS from RtspOutput and update the SDP.
                let (sps_pps_tx, sps_pps_rx) = oneshot::channel::<(Vec<u8>, Vec<u8>)>();
                let server_clone = server.clone();
                let path_clone = stream_path.clone();

                tokio::spawn(async move {
                    if let Ok((sps, pps)) = sps_pps_rx.await {
                        server_clone.update_sps_pps(&path_clone, sps, pps);
                        info!(path = %path_clone, "SDP updated with sprop-parameter-sets");
                    }
                });

                let frame_tx =
                    server.register_live_stream(stream_path.clone(), SDP_BODY.to_string(), ssrc);

                let mut output = RtspOutput::with_channel(
                    stream_path.clone(),
                    SDP_BODY.to_string(),
                    ssrc,
                    frame_tx,
                );
                output.set_sps_pps_tx(sps_pps_tx);

                hub.add_output(Box::new(output)).await;
                rtsp_mounts.push((server.clone(), stream_path.clone()));
                stream_url = Some(format!("{}/{}", rtsp_base, stream_path));
                info!(%camera_id, rtsp_url = %stream_url.as_ref().unwrap(), "RTSP output registered");
            } else {
                stream_url = None;
            }

            // ── 4a-bis. Substream RTSP mount (SPEC appendix A #20) ──────
            //
            // `live/{id}/sub` fed by a forwarder from the sub frame
            // broadcast; the SDP's sprop-parameter-sets are harvested from
            // the same broadcast (second subscriber).
            if let Some(sub_tx) = sub_frames.as_ref()
                && let Some(server) = rtsp_server
            {
                let sub_path = format!("live/{}/sub", camera_id);
                let sub_ssrc = Uuid::new_v4().as_u128() as u32;
                let frame_tx =
                    server.register_live_stream(sub_path.clone(), SDP_BODY.to_string(), sub_ssrc);
                let mut frx = sub_tx.subscribe();
                tokio::spawn(async move {
                    loop {
                        match frx.recv().await {
                            Ok(frame) => {
                                if let streaming::source::MediaFrame::Video { data, .. } = &*frame
                                    && frame_tx.send(data.clone()).is_err()
                                {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                });

                // One-shot SDP sprop update once both parameter sets
                // have been seen (mirrors the main mount's oneshot).
                let mut hrx = sub_tx.subscribe();
                let server_clone = server.clone();
                let path_clone = sub_path.clone();
                tokio::spawn(async move {
                    let mut sps: Option<Vec<u8>> = None;
                    let mut pps: Option<Vec<u8>> = None;
                    while let Ok(frame) = hrx.recv().await {
                        let streaming::source::MediaFrame::Video { data, .. } = &*frame else {
                            continue;
                        };
                        if data.is_empty() {
                            continue;
                        }
                        match data[0] & 0x1f {
                            7 => sps = Some(data.clone()),
                            8 => pps = Some(data.clone()),
                            _ => {}
                        }
                        if let (Some(s), Some(p)) = (sps.clone(), pps.clone()) {
                            server_clone.update_sps_pps(&path_clone, s, p);
                            break;
                        }
                    }
                });

                rtsp_mounts.push((server.clone(), sub_path.clone()));
            }

            // ── 4b. Optional RTMP push output ───────────────────────────
            //
            // If the DB-backed `rtmp_push` config has `enabled: true`,
            // construct an RtmpOutput from push_url + stream_name and
            // attach it to the hub. Frames will be pushed to the external
            // RTMP ingest point in parallel with RTSP serving.
            //
            // Read happens at stream creation time; toggling RTMP via
            // Web UI requires stopping and restarting the stream.
            let mut rtmp_url: Option<String> = None;
            if let Some(db) = &self.db {
                match crate::db::get_protocol_config(db, "rtmp_push").await {
                    Ok(Some(cfg)) => {
                        let enabled = cfg
                            .get("enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if enabled {
                            let push_url =
                                cfg.get("push_url").and_then(|v| v.as_str()).unwrap_or("");
                            let stream_name = cfg
                                .get("stream_name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("stream");
                            if !push_url.is_empty() {
                                let full_url = if push_url.ends_with(stream_name) {
                                    push_url.to_string()
                                } else {
                                    format!("{}/{}", push_url.trim_end_matches('/'), stream_name)
                                };
                                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    RtmpOutput::new(&full_url)
                                })) {
                                    Ok(mut output) => {
                                        if let Err(e) = output.start().await {
                                            warn!(%camera_id, error = %e, "RTMP output failed to start; stream will continue without RTMP push");
                                        } else {
                                            info!(%camera_id, rtmp_url = %full_url, "RTMP push output attached");
                                            hub.add_output(Box::new(output)).await;
                                            rtmp_url = Some(full_url);
                                        }
                                    }
                                    Err(_) => {
                                        warn!(%camera_id, "RtmpOutput::new panicked; skipping RTMP push");
                                    }
                                }
                            } else {
                                warn!(%camera_id, "RTMP push enabled but push_url is empty; skipping");
                            }
                        }
                    }
                    Ok(None) => {
                        // No rtmp_push config in DB — silently skip.
                    }
                    Err(e) => {
                        warn!(%camera_id, error = %e, "failed to read rtmp_push config from DB; skipping RTMP push");
                    }
                }
            }

            // ── 4c. Optional local recording output ──────────────────────
            //
            // If the DB-backed `recording` config has `enabled: true`,
            // construct a FileOutput that muxes H.264 into rolling MP4
            // segments. ffmpeg handles segmentation; we run a periodic
            // pruning pass to enforce the capacity limit.
            if let Some(db) = &self.db {
                match crate::db::get_protocol_config(db, "recording").await {
                    Ok(Some(cfg)) => {
                        let enabled = cfg
                            .get("enabled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if enabled {
                            let path = cfg
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("./recordings");
                            let seg = cfg
                                .get("segment_duration_secs")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(900);
                            let cap = cfg
                                .get("max_capacity_mb")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(10_240);
                            let mut output = FileOutput::new(path, &camera_id, seg, cap);
                            if let Some(flag) = &self.recording_pause_flag {
                                output = output.with_pause_flag(Arc::clone(flag));
                            }
                            let slot: streaming::output::SegmentSlot =
                                Arc::new(parking_lot::Mutex::new(None));
                            output = output.with_segment_slot(Arc::clone(&slot));
                            self.segment_slots
                                .lock()
                                .expect("segment slot registry")
                                .insert(camera_id.clone(), slot);
                            // Attach the live dimensions handle so the muxer's
                            // track metadata reflects the real negotiated
                            // resolution instead of the 1280x720 default.
                            if let Some(dims) = &dimensions {
                                output = output.with_dimensions_handle(Arc::clone(dims));
                            }
                            match output.start().await {
                                Ok(()) => {
                                    info!(%camera_id, path, segment_secs = seg, "FileOutput attached");
                                    hub.add_output(Box::new(output)).await;
                                }
                                Err(e) => {
                                    warn!(%camera_id, error = %e, "FileOutput failed to start; stream will continue without local recording");
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(%camera_id, error = %e, "failed to read recording config; skipping local recording");
                    }
                }
            }

            // Start the pipeline.
            let handle = hub.run().await;

            // Capture a runtime handle BEFORE moving hub into the monitor task.
            // This lets external callers (e.g., GB28181 INVITE handler) attach
            // additional outputs to this stream after it has started.
            let hub_handle = hub.handle();

            // Spawn a background "SPS/PPS harvester" that subscribes to the
            // frame broadcast and caches the most recent SPS (NAL type 7) and
            // PPS (type 8). New MSE/fMP4 clients read this cache so they can
            // build an init segment immediately, without waiting up to a full
            // GOP for the next IDR to carry fresh parameter sets.
            let harvester_handle = hub_handle.clone();
            let sps_pps_cache: SpsPpsCache = Arc::new(Mutex::new(None));
            let cache_clone = Arc::clone(&sps_pps_cache);
            tokio::spawn(async move {
                let mut rx = harvester_handle.subscribe_frames();
                let mut have_sps = false;
                let mut have_pps = false;
                while let Ok(frame) = rx.recv().await {
                    let streaming::source::MediaFrame::Video { data, .. } = &*frame else {
                        continue;
                    };
                    if data.is_empty() {
                        continue;
                    }
                    let nal_type = data[0] & 0x1f;
                    match nal_type {
                        7 => {
                            let mut g = cache_clone.lock();
                            let entry = g.get_or_insert_with(|| (Vec::new(), Vec::new()));
                            entry.0 = data.clone();
                            have_sps = true;
                        }
                        8 => {
                            let mut g = cache_clone.lock();
                            let entry = g.get_or_insert_with(|| (Vec::new(), Vec::new()));
                            entry.1 = data.clone();
                            have_pps = true;
                        }
                        _ => {}
                    }
                    if have_sps && have_pps {
                        // Both seen once; keep updating but no need to log again.
                    }
                }
            });

            // Spawn a monitor task that calls hub.stop() when the external
            // stop signal is received.
            let cam_id = camera_id.clone();
            tokio::spawn(async move {
                let mut rx = stop_rx;
                let _ = rx.changed().await;
                info!(%cam_id, "stream stop signal received, stopping pipeline");
                hub.stop();
            });

            // Substream SPS/PPS harvester for the MSE init-segment cache
            // (mirror of the main harvester above).
            let sub_sps_pps_cache: Option<SpsPpsCache> = sub_frames.as_ref().map(|sub_tx| {
                let cache: SpsPpsCache = Arc::new(Mutex::new(None));
                let cache_clone = Arc::clone(&cache);
                let mut rx = sub_tx.subscribe();
                tokio::spawn(async move {
                    while let Ok(frame) = rx.recv().await {
                        let streaming::source::MediaFrame::Video { data, .. } = &*frame else {
                            continue;
                        };
                        if data.is_empty() {
                            continue;
                        }
                        match data[0] & 0x1f {
                            7 => {
                                let mut g = cache_clone.lock();
                                g.get_or_insert_with(|| (Vec::new(), Vec::new())).0 = data.clone();
                            }
                            8 => {
                                let mut g = cache_clone.lock();
                                g.get_or_insert_with(|| (Vec::new(), Vec::new())).1 = data.clone();
                            }
                            _ => {}
                        }
                    }
                });
                cache
            });

            (
                handle,
                stream_url,
                rtmp_url,
                Some(hub_handle),
                Some(sps_pps_cache),
                sub_sps_pps_cache,
                rtsp_mounts,
            )
        };

        // ── 5. Store the stream handle ──────────────────────────────────
        let handle = StreamHandle {
            join_handle: Some(run_handle),
            stop_tx: Some(stop_tx),
            rtsp_url: rtsp_url.clone(),
            rtmp_url: rtmp_url.clone(),
            hub_handle,
            latest_jpeg,
            jpeg_tx,
            sps_pps_cache,
            sub_frames: sub_frames.clone(),
            sub_sps_pps_cache,
            sub_settings,
            rtsp_mounts,
            status: StreamStatus::Running,
        };

        {
            let mut streams = self.streams.write().await;
            streams.insert(camera_id.clone(), handle);
        }

        info!(%camera_id, %camera_type, rtsp_url = ?rtsp_url, "stream created");
        let status = StreamStatus::Running;

        Ok(StreamInfo {
            camera_id,
            rtsp_url,
            rtmp_url,
            status,
        })
    }

    /// Stop a running stream.
    ///
    /// Sends the stop signal, awaits the pipeline task with a 5-second
    /// timeout, and removes the stream from tracking.
    ///
    /// Returns an error if the camera ID is not found.
    #[tracing::instrument(skip_all, fields(camera_id))]
    /// Adopt a shared segment registry (the host creates it before the
    /// audio bridges spawn and hands the same Arc to the manager).
    #[must_use]
    pub fn with_segment_slots(
        mut self,
        slots: Arc<std::sync::Mutex<HashMap<String, streaming::output::SegmentSlot>>>,
    ) -> Self {
        self.segment_slots = slots;
        self
    }

    /// Registry lookup for hosts that hold the shared registry before
    /// the manager exists (hearing bridges): same semantics as
    /// [`StreamManager::segment_covering`].
    pub fn segment_covering(
        registry: &std::sync::Mutex<HashMap<String, streaming::output::SegmentSlot>>,
        camera_id: &str,
        ts_ms: u64,
    ) -> Option<String> {
        let slot = {
            let slots = registry.lock().expect("segment slot registry");
            Arc::clone(slots.get(camera_id)?)
        };
        let guard = slot.lock();
        match guard.as_ref() {
            Some((path, opened_ms)) if ts_ms >= *opened_ms => Some(path.clone()),
            _ => None,
        }
    }

    /// The MP4 segment file covering `ts_ms` for a recording stream
    /// (#30-C `media_ref`): the segment currently open, when it opened
    /// at or before the timestamp. None when the camera is not
    /// recording or the timestamp predates the current segment
    /// (rotation race — fail open to no reference).
    pub fn segment_covering_for(&self, camera_id: &str, ts_ms: u64) -> Option<String> {
        let slot = {
            let slots = self.segment_slots.lock().expect("segment slot registry");
            Arc::clone(slots.get(camera_id)?)
        };
        let guard = slot.lock();
        match guard.as_ref() {
            Some((path, opened_ms)) if ts_ms >= *opened_ms => Some(path.clone()),
            _ => None,
        }
    }

    pub async fn stop_stream(&self, camera_id: &str) -> Result<StreamInfo> {
        // Remove the handle from tracking first so concurrent calls see it gone.
        let mut handle = {
            let mut streams = self.streams.write().await;
            streams
                .remove(camera_id)
                .ok_or_else(|| anyhow::anyhow!("no active stream for camera {camera_id}"))?
        };

        // Recording correlation ends with the stream — later events
        // must not point at the dead stream's last segment.
        self.segment_slots
            .lock()
            .expect("segment slot registry")
            .remove(camera_id);

        // Release the RTSP mounts. Without this the server keeps advertising
        // `live/{id}` (and `live/{id}/sub`) as DESCRIBE-able after the encoder
        // is gone — an NVR pulling a stopped camera hangs instead of getting a
        // clean 404/454.
        for (server, path) in handle.rtsp_mounts.drain(..) {
            if server.unregister_live_stream(&path) {
                info!(%camera_id, mount = %path, "RTSP mount unregistered");
            }
        }

        // Signal stop.
        if let Some(tx) = handle.stop_tx.take() {
            let _ = tx.send(true);
        }

        // Await the pipeline task with timeout.
        if let Some(jh) = handle.join_handle.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), jh).await {
                Ok(Ok(())) => {
                    info!(%camera_id, "stream stopped gracefully");
                }
                Ok(Err(e)) => {
                    warn!(%camera_id, error = %e, "pipeline task panicked");
                }
                Err(_) => {
                    warn!(%camera_id, "stream stop timed out after 5s, abandoning handle");
                }
            }
        }

        handle.status = StreamStatus::Stopped;

        info!(%camera_id, "stream stopped");
        Ok(StreamInfo {
            camera_id: camera_id.to_string(),
            rtsp_url: handle.rtsp_url,
            rtmp_url: handle.rtmp_url,
            status: StreamStatus::Stopped,
        })
    }

    /// Stop all active streams during graceful shutdown.
    #[tracing::instrument(skip_all)]
    pub async fn shutdown_all(&self) {
        let camera_ids: Vec<String> = {
            let streams = self.streams.read().await;
            streams.keys().cloned().collect()
        };
        for id in camera_ids {
            if let Err(e) = self.stop_stream(&id).await {
                warn!(%id, error = %e, "failed to stop stream during shutdown");
            }
        }
    }

    /// Return information about all currently tracked streams.
    #[tracing::instrument(skip_all)]
    pub async fn list_active_streams(&self) -> Vec<StreamInfo> {
        let streams = self.streams.read().await;
        streams
            .iter()
            .map(|(camera_id, handle)| StreamInfo {
                camera_id: camera_id.clone(),
                rtsp_url: handle.rtsp_url.clone(),
                rtmp_url: handle.rtmp_url.clone(),
                status: handle.status.clone(),
            })
            .collect()
    }

    /// Return the number of currently active (tracked) streams.
    #[tracing::instrument(skip_all)]
    pub async fn active_stream_count(&self) -> usize {
        self.streams.read().await.len()
    }

    /// Best-effort synchronous snapshot of the tracked camera IDs
    /// (`None` when the streams lock is contended). Diagnostic surfaces
    /// that cannot await — e.g. the ONVIF DeviceHooks texts (a sync trait
    /// served from async handlers) — report through this; anything that
    /// can await must use [`Self::list_active_streams`] instead.
    pub fn try_active_camera_ids(&self) -> Option<Vec<String>> {
        let streams = self.streams.try_read().ok()?;
        Some(streams.keys().cloned().collect())
    }

    /// Check whether a stream for the given camera ID is currently tracked
    /// (active).
    pub async fn has_stream(&self, camera_id: &str) -> bool {
        self.streams.read().await.contains_key(camera_id)
    }

    /// Return the most recent JPEG frame for a camera, if available.
    ///
    /// Used by the snapshot endpoint to serve a single frame without joining
    /// the encode loop. Returns `None` if the camera has no active stream or
    /// no frame has been captured yet (the first frame may take ~100 ms).
    pub async fn latest_jpeg(&self, camera_id: &str) -> Option<Arc<[u8]>> {
        let streams = self.streams.read().await;
        let handle = streams.get(camera_id)?;
        let latest = handle.latest_jpeg.as_ref()?;
        latest.lock().clone()
    }

    /// Subscribe to the JPEG preview broadcast for a camera.
    ///
    /// Used by the MJPEG live-preview endpoint to stream frames to a web
    /// client. Returns `None` if the camera has no active stream.
    pub async fn subscribe_jpeg(&self, camera_id: &str) -> Option<broadcast::Receiver<Arc<[u8]>>> {
        let streams = self.streams.read().await;
        let handle = streams.get(camera_id)?;
        let tx = handle.jpeg_tx.as_ref()?;
        Some(tx.subscribe())
    }

    /// Subscribe to the encoded-frame broadcast for a camera.
    ///
    /// Used by the MSE/fMP4 HTTP endpoint to pull H.264 NAL units and remux
    /// them into fragmented MP4 for browser `MediaSource` playback. Returns
    /// `None` if the camera has no active stream or no hub handle.
    pub async fn subscribe_frames(
        &self,
        camera_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<Arc<streaming::source::MediaFrame>>> {
        let streams = self.streams.read().await;
        let handle = streams.get(camera_id)?;
        let hub_handle = handle.hub_handle.as_ref()?;
        Some(hub_handle.subscribe_frames())
    }

    /// Return the cached (SPS, PPS) NAL units for a camera, if both have been
    /// observed by the background harvester.
    ///
    /// New MSE clients use this to bootstrap an fMP4 init segment immediately
    /// rather than waiting up to a full GOP for the next IDR to carry fresh
    /// parameter sets. Returns `None` if the stream is unknown or no IDR has
    /// been seen yet.
    pub async fn sps_pps(&self, camera_id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        let streams = self.streams.read().await;
        let handle = streams.get(camera_id)?;
        let cache = handle.sps_pps_cache.as_ref()?;
        cache.lock().clone()
    }

    /// Subscribe to a camera's SUBSTREAM frame broadcast (SPEC appendix
    /// A #20). `None` when the camera is unknown or runs without a
    /// substream.
    pub async fn subscribe_frames_sub(
        &self,
        camera_id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<Arc<streaming::source::MediaFrame>>> {
        let streams = self.streams.read().await;
        let handle = streams.get(camera_id)?;
        let tx = handle.sub_frames.as_ref()?;
        Some(tx.subscribe())
    }

    /// Cached (SPS, PPS) of a camera's substream, if observed.
    pub async fn sps_pps_sub(&self, camera_id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        let streams = self.streams.read().await;
        let handle = streams.get(camera_id)?;
        let cache = handle.sub_sps_pps_cache.as_ref()?;
        cache.lock().clone()
    }

    /// Whether any active stream runs with a substream (drives the
    /// device-level `capabilities.substream`, SPEC appendix A #20).
    pub async fn any_substream_active(&self) -> bool {
        let streams = self.streams.read().await;
        streams.values().any(|h| h.sub_frames.is_some())
    }

    /// The first active stream's substream geometry (ONVIF `sub` profile —
    /// the ONVIF media service advertises the first active camera, so its
    /// substream is the one the extra profile must describe).
    pub async fn first_active_substream(
        &self,
    ) -> Option<streaming::capture_source::SubstreamSettings> {
        let streams = self.streams.read().await;
        streams
            .values()
            .find(|h| h.sub_frames.is_some())
            .and_then(|h| h.sub_settings.clone())
    }

    /// Attach a new output to an already-running stream.
    ///
    /// This is the runtime entry point used by external protocol handlers
    /// (e.g., GB28181 INVITE) to push frames from an active camera pipeline
    /// to a newly-connected consumer.
    ///
    /// Returns `Ok(OutputId)` if the output was attached; `Err` if the camera is
    /// not running or the runtime hub handle is no longer available
    /// (e.g., the stream is shutting down).
    #[tracing::instrument(skip_all, fields(camera_id))]
    pub async fn add_output_to_stream(
        &self,
        camera_id: &str,
        output: Box<dyn Output>,
    ) -> Result<OutputId> {
        let streams = self.streams.read().await;
        let handle = streams
            .get(camera_id)
            .ok_or_else(|| anyhow::anyhow!("no active stream for camera {camera_id}"))?;
        let hub_handle = handle
            .hub_handle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no hub handle for camera {camera_id}"))?;
        let id = hub_handle.add_output_at_runtime(output).await;
        Ok(id)
    }

    /// Remove a runtime-attached output from a running stream.
    ///
    /// Detaches the output by its [`OutputId`] and signals its task to stop.
    /// Called from protocol handlers (e.g., GB28181 BYE) when an external
    /// consumer disconnects.
    #[tracing::instrument(skip_all, fields(camera_id))]
    pub async fn remove_output_from_stream(
        &self,
        camera_id: &str,
        output_id: OutputId,
    ) -> Result<()> {
        let streams = self.streams.read().await;
        let handle = streams
            .get(camera_id)
            .ok_or_else(|| anyhow::anyhow!("no active stream for camera {camera_id}"))?;
        let hub_handle = handle
            .hub_handle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no hub handle for camera {camera_id}"))?;
        hub_handle.remove_output(output_id).await;
        Ok(())
    }
}

impl Default for StreamManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a [`QualityPreset`] from the string form used in JSON camera configs.
///
/// Accepts the serde kebab-case names (`"ultra-fast"`, `"medium"`, `"high"`,
/// `"hardware-max"`). Returns `None` for unknown values so the caller falls
/// back to the host-recommended preset.
fn parse_quality_preset(s: &str) -> Option<streaming::capability::QualityPreset> {
    use streaming::capability::QualityPreset;
    match s.trim() {
        "ultra-fast" => Some(QualityPreset::UltraFast),
        "medium" => Some(QualityPreset::Medium),
        "high" => Some(QualityPreset::High),
        "hardware-max" => Some(QualityPreset::HardwareMax),
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Validate the `config.substream` subtree without building settings —
/// used at the PUT boundary so mistakes surface next to the save, not at
/// the next stream start.
pub fn validate_substream_config(config: &serde_json::Value) -> anyhow::Result<()> {
    parse_substream_config(config).map(|_| ())
}

/// Parse the per-camera `config.substream` subtree (SPEC appendix A #20).
/// Absent or `enabled: false` → `None`; structural violations (odd/zero
/// dims, absurd bitrate) are errors — same shape as the rotation guard.
fn parse_substream_config(
    config: &serde_json::Value,
) -> anyhow::Result<Option<streaming::capture_source::SubstreamSettings>> {
    use streaming::capture_source::SubstreamSettings;
    let Some(node) = config.get("substream") else {
        return Ok(None);
    };
    let enabled = node
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }
    let width = node.get("width").and_then(|v| v.as_u64()).unwrap_or(640) as u32;
    let height = node.get("height").and_then(|v| v.as_u64()).unwrap_or(360) as u32;
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        anyhow::bail!(
            "camera config substream dimensions must be positive and even, got {width}x{height}"
        );
    }
    if width > 7680 || height > 4320 {
        anyhow::bail!("camera config substream dimensions exceed 7680x4320");
    }
    let fps = node.get("fps").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
    let bitrate_bps = node
        .get("bitrate")
        .and_then(|v| v.as_u64())
        .unwrap_or(400_000) as u32;
    if bitrate_bps == 0 || bitrate_bps > 50_000_000 {
        anyhow::bail!("camera config substream bitrate must be 1..50000000, got {bitrate_bps}");
    }
    Ok(Some(SubstreamSettings {
        width,
        height,
        fps,
        bitrate_bps,
    }))
}

#[cfg(test)]
mod segment_slot_tests {
    use super::*;

    fn registry_with(
        cam: &str,
        entry: Option<(&str, u64)>,
    ) -> std::sync::Mutex<HashMap<String, streaming::output::SegmentSlot>> {
        let mut map = HashMap::new();
        if let Some((path, opened)) = entry {
            let slot: streaming::output::SegmentSlot =
                Arc::new(parking_lot::Mutex::new(Some((path.to_string(), opened))));
            map.insert(cam.to_string(), slot);
        }
        std::sync::Mutex::new(map)
    }

    #[test]
    fn covering_returns_segment_when_ts_fits() {
        let reg = registry_with("cam0", Some(("recordings/cam0_20261001.mp4", 1_000)));
        let got = StreamManager::segment_covering(&reg, "cam0", 5_000);
        assert_eq!(got.as_deref(), Some("recordings/cam0_20261001.mp4"));
    }

    #[test]
    fn ts_before_open_or_unknown_camera_is_none() {
        // Rotation race: event older than the current segment's open.
        let reg = registry_with("cam0", Some(("recordings/late.mp4", 10_000)));
        assert_eq!(StreamManager::segment_covering(&reg, "cam0", 9_999), None);
        // Camera not recording / unknown.
        let reg = registry_with("cam0", None);
        assert_eq!(StreamManager::segment_covering(&reg, "cam0", 99_999), None);
        let reg = registry_with("cam0", Some(("x.mp4", 1)));
        assert_eq!(StreamManager::segment_covering(&reg, "other", 5), None);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_substream_config_absent_and_disabled() {
        let cfg = serde_json::json!({"device_index": 0});
        assert!(parse_substream_config(&cfg).unwrap().is_none());
        let cfg = serde_json::json!({"substream": {"enabled": false}});
        assert!(parse_substream_config(&cfg).unwrap().is_none());
    }

    #[test]
    fn parse_substream_config_defaults_and_values() {
        let cfg = serde_json::json!({"substream": {"enabled": true}});
        let s = parse_substream_config(&cfg).unwrap().unwrap();
        assert_eq!((s.width, s.height), (640, 360));
        assert_eq!(s.bitrate_bps, 400_000);
        assert_eq!(s.fps, 0.0);

        let cfg = serde_json::json!({"substream": {"enabled": true, "width": 480, "height": 270, "fps": 5, "bitrate": 250000}});
        let s = parse_substream_config(&cfg).unwrap().unwrap();
        assert_eq!((s.width, s.height, s.bitrate_bps), (480, 270, 250_000));
        assert_eq!(s.fps, 5.0);
    }

    #[test]
    fn parse_substream_config_rejects_odd_dims_and_bad_bitrate() {
        for bad in [
            serde_json::json!({"substream": {"enabled": true, "width": 641}}),
            serde_json::json!({"substream": {"enabled": true, "height": 0}}),
            serde_json::json!({"substream": {"enabled": true, "bitrate": 0}}),
            serde_json::json!({"substream": {"enabled": true, "bitrate": 60000000}}),
        ] {
            assert!(parse_substream_config(&bad).is_err(), "must reject {bad}");
        }
    }

    use super::*;

    #[tokio::test]
    async fn test_stream_manager_new() {
        let manager = StreamManager::new();
        assert_eq!(manager.max_streams(), DEFAULT_MAX_STREAMS);
        assert_eq!(manager.available_permits(), DEFAULT_MAX_STREAMS);
        assert_eq!(manager.active_stream_count().await, 0);
    }

    #[tokio::test]
    async fn test_stream_manager_with_max_streams() {
        let manager = StreamManager::with_max_streams(4);
        assert_eq!(manager.max_streams(), 4);
        assert_eq!(manager.available_permits(), 4);
    }

    #[tokio::test]
    async fn test_list_active_streams_empty() {
        let manager = StreamManager::new();
        let streams = manager.list_active_streams().await;
        assert!(streams.is_empty());
    }

    #[tokio::test]
    async fn test_has_stream_returns_false_for_unknown() {
        let manager = StreamManager::new();
        assert!(!manager.has_stream("nonexistent").await);
    }

    #[tokio::test]
    async fn test_stop_stream_returns_error_for_unknown() {
        let manager = StreamManager::new();
        let err = manager.stop_stream("nonexistent").await.unwrap_err();
        assert!(err.to_string().contains("no active stream"));
    }

    #[tokio::test]
    async fn test_create_stream_rejects_unsupported_type() {
        let manager = StreamManager::new();
        let err = manager
            .create_stream("cam-1".into(), "unknown-type", &serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unsupported camera type"),
            "expected unsupported type error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_create_stream_usb_missing_device_index() {
        let manager = StreamManager::new();
        let err = manager
            .create_stream("cam-1".into(), "usb", &serde_json::json!({}), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("device_index"),
            "expected missing device_index error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_create_stream_rejects_duplicate() {
        // We can't actually create a real source in tests (no hardware), but
        // we can verify the duplicate guard fires.  The create call will fail
        // because VideoCaptureSource::new(9999) tries to open a device that
        // doesn't exist when start() is called inside hub.run().  We rely on
        // the fact that the guard check happens *before* any source creation
        // that could panic.
        //
        // Actually the guard only checks the HashMap.  The source is created
        // before insertion.  So the second call will also try to create a
        // source but fail.  Let's test the duplicate guard directly:
        let manager = StreamManager::new();

        // Pre-insert a dummy handle to test the guard.
        {
            let (_tx, _rx) = watch::channel(false);
            let handle = StreamHandle {
                join_handle: None,
                stop_tx: None,
                rtsp_url: None,
                rtmp_url: None,
                hub_handle: None,
                latest_jpeg: None,
                jpeg_tx: None,
                sps_pps_cache: None,
                sub_frames: None,
                sub_sps_pps_cache: None,
                sub_settings: None,
                rtsp_mounts: Vec::new(),
                status: StreamStatus::Running,
            };
            manager.streams.write().await.insert("cam-1".into(), handle);
        }

        let err = manager
            .create_stream(
                "cam-1".into(),
                "usb",
                &serde_json::json!({"device_index": 0}),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected duplicate error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_list_active_streams_after_insert() {
        let manager = StreamManager::new();

        // Pre-insert a dummy handle.
        {
            let handle = StreamHandle {
                join_handle: None,
                stop_tx: None,
                rtsp_url: Some("rtsp://localhost:8554/live/test".into()),
                rtmp_url: None,
                hub_handle: None,
                latest_jpeg: None,
                jpeg_tx: None,
                sps_pps_cache: None,
                sub_frames: None,
                sub_sps_pps_cache: None,
                sub_settings: None,
                rtsp_mounts: Vec::new(),
                status: StreamStatus::Running,
            };
            manager
                .streams
                .write()
                .await
                .insert("test-cam".into(), handle);
        }

        let streams = manager.list_active_streams().await;
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].camera_id, "test-cam");
        assert_eq!(
            streams[0].rtsp_url.as_deref(),
            Some("rtsp://localhost:8554/live/test")
        );
        assert_eq!(streams[0].status, StreamStatus::Running);
        assert!(streams[0].rtmp_url.is_none());
    }

    #[tokio::test]
    async fn test_stop_stream_removes_tracking() {
        let manager = StreamManager::new();

        // Pre-insert a dummy handle with a stop channel so stop_stream works.
        let (stop_tx, _stop_rx) = watch::channel(false);
        {
            let handle = StreamHandle {
                join_handle: None,
                stop_tx: Some(stop_tx),
                rtsp_url: None,
                rtmp_url: None,
                hub_handle: None,
                latest_jpeg: None,
                jpeg_tx: None,
                sps_pps_cache: None,
                sub_frames: None,
                sub_sps_pps_cache: None,
                sub_settings: None,
                rtsp_mounts: Vec::new(),
                status: StreamStatus::Running,
            };
            manager
                .streams
                .write()
                .await
                .insert("stop-me".into(), handle);
        }

        // Stop the stream (no join handle to await = immediate).
        let info = manager.stop_stream("stop-me").await.unwrap();
        assert_eq!(info.camera_id, "stop-me");
        assert_eq!(info.status, StreamStatus::Stopped);

        // Should no longer be in the tracked set.
        assert!(!manager.has_stream("stop-me").await);
    }

    /// Regression: `RtspServer`'s live-stream registry was insert-only, so a
    /// stopped camera stayed DESCRIBE-able and an NVR could PLAY a mount that
    /// produces nothing and hang. `stop_stream` must unregister every mount the
    /// stream registered (main + substream).
    #[tokio::test]
    async fn test_stop_stream_unregisters_its_rtsp_mounts() {
        let manager = StreamManager::new();
        let server = RtspServer::new(protocols::rtsp_server::RtspServerConfig {
            port: 0,
            auth_required: false,
            ..Default::default()
        });

        server.register_live_stream("live/stop-me".to_string(), "s=L".to_string(), 1);
        server.register_live_stream("live/stop-me/sub".to_string(), "s=S".to_string(), 2);

        let (stop_tx, _stop_rx) = watch::channel(false);
        let handle = StreamHandle {
            join_handle: None,
            stop_tx: Some(stop_tx),
            rtsp_url: Some("rtsp://localhost:8554/live/stop-me".into()),
            rtmp_url: None,
            hub_handle: None,
            latest_jpeg: None,
            jpeg_tx: None,
            sps_pps_cache: None,
            sub_frames: None,
            sub_sps_pps_cache: None,
            sub_settings: None,
            rtsp_mounts: vec![
                (server.clone(), "live/stop-me".to_string()),
                (server.clone(), "live/stop-me/sub".to_string()),
            ],
            status: StreamStatus::Running,
        };
        manager
            .streams
            .write()
            .await
            .insert("stop-me".into(), handle);

        manager.stop_stream("stop-me").await.unwrap();

        // Both mounts must be gone: DESCRIBE now 404s instead of hanging.
        assert!(
            !server.unregister_live_stream("live/stop-me"),
            "main mount survived stop_stream"
        );
        assert!(
            !server.unregister_live_stream("live/stop-me/sub"),
            "substream mount survived stop_stream"
        );
    }

    #[tokio::test]
    async fn test_active_stream_count() {
        let manager = StreamManager::new();
        assert_eq!(manager.active_stream_count().await, 0);

        // Insert a dummy.
        {
            let handle = StreamHandle {
                join_handle: None,
                stop_tx: None,
                rtsp_url: None,
                rtmp_url: None,
                hub_handle: None,
                latest_jpeg: None,
                jpeg_tx: None,
                sps_pps_cache: None,
                sub_frames: None,
                sub_sps_pps_cache: None,
                sub_settings: None,
                rtsp_mounts: Vec::new(),
                status: StreamStatus::Running,
            };
            manager.streams.write().await.insert("a".into(), handle);
        }
        assert_eq!(manager.active_stream_count().await, 1);

        // Insert another.
        {
            let handle = StreamHandle {
                join_handle: None,
                stop_tx: None,
                rtsp_url: None,
                rtmp_url: None,
                hub_handle: None,
                latest_jpeg: None,
                jpeg_tx: None,
                sps_pps_cache: None,
                sub_frames: None,
                sub_sps_pps_cache: None,
                sub_settings: None,
                rtsp_mounts: Vec::new(),
                status: StreamStatus::Running,
            };
            manager.streams.write().await.insert("b".into(), handle);
        }
        assert_eq!(manager.active_stream_count().await, 2);

        // Remove one.
        manager.streams.write().await.remove("a");
        assert_eq!(manager.active_stream_count().await, 1);
    }

    #[test]
    fn test_stream_status_serialization() {
        let running = serde_json::to_value(StreamStatus::Running).unwrap();
        assert_eq!(running, serde_json::json!("Running"));

        let stopped = serde_json::to_value(StreamStatus::Stopped).unwrap();
        assert_eq!(stopped, serde_json::json!("Stopped"));

        let err = serde_json::to_value(StreamStatus::Error("oops".into())).unwrap();
        assert_eq!(err, serde_json::json!({"error": "oops"}));
    }

    #[test]
    fn test_stream_info_serialization() {
        let info = StreamInfo {
            camera_id: "cam-x".into(),
            rtsp_url: Some("rtsp://localhost:8554/live/cam-x".into()),
            rtmp_url: None,
            status: StreamStatus::Running,
        };
        let val = serde_json::to_value(&info).unwrap();
        assert_eq!(val["camera_id"], "cam-x");
        assert_eq!(val["rtsp_url"], "rtsp://localhost:8554/live/cam-x");
        assert!(val.get("rtmp_url").unwrap().is_null());
        assert_eq!(val["status"], "Running");
    }

    #[test]
    fn test_stream_info_serialization_roundtrip() {
        let info = StreamInfo {
            camera_id: "cam-y".into(),
            rtsp_url: None,
            rtmp_url: None,
            status: StreamStatus::Error("device disconnected".into()),
        };
        let json = serde_json::to_string(&info).unwrap();
        // We can't deserialize back because StreamInfo doesn't derive
        // Deserialize (it's an output-only type). Just verify the JSON shape.
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["camera_id"], "cam-y");
        assert_eq!(val["status"]["error"], "device disconnected");
    }

    /// Verify that DEFAULT_MAX_STREAMS is consistent.
    #[test]
    fn test_default_max_streams_constant() {
        assert_eq!(DEFAULT_MAX_STREAMS, 16);
    }

    /// Verify that SDP_BODY is a valid-looking SDP.
    #[test]
    fn test_sdp_body_format() {
        assert!(SDP_BODY.starts_with("v=0\r\n"));
        assert!(SDP_BODY.contains("H264/90000"));
        assert!(SDP_BODY.ends_with("\r\n"));
    }
}
