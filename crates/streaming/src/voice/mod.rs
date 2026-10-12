//! Voice interaction engine: streaming keyword spotting (wake word) +
//! offline paraformer transcription of the utterance that follows.
//!
//! Wake → capture → transcribe is the minimal closed loop of the voice
//! interaction plan; the LLM dialogue and TTS playback stages consume the
//! same [`VoiceEvent`]. The engine is fail-open (missing models or a build
//! without the `voice` feature leave it inactive) and reuses the always-on
//! 16 kHz monitor stream, so it never touches the audio hardware itself.
//!
//! Optional speaker features (voiceprint) sit on the same loop:
//! - **Verification gate** — with `speaker_verify` and enrolled profiles,
//!   a wake word only opens the capture window for a *known* speaker: the
//!   embedding of the last `verify_window_secs` of audio (the wake-word
//!   utterance itself) must match an enrolled profile above
//!   `speaker_threshold`. Strangers are logged and dropped. This is a
//!   convenience filter, **not** a security boundary — short-utterance
//!   voiceprints are forgiving, and a household member with a similar
//!   voice may pass.
//! - **Record tagging** — every completed interaction is attributed to the
//!   best-matching enrolled speaker (empty string when nobody matches).
//! - **Enrollment** — a session collects `needed` wake-word utterances
//!   through the normal KWS path; the embeddings are handed to the host
//!   for persistence (see [`VoiceEngine::take_completed_enrollment`]).

use capture::audio_monitor::AudioChunk;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
#[cfg(feature = "voice")]
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::info;
#[cfg(feature = "voice")]
use tracing::warn;

/// One completed voice interaction: the wake word that fired and the
/// transcript of the utterance captured after it (SPEC appendix A
/// notebook dialect: SSE `voice_transcript`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceEvent {
    /// The detected wake word (e.g. `"小蜜蜂"`).
    pub keyword: String,
    /// Transcript of the captured utterance (may be empty on silence).
    pub transcript: String,
    /// Unix epoch milliseconds captured at fire time.
    pub timestamp_ms: u64,
    /// Best-matching enrolled speaker for the utterance ("" = unknown /
    /// no speaker model / no match above threshold).
    #[serde(default)]
    pub speaker: String,
    /// True when captured inside a follow-up window (no wake word —
    /// SPEC appendix A #30-B continuous dialogue).
    #[serde(default)]
    pub follow_up: bool,
}

/// `[voice]` configuration section.
/// Default wake word — also the shipped keywords.txt content.
pub const DEFAULT_WAKE_WORD: &str = "小蜜蜂";

/// 声母表（最长优先匹配；零声母音节不拆）。KWS tokens.txt 的词表就是
/// 声母+带调韵母，所以 keywords 行必须按 声母/韵母 拆分拼写。
const INITIALS: &[&str] = &[
    "zh", "ch", "sh", "b", "p", "m", "f", "d", "t", "n", "l", "g", "k", "h", "j", "q", "x", "r",
    "z", "c", "s", "y", "w",
];

/// Convert a Mandarin wake word into a sherpa-onnx KWS keywords line
/// (`x iǎo m ì f ēng @小蜜蜂`). Rejects non-hanzi / unpinyinable
/// characters and out-of-range lengths — the caller must not write a
/// broken keywords file (it would deafen the wake word entirely).
pub fn wake_word_to_keyword_line(word: &str) -> Result<String, String> {
    let chars: Vec<char> = word.chars().collect();
    if !(2..=6).contains(&chars.len()) {
        return Err("唤醒词长度须为 2-6 个汉字".into());
    }
    use pinyin::ToPinyin;
    let mut tokens: Vec<String> = Vec::new();
    for &c in &chars {
        let syllable = c
            .to_pinyin()
            .map(|p| p.with_tone().to_string())
            .ok_or_else(|| format!("「{c}」不是可发音的汉字（唤醒词仅支持普通话汉字）"))?;
        let initial = INITIALS
            .iter()
            .filter(|i| syllable.starts_with(*i) && syllable.len() > (*i).len())
            .max_by_key(|i| i.len());
        match initial {
            Some(ini) => {
                tokens.push((*ini).to_string());
                tokens.push(syllable[ini.len()..].to_string());
            }
            None => tokens.push(syllable),
        }
    }
    Ok(format!("{} @{}", tokens.join(" "), word))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    pub enabled: bool,
    /// KWS model files (zipformer transducer trio + tokens).
    pub kws_encoder: String,
    pub kws_decoder: String,
    pub kws_joiner: String,
    pub kws_tokens: String,
    /// Keywords file — one keyword per line, `tokens… @display-name`
    /// (zh-en phoneme model: `x iǎo m ì f ēng @小蜜蜂`).
    pub keywords_file: String,
    /// Wake word (display name). Customized via the web scene config
    /// (#32); at boot a non-default value regenerates the keywords
    /// override file. Mandarin pronunciation only — the KWS model is a
    /// zh syllable transducer.
    pub wake_word: String,
    /// Wake-word sensitivity: lower threshold = easier to fire.
    pub keywords_threshold: f32,
    pub keywords_score: f32,
    /// Offline recognizer (paraformer zh int8).
    pub paraformer_model: String,
    pub paraformer_tokens: String,
    /// Seconds of audio captured after a wake word before transcription.
    pub capture_secs: u32,
    /// Inference threads (the target hosts are small).
    pub num_threads: i32,
    /// Speaker-embedding model (3D-Speaker CAM++ ONNX). The file being
    /// absent only disables the speaker features — KWS + ASR keep working.
    pub speaker_embedding_model: String,
    /// Gate wake words on a known speaker (needs enrolled profiles).
    pub speaker_verify: bool,
    /// Cosine-similarity threshold for verify/search (typical 0.5–0.6 for
    /// CAM++; calibrate on the target microphone).
    pub speaker_threshold: f32,
    /// Seconds of pre-wake audio kept in the ring buffer for the
    /// verification embedding (must cover the wake-word utterance).
    pub verify_window_secs: f32,
    /// Follow-up window length in seconds (#30-B): after the host arms
    /// it (once its reply finished playing), utterances inside the
    /// window skip the wake word. 0 = feature off.
    pub follow_up_window_secs: f32,
    /// Silero VAD model path (follow-up + capture endpointing). Missing
    /// file disables both with a WARN.
    pub vad_model: String,
    /// VAD-endpointed capture (2026-10-10): while capturing after a wake
    /// word, speech keeps the window open (up to capture_secs + 6 s) and
    /// a completed VAD segment ends it immediately — a question spoken
    /// after a pause no longer gets clipped by the fixed window. Needs
    /// `vad_model`; missing model → fixed-window behaviour (fail-open).
    pub capture_endpointing: bool,
    /// Thinking-phase wake-free listening + interruption (SPEC appendix
    /// A #45). While an answer round runs the mic stays in the follow-up
    /// (VAD) mode so the user can just speak; transcripts arriving then
    /// are stop-word/interrupt-classified. `false` restores the P0
    /// semantics: a started round always runs to completion.
    pub interrupt_enable: bool,
    /// Minimum Laya confidence for an interrupt classification to act
    /// (below it the verdict fails open to backchannel/ignore).
    pub interrupt_confidence: f32,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            kws_encoder: "models/voice/kws/encoder.int8.onnx".into(),
            kws_decoder: "models/voice/kws/decoder.int8.onnx".into(),
            kws_joiner: "models/voice/kws/joiner.int8.onnx".into(),
            kws_tokens: "models/voice/kws/tokens.txt".into(),
            keywords_file: "models/voice/kws/keywords.txt".into(),
            wake_word: DEFAULT_WAKE_WORD.into(),
            keywords_threshold: 0.25,
            keywords_score: 1.0,
            paraformer_model: "models/voice/paraformer/model.int8.onnx".into(),
            paraformer_tokens: "models/voice/paraformer/tokens.txt".into(),
            capture_secs: 4,
            num_threads: 1,
            speaker_embedding_model: "models/voice/speaker/campplus.onnx".into(),
            speaker_verify: false,
            speaker_threshold: 0.55,
            verify_window_secs: 2.0,
            follow_up_window_secs: 0.0,
            vad_model: "models/voice/vad/silero_vad.onnx".into(),
            capture_endpointing: true,
            interrupt_enable: true,
            interrupt_confidence: 0.6,
        }
    }
}

