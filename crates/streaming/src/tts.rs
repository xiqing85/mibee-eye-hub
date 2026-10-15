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
