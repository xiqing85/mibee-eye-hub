use anyhow::{Context, Result};
use prometheus::{
    Encoder, Gauge, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, Registry, TextEncoder,
};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Container for all custom Prometheus metrics.
///
/// Constructed once via [`register_metrics`] and stored in a global `OnceLock`.
/// Tests can construct independent `Metrics` instances to avoid global-state
/// conflicts.
pub struct Metrics {
    registry: Registry,
    active_streams: IntGauge,
    bytes_received: IntCounterVec,
    capture_errors: IntCounterVec,
    // ─── Protocol-specific counters ───────────────────────────
    rtsp_sessions: IntCounterVec,
    rtsp_bytes_sent: IntCounter,
    rtmp_push_bytes: IntCounter,
    rtmp_push_errors: IntCounter,
    onvif_discovery_requests: IntCounter,
    gb28181_register_status: IntCounterVec,
    audio_level_db: GaugeVec,
    // ─── New metrics ────────────────────────
    http_requests_total: IntCounterVec,
    auth_failures_total: IntCounterVec,
    recording_active: IntGauge,
    frame_drops_total: IntCounter,
    ai_inferences_total: IntCounter,
    // ─── Per-model resource metrics (SPEC §3.3 + appendix A #39) ──
    model_inferences_total: IntCounterVec,
    model_errors_total: IntCounterVec,
    model_inference_seconds: HistogramVec,
    model_cpu_seconds: HistogramVec,
    model_inflight: IntGaugeVec,
    model_tokens_total: IntCounterVec,
    // ─── Resource gauges (SPEC appendix A #38) ─────────────────────
    system_cpu_percent: Gauge,
    system_memory_total_bytes: IntGauge,
    system_memory_available_bytes: IntGauge,
    process_cpu_percent: Gauge,
    process_resident_memory_bytes: IntGauge,
    process_open_fds: IntGauge,
    system_net_rx_bytes: IntGauge,
    system_net_tx_bytes: IntGauge,
    // ─── Boot-time feature admission (SPEC appendix A #40) ──
    resource_budget_mib: IntGauge,
    feature_admitted: IntGaugeVec,
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

static GLOBAL_METRICS: OnceLock<Metrics> = OnceLock::new();

/// Register all custom metrics with the global Prometheus registry.
///
/// Must be called exactly once during application startup. Calling it a second
/// time will return an error.
pub fn register_metrics() -> Result<()> {
    let metrics = Metrics::new().context("failed to create metrics")?;
    // Idempotent: an already-set global satisfies the postcondition
    // (in-process re-entry and the full test suite both land here; the
    // OnceCell keeps the first instance either way).
    let _ = GLOBAL_METRICS.set(metrics);
    Ok(())
}

fn global_metrics() -> &'static Metrics {
    GLOBAL_METRICS
        .get()
        .expect("metrics not registered — call register_metrics() during startup")
}

// ---------------------------------------------------------------------------
// Public API functions
// ---------------------------------------------------------------------------

/// Increment the `mibee_eye_bytes_received` counter for a camera.
pub fn increment_bytes_received(camera_id: &str, bytes: u64) {
    global_metrics()
        .bytes_received
        .with_label_values(&[camera_id])
        .inc_by(bytes);
}

/// Increment the `mibee_eye_capture_errors` counter for a camera.
pub fn increment_capture_errors(camera_id: &str) {
    global_metrics()
        .capture_errors
        .with_label_values(&[camera_id])
        .inc();
}

// ─── Protocol-specific counter helpers ─────────────────────────────────

/// Increment the `mibee_eye_rtsp_sessions_total` counter by status.
pub fn increment_rtsp_sessions(status: &str) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.rtsp_sessions.with_label_values(&[status]).inc();
    }
}

/// Increment the `mibee_eye_rtsp_bytes_sent_total` counter.
pub fn increment_rtsp_bytes_sent(bytes: u64) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.rtsp_bytes_sent.inc_by(bytes);
    }
}

/// Increment the `mibee_eye_rtmp_push_bytes_total` counter.
pub fn increment_rtmp_push_bytes(bytes: u64) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.rtmp_push_bytes.inc_by(bytes);
    }
}

/// Increment the `mibee_eye_rtmp_push_errors_total` counter.
pub fn increment_rtmp_push_errors() {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.rtmp_push_errors.inc();
    }
}

/// Increment the `mibee_eye_onvif_discovery_requests_total` counter.
pub fn increment_onvif_discovery_requests() {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.onvif_discovery_requests.inc();
    }
}

/// Increment the `mibee_eye_gb28181_register_status` counter by status.
pub fn increment_gb28181_register_status(status: &str) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.gb28181_register_status.with_label_values(&[status]).inc();
    }
}