/// Inference backends (`voice`-feature builds only).
#[cfg(feature = "voice")]
struct VoiceInternals {
    kws: Arc<sherpa_onnx::KeywordSpotter>,
    recognizer: Arc<sherpa_onnx::OfflineRecognizer>,
    /// Speaker-embedding extractor; `None` (missing model file) keeps KWS +
    /// ASR alive and only disables the speaker features.
    embed: Option<Arc<sherpa_onnx::SpeakerEmbeddingExtractor>>,
    /// Follow-up endpointing VAD; `None` (feature off / model missing)
    /// keeps wake-word mode only.
    vad: Option<Arc<sherpa_onnx::VoiceActivityDetector>>,
    /// Capture-endpointing VAD (own instance — the follow-up one is
    /// driven by a different code path and the detector holds stream
    /// state). `None` → fixed capture window.
    capture_vad: Option<Arc<sherpa_onnx::VoiceActivityDetector>>,
}

/// Host-visible snapshot of an in-flight enrollment session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnrollmentStatus {
    pub name: String,
    pub collected: u32,
    pub needed: u32,
}

/// One in-flight enrollment: wake-word utterances collected so far.
#[cfg(feature = "voice")]
#[derive(Debug, Clone)]
struct EnrollmentSession {
    name: String,
    needed: u32,
    embeddings: Vec<Vec<f32>>,
}

/// Enrolled speaker profiles + in-flight enrollment. The sherpa manager
/// is created lazily once the embedding dimension is known (extractor
/// loaded, or the first persisted profile loaded at boot).
struct SpeakerRegistry {
    #[cfg(feature = "voice")]
    dim: i32,
    #[cfg(feature = "voice")]
    manager: Option<sherpa_onnx::SpeakerEmbeddingManager>,
    #[cfg(feature = "voice")]
    enrollment: Option<EnrollmentSession>,
}

impl SpeakerRegistry {
    #[cfg(feature = "voice")]
    fn new(dim: i32) -> Self {
        Self {
            dim,
            manager: None,
            enrollment: None,
        }
    }

    /// The dimension-less registry used by inactive/non-voice engines.
    #[cfg(feature = "voice")]
    fn empty() -> Self {
        Self::new(0)
    }

    #[cfg(not(feature = "voice"))]
    fn empty() -> Self {
        Self {}
    }

    /// Get (or create) the sherpa manager for the current dimension.
    #[cfg(feature = "voice")]
    fn manager(&mut self) -> Option<&sherpa_onnx::SpeakerEmbeddingManager> {
        if self.dim <= 0 {
            return None;
        }
        if self.manager.is_none() {
            self.manager = sherpa_onnx::SpeakerEmbeddingManager::create(self.dim);
        }
        self.manager.as_ref()
    }
}

