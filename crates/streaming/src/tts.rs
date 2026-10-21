//! TTS playback via the sherpa-onnx CLI as a subprocess (GPL isolation).
//!
//! The sherpa-onnx-offline-tts binary statically links espeak-ng
//! (GPL-3.0); running it as a child process keeps that obligation out of
//! the Apache-2.0 binary. `speak()` synthesizes to a temp WAV and plays it
//! through `aplay` (present on all our target hosts) — two short-lived
//! subprocesses per utterance, no daemons.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[tts]` configuration section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    pub enabled: bool,
    /// sherpa-onnx-offline-tts binary path.
    pub binary: String,
    /// vits-melo-tts-zh_en assets.
    pub model: String,
    pub lexicon: String,
    pub tokens: String,
    pub dict_dir: String,
    /// Number/date normalization FSTs (comma-joined when both exist).
    pub rule_fsts: String,
    /// Playback command (`aplay -q`); empty = synthesize only.
    pub player: String,
    /// Voice character (M4-b first release): `"off"` (default) plays the
    /// synthesized voice as-is; `"high"` / `"low"` shift pitch+tempo by
    /// resampling (chipmunk / deep). Applied in synthesis, so every
    /// consumer (speak, pipeline, playback) hears the same character.
    pub voice_change: String,
    /// sherpa-onnx-offline-tts `--num-threads`. The 2 default
    /// under-uses ≥4-core hosts (a 5 s utterance synthesized in ~16 s);
    /// 4 halves it. Decode is the bottleneck, not the player.
    pub num_threads: u32,
    // -- Trilingual profiles (SPEC appendix A #30-D) -------------------
    // Optional per-language models; empty = the language falls back to
    // the primary (Mandarin melo) voice.
    /// Cantonese vits model (empty = no Cantonese voice).
    pub yue_model: String,
    pub yue_lexicon: String,
    pub yue_dict_dir: String,
    /// English vits model (empty = no English voice).
    pub en_model: String,
    pub en_lexicon: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            voice_change: "off".into(),
            enabled: false,
            binary: "tmp/sherpa-libs/tools-bin/bin/sherpa-onnx-offline-tts".into(),
            model: "models/voice/melo/model.onnx".into(),
            lexicon: "models/voice/melo/lexicon.txt".into(),
            tokens: "models/voice/melo/tokens.txt".into(),
            dict_dir: "models/voice/melo/dict".into(),
            rule_fsts: "models/voice/melo/number.fst,models/voice/melo/date.fst".into(),
            player: "aplay -q".into(),
            num_threads: 2,
            yue_model: String::new(),
            yue_lexicon: String::new(),
            yue_dict_dir: String::new(),
            en_model: String::new(),
            en_lexicon: String::new(),
        }
    }
}

/// Token table for a per-language model: prefer the `tokens.txt` sitting
/// next to the model (every sherpa TTS bundle ships one — reusing the
/// primary model's table makes the subprocess exit 255 on mismatched
/// symbols), fall back to the primary table when absent.
#[must_use]
pub fn companion_tokens(model_path: &str, fallback: &str) -> String {
    let companion = std::path::Path::new(model_path)
        .parent()
        .map(|d| d.join("tokens.txt"));
    match companion {
        Some(p) if p.is_file() => p.to_string_lossy().into_owned(),
        _ => fallback.to_string(),
    }
}

/// Model files available for one spoken language.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceProfile {
    pub model: String,
    pub lexicon: String,
    pub tokens: String,
    pub dict_dir: String,
    pub rule_fsts: String,
}

/// TTS engine (fail-open: missing binary/model leaves it inactive).
#[derive(Debug)]
pub struct TtsEngine {
    config: TtsConfig,
    active: bool,
    inactive_reason: String,
}

impl TtsEngine {
    /// Build from config (fail-open).
    #[must_use]
    pub fn from_config(config: &TtsConfig) -> Self {
        let reason = Self::check(config);
        if reason.is_empty() {
            tracing::info!(model = %config.model, "tts: engine ready");
        } else {
            tracing::info!(%reason, "tts: disabled");
        }
        Self {
            config: config.clone(),
            active: reason.is_empty(),
            inactive_reason: reason,
        }
    }