/// Set the `mibee_eye_streams_active` gauge to an absolute value.
pub fn set_active_streams(count: i64) {
    global_metrics().active_streams.set(count);
}
/// Set the `mibee_eye_audio_level_db` gauge for a stream.
///
/// Silently returns if metrics have not yet been registered — this allows
/// audio capture callbacks to fire before `register_metrics()` is called
/// during startup without panicking.
pub fn set_audio_level(stream_id: &str, db_level: f64) {
    // SAFETY: Audio callbacks run on real-time threads; we must not panic.
    // Using GLOBAL_METRICS.get() avoids the `expect()` in global_metrics().
    if let Some(metrics) = GLOBAL_METRICS.get() {
        metrics
            .audio_level_db
            .with_label_values(&[stream_id])
            .set(db_level);
    }
}

// ─── New metric helpers ─────────────────────────────────────────

/// Increment the `mibee_http_requests_total` counter.
pub fn increment_http_requests(method: &str, path: &str, status: u16) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.http_requests_total
            .with_label_values(&[method, path, &status.to_string()])
            .inc();
    }
}

/// Increment the `mibee_auth_failures_total` counter by failure type.
///
/// `failure_type` must be one of `"bad_password"`, `"locked_out"`, or `"rate_limited"`.
pub fn increment_auth_failures(failure_type: &str) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.auth_failures_total
            .with_label_values(&[failure_type])
            .inc();
    }
}

/// Set the `mibee_eyeording_active` gauge to an absolute value.
pub fn set_recording_active(count: i64) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.recording_active.set(count);
    }
}

/// Increment the `mibee_eyeording_active` gauge by 1.
pub fn inc_recording_active() {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.recording_active.inc();
    }
}

/// Decrement the `mibee_eyeording_active` gauge by 1.
pub fn dec_recording_active() {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.recording_active.dec();
    }
}

/// Increment the `mibee_frame_drops_total` counter.
pub fn increment_frame_drops() {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.frame_drops_total.inc();
    }
}

/// Increment the `mibee_ai_inferences_total` counter (one per completed
/// AI detection inference, across all cameras).
pub fn increment_ai_inferences() {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.ai_inferences_total.inc();
    }
}

/// Publish the periodic /proc resource sample as Prometheus gauges
/// (SPEC appendix A #38 — the same numbers `/api/metrics/summary`
/// serves, on the scrape surface). No-op when metrics are not
/// registered.
pub fn publish_resource_gauges(sample: &ResourceSample) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.system_cpu_percent.set(sample.system_cpu_percent);
        m.system_memory_total_bytes
            .set(sample.system_memory_total_bytes as i64);
        m.system_memory_available_bytes
            .set(sample.system_memory_available_bytes as i64);
        m.process_cpu_percent.set(sample.process_cpu_percent);
        m.process_resident_memory_bytes
            .set(sample.process_resident_memory_bytes as i64);
        m.process_open_fds.set(sample.process_open_fds as i64);
        m.system_net_rx_bytes.set(sample.system_net_rx_bytes as i64);
        m.system_net_tx_bytes.set(sample.system_net_tx_bytes as i64);
    }
}

/// Publish the boot-time feature-admission plan (SPEC appendix A #40):
/// the memory budget as a gauge and one 1/0 gauge per feature. Called
/// once after `register_metrics`; no-op when metrics are not registered.
pub fn publish_resource_profile(budget_mib: u64, features: &[(&str, bool)]) {
    if let Some(m) = GLOBAL_METRICS.get() {
        m.resource_budget_mib.set(budget_mib as i64);
        for (name, admitted) in features {
            m.feature_admitted
                .with_label_values(&[name])
                .set(i64::from(*admitted));
        }
    }
}

/// One periodic /proc resource sample for [`publish_resource_gauges`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceSample {
    pub system_cpu_percent: f64,
    pub system_memory_total_bytes: u64,
    pub system_memory_available_bytes: u64,
    pub process_cpu_percent: f64,
    pub process_resident_memory_bytes: u64,
    pub process_open_fds: u64,
    pub system_net_rx_bytes: u64,
    pub system_net_tx_bytes: u64,
}

// ---------------------------------------------------------------------------
// Per-model resource metrics (SPEC §3.3 + appendix A #39)
// ---------------------------------------------------------------------------

/// Histogram buckets for one model invocation (seconds). The range spans
/// wake-word-sized micro-classifiers (sub-ms) to CPU VLM answers (minutes).
const MODEL_SECONDS_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 120.0, 600.0,
];

/// Guard for one model invocation. Created by [`model_call`]; records
/// duration / CPU-delta / outcome exactly once — via an explicit
/// `finish_ok`/`finish_err`, or implicitly on drop (counted as a
/// non-error inference with unknown token usage).
///
/// While alive it holds the `mibee_model_inflight{model}` gauge up and an
/// OTel `model_call` span open (`model` / `variant` attributes) — the
/// span's duration is the guard's lifetime, so externally collected
/// traces see every inference even outside conversations.
#[must_use = "dropping the guard un-finished still records, but you lose token counts"]
pub struct ModelCallGuard {
    model: String,
    variant: String,
    started: Instant,
    cpu_start: Option<Duration>,
    span: tracing::Span,
    finished: bool,
}

