//! On-demand meeting mode: record the 16 kHz monitor stream for an explicit
//! session, then run offline speaker diarization + per-segment ASR to produce
//! a text minute (SPEC appendix A #27).
//!
//! Privacy posture: nothing is recorded outside an explicit
//! `start` → `stop` window. The WAV lands in `meeting.audio_dir` and is
//! deleted after processing unless `keep_audio` is set — a failed pipeline
//! deletes it too (privacy wins over debuggability; the error text survives
//! in the meeting row). `max_duration_secs` auto-stops a forgotten session
//! and feeds it through the same pipeline.
//!
//! The recording/WAV machinery below is plain Rust and compiles in every
//! feature set (so CI covers it); only `process_meeting` (diarization +
//! ASR + punctuation models via sherpa-onnx) needs the `voice` feature.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

/// Meeting-mode configuration (`[meeting]` TOML section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeetingConfig {
    /// Master switch — default off keeps the no-audio-at-standby promise.
    pub enabled: bool,
    /// pyannote segmentation ONNX (sherpa-onnx converted, ~7 MB, MIT).
    pub segmentation_model: String,
    /// ct-transformer punctuation ONNX (int8, ~65 MB). Empty string
    /// disables punctuation restoration.
    pub punctuation_model: String,
    /// Fast-clustering distance threshold (higher = fewer speakers).
    pub clustering_threshold: f32,
    /// Diarization tuning: minimum voiced/unvoiced segment durations (s).
    pub min_duration_on: f32,
    pub min_duration_off: f32,
    /// Keep the session WAV after processing (default: delete).
    pub keep_audio: bool,
    /// Safety cap: a recording auto-stops at this age and is processed.
    pub max_duration_secs: u64,
    /// Directory (relative to the working directory) for session WAVs.
    pub audio_dir: String,
    /// Inference threads for the meeting models.
    pub num_threads: i32,
}

impl Default for MeetingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            segmentation_model: "models/voice/diarization/pyannote.onnx".into(),
            punctuation_model: "models/voice/punct/model.onnx".into(),
            clustering_threshold: 0.5,
            min_duration_on: 0.3,
            min_duration_off: 0.5,
            keep_audio: false,
            max_duration_secs: 7200,
            audio_dir: "meetings".into(),
            num_threads: 1,
        }
    }
}

/// Voiceprint resolver: embeds a 16 kHz clip and returns the best
/// enrolled-speaker name (None = anonymous). Wired by the host from the
/// voice engine's registry.
pub type SpeakerLookup = std::sync::Arc<dyn Fn(&[f32]) -> Option<String> + Send + Sync>;

/// A finished recording handed to the processing pipeline.
#[derive(Debug, Clone)]
pub struct RecordedMeeting {
    /// Database row id (assigned by the web layer when the row was created).
    pub id: i64,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    /// The finalized WAV file (16 kHz mono i16 PCM).
    pub path: PathBuf,
}

/// One diarized + transcribed segment of a processed meeting.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Cluster index from the diarizer (0-based).
    pub speaker_index: i32,
    /// Enrolled voiceprint name when a majority of the cluster's segments
    /// matched a profile; empty string otherwise (UI renders "speaker N").
    pub speaker: String,
    pub text: String,
}

/// The full result of processing one recorded meeting.
#[derive(Debug, Clone)]
pub struct MeetingTranscript {
    pub num_speakers: i32,
    pub duration_ms: u64,
    pub segments: Vec<TranscriptSegment>,
}

// ---------------------------------------------------------------------------
// WAV writer
// ---------------------------------------------------------------------------

/// Minimal streaming 16-bit PCM mono WAV writer. The header is written with
/// zero sizes and patched on `finalize` — crash-safe enough for a session
/// recording (a torn file loses the tail but stays parseable after patching
/// is skipped; `finalize` is the only path that rewrites the header).
#[derive(Debug)]
pub(crate) struct WavWriter {
    file: std::fs::File,
    samples: u64,
}