    fn check(config: &TtsConfig) -> String {
        if !config.enabled {
            return "disabled by configuration".into();
        }
        for (label, path) in [
            ("tts.binary", &config.binary),
            ("tts.model", &config.model),
            ("tts.lexicon", &config.lexicon),
            ("tts.tokens", &config.tokens),
        ] {
            if !std::path::Path::new(path).exists() {
                return format!("{label}: file not found: {path}");
            }
        }
        String::new()
    }

    /// Whether TTS is usable.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Why TTS is inactive (empty string when active).
    #[must_use]
    pub fn inactive_reason(&self) -> &str {
        &self.inactive_reason
    }

    /// Pick the voice profile for a reply's language (#30-D):
    /// Cantonese/English fall back to the primary model when their
    /// optional model is not configured (honest — the accent will be
    /// Mandarin, the text is still spoken).
    #[must_use]
    pub fn profile_for(&self, lang: crate::lang::SpokenLang) -> VoiceProfile {
        let primary = VoiceProfile {
            model: self.config.model.clone(),
            lexicon: self.config.lexicon.clone(),
            tokens: self.config.tokens.clone(),
            dict_dir: self.config.dict_dir.clone(),
            rule_fsts: self.config.rule_fsts.clone(),
        };
        match lang {
            crate::lang::SpokenLang::Cantonese if !self.config.yue_model.is_empty() => {
                VoiceProfile {
                    model: self.config.yue_model.clone(),
                    lexicon: self.config.yue_lexicon.clone(),
                    tokens: companion_tokens(&self.config.yue_model, &self.config.tokens),
                    dict_dir: self.config.yue_dict_dir.clone(),
                    rule_fsts: String::new(),
                }
            }
            crate::lang::SpokenLang::English if !self.config.en_model.is_empty() => VoiceProfile {
                model: self.config.en_model.clone(),
                lexicon: self.config.en_lexicon.clone(),
                tokens: companion_tokens(&self.config.en_model, &self.config.tokens),
                dict_dir: String::new(),
                rule_fsts: String::new(),
            },
            _ => primary,
        }
    }

    /// Synthesize (and play, when a player is configured) one utterance
    /// with the voice matching its language. Blocking — call from
    /// `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Inactive engine or subprocess failure.
    pub fn speak(&self, text: &str) -> anyhow::Result<PathBuf> {
        let out = self.synthesize(text)?;
        if let Err(e) = self.play(&out) {
            tracing::warn!(error = %e, "tts: playback failed (file kept at {})", out.display());
        }
        Ok(out)
    }

    /// Synthesize one utterance to a WAV file without playing it — the
    /// sentence-streaming pipeline (2026-10-12 P0) synthesizes the next
    /// sentence while the previous one is still on the speaker. Blocking
    /// like [`Self::speak`]; belongs on `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// Inactive engine, empty text, or synth subprocess failure.
    pub fn synthesize(&self, text: &str) -> anyhow::Result<PathBuf> {
        if !self.active {
            anyhow::bail!("tts inactive: {}", self.inactive_reason);
        }
        if text.trim().is_empty() {
            anyhow::bail!("tts: empty text");
        }
        let lang = crate::lang::detect(text);
        let profile = self.profile_for(lang);
        // Per-model metric span (SPEC appendix A #39): the synthesis
        // stage (the pipeline measures playback separately); the label
        // names the spoken language's vits voice.
        let model_id = match lang {
            crate::lang::SpokenLang::Cantonese => "tts.yue",
            crate::lang::SpokenLang::English => "tts.en",
            crate::lang::SpokenLang::Mandarin => "tts.zh",
        };
        let variant = std::path::Path::new(&profile.model)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("vits")
            .to_string();
        let call = observability::model_call(model_id, &variant);
        let out = std::env::temp_dir().join(format!("mibee-tts-{}.wav", std::process::id()));
        let mut cmd = std::process::Command::new(&self.config.binary);
        cmd.arg(format!("--vits-model={}", profile.model))
            .arg(format!("--vits-lexicon={}", profile.lexicon))
            .arg(format!("--vits-tokens={}", profile.tokens))
            .arg(format!("--vits-dict-dir={}", profile.dict_dir))
            .arg(format!("--tts-rule-fsts={}", profile.rule_fsts))
            .arg(format!("--num-threads={}", self.config.num_threads.max(1)))
            .arg(format!("--output-filename={}", out.display()))
            .arg(text);
        let result = (|| {
            let status = cmd
                .status()
                .map_err(|e| anyhow::anyhow!("spawn tts: {e}"))?;
            if !status.success() {
                anyhow::bail!("tts subprocess failed: {status}");
            }
            Ok(out)
        })();
        // Voice character (M4-b): resample in place — a failure of the
        // transform is fail-open (the original voice plays).
        if let Ok(path) = &result {
            let factor = voice_change_factor(&self.config.voice_change);
            if factor != 1.0
                && let Err(e) = rewrite_wav_pitch(path, factor)
            {
                tracing::warn!(error = %e, "tts: voice-change resample failed — original voice");
            }
        }
        match &result {
            Ok(_) => call.finish_ok(None, None),
            Err(_) => call.finish_err(),
        }
        result
    }

