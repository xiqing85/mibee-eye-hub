#![cfg_attr(test, deny(warnings))]

/// Stream pipeline: capture -> encode -> distribute
///
/// Central hub connecting media sources to outputs with fan-out,
/// buffer management, and resource control.
/// On-device AI object detection (NanoDet-Plus ONNX, SPEC v1 §4.6).
pub mod ai;
/// On-device audio intelligence (YAMNet sound events + Silero voice
/// presence) fed by the always-on 16 kHz monitor.
pub mod audio_ai;
pub mod audio_level;
pub mod buffer;
pub mod capability;
pub mod capture_source;
pub mod decision;
/// Native (ffmpeg-free) codec stack: H.264 encode (openh264), pixel-format
/// conversion (MJPEG/YUYV → YUV420p), and audio encode (G.711 / AAC).
pub mod encoder;
pub mod face;
/// Boot-time feature-level resource gating (`[resources] feature_gate`,
/// SPEC appendix A #40).
pub mod feature_gate;
/// Fragmented-MP4 remuxer for MSE / `MediaSource` playback.
pub mod fmp4;
pub mod hub;
pub mod lang;

// ─── Voice-round yielding (SPEC appendix A #55) ──────────────────────
//
// A latency-critical voice round (wake hit → transcript delivered) is
// tracked process-wide so heavy inference can get out of its way —
// the hex "dictation always comes first" shape. The 2026-10-12 field
// incident: a VLM describe at full tilt starved KWS so completely
// that five wake words in a row went unheard with no lag log. nice
// demotion fixed the worst of it; yielding closes the gap for the
// normal-priority decoders (LLM decode) and keeps background jobs
// from even competing while the user is mid-utterance.

use std::sync::atomic::{AtomicU64, Ordering};

/// Unix-ms deadline of the current latency-critical window (0 = none).
/// A deadline, not a counter: every voice-engine exit path would have
/// to clear a counter for it to ever release — one missed path and
/// the decoders would yield forever. The deadline self-expires even
/// if a path is missed; each new segment extends it.
static VOICE_PRIORITY_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Mark a latency-critical voice segment: KWS/ASR will be competing
/// for cores for roughly the next `bound_ms` (the caller's bound on
/// capture + transcription; the transcript delivery extends it
/// briefly). A later `voice_round_begin` extends the window.
pub fn voice_round_begin(bound_ms: u64) {
    let until = unix_now_ms().saturating_add(bound_ms);
    VOICE_PRIORITY_UNTIL_MS.fetch_max(until, Ordering::SeqCst);
}

/// Whether a latency-critical voice segment's window is still open.
#[must_use]
pub fn voice_round_active() -> bool {
    VOICE_PRIORITY_UNTIL_MS.load(Ordering::SeqCst) > unix_now_ms()
}

/// Block until no voice round is in flight, at most `max`. For
/// background jobs about to grab the inference cores (VLM describes,
/// meeting transcription): starting mid-utterance is what starved KWS
/// in the field; waiting a bounded beat keeps the user's round snappy
/// and never wedges the background work (fail-open past the bound).
pub fn wait_voice_idle(max: std::time::Duration) {
    let deadline = std::time::Instant::now() + max;
    while voice_round_active() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Local LLM dialogue (llama.cpp; `llm` feature).
pub mod llm;
/// On-demand meeting mode (record → diarize → transcribe; SPEC #27).
/// Recording machinery builds in every feature set; processing needs the
/// `voice` feature.
pub mod meeting;
pub mod model_route;
pub mod models;
#[cfg(feature = "ai")]
pub mod ocr;
pub mod output;
pub mod resource;
pub mod sentence;
pub mod source;
/// TTS playback via the sherpa-onnx CLI subprocess (GPL isolation).
pub mod tools;
pub mod tts;
/// Voice interaction (wake word + offline transcription; `voice` feature).
pub mod vlm;
pub mod voice;
pub mod watermark;

#[cfg(test)]
mod yielding_tests {
    use super::*;

    #[test]
    fn voice_round_latch_self_expires_and_extends() {
        // Short-lived windows are hard to observe reliably across
        // clock granularity, so probe the mechanism's edges: an
        // unbounded window reads active, the empty window reads
        // inactive, and a fresh short window extends nothing beyond
        // its own deadline.
        assert!(!voice_round_active(), "idle at rest");
        voice_round_begin(50);
        assert!(voice_round_active(), "armed window reads active");
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(!voice_round_active(), "window self-expired");
        voice_round_begin(50);
        std::thread::sleep(std::time::Duration::from_millis(10));
        voice_round_begin(50);
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(
            !voice_round_active(),
            "expired despite the earlier extension"
        );
    }

    #[test]
    fn wait_voice_idle_returns_promptly_when_idle() {
        let started = std::time::Instant::now();
        wait_voice_idle(std::time::Duration::from_secs(5));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "idle wait must not block"
        );
    }
}