impl WavWriter {
    /// Create a new WAV at `path` (parent directories are not created).
    ///
    /// # Errors
    ///
    /// Filesystem errors from creating/writing the file.
    pub(crate) fn create(path: &std::path::Path) -> std::io::Result<Self> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(&Self::header(0))?;
        Ok(Self { file, samples: 0 })
    }

    fn header(samples: u64) -> Vec<u8> {
        let data_len = (samples * 2) as u32;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data_len).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16_u32.to_le_bytes());
        h.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        h.extend_from_slice(&1_u16.to_le_bytes()); // mono
        h.extend_from_slice(&16_000_u32.to_le_bytes());
        h.extend_from_slice(&(16_000 * 2_u32).to_le_bytes()); // byte rate
        h.extend_from_slice(&2_u16.to_le_bytes()); // block align
        h.extend_from_slice(&16_u16.to_le_bytes()); // bits
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    }

    /// Append i16 samples (assumed 16 kHz mono, matching the monitor).
    ///
    /// # Errors
    ///
    /// Filesystem write errors (disk full is the realistic one).
    pub(crate) fn write_i16(&mut self, samples: &[i16]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    /// Patch the header sizes and consume the writer.
    ///
    /// # Errors
    ///
    /// Seek/write errors while patching the header.
    pub(crate) fn finalize(mut self) -> std::io::Result<()> {
        use std::io::Seek;
        self.file.flush()?;
        self.file.rewind()?;
        self.file.write_all(&Self::header(self.samples))?;
        self.file.flush()
    }

    #[cfg(test)]
    fn samples_written(&self) -> u64 {
        self.samples
    }
}

// ---------------------------------------------------------------------------
// Pure pipeline helpers (unit-tested without any model)
// ---------------------------------------------------------------------------

/// A raw diarization segment before merging.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(crate) struct DiarSegment {
    pub start: f32,
    pub end: f32,
    pub speaker: i32,
}

/// Merge adjacent segments of the same speaker when the gap between them is
/// at most `gap_tolerance` seconds — pyannote emits fine-grained turns and
/// one person's continuous sentence should become one ASR call.
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(crate) fn merge_segments(segments: Vec<DiarSegment>, gap_tolerance: f32) -> Vec<DiarSegment> {
    let mut merged: Vec<DiarSegment> = Vec::new();
    for seg in segments {
        match merged.last_mut() {
            Some(prev) if prev.speaker == seg.speaker && seg.start - prev.end <= gap_tolerance => {
                prev.end = seg.end.max(prev.end);
            }
            _ => merged.push(seg),
        }
    }
    merged
}