    /// Play a synthesized WAV through the configured player, blocking
    /// until it finishes. Split from synthesis so the pipeline can
    /// interleave the two; a playback failure never fails the round —
    /// callers log and keep going.
    ///
    /// # Errors
    ///
    /// Player subprocess spawn or non-zero exit.
    pub fn play(&self, wav: &std::path::Path) -> anyhow::Result<()> {
        if self.config.player.trim().is_empty() {
            return Ok(());
        }
        let mut parts = self.config.player.split_whitespace();
        let Some(prog) = parts.next() else {
            return Ok(());
        };
        let status = std::process::Command::new(prog)
            .args(parts)
            .arg(wav)
            .status()
            .map_err(|e| anyhow::anyhow!("tts play: {e}"))?;
        if !status.success() {
            anyhow::bail!("tts playback failed: {status}");
        }
        Ok(())
    }

    /// [`Self::play`] without waiting — the interruptible pipeline
    /// (SPEC appendix A #45) polls the child and kills it when the user
    /// interrupts mid-sentence. `None` when no player is configured.
    ///
    /// # Errors
    ///
    /// Player subprocess spawn failure.
    pub fn spawn_play(&self, wav: &std::path::Path) -> anyhow::Result<Option<PlayerProcess>> {
        if self.config.player.trim().is_empty() {
            return Ok(None);
        }
        let mut parts = self.config.player.split_whitespace();
        let Some(prog) = parts.next() else {
            return Ok(None);
        };
        std::process::Command::new(prog)
            .args(parts)
            .arg(wav)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map(|child| Some(PlayerProcess::new(child)))
            .map_err(|e| anyhow::anyhow!("tts play spawn: {e}"))
    }
}

/// A spawned audio player that is killed when it goes out of scope.
///
/// `std::process::Child` deliberately does *not* kill on drop, which is the
/// right default for a CLI and the wrong one here. The TTS player keeps
/// talking to the physical speakers after the future that spawned it is
/// cancelled — a SIGTERM during a reply, an aborted round, a timeout — and by
/// then `end_playback_mute()` has already lifted the self-mute, so the wake
/// word hears the tail of our own sentence. That is the self-talk loop that
/// SPEC appendix A #45 exists to prevent, re-entering through the one path the
/// explicit `cancel` flag cannot cover.
///
/// Callers poll with [`Self::try_wait`] and either let the process exit
/// normally (disarmed on drop) or call [`Self::kill`] for an explicit
/// barge-in, which is disarmed too.
#[derive(Debug)]
pub struct PlayerProcess {
    child: Option<std::process::Child>,
}

impl PlayerProcess {
    /// Wrap a freshly spawned player process.
    #[must_use]
    pub fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// Non-blocking status check. `Ok(None)` while still running.
    ///
    /// A `Some(_)` status disarms the drop guard: the process finished on its
    /// own, so there is nothing to clean up.
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let Some(status) = self.child.as_mut().map(std::process::Child::try_wait) else {
            return Ok(None);
        };
        let status = status?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }

    /// Kill the player immediately (barge-in). Disarms the drop guard because
    /// the kill plus the reap is the whole teardown.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// True once the process exited and no kill is pending.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.child.is_none()
    }
}

