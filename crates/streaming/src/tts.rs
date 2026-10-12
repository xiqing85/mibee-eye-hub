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
    pub fn spawn_play(&self, wav: &std::path::Path) -> anyhow::Result<Option<std::process::Child>> {
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
            .map(Some)
            .map_err(|e| anyhow::anyhow!("tts play spawn: {e}"))
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