/// Begin one model invocation: inflight gauge up + an OTel `model_call`
/// span that lives until the guard finishes. In blocking closures, pair
/// it with [`ModelCallGuard::enter`] so nested events/logs attach; in
/// async code the bare guard already measures the right duration.
///
/// No-op-safe: when metrics were never registered (tests, early startup)
/// the guard still measures and exports the span, but touches no metrics.
pub fn model_call(model: &str, variant: &str) -> ModelCallGuard {
    let span = tracing::info_span!(
        "model_call",
        model,
        variant,
        otel.name = format!("model_call/{model}"),
    );
    if let Some(m) = GLOBAL_METRICS.get() {
        m.model_inflight.with_label_values(&[model]).inc();
    }
    ModelCallGuard {
        model: model.to_string(),
        variant: variant.to_string(),
        started: Instant::now(),
        cpu_start: read_process_cpu_time(),
        span,
        finished: false,
    }
}

impl ModelCallGuard {
    /// Make this call the ambient span on the current thread (use inside
    /// the blocking closure that runs the inference, right after
    /// [`model_call`]). The returned guard must be dropped before
    /// `finish_*` on the same thread.
    pub fn enter(&self) -> tracing::span::Entered<'_> {
        self.span.enter()
    }

    /// Record a successful invocation (token counts where the model bills
    /// by token; `None` for non-LLM models).
    pub fn finish_ok(mut self, tokens_prompt: Option<u64>, tokens_completion: Option<u64>) {
        self.record(false);
        if let Some(m) = GLOBAL_METRICS.get() {
            if let Some(p) = tokens_prompt {
                m.model_tokens_total
                    .with_label_values(&[&self.model, &self.variant, "prompt"])
                    .inc_by(p);
            }
            if let Some(c) = tokens_completion {
                m.model_tokens_total
                    .with_label_values(&[&self.model, &self.variant, "completion"])
                    .inc_by(c);
            }
        }
    }

    /// Record a failed invocation (error counter instead of token counts).
    pub fn finish_err(mut self) {
        self.record(true);
    }

    /// Shared tail: duration, CPU delta, outcome counters, inflight down.
    /// Idempotent via `finished` (Drop after an explicit finish is a no-op).
    fn record(&mut self, errored: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        let duration = self.started.elapsed();
        let cpu = read_process_cpu_time()
            .zip(self.cpu_start)
            .map(|(now, start)| now.saturating_sub(start));
        if let Some(m) = GLOBAL_METRICS.get() {
            m.record_model_call(&self.model, &self.variant, duration, cpu, errored);
        }
        if errored {
            // Debug, not warn: fail-open engines (e.g. face matching with
            // no face in frame) can fire this per detection event — the
            // mibee_model_errors_total counter is the durable signal.
            tracing::debug!(
                model = %self.model,
                variant = %self.variant,
                duration_ms = duration.as_millis() as u64,
                "model_call failed"
            );
        }
    }
}

impl Drop for ModelCallGuard {
    fn drop(&mut self) {
        // Implicit finish: a dropped guard still counts the inference
        // (duration/CPU/outcome unknown → not an error).
        self.record(false);
    }
}

/// Process CPU time (user+system) from `/proc/self/stat`, or `None` off
/// Linux / on parse failure. Field 14+15, in clock ticks — `USER_HZ` is
/// 100 on every mainstream Linux (glibc `sysconf(_SC_CLK_TCK)`).
fn read_process_cpu_time() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The comm field (2nd) may contain spaces/parens — parse after the
    // final ')'.
    let after = stat.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    // state(3) ppid(4) pgrp(5) session(6) tty(7) tpgid(8) flags(9)
    // minflt(10) cminflt(11) majflt(12) cmajflt(13) utime(14) stime(15)
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(Duration::from_millis((utime + stime) * 10))
}