impl Drop for PlayerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort: the point is that a cancelled reply stops talking.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Deterministic offline self-test (`--selftest-tts`): synthesize one
/// utterance, report the WAV path/size.
///
/// # Errors
///
/// Inactive engine or subprocess failure.
pub fn selftest_tts(text: &str) -> anyhow::Result<serde_json::Value> {
    let mut config = TtsConfig {
        enabled: true,
        player: String::new(), // self-test does not play audio
        ..TtsConfig::default()
    };
    // Deployment hosts keep the TTS binary elsewhere than the workstation
    // default — allow pointing the self-test at it without editing the
    // config default (same pattern as VOICE_SELFTEST_THRESHOLD).
    if let Ok(bin) = std::env::var("MIBEE_TTS_BIN")
        && !bin.is_empty()
    {
        config.binary = bin;
    }
    let engine = TtsEngine::from_config(&config);
    if !engine.is_active() {
        anyhow::bail!("tts inactive: {}", engine.inactive_reason());
    }
    let started = std::time::Instant::now();
    let wav = engine.speak(text)?;
    let size = std::fs::metadata(&wav).map(|m| m.len()).unwrap_or(0);
    Ok(serde_json::json!({
        "text": text,
        "wav": wav.display().to_string(),
        "bytes": size,
        "elapsed_s": (started.elapsed().as_secs_f64() * 100.0).round() / 100.0,
    }))
}

/// Voice-change factor for a `[tts] voice_change` value (M4-b).
/// Unknown values are `"off"` — a typo must not transpose the voice.
#[must_use]
pub fn voice_change_factor(setting: &str) -> f32 {
    match setting {
        "high" => 1.22,
        "low" => 0.82,
        _ => 1.0,
    }
}

/// Resample 16-bit PCM WAV samples by `factor` (>1 = more samples =
/// higher pitch+tempo at the header's unchanged sample rate). Pure:
/// parses RIFF chunks, only rewrites the `data` payload and sizes —
/// unknown layouts return `Ok(())` untouched (fail-open).
fn resample_wav_bytes(wav: &[u8], factor: f32) -> Vec<u8> {
    if !(0.5..=2.0).contains(&factor) || wav.len() < 44 || &wav[0..4] != b"RIFF" {
        return wav.to_vec();
    }
    // Walk chunks: 12-byte RIFF header, then (id, size, payload) triples.
    let mut out = wav[..12].to_vec();
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size =
            u32::from_le_bytes([wav[pos + 4], wav[pos + 5], wav[pos + 6], wav[pos + 7]]) as usize;
        let end = (pos + 8 + size).min(wav.len());
        let payload = &wav[pos + 8..end];
        if id == b"data" && !payload.is_empty() && payload.len().is_multiple_of(2) {
            let samples: Vec<i16> = payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            let resampled = resample_i16(&samples, factor);
            let bytes: Vec<u8> = resampled.iter().flat_map(|s| s.to_le_bytes()).collect();
            out.extend_from_slice(id);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&bytes);
        } else {
            out.extend_from_slice(&wav[pos..end]);
        }
        pos = end + (size & 1); // chunk padding (odd sizes get a pad byte)
        if size == 0 {
            break;
        }
    }
    // Fix RIFF size.
    let total = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&total.to_le_bytes());
    out
}

/// Linear-interpolation resample of mono/stereo interleaved i16 frames.
fn resample_i16(samples: &[i16], factor: f32) -> Vec<i16> {
    let n = samples.len();
    if n == 0 {
        return Vec::new();
    }
    let out_len = ((n as f32) * factor).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f32 / factor;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(n - 1);
        let frac = src - i0 as f32;
        let v = f32::from(samples[i0]) * (1.0 - frac) + f32::from(samples[i1]) * frac;
        out.push(v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
    }
    out
}