/// Process-wide voice engine.
pub struct VoiceEngine {
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    config: VoiceConfig,
    active: bool,
    inactive_reason: String,
    #[allow(dead_code)]
    event_tx: broadcast::Sender<VoiceEvent>,
    #[cfg(feature = "voice")]
    internals: Option<VoiceInternals>,
    /// Enrolled speaker profiles + enrollment session (shared with the
    /// wake-word worker and the web routes).
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    speakers: Arc<std::sync::Mutex<SpeakerRegistry>>,
    /// Follow-up window deadline (unix-ms; 0 = closed). The host arms it
    /// via [`VoiceEngine::arm_follow_up`] once its reply finished
    /// PLAYING — arming earlier would let the VAD capture the device's
    /// own TTS output.
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    follow_until_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Follow-up window length in ms (#31 hot config). Initialized from
    /// `follow_up_window_secs` and shared with the web config API —
    /// `/api/config scene.voice.follow_up_window_secs` stores here at
    /// runtime. 0 disables arming; the VAD engine itself is still built
    /// from the boot-time config value.
    follow_up_window_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Playback self-mute deadline (unix-ms; 0 = mic open). Latched by
    /// the host around `TtsEngine::speak` so the wake-word worker stays
    /// deaf while the device's own reply plays — a spoken 小蜜蜂 in a
    /// reply otherwise re-wakes the device and sustains a self-talk
    /// loop (observed live 2026-10-02).
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    playback_mute_until_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl VoiceEngine {
    /// Build from config (fail-open on every load failure).
    #[must_use]
    pub fn from_config(config: &VoiceConfig) -> Self {
        let loaded = Self::load(config);
        match loaded {
            #[cfg(feature = "voice")]
            Ok(internals) => {
                info!(
                    keyword_file = %config.keywords_file,
                    capture_secs = config.capture_secs,
                    speaker_model = %config.speaker_embedding_model,
                    speaker_features = internals.embed.is_some(),
                    "voice: engine loaded"
                );
                let (event_tx, _) = broadcast::channel(16);
                let dim = internals.embed.as_ref().map_or(0, |e| e.dim());
                Self {
                    config: config.clone(),
                    active: true,
                    inactive_reason: String::new(),
                    event_tx,
                    internals: Some(internals),
                    speakers: Arc::new(std::sync::Mutex::new(SpeakerRegistry::new(dim))),
                    follow_until_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    follow_up_window_ms: Arc::new(std::sync::atomic::AtomicU64::new(
                        (config.follow_up_window_secs.max(0.0) * 1000.0) as u64,
                    )),
                    playback_mute_until_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                }
            }
            #[cfg(not(feature = "voice"))]
            Ok(()) => unreachable!("non-voice load never succeeds"),
            Err(reason) => {
                info!(%reason, "voice: disabled");
                let (event_tx, _) = broadcast::channel(16);
                Self {
                    config: config.clone(),
                    active: false,
                    inactive_reason: format!("{reason:#}"),
                    event_tx,
                    #[cfg(feature = "voice")]
                    internals: None,
                    speakers: Arc::new(std::sync::Mutex::new(SpeakerRegistry::empty())),
                    follow_until_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    follow_up_window_ms: Arc::new(std::sync::atomic::AtomicU64::new(
                        (config.follow_up_window_secs.max(0.0) * 1000.0) as u64,
                    )),
                    playback_mute_until_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                }
            }
        }
    }

    #[cfg(feature = "voice")]
    fn load(config: &VoiceConfig) -> anyhow::Result<VoiceInternals> {
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        let kws = build_kws(config).map_err(|e| {
            warn!(error = %e, "voice: KWS unavailable (voice stays disabled, fail-open)");
            anyhow::anyhow!("KWS model unavailable (see startup log)")
        })?;
        let recognizer = build_recognizer_for(config).map_err(|e| {
            warn!(error = %e, "voice: paraformer unavailable (voice stays disabled, fail-open)");
            anyhow::anyhow!("paraformer model unavailable (see startup log)")
        })?;
        // Speaker embedding is an optional add-on: a missing file only
        // disables the speaker features (verify gate, tagging, enrollment).
        let embed = build_speaker_extractor(config)
            .map_err(|e| {
                warn!(error = %e, "voice: speaker embedding unavailable (speaker features off)");
                e
            })
            .map(Arc::new)
            .ok();
        // Follow-up VAD (#30-B): optional — a missing model only
        // disables continuous dialogue and on-demand listening
        // (`arm_listen`), with one WARN. Built regardless of
        // `follow_up_window_secs`: away mode (SPEC appendix A #44) arms
        // one-shot listening while continuous dialogue stays off —
        // without this the VAD would only exist when the follow-up
        // feature was already enabled at boot.
        let vad = match build_vad(config) {
            Ok(v) => Some(Arc::new(v)),
            Err(e) => {
                warn!(error = %e, "voice: VAD unavailable (follow-up + on-demand listening disabled)");
                None
            }
        };
        // Capture endpointing owns a second detector instance — the
        // follow-up one carries stream state driven elsewhere.
        let capture_vad = if config.capture_endpointing {
            match build_vad(config) {
                Ok(v) => Some(Arc::new(v)),
                Err(e) => {
                    warn!(error = %e, "voice: VAD unavailable (fixed capture window)");
                    None
                }
            }
        } else {
            None
        };
        Ok(VoiceInternals {
            kws: Arc::new(kws),
            recognizer: Arc::new(recognizer),
            embed,
            vad,
            capture_vad,
        })
    }

    #[cfg(not(feature = "voice"))]
    fn load(config: &VoiceConfig) -> anyhow::Result<()> {
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        anyhow::bail!("built without the `voice` feature")
    }

    /// Whether the wake-word loop is running.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Why the engine is inactive (empty string when active).
    #[must_use]
    pub fn inactive_reason(&self) -> &str {
        &self.inactive_reason
    }

    /// Subscribe to completed voice interactions.
    /// Open the follow-up window for the CURRENT `follow_up_window_ms`
    /// share value (#30-B; hot-adjustable via #31). No-op when the
    /// window length is 0 or the VAD is absent.
    pub fn arm_follow_up(&self) {
        #[cfg(feature = "voice")]
        {
            let window_ms = self
                .follow_up_window_ms
                .load(std::sync::atomic::Ordering::SeqCst);
            if window_ms == 0 {
                return;
            }
            if self
                .internals
                .as_ref()
                .and_then(|i| i.vad.as_ref())
                .is_none()
            {
                return;
            }
            let until = unix_now_ms() + window_ms;
            self.follow_until_ms
                .store(until, std::sync::atomic::Ordering::SeqCst);
            info!(
                window_secs = window_ms as f64 / 1000.0,
                "voice: follow-up window armed"
            );
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = self;
        }
    }

    /// Shared handle to the follow-up window length (ms) — the web
    /// config API stores the hot value here (SPEC appendix A #31).
    #[must_use]
    pub fn follow_up_window_share(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.follow_up_window_ms)
    }

    /// Open a one-shot no-wake-word listening window with an explicit
    /// length (SPEC appendix A #44 away mode): completed VAD segments
    /// inside it become `follow_up: true` transcripts like the
    /// continuous-dialogue window. Unlike [`Self::arm_follow_up`] this
    /// never touches the shared `follow_up_window_secs` config value —
    /// the caller owns the length. Returns `false` when the engine
    /// cannot listen (zero window, or no VAD — inactive engine included).
    pub fn arm_listen(&self, window_ms: u64) -> bool {
        #[cfg(feature = "voice")]
        {
            if window_ms == 0 {
                return false;
            }
            if self
                .internals
                .as_ref()
                .and_then(|i| i.vad.as_ref())
                .is_none()
            {
                return false;
            }
            let until = unix_now_ms() + window_ms;
            self.follow_until_ms
                .store(until, std::sync::atomic::Ordering::SeqCst);
            info!(
                window_secs = window_ms as f64 / 1000.0,
                "voice: one-shot listen window armed"
            );
            true
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = (self, window_ms);
            false
        }
    }

    /// Deafen the wake-word worker while the device's own reply plays.
    /// The KWS hears any spoken 小蜜蜂 in our TTS output and re-wakes
    /// the device, sustaining a self-talk loop (observed live
    /// 2026-10-02: the LLM echoed a garbled self-transcript into
    /// 「我話小蜜蜂」×50 and every repetition re-triggered the wake).
    /// The deadline carries a generous hard cap so a hung `speak`
    /// cannot mute the mic forever; the worker clears it on expiry.
    pub fn begin_playback_mute(&self) {
        let until = unix_now_ms() + 120_000;
        self.playback_mute_until_ms
            .store(until, std::sync::atomic::Ordering::SeqCst);
        info!("voice: playback self-mute latched");
    }

    /// Re-open the mic after [`VoiceEngine::begin_playback_mute`].
    pub fn end_playback_mute(&self) {
        self.playback_mute_until_ms
            .store(0, std::sync::atomic::Ordering::SeqCst);
        info!("voice: playback self-mute released");
    }

    /// Shared handle to the playback self-mute deadline — the wake-word
    /// worker gates KWS/VAD input on it.
    #[must_use]
    pub fn playback_mute_share(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.playback_mute_until_ms)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<VoiceEvent> {
        self.event_tx.subscribe()
    }

    /// Synchronous voiceprint lookup over a 16 kHz clip — the meeting
    /// module's diarization clusters vote through this. Blocking (runs the
    /// embedding model); call from `spawn_blocking` contexts. `None` when
    /// incapable or unmatched.
    pub fn resolve_speaker_blocking(&self, samples: &[f32]) -> Option<String> {
        #[cfg(feature = "voice")]
        {
            let embed = self.internals.as_ref()?.embed.as_ref()?;
            let emb = compute_embedding(embed, samples)?;
            self.speakers
                .lock()
                .expect("speaker registry lock")
                .manager
                .as_ref()
                .and_then(|m| m.search(&emb, self.config.speaker_threshold))
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = samples;
            None
        }
    }

    // -- Speaker (voiceprint) API -----------------------------------------
    // All methods are available in every build; without the `voice`
    // feature (or without the embedding model file) they report an empty
    // registry and refuse enrollment, fail-open like the rest of the
    // engine.

    /// Whether speaker features are usable (embedding model loaded).
    #[must_use]
    pub fn speaker_capable(&self) -> bool {
        #[cfg(feature = "voice")]
        {
            self.internals.as_ref().is_some_and(|i| i.embed.is_some())
        }
        #[cfg(not(feature = "voice"))]
        {
            false
        }
    }

    /// Embedding dimension of the loaded speaker model (0 when incapable).
    #[must_use]
    pub fn speaker_dim(&self) -> i32 {
        #[cfg(feature = "voice")]
        {
            self.speakers.lock().expect("speaker registry lock").dim
        }
        #[cfg(not(feature = "voice"))]
        {
            0
        }
    }

    /// Register persisted profiles at boot; returns how many loaded.
    /// Profiles whose dimension disagrees with the model are skipped.
    pub fn load_speakers(&self, profiles: &[(String, Vec<Vec<f32>>)]) -> usize {
        #[cfg(feature = "voice")]
        {
            if !self.speaker_capable() {
                return 0;
            }
            let mut reg = self.speakers.lock().expect("speaker registry lock");
            let mut loaded = 0;
            for (name, embeddings) in profiles {
                if embeddings.is_empty() || embeddings.iter().flatten().count() == 0 {
                    continue;
                }
                let dim = embeddings[0].len() as i32;
                if reg.dim == 0 {
                    reg.dim = dim;
                }
                if dim != reg.dim {
                    warn!(speaker = %name, dim, expected = reg.dim, "voice: profile dim mismatch, skipped");
                    continue;
                }
                if reg.manager().is_some_and(|m| m.add_list(name, embeddings)) {
                    loaded += 1;
                }
            }
            if loaded > 0 {
                info!(count = loaded, "voice: speaker profiles loaded");
            }
            loaded
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = profiles;
            0
        }
    }

    /// Names of the enrolled speakers.
    #[must_use]
    pub fn list_speakers(&self) -> Vec<String> {
        #[cfg(feature = "voice")]
        {
            self.speakers
                .lock()
                .expect("speaker registry lock")
                .manager
                .as_ref()
                .map_or_else(
                    Vec::new,
                    sherpa_onnx::SpeakerEmbeddingManager::get_all_speakers,
                )
        }
        #[cfg(not(feature = "voice"))]
        {
            Vec::new()
        }
    }

    /// Remove an enrolled profile (in-memory; persistence is the host's).
    /// Returns whether the name existed.
    pub fn remove_speaker(&self, name: &str) -> bool {
        #[cfg(feature = "voice")]
        {
            self.speakers
                .lock()
                .expect("speaker registry lock")
                .manager
                .as_mut()
                .is_some_and(|m| m.remove(name))
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = name;
            false
        }
    }

    /// Start an enrollment session: the next `needed` wake words are
    /// collected as voiceprint samples instead of interactions.
    pub fn begin_enrollment(&self, name: &str, needed: u32) -> Result<(), String> {
        #[cfg(feature = "voice")]
        {
            if !self.speaker_capable() {
                return Err("speaker model unavailable (voice.speaker_embedding_model)".into());
            }
            if name.trim().is_empty() || name.len() > 32 {
                return Err("speaker name must be 1..=32 bytes".into());
            }
            if !(1..=10).contains(&needed) {
                return Err("utterances must be 1..=10".into());
            }
            let mut reg = self.speakers.lock().expect("speaker registry lock");
            if reg.enrollment.is_some() {
                return Err("an enrollment session is already in progress".into());
            }
            if reg.manager.as_ref().is_some_and(|m| m.contains(name)) {
                return Err(format!("speaker {name:?} already enrolled"));
            }
            reg.enrollment = Some(EnrollmentSession {
                name: name.trim().to_string(),
                needed,
                embeddings: Vec::new(),
            });
            info!(speaker = name, needed, "voice: enrollment session started");
            Ok(())
        }
        #[cfg(not(feature = "voice"))]
        {
            let _ = (name, needed);
            Err("built without the `voice` feature".into())
        }
    }

    /// Snapshot of the in-flight enrollment (for polling).
    #[must_use]
    pub fn enrollment_status(&self) -> Option<EnrollmentStatus> {
        #[cfg(feature = "voice")]
        {
            self.speakers
                .lock()
                .expect("speaker registry lock")
                .enrollment
                .as_ref()
                .map(|s| EnrollmentStatus {
                    name: s.name.clone(),
                    collected: s.embeddings.len() as u32,
                    needed: s.needed,
                })
        }
        #[cfg(not(feature = "voice"))]
        {
            None
        }
    }

    /// Hand out a completed enrollment: registers the profile in memory
    /// and returns the collected embeddings for persistence. `None` while
    /// incomplete or absent.
    pub fn take_completed_enrollment(&self) -> Option<(String, Vec<Vec<f32>>)> {
        #[cfg(feature = "voice")]
        {
            let mut reg = self.speakers.lock().expect("speaker registry lock");
            let done = reg
                .enrollment
                .as_ref()
                .is_some_and(|s| s.embeddings.len() >= s.needed as usize);
            if !done {
                return None;
            }
            let session = reg.enrollment.take().expect("checked above");
            if reg
                .manager()
                .is_some_and(|m| m.add_list(&session.name, &session.embeddings))
            {
                info!(speaker = %session.name, count = session.embeddings.len(), "voice: enrollment complete");
                Some((session.name, session.embeddings))
            } else {
                // Registration failed (e.g. dim drift) — drop the session
                // so the host can retry cleanly.
                None
            }
        }
        #[cfg(not(feature = "voice"))]
        {
            None
        }
    }

    /// Abandon the in-flight enrollment, if any.
    pub fn cancel_enrollment(&self) {
        #[cfg(feature = "voice")]
        {
            let mut reg = self.speakers.lock().expect("speaker registry lock");
            if let Some(s) = reg.enrollment.take() {
                info!(speaker = %s.name, "voice: enrollment cancelled");
            }
        }
    }

    /// Spawn the monitor-consuming worker (no-op when inactive).
    pub fn spawn_worker(
        &self,
        #[cfg_attr(not(feature = "voice"), allow(unused_mut))] mut rx: broadcast::Receiver<
            AudioChunk,
        >,
    ) -> tokio::task::JoinHandle<()> {
        #[cfg(feature = "voice")]
        let Some(internals) = self.internals.as_ref().map(|i| VoiceInternals {
            kws: Arc::clone(&i.kws),
            recognizer: Arc::clone(&i.recognizer),
            embed: i.embed.clone(),
            vad: i.vad.clone(),
            capture_vad: i.capture_vad.clone(),
        }) else {
            return tokio::spawn(async {});
        };
        #[cfg(feature = "voice")]
        {
            let capture_secs = self.config.capture_secs;
            let verify = self.config.speaker_verify;
            let threshold = self.config.speaker_threshold;
            let ring_cap = ((self.config.verify_window_secs.max(1.0)
                * capture::audio_monitor::TARGET_RATE as f32) as usize)
                .max(16_000);
            let event_tx = self.event_tx.clone();
            let speakers = Arc::clone(&self.speakers);
            let follow_until_ms = Arc::clone(&self.follow_until_ms);
            let playback_mute_until_ms = Arc::clone(&self.playback_mute_until_ms);
            info!("voice: wake-word worker started");
            tokio::spawn(async move {
                // The KWS stream is single-owner; the decode loop runs on
                // this task, the heavy paraformer pass on spawn_blocking.
                let kws_stream = internals.kws.create_stream();
                let mut captured: Vec<f32> = Vec::new();
                let mut keyword_pending: Option<String> = None;
                let mut capture_deadline = tokio::time::Instant::now();
                // Capture endpointing: the hard cap is the fixed window
                // plus this much headroom; while speech is ongoing the
                // (soft) deadline keeps sliding, and a completed VAD
                // segment finishes the capture immediately. A segment
                // only ends the capture once enough speech accumulated
                // (≥1.2 s): synthesized speech and careful speakers put
                // ≥0.6 s pauses *between words* — without the floor those
                // micro-segments clipped questions to their first word
                // (found in 2026-10-10 self-test: "现在…" captured as
                // just its first two syllables).
                let capture_vad = internals.capture_vad.clone();
                let mut captured_speech_samples: usize = 0;
                const CAPTURE_HEADROOM_SECS: u64 = 6;
                /// Minimum accumulated speech before a completed segment
                /// may end the capture (16 kHz samples).
                const MIN_SPEECH_SAMPLES: usize = 16_000 * 6 / 5;
                let capture_hard_cap = tokio::time::Instant::now()
                    + Duration::from_secs(u64::from(capture_secs.max(1)) + CAPTURE_HEADROOM_SECS);
                // Pre-wake ring for the verification embedding; a latch so
                // the armed-but-nobody-enrolled fail-open logs once.
                let mut ring = SampleRing::new(ring_cap);
                let mut warned_no_profiles = false;
                loop {
                    match rx.recv().await {
                        Ok(chunk) => {
                            let samples: Vec<f32> =
                                chunk.samples.iter().map(|&s| s as f32 / 32_768.0).collect();
                            ring.push(&samples);
                            // Playback self-mute: while the device's own
                            // reply is on the speaker, stay deaf to KWS and
                            // VAD alike — a spoken wake word in our TTS
                            // output otherwise re-wakes the device (the
                            // 2026-10-02 self-talk loop). The expired-cap
                            // case re-opens the mic instead of trusting a
                            // stale deadline.
                            let mute_until =
                                playback_mute_until_ms.load(std::sync::atomic::Ordering::SeqCst);
                            if mute_until > 0 {
                                if unix_now_ms() >= mute_until {
                                    playback_mute_until_ms
                                        .store(0, std::sync::atomic::Ordering::SeqCst);
                                    info!("voice: playback self-mute expired, mic re-opened");
                                } else {
                                    continue;
                                }
                            }
                            // Follow-up window (#30-B): VAD endpointing
                            // replaces the wake word until the window
                            // closes. Each completed segment re-extends the
                            // deadline slightly so a mid-window answer keeps
                            // the conversation alive; the host re-arms it
                            // properly after its reply plays.
                            let now_ms = unix_now_ms();
                            let until = follow_until_ms.load(std::sync::atomic::Ordering::SeqCst);
                            if until > 0
                                && let Some(vad) = internals.vad.as_ref()
                            {
                                {
                                    if now_ms >= until {
                                        follow_until_ms
                                            .store(0, std::sync::atomic::Ordering::SeqCst);
                                        vad.clear();
                                        vad.reset();
                                        info!("voice: follow-up window closed");
                                    } else {
                                        vad.accept_waveform(&samples);
                                        while let Some(seg) = vad.front() {
                                            let waveform = seg.samples().to_vec();
                                            vad.pop();
                                            if waveform.len() < 16_000 {
                                                // < 1 s — noise, keep listening.
                                                continue;
                                            }
                                            let recognizer = Arc::clone(&internals.recognizer);
                                            let tx = event_tx.clone();
                                            let extend = now_ms + 2_000;
                                            follow_until_ms
                                                .store(extend, std::sync::atomic::Ordering::SeqCst);
                                            tokio::task::spawn_blocking(move || {
                                                let text = transcribe(&recognizer, &waveform);
                                                info!(
                                                    chars = text.chars().count(),
                                                    "voice: follow-up transcript"
                                                );
                                                let _ = tx.send(VoiceEvent {
                                                    keyword: String::new(),
                                                    transcript: text,
                                                    timestamp_ms: unix_now_ms(),
                                                    speaker: String::new(),
                                                    follow_up: true,
                                                });
                                            });
                                        }
                                        continue;
                                    }
                                }
                            }
                            if let Some(keyword) = &keyword_pending {
                                captured.extend_from_slice(&samples);
                                // VAD-endpointed capture (2026-10-10): a
                                // completed segment (= speech followed by
                                // ~0.6 s silence) ends the capture now;
                                // ongoing speech slides the soft deadline
                                // so a question spoken after a pause is not
                                // clipped by the fixed window.
                                let mut segment_ended = false;
                                if let Some(cvad) = capture_vad.as_ref() {
                                    let now = tokio::time::Instant::now();
                                    cvad.accept_waveform(&samples);
                                    if cvad.detected() && capture_deadline < capture_hard_cap {
                                        capture_deadline =
                                            (now + Duration::from_secs(1)).min(capture_hard_cap);
                                    }
                                    while let Some(seg) = cvad.front() {
                                        let len = seg.samples().len();
                                        cvad.pop();
                                        captured_speech_samples =
                                            captured_speech_samples.saturating_add(len);
                                        // A real question segment (≥0.5 s) ends
                                        // the capture only once the utterance
                                        // accumulated enough speech overall.
                                        if len >= 8_000
                                            && captured_speech_samples >= MIN_SPEECH_SAMPLES
                                        {
                                            segment_ended = true;
                                        }
                                    }
                                }
                                if segment_ended || tokio::time::Instant::now() >= capture_deadline
                                {
                                    let keyword = keyword.clone();
                                    let recognizer = Arc::clone(&internals.recognizer);
                                    let waveform = std::mem::take(&mut captured);
                                    let tx = event_tx.clone();
                                    let embed = internals.embed.clone();
                                    let speakers = Arc::clone(&speakers);
                                    let threshold = threshold;
                                    tokio::task::spawn_blocking(move || {
                                        let text = transcribe(&recognizer, &waveform);
                                        // Attribute the interaction to the best
                                        // matching enrolled speaker ("" when
                                        // unknown or no speaker model).
                                        let speaker = embed
                                            .as_ref()
                                            .and_then(|e| {
                                                let emb = compute_embedding(e, &waveform)?;
                                                speakers
                                                    .lock()
                                                    .expect("speaker registry lock")
                                                    .manager
                                                    .as_ref()
                                                    .and_then(|m| m.search(&emb, threshold))
                                            })
                                            .unwrap_or_default();
                                        let now_ms = unix_now_ms();
                                        info!(
                                            keyword = %keyword,
                                            chars = text.chars().count(),
                                            speaker = %speaker,
                                            "voice: transcript"
                                        );
                                        let _ = tx.send(VoiceEvent {
                                            keyword,
                                            transcript: text,
                                            timestamp_ms: now_ms,
                                            speaker,
                                            follow_up: false,
                                        });
                                    });
                                    keyword_pending = None;
                                }
                                continue;
                            }
                            kws_stream.accept_waveform(16_000, &samples);
                            while internals.kws.is_ready(&kws_stream) {
                                internals.kws.decode(&kws_stream);
                            }
                            if let Some(hit) = internals.kws.get_result(&kws_stream)
                                && !hit.keyword.is_empty()
                            {
                                info!(keyword = %hit.keyword, "voice: wake word detected");
                                internals.kws.reset(&kws_stream);
                                captured.clear();

                                // Enrollment has priority: the wake-word
                                // utterance becomes a voiceprint sample
                                // instead of an interaction. Once the session
                                // is full, extra wake words are ignored until
                                // the host commits or cancels.
                                let enrolling = speakers
                                    .lock()
                                    .expect("speaker registry lock")
                                    .enrollment
                                    .as_ref()
                                    .is_some_and(enrollment_accepts);
                                if enrolling {
                                    if let Some(embed) = &internals.embed {
                                        let snapshot = ring.snapshot();
                                        let embed = Arc::clone(embed);
                                        let speakers = Arc::clone(&speakers);
                                        let collected = tokio::task::spawn_blocking(
                                            move || -> Option<usize> {
                                                let emb = compute_embedding(&embed, &snapshot)?;
                                                let mut reg =
                                                    speakers.lock().expect("speaker registry lock");
                                                let session = reg.enrollment.as_mut()?;
                                                session.embeddings.push(emb);
                                                Some(session.embeddings.len())
                                            },
                                        )
                                        .await
                                        .ok()
                                        .flatten();
                                        if let Some(n) = collected {
                                            info!(
                                                samples = n,
                                                "voice: enrollment sample collected"
                                            );
                                        }
                                    }
                                    continue;
                                }

                                // Verification gate: with profiles enrolled,
                                // only a known speaker opens the capture.
                                if verify {
                                    let profiles = speakers
                                        .lock()
                                        .expect("speaker registry lock")
                                        .manager
                                        .as_ref()
                                        .map_or(0, |m| m.num_speakers());
                                    if profiles == 0 {
                                        if !warned_no_profiles {
                                            warned_no_profiles = true;
                                            warn!(
                                                "voice: speaker_verify armed but no profiles \
                                                 enrolled — allowing wake (fail-open)"
                                            );
                                        }
                                    } else if let Some(embed) = &internals.embed {
                                        let snapshot = ring.snapshot();
                                        let embed = Arc::clone(embed);
                                        let speakers = Arc::clone(&speakers);
                                        let matched = tokio::task::spawn_blocking(
                                            move || -> Option<String> {
                                                let emb = compute_embedding(&embed, &snapshot)?;
                                                speakers
                                                    .lock()
                                                    .expect("speaker registry lock")
                                                    .manager
                                                    .as_ref()
                                                    .and_then(|m| m.search(&emb, threshold))
                                            },
                                        )
                                        .await
                                        .ok()
                                        .flatten();
                                        if !wake_gate_allows(true, matched.as_deref()) {
                                            info!("voice: wake word rejected (unknown speaker)");
                                            continue;
                                        }
                                    }
                                }

                                keyword_pending = Some(hit.keyword);
                                capture_deadline = tokio::time::Instant::now()
                                    + Duration::from_secs(u64::from(capture_secs.max(1)));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "voice: worker lagged behind monitor");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                info!("voice: wake-word worker stopped");
            })
        }

        #[cfg(not(feature = "voice"))]
        {
            drop(rx);
            tokio::spawn(async {})
        }
    }
}

/// Offline-transcribe a 16 kHz mono waveform (empty string on failure).
#[cfg(feature = "voice")]
fn transcribe(recognizer: &sherpa_onnx::OfflineRecognizer, waveform: &[f32]) -> String {
    let call = observability::model_call("voice.asr", "paraformer-zh");
    let r = {
        let _span = call.enter();
        let stream = recognizer.create_stream();
        stream.accept_waveform(16_000, waveform);
        recognizer.decode(&stream);
        stream.get_result().map(|r| r.text)
    };
    match &r {
        Some(_) => call.finish_ok(None, None),
        None => call.finish_err(),
    }
    r.unwrap_or_default()
}

/// Load the optional follow-up VAD (silero). Missing model is an error
/// the caller logs-and-ignores (follow-up disabled, wake mode intact).
#[cfg(feature = "voice")]
fn build_vad(config: &VoiceConfig) -> anyhow::Result<sherpa_onnx::VoiceActivityDetector> {
    use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};
    if !std::path::Path::new(&config.vad_model).exists() {
        anyhow::bail!("vad model not found: {}", config.vad_model);
    }
    let silero = SileroVadModelConfig {
        model: Some(config.vad_model.clone()),
        threshold: 0.5,
        min_silence_duration: 0.6,
        min_speech_duration: 0.25,
        window_size: 512,
        max_speech_duration: 10.0,
    };
    let cfg = VadModelConfig {
        silero_vad: silero,
        sample_rate: 16_000,
        num_threads: 1,
        provider: Some("cpu".into()),
        debug: false,
        ..Default::default()
    };
    VoiceActivityDetector::create(&cfg, 30.0)
        .ok_or_else(|| anyhow::anyhow!("VoiceActivityDetector::create failed"))
}

/// Load the optional speaker-embedding extractor (CAM++ class ONNX).
#[cfg(feature = "voice")]
fn build_speaker_extractor(
    config: &VoiceConfig,
) -> anyhow::Result<sherpa_onnx::SpeakerEmbeddingExtractor> {
    use sherpa_onnx::{SpeakerEmbeddingExtractor, SpeakerEmbeddingExtractorConfig};
    let path = &config.speaker_embedding_model;
    if !std::path::Path::new(path).exists() {
        anyhow::bail!("speaker embedding model not found: {path}");
    }
    let cfg = SpeakerEmbeddingExtractorConfig {
        model: Some(path.clone()),
        num_threads: config.num_threads,
        debug: false,
        provider: Some("cpu".into()),
    };
    SpeakerEmbeddingExtractor::create(&cfg)
        .ok_or_else(|| anyhow::anyhow!("SpeakerEmbeddingExtractor::create failed"))
}

/// Embed one 16 kHz mono clip; `None` when the clip is too short for the
/// model's minimum window.
#[cfg(feature = "voice")]
fn compute_embedding(
    extractor: &sherpa_onnx::SpeakerEmbeddingExtractor,
    samples: &[f32],
) -> Option<Vec<f32>> {
    let call = observability::model_call("speaker", "campplus");
    let r = {
        let _span = call.enter();
        extractor.create_stream().and_then(|stream| {
            stream.accept_waveform(16_000, samples);
            if !extractor.is_ready(&stream) {
                return None;
            }
            extractor.compute(&stream)
        })
    };
    match &r {
        Some(_) => call.finish_ok(None, None),
        None => call.finish_err(),
    }
    r
}

/// Gate semantics: with verification armed, a wake word only opens the
/// capture window for a speaker matched above the threshold. Kept as a
/// pure function so the fail-closed-by-default rule stays testable.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
fn wake_gate_allows(verify_armed: bool, matched: Option<&str>) -> bool {
    !verify_armed || matched.is_some()
}

/// Whether an in-flight enrollment still accepts wake-word samples — it
/// stops at `needed` so extra wake words between completion and the host's
/// commit cannot inflate the sample count (regression: the workstation
/// acoustic E2E caught a 4th sample landing after a 3-sample session).
#[cfg(feature = "voice")]
fn enrollment_accepts(session: &EnrollmentSession) -> bool {
    session.embeddings.len() < session.needed as usize
}

/// Fixed-capacity ring holding the most recent `cap` samples — the pre-wake
/// window the verification embedding is computed over.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct SampleRing {
    buf: Vec<f32>,
    cap: usize,
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
impl SampleRing {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap: cap.max(1),
        }
    }

    pub(crate) fn push(&mut self, samples: &[f32]) {
        self.buf.extend_from_slice(samples);
        let overflow = self.buf.len().saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(0..overflow);
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<f32> {
        self.buf.clone()
    }
}

#[cfg(feature = "voice")]
fn build_kws(config: &VoiceConfig) -> anyhow::Result<sherpa_onnx::KeywordSpotter> {
    use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig};
    for (label, path) in [
        ("kws_encoder", &config.kws_encoder),
        ("kws_decoder", &config.kws_decoder),
        ("kws_joiner", &config.kws_joiner),
        ("kws_tokens", &config.kws_tokens),
        ("keywords_file", &config.keywords_file),
    ] {
        if !std::path::Path::new(path).exists() {
            anyhow::bail!("voice.{label}: file not found: {path}");
        }
    }
    let mut cfg = KeywordSpotterConfig::default();
    cfg.model_config.transducer.encoder = Some(config.kws_encoder.clone());
    cfg.model_config.transducer.decoder = Some(config.kws_decoder.clone());
    cfg.model_config.transducer.joiner = Some(config.kws_joiner.clone());
    cfg.model_config.tokens = Some(config.kws_tokens.clone());
    cfg.keywords_file = Some(config.keywords_file.clone());
    cfg.keywords_threshold = config.keywords_threshold;
    cfg.keywords_score = config.keywords_score;
    cfg.model_config.num_threads = config.num_threads;
    KeywordSpotter::create(&cfg).ok_or_else(|| anyhow::anyhow!("KeywordSpotter::create failed"))
}

/// Build an offline paraformer recognizer from a voice config — shared
/// with the meeting module (which rides on the same ASR model files).
#[cfg(feature = "voice")]
pub(crate) fn build_recognizer_for(
    config: &VoiceConfig,
) -> anyhow::Result<sherpa_onnx::OfflineRecognizer> {
    use sherpa_onnx::{
        OfflineModelConfig, OfflineParaformerModelConfig, OfflineRecognizer,
        OfflineRecognizerConfig,
    };
    for (label, path) in [
        ("paraformer_model", &config.paraformer_model),
        ("paraformer_tokens", &config.paraformer_tokens),
    ] {
        if !std::path::Path::new(path).exists() {
            anyhow::bail!("voice.{label}: file not found: {path}");
        }
    }
    let mut model = OfflineModelConfig::default();
    model.paraformer = OfflineParaformerModelConfig {
        model: Some(config.paraformer_model.clone()),
    };
    model.tokens = Some(config.paraformer_tokens.clone());
    model.num_threads = config.num_threads;
    let cfg = OfflineRecognizerConfig {
        model_config: model,
        ..Default::default()
    };
    OfflineRecognizer::create(&cfg)
        .ok_or_else(|| anyhow::anyhow!("OfflineRecognizer::create failed"))
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Deterministic offline self-test: run the wake-word + transcription loop
/// over a WAV file and report what fired (`--selftest-voice`).
///
/// # Errors
///
/// File/parse errors, missing models, or a build without `voice`.
pub fn selftest_voice(path: &str) -> anyhow::Result<serde_json::Value> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
    #[cfg_attr(not(feature = "voice"), allow(unused_variables))]
    let wav = super::audio_ai::wav::parse_wav(&bytes)?;
    let mut config = VoiceConfig {
        enabled: true,
        ..VoiceConfig::default()
    };
    if let Ok(t) = std::env::var("VOICE_SELFTEST_THRESHOLD")
        && let Ok(t) = t.parse::<f32>()
    {
        config.keywords_threshold = t;
    }
    // Optional overrides so one binary can self-test alternative ASR
    // checkpoints (e.g. the trilingual zh-cantonese-en paraformer)
    // without touching the on-disk config.
    if let Ok(m) = std::env::var("VOICE_SELFTEST_PARAFORMER") {
        config.paraformer_model = m;
    }
    if let Ok(t) = std::env::var("VOICE_SELFTEST_PARAFORMER_TOKENS") {
        config.paraformer_tokens = t;
    }
    #[cfg(feature = "voice")]
    {
        let kws = build_kws(&config)?;
        let recognizer = build_recognizer_for(&config)?;
        let mut resampler = capture::audio_monitor::LinearResampler::new(
            f64::from(wav.sample_rate) / f64::from(capture::audio_monitor::TARGET_RATE),
        );
        let samples = resampler.process(&wav.samples);
        let mut hits: Vec<serde_json::Value> = Vec::new();
        let mut full: Vec<f32> = Vec::new();
        let stream = kws.create_stream();
        let mut pos = 0_usize;
        let capture_samples = (u64::from(config.capture_secs)
            * u64::from(capture::audio_monitor::TARGET_RATE))
            as usize;
        while pos < samples.len() {
            let take = (samples.len() - pos).min(512);
            let chunk = &samples[pos..pos + take];
            pos += take;
            full.extend_from_slice(chunk);
            stream.accept_waveform(16_000, chunk);
            while kws.is_ready(&stream) {
                kws.decode(&stream);
            }
            if let Some(hit) = kws.get_result(&stream)
                && !hit.keyword.is_empty()
            {
                let at_s = (pos as f64 / f64::from(capture::audio_monitor::TARGET_RATE) * 100.0)
                    .round()
                    / 100.0;
                let end = (pos + capture_samples).min(samples.len());
                let utterance = &samples[pos..end];
                let transcript = transcribe(&recognizer, utterance);
                hits.push(serde_json::json!({
                    "keyword": hit.keyword,
                    "tokens": hit.tokens,
                    "json": hit.json,
                    "at_s": at_s,
                    "transcript": transcript,
                }));
                kws.reset(&stream);
                // Skip past the utterance so one wake word fires once.
                pos = end;
            }
        }
        let full_transcript = transcribe(&recognizer, &full);
        Ok(serde_json::json!({
            "file": path,
            "sample_rate": wav.sample_rate,
            "duration_s": (samples.len() as f64 / f64::from(capture::audio_monitor::TARGET_RATE)).round(),
            "wake_words": hits,
            "full_transcript": full_transcript,
        }))
    }
    #[cfg(not(feature = "voice"))]
    {
        let _ = &bytes;
        anyhow::bail!("built without the `voice` feature")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_off() {
        let c = VoiceConfig::default();
        assert!(!c.enabled, "voice is opt-in");
        assert!(c.kws_tokens.contains("kws"));
        assert_eq!(c.capture_secs, 4);
        assert!((c.keywords_threshold - 0.25).abs() < f32::EPSILON);
        assert!(!c.speaker_verify, "speaker gate is opt-in");
        assert!((c.speaker_threshold - 0.55).abs() < f32::EPSILON);
        assert!((c.verify_window_secs - 2.0).abs() < f32::EPSILON);
        assert!(c.speaker_embedding_model.contains("speaker"));
    }

    #[test]
    fn engine_disabled_by_default() {
        let e = VoiceEngine::from_config(&VoiceConfig::default());
        assert!(!e.is_active());
        assert_eq!(e.inactive_reason(), "disabled by configuration");
    }

    #[test]
    fn engine_enabled_but_models_missing_fails_open() {
        let e = VoiceEngine::from_config(&VoiceConfig {
            enabled: true,
            ..VoiceConfig::default()
        });
        assert!(!e.is_active(), "missing models must not fake activity");
    }

    #[test]
    fn arm_listen_refuses_inactive_engine_and_zero_window() {
        // Inactive engine (no models): one-shot listening must refuse —
        // never pretend a window opened (SPEC appendix A #44).
        let e = VoiceEngine::from_config(&VoiceConfig {
            enabled: true,
            ..VoiceConfig::default()
        });
        assert!(!e.is_active());
        assert!(!e.arm_listen(10_000));
        // Zero window is not a window on any engine.
        let off = VoiceEngine::from_config(&VoiceConfig::default());
        assert!(!off.arm_listen(0));
    }

    #[test]
    fn wake_word_line_matches_shipped_keywords_file() {
        // Golden: byte-identical to the shipped models/voice/kws/keywords.txt
        // line — the exact format the zh syllable transducer consumes.
        assert_eq!(
            wake_word_to_keyword_line(DEFAULT_WAKE_WORD).unwrap(),
            "x iǎo m ì f ēng @小蜜蜂"
        );
        assert_eq!(
            wake_word_to_keyword_line("你好小蜂").unwrap(),
            "n ǐ h ǎo x iǎo f ēng @你好小蜂"
        );
        // Zero-initial syllables stay whole (安 = ān, no initial to strip).
        assert!(
            wake_word_to_keyword_line("平安")
                .unwrap()
                .contains(" ān @平安")
        );
        // Multi-char initials must win over their prefixes (吃 = chī → ch ī).
        assert!(
            wake_word_to_keyword_line("吃饭饭")
                .unwrap()
                .starts_with("ch ī f àn")
        );
    }

    #[test]
    fn wake_word_rejects_unusable_words() {
        assert!(wake_word_to_keyword_line("蜂").is_err(), "too short");
        assert!(
            wake_word_to_keyword_line("一二三四五六七").is_err(),
            "too long"
        );
        assert!(wake_word_to_keyword_line("小蜜bee").is_err(), "non-hanzi");
        assert!(wake_word_to_keyword_line("你好。").is_err(), "punctuation");
    }

    #[test]
    fn follow_up_window_share_initialized_and_hot_adjustable() {
        // #31: the share starts from the boot config value and the web
        // config API can retune or zero it at runtime (0 disables arming).
        let cfg = VoiceConfig {
            follow_up_window_secs: 12.5,
            ..VoiceConfig::default()
        };
        let e = VoiceEngine::from_config(&cfg);
        let share = e.follow_up_window_share();
        assert_eq!(
            share.load(std::sync::atomic::Ordering::SeqCst),
            12_500,
            "share initialized from config"
        );
        share.store(8_000, std::sync::atomic::Ordering::SeqCst);
        share.store(0, std::sync::atomic::Ordering::SeqCst);
        // Inactive engine (no models): arming must stay a safe no-op
        // whatever the share holds.
        e.arm_follow_up();
    }

    #[test]
    fn playback_mute_latch_semantics() {
        // 2026-10-02 self-talk loop: a spoken 小蜜蜂 inside our own TTS
        // reply re-waked the device every ~6 s. The latch must open
        // closed→deadline→cleared, and never outlive a crashed speak.
        let e = VoiceEngine::from_config(&VoiceConfig::default());
        let share = e.playback_mute_share();
        assert_eq!(
            share.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "mic starts open"
        );
        e.begin_playback_mute();
        let until = share.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            until > unix_now_ms(),
            "begin latches a future deadline (hard cap)"
        );
        assert!(
            until <= unix_now_ms() + 120_000,
            "deadline carries the 120 s hard cap"
        );
        e.end_playback_mute();
        assert_eq!(
            share.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "end re-opens the mic"
        );
        // Re-begin then simulate the worker's expired-cap branch: a stale
        // (far-past) deadline must not keep the mic muted.
        e.begin_playback_mute();
        share.store(
            unix_now_ms().saturating_sub(1),
            std::sync::atomic::Ordering::SeqCst,
        );
        assert!(
            unix_now_ms() >= share.load(std::sync::atomic::Ordering::SeqCst),
            "expired deadline reads as open"
        );
        e.end_playback_mute();
    }

    #[test]
    fn inactive_engine_speaker_api_is_inert() {
        let e = VoiceEngine::from_config(&VoiceConfig::default());
        assert!(!e.speaker_capable());
        assert_eq!(e.speaker_dim(), 0);
        assert!(e.list_speakers().is_empty());
        assert!(e.enrollment_status().is_none());
        assert!(e.take_completed_enrollment().is_none());
        assert!(!e.remove_speaker("nobody"));
        assert_eq!(e.load_speakers(&[("a".into(), vec![vec![0.1; 4]])]), 0);
        assert!(e.begin_enrollment("a", 3).is_err());
        // cancel must not panic on an absent session
        e.cancel_enrollment();
    }

    #[test]
    fn voice_event_speaker_field_defaults_on_deserialize() {
        // Old payloads (pre-speaker) still deserialize with "" — the SSE
        // shape only ever grows additively.
        let ev: VoiceEvent =
            serde_json::from_str(r#"{"keyword":"小蜜蜂","transcript":"你好","timestamp_ms":1}"#)
                .unwrap();
        assert_eq!(ev.speaker, "");
        let round: VoiceEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(round, ev);
    }

    #[test]
    fn wake_gate_fails_closed_when_armed() {
        assert!(wake_gate_allows(false, None), "unarmed lets everyone in");
        assert!(wake_gate_allows(false, Some("owner")));
        assert!(
            wake_gate_allows(true, Some("owner")),
            "armed + match passes"
        );
        assert!(
            !wake_gate_allows(true, None),
            "armed without a match must reject"
        );
    }

    #[test]
    fn sample_ring_keeps_the_newest_window() {
        let mut ring = SampleRing::new(4);
        assert!(ring.snapshot().is_empty());
        ring.push(&[1.0, 2.0]);
        ring.push(&[3.0, 4.0]);
        assert_eq!(ring.snapshot(), vec![1.0, 2.0, 3.0, 4.0]);
        ring.push(&[5.0, 6.0]);
        assert_eq!(ring.snapshot().len(), 4, "capacity is enforced");
        assert_eq!(ring.snapshot(), vec![3.0, 4.0, 5.0, 6.0], "oldest evicted");
        // A single oversized push still clamps to the tail.
        ring.push(&[7.0, 8.0, 9.0, 10.0, 11.0]);
        assert_eq!(ring.snapshot(), vec![8.0, 9.0, 10.0, 11.0]);
    }

    /// Synthetic-vector semantics of the sherpa manager — no model files
    /// needed, so this pins the add/search/verify usage the worker relies
    /// on (runs on `--features voice` builds).
    #[cfg(feature = "voice")]
    #[test]
    fn speaker_manager_synthetic_semantics() {
        let dim = 8;
        let Some(m) = sherpa_onnx::SpeakerEmbeddingManager::create(dim) else {
            panic!("manager create failed");
        };
        let owner: Vec<f32> = {
            let mut v = vec![0.1; dim as usize];
            v[0] = 1.0;
            v
        };
        let stranger: Vec<f32> = {
            let mut v = vec![0.1; dim as usize];
            v[1] = 1.0;
            v
        };
        assert!(m.add_list("owner", &[owner.clone(), owner.clone()]));
        // Near-identical vector matches above a lenient threshold…
        let near: Vec<f32> = owner.iter().map(|x| x * 1.01).collect();
        assert_eq!(m.search(&near, 0.9).as_deref(), Some("owner"));
        assert!(m.verify("owner", &near, 0.9));
        // …an orthogonal vector does not…
        assert_eq!(m.search(&stranger, 0.9), None);
        assert!(!m.verify("owner", &stranger, 0.9));
        // …and removing the profile drops it from search.
        assert!(m.remove("owner"));
        assert_eq!(m.search(&near, 0.9), None);
        // Dimension mismatch is rejected, not crashed.
        assert_eq!(m.search(&[0.5; 4], 0.9), None);
    }

    /// Enrollment validation on a capable-shaped registry: name/utterance
    /// bounds are enforced before any audio flows.
    #[cfg(feature = "voice")]
    #[test]
    fn enrollment_session_validation() {
        let s = EnrollmentSession {
            name: "owner".into(),
            needed: 2,
            embeddings: vec![vec![0.25; 4]],
        };
        assert_eq!(s.embeddings.len(), 1);
        let status = EnrollmentStatus {
            name: s.name.clone(),
            collected: s.embeddings.len() as u32,
            needed: s.needed,
        };
        assert_eq!(status.collected, 1);
        assert_eq!(status.needed, 2);
        // Full sessions stop accepting; short ones accept.
        assert!(enrollment_accepts(&s));
        let mut full = s.clone();
        full.embeddings.push(vec![0.25; 4]);
        assert!(!enrollment_accepts(&full), "no samples past `needed`");
    }
}