/// Render all registered metrics in Prometheus text-0.0.4 exposition format.
pub fn render_metrics() -> String {
    let metric_families = global_metrics().registry.gather();
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    // encode() only fails if the writer errors — Vec<u8> never does.
    encoder
        .encode(&metric_families, &mut buffer)
        .expect("prometheus text encoding should never fail for Vec<u8>");
    String::from_utf8(buffer).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Internal implementation
// ---------------------------------------------------------------------------

impl Metrics {
    /// Construct a new `Metrics` instance with a fresh registry and all
    /// custom metrics registered.
    fn new() -> Result<Self> {
        let registry = Registry::new();

        let active_streams = IntGauge::new(
            "mibee_eye_streams_active",
            "Number of currently active streams",
        )?;
        registry.register(Box::new(active_streams.clone()))?;

        let bytes_received = IntCounterVec::new(
            Opts::new(
                "mibee_eye_bytes_received",
                "Total bytes received from cameras",
            ),
            &["camera_id"],
        )?;
        registry.register(Box::new(bytes_received.clone()))?;

        let capture_errors = IntCounterVec::new(
            Opts::new("mibee_eye_capture_errors", "Total capture errors by camera"),
            &["camera_id"],
        )?;
        registry.register(Box::new(capture_errors.clone()))?;

        // ─── Protocol-specific counters ───────────────────────────
        let rtsp_sessions = IntCounterVec::new(
            Opts::new(
                "mibee_eye_rtsp_sessions_total",
                "Total number of RTSP sessions by status",
            ),
            &["status"],
        )?;
        registry.register(Box::new(rtsp_sessions.clone()))?;

        let rtsp_bytes_sent = IntCounter::new(
            "mibee_eye_rtsp_bytes_sent_total",
            "Total RTSP/RTP bytes transmitted",
        )?;
        registry.register(Box::new(rtsp_bytes_sent.clone()))?;

        let rtmp_push_bytes = IntCounter::new(
            "mibee_eye_rtmp_push_bytes_total",
            "Total bytes pushed via RTMP",
        )?;
        registry.register(Box::new(rtmp_push_bytes.clone()))?;

        let rtmp_push_errors =
            IntCounter::new("mibee_eye_rtmp_push_errors_total", "Total RTMP push errors")?;
        registry.register(Box::new(rtmp_push_errors.clone()))?;

        let onvif_discovery_requests = IntCounter::new(
            "mibee_eye_onvif_discovery_requests_total",
            "Total ONVIF WS-Discovery probe requests received",
        )?;
        registry.register(Box::new(onvif_discovery_requests.clone()))?;

        let gb28181_register_status = IntCounterVec::new(
            Opts::new(
                "mibee_eye_gb28181_register_status",
                "Total GB28181 registration attempts by status",
            ),
            &["status"],
        )?;
        registry.register(Box::new(gb28181_register_status.clone()))?;

        // ─── Audio level gauge ───────────────────────────────────
        let audio_level_db = GaugeVec::new(
            Opts::new(
                "mibee_eye_audio_level_db",
                "Current audio input level in dBFS",
            ),
            &["stream_id"],
        )?;
        registry.register(Box::new(audio_level_db.clone()))?;

        // ─── New metrics ───────────────────────────────────
        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "mibee_http_requests_total",
                "Total HTTP requests by method, path, and status",
            ),
            &["method", "path", "status"],
        )?;
        registry.register(Box::new(http_requests_total.clone()))?;

        let auth_failures_total = IntCounterVec::new(
            Opts::new(
                "mibee_auth_failures_total",
                "Total authentication failures by type",
            ),
            &["type"],
        )?;
        registry.register(Box::new(auth_failures_total.clone()))?;

        let recording_active = IntGauge::new(
            "mibee_eyeording_active",
            "Number of active recording outputs",
        )?;
        registry.register(Box::new(recording_active.clone()))?;

        let frame_drops_total = IntCounter::new(
            "mibee_frame_drops_total",
            "Total number of dropped frames in broadcast",
        )?;
        registry.register(Box::new(frame_drops_total.clone()))?;

        let ai_inferences_total = IntCounter::new(
            "mibee_ai_inferences_total",
            "Total AI detection inferences completed",
        )?;
        registry.register(Box::new(ai_inferences_total.clone()))?;

        // ─── Per-model resource metrics (SPEC §3.3 + appendix A #39) ──
        let model_inferences_total = IntCounterVec::new(
            Opts::new(
                "mibee_model_inferences_total",
                "Total model invocations by model capability and variant",
            ),
            &["model", "variant"],
        )?;
        registry.register(Box::new(model_inferences_total.clone()))?;

        let model_errors_total = IntCounterVec::new(
            Opts::new(
                "mibee_model_errors_total",
                "Total failed model invocations by model and variant",
            ),
            &["model", "variant"],
        )?;
        registry.register(Box::new(model_errors_total.clone()))?;

        let model_inference_seconds = HistogramVec::new(
            HistogramOpts::new(
                "mibee_model_inference_seconds",
                "Wall-clock duration of one model invocation (seconds)",
            )
            .buckets(MODEL_SECONDS_BUCKETS.to_vec()),
            &["model", "variant"],
        )?;
        registry.register(Box::new(model_inference_seconds.clone()))?;

        let model_cpu_seconds = HistogramVec::new(
            HistogramOpts::new(
                "mibee_model_cpu_seconds",
                "Process-wide CPU time consumed during one model invocation (seconds)",
            )
            .buckets(MODEL_SECONDS_BUCKETS.to_vec()),
            &["model", "variant"],
        )?;
        registry.register(Box::new(model_cpu_seconds.clone()))?;

        let model_inflight = IntGaugeVec::new(
            Opts::new(
                "mibee_model_inflight",
                "Model invocations currently executing",
            ),
            &["model"],
        )?;
        registry.register(Box::new(model_inflight.clone()))?;

        let model_tokens_total = IntCounterVec::new(
            Opts::new(
                "mibee_model_tokens_total",
                "Tokens processed by token-billed models (prompt/completion)",
            ),
            &["model", "variant", "kind"],
        )?;
        registry.register(Box::new(model_tokens_total.clone()))?;

        // ─── Resource gauges (SPEC appendix A #38) ─────────────────────
        let system_cpu_percent =
            Gauge::new("mibee_eye_system_cpu_percent", "System CPU busy percent")?;
        registry.register(Box::new(system_cpu_percent.clone()))?;
        let system_memory_total_bytes = IntGauge::new(
            "mibee_eye_system_memory_total_bytes",
            "System memory total (bytes)",
        )?;
        registry.register(Box::new(system_memory_total_bytes.clone()))?;
        let system_memory_available_bytes = IntGauge::new(
            "mibee_eye_system_memory_available_bytes",
            "System memory available (bytes)",
        )?;
        registry.register(Box::new(system_memory_available_bytes.clone()))?;
        let process_cpu_percent = Gauge::new(
            "mibee_eye_process_cpu_percent",
            "Service process CPU percent",
        )?;
        registry.register(Box::new(process_cpu_percent.clone()))?;
        let process_resident_memory_bytes = IntGauge::new(
            "mibee_eye_process_resident_memory_bytes",
            "Service process resident memory (bytes)",
        )?;
        registry.register(Box::new(process_resident_memory_bytes.clone()))?;
        let process_open_fds = IntGauge::new(
            "mibee_eye_process_open_fds",
            "Service process open file descriptors",
        )?;
        registry.register(Box::new(process_open_fds.clone()))?;
        let system_net_rx_bytes = IntGauge::new(
            "mibee_eye_system_net_rx_bytes",
            "Aggregate NIC receive bytes (absolute counter)",
        )?;
        registry.register(Box::new(system_net_rx_bytes.clone()))?;
        let system_net_tx_bytes = IntGauge::new(
            "mibee_eye_system_net_tx_bytes",
            "Aggregate NIC transmit bytes (absolute counter)",
        )?;
        registry.register(Box::new(system_net_tx_bytes.clone()))?;

        let resource_budget_mib = IntGauge::new(
            "mibee_eye_resource_budget_mib",
            "Boot-time feature-admission memory budget (SPEC A #40)",
        )?;
        registry.register(Box::new(resource_budget_mib.clone()))?;
        let feature_admitted = IntGaugeVec::new(
            Opts::new(
                "mibee_eye_feature_admitted",
                "Whether a feature was admitted by the boot resource gate (1/0)",
            ),
            &["name"],
        )?;
        registry.register(Box::new(feature_admitted.clone()))?;

        Ok(Metrics {
            registry,
            active_streams,
            bytes_received,
            capture_errors,
            rtsp_sessions,
            rtsp_bytes_sent,
            rtmp_push_bytes,
            rtmp_push_errors,
            onvif_discovery_requests,
            gb28181_register_status,
            audio_level_db,
            http_requests_total,
            auth_failures_total,
            recording_active,
            frame_drops_total,
            ai_inferences_total,
            model_inferences_total,
            model_errors_total,
            model_inference_seconds,
            model_cpu_seconds,
            model_inflight,
            model_tokens_total,
            system_cpu_percent,
            system_memory_total_bytes,
            system_memory_available_bytes,
            process_cpu_percent,
            process_resident_memory_bytes,
            process_open_fds,
            system_net_rx_bytes,
            system_net_tx_bytes,
            resource_budget_mib,
            feature_admitted,
        })
    }

    /// Record one finished model invocation into the per-model families
    /// (inflight is decremented here — [`model_call`] incremented it).
    fn record_model_call(
        &self,
        model: &str,
        variant: &str,
        duration: Duration,
        cpu: Option<Duration>,
        errored: bool,
    ) {
        self.model_inflight.with_label_values(&[model]).dec();
        self.model_inferences_total
            .with_label_values(&[model, variant])
            .inc();
        if errored {
            self.model_errors_total
                .with_label_values(&[model, variant])
                .inc();
        }
        self.model_inference_seconds
            .with_label_values(&[model, variant])
            .observe(duration.as_secs_f64());
        if let Some(cpu) = cpu {
            self.model_cpu_seconds
                .with_label_values(&[model, variant])
                .observe(cpu.as_secs_f64());
        }
    }

    #[cfg(test)]
    fn render(&self) -> String {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .expect("prometheus text encoding should never fail for Vec<u8>");
        String::from_utf8(buffer).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole public helper family through the registered global:
    /// every counter/gauge must accept its call without panicking and
    /// land on the Prometheus surface.
    #[test]
    fn helper_family_registers_and_renders() {
        register_metrics().expect("register");
        increment_bytes_received("cam0", 1024);
        increment_capture_errors("cam0");
        increment_rtsp_sessions("start");
        increment_rtsp_bytes_sent(2048);
        increment_rtmp_push_bytes(512);
        increment_rtmp_push_errors();
        increment_onvif_discovery_requests();
        increment_gb28181_register_status("ok");
        set_active_streams(2);
        set_audio_level("cam0", 0.42);
        increment_http_requests("GET", "/api/health", 200);
        increment_auth_failures("login");
        set_recording_active(1);
        increment_frame_drops();
        increment_ai_inferences();
        register_metrics().expect("idempotent re-register");

        let families = global_metrics().registry.gather();
        let text = prometheus::TextEncoder::new()
            .encode_to_string(&families)
            .expect("encode");
        assert!(text.contains("mibee_eye_bytes_received"), "{text}");
        assert!(text.contains("mibee_ai_inferences_total"), "{text}");
        assert!(text.contains("mibee_eye_streams_active 2"), "{text}");
    }

    #[test]
    fn test_metrics_create_and_render() {
        let m = Metrics::new().expect("metrics creation should succeed");

        // Increment counters/gauges so they appear in output (Prometheus
        // omits zero-value counter vecs from rendered output)
        m.bytes_received.with_label_values(&["test"]).inc_by(1);
        m.capture_errors.with_label_values(&["test"]).inc();
        m.active_streams.set(0);
        // Activate protocol counters so they appear in output
        m.rtsp_sessions.with_label_values(&["active"]).inc();
        m.rtsp_bytes_sent.inc_by(100);
        m.rtmp_push_bytes.inc_by(200);
        m.rtmp_push_errors.inc();
        m.onvif_discovery_requests.inc();
        m.gb28181_register_status
            .with_label_values(&["registered"])
            .inc();
        // Activate new metrics so they appear in output
        m.http_requests_total
            .with_label_values(&["GET", "/health", "200"])
            .inc();
        m.auth_failures_total
            .with_label_values(&["bad_password"])
            .inc();
        m.recording_active.set(1);
        m.frame_drops_total.inc_by(3);

        let output = m.render();
        // All metrics should appear in the rendered text
        assert!(
            output.contains("mibee_eye_streams_active"),
            "output should contain active_streams gauge"
        );
        assert!(
            output.contains("mibee_eye_bytes_received"),
            "output should contain bytes_received counter"
        );
        assert!(
            output.contains("mibee_eye_capture_errors"),
            "output should contain capture_errors counter"
        );
        assert!(
            output.contains("mibee_eye_rtsp_sessions_total"),
            "output should contain rtsp_sessions counter"
        );
        assert!(
            output.contains("mibee_eye_rtsp_bytes_sent_total"),
            "output should contain rtsp_bytes_sent counter"
        );
        assert!(
            output.contains("mibee_eye_rtmp_push_bytes_total"),
            "output should contain rtmp_push_bytes counter"
        );
        assert!(
            output.contains("mibee_eye_rtmp_push_errors_total"),
            "output should contain rtmp_push_errors counter"
        );
        assert!(
            output.contains("mibee_eye_onvif_discovery_requests_total"),
            "output should contain onvif_discovery_requests counter"
        );
        assert!(
            output.contains("mibee_eye_gb28181_register_status"),
            "output should contain gb28181_register_status counter"
        );
        assert!(
            output.contains("mibee_http_requests_total"),
            "output should contain http_requests_total counter"
        );
        assert!(
            output.contains("mibee_auth_failures_total"),
            "output should contain auth_failures_total counter"
        );
        assert!(
            output.contains("mibee_eyeording_active"),
            "output should contain recording_active gauge"
        );
        assert!(
            output.contains("mibee_frame_drops_total"),
            "output should contain frame_drops_total counter"
        );
    }

    #[test]
    fn test_protocol_counters_increment() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.rtsp_sessions.with_label_values(&["active"]).inc();
        m.rtsp_sessions.with_label_values(&["active"]).inc();
        m.rtsp_sessions.with_label_values(&["closed"]).inc();
        m.rtsp_bytes_sent.inc_by(1500);
        m.rtmp_push_bytes.inc_by(4096);
        m.rtmp_push_errors.inc();
        m.rtmp_push_errors.inc();
        m.onvif_discovery_requests.inc();
        m.gb28181_register_status
            .with_label_values(&["registered"])
            .inc();
        m.gb28181_register_status
            .with_label_values(&["failed"])
            .inc();
        m.gb28181_register_status
            .with_label_values(&["failed"])
            .inc();

        let output = m.render();

        assert!(
            output.contains("mibee_eye_rtsp_sessions_total{status=\"active\"} 2"),
            "active sessions should be 2\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_rtsp_sessions_total{status=\"closed\"} 1"),
            "closed sessions should be 1\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_rtsp_bytes_sent_total 1500"),
            "rtsp bytes should be 1500\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_rtmp_push_bytes_total 4096"),
            "rtmp bytes should be 4096\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_rtmp_push_errors_total 2"),
            "rtmp errors should be 2\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_onvif_discovery_requests_total 1"),
            "discovery requests should be 1\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_gb28181_register_status{status=\"registered\"} 1"),
            "registered should be 1\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_eye_gb28181_register_status{status=\"failed\"} 2"),
            "failed should be 2\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_increment_bytes_received() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.bytes_received.with_label_values(&["cam_1"]).inc_by(100);
        m.bytes_received.with_label_values(&["cam_1"]).inc_by(50);
        m.bytes_received.with_label_values(&["cam_2"]).inc_by(200);

        let output = m.render();

        // Total for cam_1 should be 150
        assert!(
            output.contains(r#"mibee_eye_bytes_received{camera_id="cam_1"} 150"#),
            "cam_1 should show 150 bytes\n=== output ===\n{}",
            output
        );
        // Total for cam_2 should be 200
        assert!(
            output.contains(r#"mibee_eye_bytes_received{camera_id="cam_2"} 200"#),
            "cam_2 should show 200 bytes\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_increment_capture_errors() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.capture_errors.with_label_values(&["cam_1"]).inc();
        m.capture_errors.with_label_values(&["cam_1"]).inc();
        m.capture_errors.with_label_values(&["cam_2"]).inc();

        let output = m.render();

        assert!(
            output.contains(r#"mibee_eye_capture_errors{camera_id="cam_1"} 2"#),
            "cam_1 should have 2 errors\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(r#"mibee_eye_capture_errors{camera_id="cam_2"} 1"#),
            "cam_2 should have 1 error\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_set_active_streams() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.active_streams.set(3);
        let output = m.render();
        assert!(
            output.contains("mibee_eye_streams_active 3"),
            "active_streams should be 3\n=== output ===\n{}",
            output
        );

        m.active_streams.set(0);
        let output = m.render();
        assert!(
            output.contains("mibee_eye_streams_active 0"),
            "active_streams should be 0\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_registry_freshness() {
        // Each Metrics instance gets its own registry — they should not
        // interfere.
        let m1 = Metrics::new().expect("m1 creation");
        let m2 = Metrics::new().expect("m2 creation");

        m1.active_streams.set(42);
        m2.active_streams.set(99);

        let out1 = m1.render();
        let out2 = m2.render();

        assert!(
            out1.contains("mibee_eye_streams_active 42"),
            "m1 should have 42\n=== out1 ===\n{}",
            out1
        );
        assert!(
            out2.contains("mibee_eye_streams_active 99"),
            "m2 should have 99\n=== out2 ===\n{}",
            out2
        );
    }

    #[test]
    fn test_audio_level_gauge() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.audio_level_db.with_label_values(&["default"]).set(-12.5);
        let output = m.render();
        assert!(
            output.contains(r#"mibee_eye_audio_level_db{stream_id="default"} -12.5"#),
            "audio_level_db should show -12.5\n=== output ===\n{}",
            output
        );

        m.audio_level_db.with_label_values(&["default"]).set(0.0);
        let output = m.render();
        assert!(
            output.contains(r#"mibee_eye_audio_level_db{stream_id="default"} 0"#),
            "audio_level_db should show 0\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_increment_http_requests() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.http_requests_total
            .with_label_values(&["POST", "/api/auth/login", "200"])
            .inc();
        m.http_requests_total
            .with_label_values(&["GET", "/health", "200"])
            .inc();
        m.http_requests_total
            .with_label_values(&["GET", "/health", "200"])
            .inc();

        let output = m.render();
        assert!(
            output.contains(
                r#"mibee_http_requests_total{method="POST",path="/api/auth/login",status="200"} 1"#
            ),
            "POST login should appear once\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(
                r#"mibee_http_requests_total{method="GET",path="/health",status="200"} 2"#
            ),
            "GET health should appear twice\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_increment_auth_failures() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.auth_failures_total
            .with_label_values(&["bad_password"])
            .inc();
        m.auth_failures_total
            .with_label_values(&["bad_password"])
            .inc();
        m.auth_failures_total
            .with_label_values(&["locked_out"])
            .inc();
        m.auth_failures_total
            .with_label_values(&["rate_limited"])
            .inc();

        let output = m.render();
        assert!(
            output.contains(r#"mibee_auth_failures_total{type="bad_password"} 2"#),
            "bad_password should be 2\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(r#"mibee_auth_failures_total{type="locked_out"} 1"#),
            "locked_out should be 1\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(r#"mibee_auth_failures_total{type="rate_limited"} 1"#),
            "rate_limited should be 1\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_set_recording_active() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.recording_active.set(2);
        let output = m.render();
        assert!(
            output.contains("mibee_eyeording_active 2"),
            "recording_active should be 2\n=== output ===\n{}",
            output
        );

        m.recording_active.set(0);
        let output = m.render();
        assert!(
            output.contains("mibee_eyeording_active 0"),
            "recording_active should be 0\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_increment_frame_drops() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.frame_drops_total.inc();
        m.frame_drops_total.inc();
        m.frame_drops_total.inc();

        let output = m.render();
        assert!(
            output.contains("mibee_frame_drops_total 3"),
            "frame_drops should be 3\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_model_families_render_with_labels() {
        let m = Metrics::new().expect("metrics creation should succeed");

        m.model_inflight.with_label_values(&["llm"]).inc();
        m.model_inflight.with_label_values(&["llm"]).inc();
        m.record_model_call(
            "llm",
            "qwen3-4b",
            Duration::from_millis(1200),
            Some(Duration::from_millis(900)),
            false,
        );
        m.record_model_call("llm", "qwen3-4b", Duration::from_millis(300), None, true);
        m.record_model_call(
            "vlm",
            "qwen3-vl-2b",
            Duration::from_secs(4),
            Some(Duration::from_secs(3)),
            false,
        );
        m.model_tokens_total
            .with_label_values(&["llm", "qwen3-4b", "prompt"])
            .inc_by(512);
        m.model_tokens_total
            .with_label_values(&["llm", "qwen3-4b", "completion"])
            .inc_by(64);

        let output = m.render();
        assert!(
            output.contains(r#"mibee_model_inferences_total{model="llm",variant="qwen3-4b"} 2"#),
            "llm inferences should be 2\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(r#"mibee_model_errors_total{model="llm",variant="qwen3-4b"} 1"#),
            "one llm error\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_model_inference_seconds_bucket"),
            "duration histogram present\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains("mibee_model_cpu_seconds_bucket"),
            "cpu histogram present\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(r#"mibee_model_inflight{model="llm"} 0"#),
            "inflight net 0 after inc/inc + two recorded calls\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(
                r#"mibee_model_tokens_total{kind="prompt",model="llm",variant="qwen3-4b"} 512"#
            ),
            "prompt tokens\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(
                r#"mibee_model_tokens_total{kind="completion",model="llm",variant="qwen3-4b"} 64"#
            ),
            "completion tokens\n=== output ===\n{}",
            output
        );
        assert!(
            output.contains(
                r#"mibee_model_inference_seconds_sum{model="vlm",variant="qwen3-vl-2b"} 4"#
            ),
            "vlm duration sum\n=== output ===\n{}",
            output
        );
    }

    #[test]
    fn test_model_call_guard_is_noop_safe_without_registration() {
        // Unit tests never call register_metrics() — the global is absent,
        // so the guard must measure-and-export only, never panic.
        let guard = model_call("ocr", "pp-ocrv5");
        guard.finish_ok(Some(1), Some(2));
        let guard = model_call("ocr", "pp-ocrv5");
        guard.finish_err();
        let _implicit = model_call("decision", "laya"); // dropped un-finished
    }

    #[test]
    fn test_resource_gauges_render() {
        let m = Metrics::new().expect("metrics creation should succeed");
        m.system_cpu_percent.set(23.5);
        m.system_memory_total_bytes.set(8);
        m.system_memory_available_bytes.set(3);
        m.process_cpu_percent.set(12.0);
        m.process_resident_memory_bytes.set(999);
        m.process_open_fds.set(42);
        m.system_net_rx_bytes.set(1_000);
        m.system_net_tx_bytes.set(2_000);
        // Boot-time feature admission (SPEC appendix A #40).
        m.resource_budget_mib.set(1_596);
        m.feature_admitted.with_label_values(&["vlm"]).set(0);
        m.feature_admitted.with_label_values(&["ai"]).set(1);
        let output = m.render();
        assert!(
            output.contains("mibee_eye_system_cpu_percent 23.5"),
            "{output}"
        );
        assert!(
            output.contains("mibee_eye_system_memory_total_bytes 8"),
            "{output}"
        );
        assert!(
            output.contains("mibee_eye_process_resident_memory_bytes 999"),
            "{output}"
        );
        assert!(
            output.contains("mibee_eye_system_net_rx_bytes 1000"),
            "{output}"
        );
        assert!(
            output.contains("mibee_eye_resource_budget_mib 1596"),
            "{output}"
        );
        assert!(
            output.contains("mibee_eye_feature_admitted{name=\"ai\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("mibee_eye_feature_admitted{name=\"vlm\"} 0"),
            "{output}"
        );
    }

    #[test]
    fn test_read_process_cpu_time_parses_proc_self_stat() {
        let a = read_process_cpu_time();
        // Linux workstation/Pi: /proc/self/stat always present and the
        // parse must succeed; the value is small but non-negative.
        assert!(a.is_some(), "/proc/self/stat should parse on Linux");
        let _b = read_process_cpu_time();
    }
}
