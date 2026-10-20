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