/// Majority vote over a cluster's per-segment voiceprint lookups. A name
/// wins only with strictly more than half of the non-None votes (ties and
/// empty tallies stay anonymous — an honest miss beats a coin flip).
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub(crate) fn vote_cluster_name(votes: &[Option<String>]) -> Option<String> {
    let cast: Vec<&String> = votes.iter().flatten().collect();
    if cast.is_empty() {
        return None;
    }
    let mut counts: std::collections::HashMap<&String, usize> = Default::default();
    for v in &cast {
        *counts.entry(v).or_default() += 1;
    }
    let (name, n) = counts.iter().max_by_key(|&(_, n)| *n)?;
    (n * 2 > cast.len()).then(|| (*name).clone())
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

struct RecordingState {
    id: i64,
    started_at_ms: u64,
    path: PathBuf,
    writer: WavWriter,
}

/// On-demand meeting recorder + processor. Recording works in every feature
/// set; processing (diarization/ASR/punctuation) requires `voice`.
pub struct MeetingEngine {
    config: MeetingConfig,
    #[cfg_attr(not(feature = "voice"), allow(dead_code))]
    voice: super::voice::VoiceConfig,
    /// Shared with the recorder worker (the web routes create sessions,
    /// the worker appends chunks and auto-stops).
    recording: std::sync::Arc<std::sync::Mutex<Option<RecordingState>>>,
    finished_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RecordedMeeting>>>,
    finished_tx: tokio::sync::mpsc::UnboundedSender<RecordedMeeting>,
    /// Voiceprint resolver wired by the host (voice engine); `None` leaves
    /// every cluster anonymous.
    speaker_lookup: std::sync::Mutex<Option<SpeakerLookup>>,
    /// Lazily built model pile (`voice` builds only).
    #[cfg(feature = "voice")]
    models: std::sync::Mutex<Option<std::sync::Arc<MeetingModels>>>,
    /// Why the engine is inactive ("" when active).
    inactive_reason: String,
}

impl MeetingEngine {
    /// Build an engine from the `[meeting]` + `[voice]` configs. The voice
    /// config supplies the ASR (paraformer) model paths meetings reuse.
    pub fn from_config(config: &MeetingConfig, voice: &super::voice::VoiceConfig) -> Self {
        let (finished_tx, finished_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut reason = String::new();
        if !config.enabled {
            reason = "disabled in config".into();
        } else if !std::path::Path::new(&config.segmentation_model).exists() {
            reason = format!(
                "segmentation model not found: {}",
                config.segmentation_model
            );
        } else if !std::path::Path::new(&voice.paraformer_model).exists() {
            reason = format!("ASR model not found: {}", voice.paraformer_model);
        } else if !std::path::Path::new(&voice.paraformer_tokens).exists() {
            reason = format!("ASR tokens not found: {}", voice.paraformer_tokens);
        }
        Self {
            config: config.clone(),
            voice: voice.clone(),
            recording: std::sync::Arc::new(std::sync::Mutex::new(None)),
            finished_rx: std::sync::Mutex::new(Some(finished_rx)),
            finished_tx,
            speaker_lookup: std::sync::Mutex::new(None),
            #[cfg(feature = "voice")]
            models: std::sync::Mutex::new(None),
            inactive_reason: reason,
        }
    }

    pub fn is_active(&self) -> bool {
        self.inactive_reason.is_empty()
    }

    pub fn inactive_reason(&self) -> &str {
        &self.inactive_reason
    }

    /// Wire the voiceprint resolver (host-side closure over the voice
    /// engine's speaker registry). Anonymous clusters when unset.
    pub fn set_speaker_lookup(&self, f: SpeakerLookup) {
        *self
            .speaker_lookup
            .lock()
            .expect("meeting speaker lookup lock") = Some(f);
    }

    /// Take the auto-stop finished-meeting receiver (one-shot; the host
    /// spawns the processing bridge over it).
    pub fn finished_receiver(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<RecordedMeeting>> {
        self.finished_rx.lock().expect("finished rx lock").take()
    }

    /// Begin a recording session for DB row `id`. Fails when a session is
    /// already running or the audio directory cannot be created.
    pub fn begin_recording(&self, id: i64) -> Result<PathBuf, String> {
        let mut guard = self.recording.lock().expect("meeting state lock");
        if !self.is_active() {
            return Err("meeting engine inactive".into());
        }
        if guard.is_some() {
            return Err("already recording".into());
        }
        let started = unix_now_ms();
        let dir = PathBuf::from(&self.config.audio_dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let path = dir.join(format!("meeting-{id}-{started}.wav"));
        let writer = WavWriter::create(&path).map_err(|e| format!("create wav: {e}"))?;
        *guard = Some(RecordingState {
            id,
            started_at_ms: started,
            path: path.clone(),
            writer,
        });
        Ok(path)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.lock().expect("meeting state lock").is_some()
    }

    /// Id of the running session (None when idle).
    pub fn recording_id(&self) -> Option<i64> {
        self.recording
            .lock()
            .expect("meeting state lock")
            .as_ref()
            .map(|r| r.id)
    }

    /// Stop the current session and return the recorded meeting. The id
    /// must match the running session (stale stop requests fail).
    pub fn finish_recording(&self, id: i64) -> Result<RecordedMeeting, String> {
        let mut guard = self.recording.lock().expect("meeting state lock");
        match guard.take() {
            Some(state) if state.id == id => {
                state
                    .writer
                    .finalize()
                    .map_err(|e| format!("finalize wav: {e}"))?;
                Ok(RecordedMeeting {
                    id: state.id,
                    started_at_ms: state.started_at_ms,
                    ended_at_ms: unix_now_ms(),
                    path: state.path,
                })
            }
            Some(state) => {
                // Not the caller's session — put it back untouched.
                *guard = Some(state);
                Err("id does not match the running meeting".into())
            }
            None => Err("not recording".into()),
        }
    }

    /// Spawn the monitor-consuming worker: appends chunks to the running
    /// session's WAV and auto-stops at `max_duration_secs` (the finished
    /// meeting rides the `finished` channel to the host bridge).
    pub fn spawn_worker(
        &self,
        mut rx: tokio::sync::broadcast::Receiver<capture::audio_monitor::AudioChunk>,
    ) -> tokio::task::JoinHandle<()> {
        if !self.is_active() {
            return tokio::spawn(async {});
        }
        let max_secs = self.config.max_duration_secs;
        // The worker owns cloned handles (shared recording slot + the
        // finished channel) — not the whole engine.
        let state = SharedState {
            recording: std::sync::Arc::clone(&self.recording),
            finished: self.finished_tx.clone(),
        };
        tracing::info!("meeting: recorder worker started");
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(chunk) => {
                        let now = unix_now_ms();
                        let auto_stop = {
                            let mut guard = state.recording.lock().unwrap();
                            match guard.as_mut() {
                                Some(rec) => {
                                    if let Err(e) = rec.writer.write_i16(&chunk.samples) {
                                        tracing::error!(error = %e, "meeting: wav write failed");
                                        // A failing disk must not silently
                                        // produce a broken minute: stop the
                                        // session and let processing see the
                                        // truncated file.
                                        true
                                    } else {
                                        now.saturating_sub(rec.started_at_ms)
                                            >= max_secs.saturating_mul(1000)
                                    }
                                }
                                None => false,
                            }
                        };
                        if auto_stop
                            && let Some(rec) = state.finish_current()
                            && state.finished.send(rec).is_err()
                        {
                            tracing::warn!("meeting: auto-stop receiver gone");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "meeting: worker lagged behind monitor");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::info!("meeting: recorder worker stopped");
        })
    }

    /// Process a finished recording end to end (blocking — run inside
    /// `spawn_blocking`). Diarize → merge → transcribe → punctuate →
    /// vote speaker names.
    ///
    /// # Errors
    ///
    /// Model/filesystem failures; the caller records the error string on
    /// the meeting row and (per config) deletes the WAV.
    pub fn process_meeting(&self, rec: &RecordedMeeting) -> anyhow::Result<MeetingTranscript> {
        #[cfg(not(feature = "voice"))]
        {
            let _ = rec;
            anyhow::bail!("meeting processing requires a build with the `voice` feature");
        }
        #[cfg(feature = "voice")]
        {
            // Voice-round yielding (SPEC appendix A #55): meeting batch
            // transcription is pure background work; a live utterance
            // outranks it. Bounded wait, fail-open past the bound.
            crate::wait_voice_idle(std::time::Duration::from_secs(3));
            // One span for the whole batch pipeline (diarize → ASR →
            // punctuate → name-vote; SPEC appendix A #39: model id
            // `meeting` — a long-running composite job, not per-frame).
            let call = observability::model_call("meeting", "pyannote+paraformer");
            let r = self.process_meeting_inner(rec);
            match &r {
                Ok(_) => call.finish_ok(None, None),
                Err(_) => call.finish_err(),
            }
            r
        }
    }

    #[cfg(feature = "voice")]
    fn process_meeting_inner(&self, rec: &RecordedMeeting) -> anyhow::Result<MeetingTranscript> {
        {
            let bytes = std::fs::read(&rec.path)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", rec.path.display()))?;
            let wav = super::audio_ai::wav::parse_wav(&bytes)?;
            let samples = resample_to_diarizer_rate(&wav, self).map_err(anyhow::Error::msg)?;

            let models = self.models().map_err(|e| anyhow::anyhow!("{e}"))?;
            let raw = models
                .diarizer
                .process(&samples)
                .ok_or_else(|| anyhow::anyhow!("diarization process failed"))?;
            let num_speakers = raw.num_speakers();
            let diar: Vec<DiarSegment> = raw
                .sort_by_start_time()
                .into_iter()
                .map(|s| DiarSegment {
                    start: s.start,
                    end: s.end,
                    speaker: s.speaker,
                })
                .collect();
            let merged = merge_segments(diar, self.config.min_duration_off);

            // Per-cluster voiceprint votes (bounded work: first 5 segments
            // per cluster, ≥1s each).
            let lookup = self
                .speaker_lookup
                .lock()
                .expect("meeting speaker lookup lock")
                .clone();
            let mut names: std::collections::HashMap<i32, String> = Default::default();
            if let Some(lookup) = lookup {
                let mut votes: std::collections::HashMap<i32, Vec<Option<String>>> =
                    Default::default();
                for seg in &merged {
                    let bucket = votes.entry(seg.speaker).or_default();
                    if bucket.len() >= 5 {
                        continue;
                    }
                    let from = sample_index(&samples, seg.start);
                    let to = sample_index(&samples, seg.end);
                    if to.saturating_sub(from) < 16_000 {
                        continue;
                    }
                    bucket.push(lookup(&samples[from..to]));
                }
                for (idx, v) in votes {
                    if let Some(name) = vote_cluster_name(&v) {
                        names.insert(idx, name);
                    }
                }
            }

            let duration_ms = (samples.len() as f64 / 16_000.0 * 1000.0).round() as u64;
            let mut segments = Vec::with_capacity(merged.len());
            for seg in merged {
                let from = sample_index(&samples, seg.start);
                let to = sample_index(&samples, seg.end);
                let text = transcribe_segment(&models.recognizer, &samples[from..to]);
                let text = models
                    .punct
                    .as_ref()
                    .and_then(|p| p.add_punctuation(&text))
                    .unwrap_or(text);
                segments.push(TranscriptSegment {
                    start_ms: (seg.start * 1000.0).round() as u64,
                    end_ms: (seg.end * 1000.0).round() as u64,
                    speaker_index: seg.speaker,
                    speaker: names.get(&seg.speaker).cloned().unwrap_or_default(),
                    text,
                });
            }
            Ok(MeetingTranscript {
                num_speakers,
                duration_ms,
                segments,
            })
        }
    }

    /// Delete the session WAV (unless `keep_audio`) — applied on both
    /// success and failure paths. No-op for missing files.
    pub fn cleanup_audio(&self, rec: &RecordedMeeting) -> Option<PathBuf> {
        if self.config.keep_audio {
            return Some(rec.path.clone());
        }
        match std::fs::remove_file(&rec.path) {
            Ok(()) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(error = %e, path = %rec.path.display(), "meeting: audio cleanup failed");
                Some(rec.path.clone())
            }
        }
    }

    #[cfg(feature = "voice")]
    fn models(&self) -> Result<std::sync::Arc<MeetingModels>, String> {
        let mut guard = self.models.lock().expect("meeting models lock");
        if let Some(m) = guard.as_ref() {
            return Ok(std::sync::Arc::clone(m));
        }
        let built = MeetingModels::build(&self.config, &self.voice)?;
        let arc = std::sync::Arc::new(built);
        *guard = Some(std::sync::Arc::clone(&arc));
        Ok(arc)
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg_attr(not(feature = "voice"), allow(dead_code))]
fn sample_index(samples: &[f32], secs: f32) -> usize {
    ((secs.max(0.0) * 16_000.0) as usize).min(samples.len())
}

/// Worker-side handle: the pieces of the engine the recording loop needs,
/// split out so the spawned task does not carry the whole engine.
struct SharedState {
    recording: std::sync::Arc<std::sync::Mutex<Option<RecordingState>>>,
    finished: tokio::sync::mpsc::UnboundedSender<RecordedMeeting>,
}

impl SharedState {
    fn finish_current(&self) -> Option<RecordedMeeting> {
        let mut guard = self.recording.lock().unwrap();
        guard.take().map(|state| {
            if let Err(e) = state.writer.finalize() {
                tracing::error!(error = %e, "meeting: finalize wav failed");
            }
            RecordedMeeting {
                id: state.id,
                started_at_ms: state.started_at_ms,
                ended_at_ms: unix_now_ms(),
                path: state.path,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Model pile (`voice` builds)
// ---------------------------------------------------------------------------

#[cfg(feature = "voice")]
struct MeetingModels {
    diarizer: sherpa_onnx::OfflineSpeakerDiarization,
    recognizer: sherpa_onnx::OfflineRecognizer,
    punct: Option<sherpa_onnx::OfflinePunctuation>,
}

#[cfg(feature = "voice")]
impl MeetingModels {
    fn build(config: &MeetingConfig, voice: &super::voice::VoiceConfig) -> Result<Self, String> {
        use sherpa_onnx::{
            FastClusteringConfig, OfflinePunctuation, OfflinePunctuationConfig,
            OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
            OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
            SpeakerEmbeddingExtractorConfig,
        };
        if !std::path::Path::new(&config.segmentation_model).exists() {
            return Err(format!(
                "segmentation model not found: {}",
                config.segmentation_model
            ));
        }
        // The diarizer runs segmentation + embedding + clustering as one
        // pipeline — the embedding model is REQUIRED here (unlike the
        // wake-word engine where it only gates the optional voiceprint
        // features).
        if !std::path::Path::new(&voice.speaker_embedding_model).exists() {
            return Err(format!(
                "speaker embedding model not found: {}",
                voice.speaker_embedding_model
            ));
        }
        let embedding = SpeakerEmbeddingExtractorConfig {
            model: Some(voice.speaker_embedding_model.clone()),
            num_threads: config.num_threads,
            debug: false,
            provider: Some("cpu".into()),
        };
        let diarizer = OfflineSpeakerDiarization::create(&OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(config.segmentation_model.clone()),
                    window_shift_ratio: 0.1,
                },
                num_threads: config.num_threads,
                debug: false,
                provider: Some("cpu".into()),
            },
            embedding,
            clustering: FastClusteringConfig {
                num_clusters: -1,
                threshold: config.clustering_threshold,
                compute_confidence: false,
            },
            min_duration_on: config.min_duration_on,
            min_duration_off: config.min_duration_off,
        })
        .ok_or("OfflineSpeakerDiarization::create failed")?;

        let recognizer =
            super::voice::build_recognizer_for(voice).map_err(|e| format!("ASR model: {e}"))?;

        let punct = if config.punctuation_model.is_empty() {
            None
        } else if std::path::Path::new(&config.punctuation_model).exists() {
            let cfg = OfflinePunctuationConfig {
                model: sherpa_onnx::OfflinePunctuationModelConfig {
                    ct_transformer: Some(config.punctuation_model.clone()),
                    num_threads: config.num_threads,
                    debug: false,
                    provider: Some("cpu".into()),
                },
            };
            OfflinePunctuation::create(&cfg)
        } else {
            tracing::warn!(
                path = %config.punctuation_model,
                "meeting: punctuation model missing — continuing without"
            );
            None
        };

        Ok(Self {
            diarizer,
            recognizer,
            punct,
        })
    }
}

#[cfg(feature = "voice")]
fn transcribe_segment(recognizer: &sherpa_onnx::OfflineRecognizer, waveform: &[f32]) -> String {
    let stream = recognizer.create_stream();
    stream.accept_waveform(16_000, waveform);
    recognizer.decode(&stream);
    stream.get_result().map(|r| r.text).unwrap_or_default()
}

/// Resample the parsed WAV to the diarizer's expected rate when needed
/// (monitor audio is 16 kHz; the pyannote conversion also expects 16 kHz so
/// this is a no-op in practice — kept for the API's honesty).
#[cfg(feature = "voice")]
fn resample_to_diarizer_rate(
    wav: &super::audio_ai::wav::WavData,
    engine: &MeetingEngine,
) -> Result<Vec<f32>, String> {
    let rate = engine.models()?.diarizer.sample_rate();
    if rate == wav.sample_rate as i32 {
        return Ok(wav.samples.clone());
    }
    let ratio = f64::from(wav.sample_rate) / f64::from(rate.max(1));
    let mut resampler = capture::audio_monitor::LinearResampler::new(ratio);
    Ok(resampler.process(&wav.samples))
}

// ---------------------------------------------------------------------------
// Self-test (voice builds): diarization only, on a user-supplied WAV
// ---------------------------------------------------------------------------

/// Diarization-only self-test over a WAV file (multi-speaker samples ship
/// in the sherpa-onnx `speaker-segmentation-models` release). Model paths
/// can be overridden via `MEETING_SELFTEST_SEGMENTATION` /
/// `MEETING_SELFTEST_THRESHOLD`.
pub fn selftest_meeting(path: &str) -> anyhow::Result<serde_json::Value> {
    #[cfg(feature = "voice")]
    {
        let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
        let wav = super::audio_ai::wav::parse_wav(&bytes)?;
        let mut config = MeetingConfig {
            enabled: true,
            ..MeetingConfig::default()
        };
        if let Ok(m) = std::env::var("MEETING_SELFTEST_SEGMENTATION") {
            config.segmentation_model = m;
        }
        let mut voice = super::voice::VoiceConfig::default();
        if let Ok(m) = std::env::var("MEETING_SELFTEST_EMBEDDING") {
            voice.speaker_embedding_model = m;
        }
        if let Ok(t) = std::env::var("MEETING_SELFTEST_THRESHOLD")
            && let Ok(t) = t.parse::<f32>()
        {
            config.clustering_threshold = t;
        }
        let models = MeetingModels::build(&config, &voice).map_err(|e| anyhow::anyhow!("{e}"))?;
        let rate = models.diarizer.sample_rate();
        let samples = if rate == wav.sample_rate as i32 {
            wav.samples.clone()
        } else {
            let ratio = f64::from(wav.sample_rate) / f64::from(rate.max(1));
            let mut r = capture::audio_monitor::LinearResampler::new(ratio);
            r.process(&wav.samples)
        };
        let started = std::time::Instant::now();
        let raw = models
            .diarizer
            .process(&samples)
            .ok_or_else(|| anyhow::anyhow!("diarization process failed"))?;
        let segs = raw.sort_by_start_time();
        Ok(serde_json::json!({
            "sample_rate": rate,
            "input_rate": wav.sample_rate,
            "duration_s": (samples.len() as f64 / f64::from(rate.max(1))).round(),
            "num_speakers": raw.num_speakers(),
            "num_segments": segs.len(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "segments": segs.iter().map(|s| serde_json::json!({
                "start_s": (s.start * 100.0).round() / 100.0,
                "end_s": (s.end * 100.0).round() / 100.0,
                "speaker": s.speaker,
            })).collect::<Vec<_>>(),
        }))
    }
    #[cfg(not(feature = "voice"))]
    {
        let _ = path;
        anyhow::bail!("meeting selftest requires a build with the `voice` feature");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute path of this source file — a stand-in "existing file" for
    /// the engine's model-path existence checks in default-feature tests.
    fn existing_file() -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/meeting.rs")
            .display()
            .to_string()
    }

    #[test]
    fn config_defaults_match_spec() {
        let c = MeetingConfig::default();
        assert!(!c.enabled);
        assert_eq!(
            c.segmentation_model,
            "models/voice/diarization/pyannote.onnx"
        );
        assert_eq!(c.punctuation_model, "models/voice/punct/model.onnx");
        assert!((c.clustering_threshold - 0.5).abs() < f32::EPSILON);
        assert!(!c.keep_audio);
        assert_eq!(c.max_duration_secs, 7200);
        assert_eq!(c.audio_dir, "meetings");
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let c = MeetingConfig::default();
        let s = toml::to_string(&c).expect("serialize");
        let back: MeetingConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(c, back);
    }

    #[test]
    fn wav_writer_roundtrips() {
        let dir = std::env::temp_dir().join(format!("mibee-meeting-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("t.wav");
        let mut w = WavWriter::create(&path).expect("create");
        w.write_i16(&[0, 16_384, -16_384, 8_192]).expect("write");
        assert_eq!(w.samples_written(), 4);
        w.finalize().expect("finalize");
        let bytes = std::fs::read(&path).expect("read");
        let wav = crate::audio_ai::wav::parse_wav(&bytes).expect("parse");
        assert_eq!(wav.sample_rate, 16_000);
        assert_eq!(wav.samples.len(), 4);
        assert!((wav.samples[1] - 0.5).abs() < 0.01);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_segments_joins_same_speaker_within_gap() {
        let segs = vec![
            DiarSegment {
                start: 0.0,
                end: 1.0,
                speaker: 0,
            },
            DiarSegment {
                start: 1.3,
                end: 2.0,
                speaker: 0,
            }, // gap 0.3 <= 0.5 → merge
            DiarSegment {
                start: 3.0,
                end: 3.5,
                speaker: 0,
            }, // gap 1.0 > 0.5 → separate
            DiarSegment {
                start: 3.4,
                end: 4.0,
                speaker: 1,
            }, // different speaker
        ];
        let merged = merge_segments(segs, 0.5);
        assert_eq!(merged.len(), 3);
        assert!((merged[0].end - 2.0).abs() < f32::EPSILON);
        assert_eq!(merged[1].speaker, 0);
        assert_eq!(merged[2].speaker, 1);
    }

    #[test]
    fn merge_segments_keeps_order_for_unsorted_gaps() {
        // Defensive: an end earlier than the previous end must not shrink.
        let segs = vec![
            DiarSegment {
                start: 0.0,
                end: 2.0,
                speaker: 0,
            },
            DiarSegment {
                start: 2.1,
                end: 1.5,
                speaker: 0,
            }, // overlapping, odd end
        ];
        let merged = merge_segments(segs, 0.5);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].end - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn vote_requires_strict_majority() {
        assert_eq!(vote_cluster_name(&[]), None);
        assert_eq!(vote_cluster_name(&[None, None]), None);
        assert_eq!(
            vote_cluster_name(&[Some("a".into()), Some("a".into())]),
            Some("a".into())
        );
        // 2 of 4 is not a strict majority.
        assert_eq!(
            vote_cluster_name(&[
                Some("a".into()),
                Some("a".into()),
                Some("b".into()),
                Some("b".into())
            ]),
            None
        );
        // 3 of 5 wins.
        assert_eq!(
            vote_cluster_name(&[
                Some("a".into()),
                Some("a".into()),
                Some("a".into()),
                Some("b".into()),
                None
            ]),
            Some("a".into())
        );
        // Anonymous votes don't dilute the cast ballots.
        assert_eq!(
            vote_cluster_name(&[Some("a".into()), None, None, None]),
            Some("a".into())
        );
    }

    #[test]
    fn inert_engine_reports_inactive() {
        let engine = MeetingEngine::from_config(
            &MeetingConfig::default(),
            &crate::voice::VoiceConfig::default(),
        );
        assert!(!engine.is_active());
        assert_eq!(engine.inactive_reason(), "disabled in config");
        assert!(!engine.is_recording());
        assert!(engine.begin_recording(1).is_err());
    }

    #[test]
    fn engine_missing_models_reports_reason() {
        let engine = MeetingEngine::from_config(
            &MeetingConfig {
                enabled: true,
                ..MeetingConfig::default()
            },
            &crate::voice::VoiceConfig::default(),
        );
        assert!(!engine.is_active());
        assert!(
            engine
                .inactive_reason()
                .contains("segmentation model not found")
        );
    }

    #[test]
    fn finish_recording_requires_matching_id() {
        // Activate with a real temp wav dir so begin() succeeds: point the
        // model paths at this test file (existence check only).
        let here = existing_file();
        let engine = MeetingEngine::from_config(
            &MeetingConfig {
                enabled: true,
                segmentation_model: here.clone(),
                audio_dir: std::env::temp_dir().display().to_string(),
                ..MeetingConfig::default()
            },
            &crate::voice::VoiceConfig {
                paraformer_model: here.clone(),
                paraformer_tokens: here.clone(),
                ..Default::default()
            },
        );
        assert!(engine.is_active(), "{}", engine.inactive_reason());
        engine.begin_recording(7).expect("begin");
        assert_eq!(engine.recording_id(), Some(7));
        // Stale id fails and leaves the session running.
        assert!(engine.finish_recording(6).is_err());
        assert!(engine.is_recording());
        let rec = engine.finish_recording(7).expect("finish");
        assert_eq!(rec.id, 7);
        assert!(rec.path.exists());
        assert!(!engine.is_recording());
        std::fs::remove_file(&rec.path).ok();
    }

    #[test]
    fn begin_recording_rejects_second_session() {
        let here = existing_file();
        let tmp = std::env::temp_dir().join(format!("mibee-meeting-begin-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("mkdir");
        let engine = MeetingEngine::from_config(
            &MeetingConfig {
                enabled: true,
                segmentation_model: here.clone(),
                audio_dir: tmp.display().to_string(),
                ..MeetingConfig::default()
            },
            &crate::voice::VoiceConfig {
                paraformer_model: here.clone(),
                paraformer_tokens: here.clone(),
                ..Default::default()
            },
        );
        engine.begin_recording(1).expect("first");
        assert_eq!(
            engine.begin_recording(2).expect_err("second must fail"),
            "already recording"
        );
        let rec = engine.finish_recording(1).expect("finish");
        std::fs::remove_file(&rec.path).ok();
        std::fs::remove_dir_all(&tmp).ok();
    }
}
