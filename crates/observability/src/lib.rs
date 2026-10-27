#![cfg_attr(test, deny(warnings))]

/// Tracing subscriber initialisation (EnvFilter, fmt, OpenTelemetry OTLP).
pub mod tracing_setup;

/// In-process log ring backing `GET /api/logs` (SPEC v1 §3.2).
pub mod log_ring;
/// Loki log shipping layer.
pub mod loki_layer;
/// Custom Prometheus metrics (streams, bytes, errors).
pub mod metrics;

// Re-export the most commonly used functions for ergonomic access.
pub use metrics::{
    ModelCallGuard,
    ModelCallPhase,
    ModelCallSignal,
    ModelEventSink,
    ResourceSample,
    dec_recording_active,
    inc_recording_active,
    increment_ai_inferences,
    increment_auth_failures,
    increment_bytes_received,
    increment_capture_errors,
    increment_frame_drops,
    increment_gb28181_register_status,
    // New metrics
    increment_http_requests,
    increment_onvif_discovery_requests,
    increment_rtmp_push_bytes,
    increment_rtmp_push_errors,
    increment_rtsp_bytes_sent,
    increment_rtsp_sessions,
    model_call,
    model_event_sink_installed,
    publish_resource_gauges,
    publish_resource_profile,
    register_metrics,
    render_metrics,
    set_active_streams,
    set_audio_level,
    set_model_event_sink,
    set_recording_active,
};
pub use tracing_setup::init_tracing;