/// Rewrite a WAV file's samples in place.
fn rewrite_wav_pitch(path: &std::path::Path, factor: f32) -> anyhow::Result<()> {
    let wav = std::fs::read(path)?;
    let shifted = resample_wav_bytes(&wav, factor);
    if shifted.len() != wav.len() {
        // Sizes moved: the data chunk genuinely changed — write it.
        std::fs::write(path, shifted)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_off() {
        let c = TtsConfig::default();
        assert!(!c.enabled, "tts is opt-in");
        assert!(c.binary.contains("sherpa-onnx-offline-tts"));
    }

    #[test]
    fn engine_disabled_by_default() {
        let e = TtsEngine::from_config(&TtsConfig::default());
        assert!(!e.is_active());
        assert_eq!(e.inactive_reason(), "disabled by configuration");
    }

    #[test]
    fn engine_enabled_but_binary_missing_fails_open() {
        let e = TtsEngine::from_config(&TtsConfig {
            enabled: true,
            ..TtsConfig::default()
        });
        assert!(!e.is_active(), "missing assets must not fake activity");
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    fn engine_with(yue: bool, en: bool) -> TtsEngine {
        let mut c = TtsConfig::default();
        if yue {
            c.yue_model = "models/voice/tts-yue/cantonese.onnx".into();
            c.yue_lexicon = "models/voice/tts-yue/lexicon.txt".into();
            c.yue_dict_dir = "models/voice/tts-yue/dict".into();
        }
        if en {
            c.en_model = "models/voice/tts-en/model.onnx".into();
            c.en_lexicon = "models/voice/tts-en/lexicon.txt".into();
        }
        TtsEngine::from_config(&c)
    }

    #[test]
    fn profile_follows_language_with_fallback() {
        let e = engine_with(true, true);
        assert_eq!(
            e.profile_for(crate::lang::SpokenLang::Cantonese).model,
            "models/voice/tts-yue/cantonese.onnx"
        );
        assert_eq!(
            e.profile_for(crate::lang::SpokenLang::English).model,
            "models/voice/tts-en/model.onnx"
        );
        assert_eq!(
            e.profile_for(crate::lang::SpokenLang::Mandarin).model,
            "models/voice/melo/model.onnx"
        );
    }

    #[test]
    fn companion_tokens_prefers_model_directory_table() {
        let dir = std::env::temp_dir().join("mibee-tts-tokens-test");
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("model.onnx");
        std::fs::write(&model, b"x").unwrap();
        std::fs::write(dir.join("tokens.txt"), b"AA").unwrap();
        let m = model.to_string_lossy();
        assert_eq!(
            companion_tokens(&m, "fallback/tokens.txt"),
            format!("{}/tokens.txt", dir.display())
        );
        std::fs::remove_file(dir.join("tokens.txt")).unwrap();
        assert_eq!(
            companion_tokens(&m, "fallback/tokens.txt"),
            "fallback/tokens.txt"
        );
    }

    #[test]
    fn missing_language_model_falls_back_to_primary() {
        let e = engine_with(false, false);
        for lang in [
            crate::lang::SpokenLang::Cantonese,
            crate::lang::SpokenLang::English,
            crate::lang::SpokenLang::Mandarin,
        ] {
            assert_eq!(e.profile_for(lang).model, "models/voice/melo/model.onnx");
        }
    }
}

#[cfg(test)]
mod player_process_tests {
    use super::PlayerProcess;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// The regression: `std::process::Child` does not kill on drop, so a
    /// cancelled playback future left the device speaking to the speakers
    /// after `end_playback_mute()` had already lifted the self-mute — the
    /// wake word then heard our own sentence and re-entered the self-talk
    /// loop SPEC appendix A #45 exists to prevent.
    ///
    /// The PID comes from `Child::id()` rather than from a file the child
    /// writes for itself. An earlier version had the spawned shell record its
    /// own pid, which races twice over: the file exists from the moment the
    /// redirection opens it, so it can be read while still empty, and the
    /// value read is the shell's pid while the process that actually holds the
    /// 30-second sleep may be a separate `sleep` child. Either way the
    /// assertion intermittently watched a process the drop never touched.
    #[test]
    fn dropping_the_wrapper_stops_the_player() {
        let child = Command::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test player");
        let pid = child.id() as i32;
        assert!(pid > 0);

        // Confirm it is really running before the drop, so a green result
        // cannot come from the process never having started.
        assert!(
            process_alive(pid),
            "test player {pid} was not running before the drop"
        );

        drop(PlayerProcess::new(child));

        // The process must not survive the drop. `Drop` kills and then reaps,
        // so the pid must become fully unobservable (ESRCH), not merely a
        // zombie — a zombie still answers `kill(pid, 0)`.
        let mut gone = false;
        for _ in 0..500 {
            if !process_alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(gone, "player pid {pid} survived the PlayerProcess drop");
    }

    /// Whether a pid is still observable to `kill(pid, 0)`.
    fn process_alive(pid: i32) -> bool {
        // SAFETY: signal 0 performs the permission and existence checks
        // without delivering anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// An explicit barge-in kill must be just as effective, and must not
    /// double-kill (which would surface as a spurious error to the caller).
    #[test]
    fn explicit_kill_is_idempotent() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test player");
        let mut player = PlayerProcess::new(child);
        assert!(!player.is_finished());
        player.kill();
        assert!(player.is_finished());
        // Second kill is a no-op rather than a panic.
        player.kill();
    }

    /// A player that exited on its own is disarmed, so the drop guard does
    /// not report a spurious failure or try to reap twice.
    #[test]
    fn a_finished_player_reports_finished() {
        let child = Command::new("sh")
            .arg("-c")
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn test player");
        let mut player = PlayerProcess::new(child);
        let mut saw_exit = false;
        for _ in 0..200 {
            match player.try_wait() {
                Ok(Some(_)) => {
                    saw_exit = true;
                    break;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        assert!(saw_exit, "a trivial process should have exited");
        assert!(player.is_finished());
    }

    // Keep the unused-import lint honest for writers of future tests.
    #[allow(dead_code)]
    fn _assert_write_is_used(mut w: impl Write) {
        let _ = w.write_all(b"");
    }
}

#[cfg(test)]
mod voice_change_tests {
    use super::*;

    /// A minimal canonical 44-byte PCM WAV: RIFF + fmt(16) + data(n).
    fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // PCM
        b.extend_from_slice(&1u16.to_le_bytes()); // mono
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
        b.extend_from_slice(&2u16.to_le_bytes()); // block align
        b.extend_from_slice(&16u16.to_le_bytes()); // bits
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    fn data_samples(w: &[u8]) -> Vec<i16> {
        let pos = w.windows(4).position(|c| c == b"data").expect("data chunk");
        let size = u32::from_le_bytes([w[pos + 4], w[pos + 5], w[pos + 6], w[pos + 7]]) as usize;
        w[pos + 8..pos + 8 + size]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn factor_table_and_fail_open_typos() {
        assert_eq!(voice_change_factor("off"), 1.0);
        assert!(voice_change_factor("high") > 1.0);
        assert!(voice_change_factor("low") < 1.0);
        assert_eq!(voice_change_factor("HIGH"), 1.0, "typo stays off");
        assert_eq!(voice_change_factor(""), 1.0);
    }

    #[test]
    fn resample_grows_and_shrinks_sample_count() {
        let samples: Vec<i16> = (0..1000)
            .map(|i| ((i as f32 * 0.1).sin() * 8000.0) as i16)
            .collect();
        let w = wav(&samples, 16_000);
        let up = resample_wav_bytes(&w, 1.22);
        let down = resample_wav_bytes(&w, 0.82);
        assert_eq!(data_samples(&up).len(), 1220, "22% more samples");
        assert_eq!(data_samples(&down).len(), 820, "18% fewer");
        assert_eq!(&up[0..4], b"RIFF");
        let riff = u32::from_le_bytes([up[4], up[5], up[6], up[7]]) as usize;
        assert_eq!(riff + 8, up.len());
        assert_eq!(up[24..28], w[24..28], "sample rate unchanged");
    }

    #[test]
    fn non_wav_input_passes_through_untouched() {
        let garbage = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(resample_wav_bytes(&garbage, 1.22), garbage);
        let w = wav(&[100, -100, 50], 8000);
        assert_eq!(resample_wav_bytes(&w, 3.0), w);
    }

    #[test]
    fn resampled_waveform_tracks_the_source() {
        // A constant-DC source resamples to itself regardless of factor.
        let w = wav(&[1234; 512], 8000);
        let up = resample_wav_bytes(&w, 1.5);
        let s = data_samples(&up);
        assert!(s.iter().all(|v| (v - 1234).abs() <= 1), "{s:?}");
    }
}
