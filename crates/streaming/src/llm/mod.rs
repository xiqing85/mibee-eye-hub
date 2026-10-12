//! Local LLM dialogue (llama.cpp via `llama-cpp-2`, Qwen3 GGUF).
//!
//! Greedy (temperature-0) single-shot completion over the model's embedded
//! chat template — deterministic answers, no sampler bookkeeping, small
//! enough for the weakest target host. Qwen3's thinking mode is disabled
//! by appending `/no_think` to the user turn (the soft switch works with
//! the GGUF's stock chatml template; the reply simply starts sooner).

use serde::{Deserialize, Serialize};

/// `[llm]` configuration section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub enabled: bool,
    /// GGUF model path (e.g. Qwen3-0.6B Q4_K_M).
    pub model_path: String,
    /// Context window for the dialogue.
    pub n_ctx: u32,
    /// CPU threads (the target hosts are small).
    pub n_threads: u32,
    /// Generation cap per reply.
    pub max_tokens: u32,
    /// Append `/no_think` to user turns (Qwen3 thinking off).
    pub no_think: bool,
    /// Mid-tier model for `[resources] auto_tier` (4–10 GiB available;
    /// empty = reuse `model_path`).
    pub model_path_mid: String,
    /// Lite-tier model for auto_tier (<4 GiB; empty = reuse `model_path`).
    pub model_path_lite: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: "models/llm/qwen3-0.6b-q8_0.gguf".into(),
            // Tool prompts (SPEC §3.5) run ~500-1700 tokens; 1024
            // overflowed and silently degraded the first agent turns.
            n_ctx: 2048,
            // Half the logical cores, 2..=4 — measured on a 4C/8T i5:
            // more threads than physical cores regressed generation.
            n_threads: std::thread::available_parallelism()
                .map(|n| (n.get() as u32 / 2).clamp(2, 4))
                .unwrap_or(2),
            max_tokens: 200,
            no_think: true,
            model_path_mid: String::new(),
            model_path_lite: String::new(),
        }
    }
}

/// One dialogue turn (`role` is `system` | `user` | `assistant`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

/// Local LLM engine (fail-open: missing model or a build without the
/// `llm` feature leaves it inactive).
pub struct ChatEngine {
    #[cfg_attr(not(feature = "llm"), allow(dead_code))]
    config: LlmConfig,
    active: bool,
    inactive_reason: String,
    #[cfg(feature = "llm")]
    model: Option<std::sync::Arc<llama_cpp_2::model::LlamaModel>>,
}

impl ChatEngine {
    /// Build from config (fail-open).
    #[must_use]
    pub fn from_config(config: &LlmConfig) -> Self {
        match Self::load(config) {
            #[cfg(feature = "llm")]
            Ok(model) => {
                tracing::info!(model = %config.model_path, "llm: chat engine loaded");
                Self {
                    config: config.clone(),
                    active: true,
                    inactive_reason: String::new(),
                    model: Some(std::sync::Arc::new(model)),
                }
            }
            #[cfg(not(feature = "llm"))]
            Ok(()) => unreachable!("non-voice load never succeeds"),
            Err(reason) => {
                tracing::info!(%reason, "llm: disabled");
                Self {
                    config: config.clone(),
                    active: false,
                    inactive_reason: format!("{reason:#}"),
                    #[cfg(feature = "llm")]
                    model: None,
                }
            }
        }
    }

    #[cfg(feature = "llm")]
    fn load(config: &LlmConfig) -> anyhow::Result<llama_cpp_2::model::LlamaModel> {
        use llama_cpp_2::model::LlamaModel;
        use llama_cpp_2::model::params::LlamaModelParams;
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        if !std::path::Path::new(&config.model_path).exists() {
            anyhow::bail!("llm.model_path: file not found: {}", config.model_path);
        }
        // Memory guardrail: refuse to load a model larger than 2/3 of
        // available memory — hosts without swap get OOM-killed otherwise
        // (fail-open keeps chat off instead of taking the process down).
        let model_bytes = std::fs::metadata(&config.model_path)
            .map(|m| m.len())
            .unwrap_or(0);
        if let Some(avail) = available_memory_bytes()
            && model_bytes > avail * 2 / 3
        {
            anyhow::bail!(
                "llm.model_path: model {} MiB exceeds 2/3 of available memory ({} MiB) — refusing to load",
                model_bytes / 1_048_576,
                avail / 1_048_576
            );
        }
        // Process-wide backend (shared with run_completion via backend()).
        let backend = backend_shared();
        let params = LlamaModelParams::default();
        LlamaModel::load_from_file(backend, std::path::Path::new(&config.model_path), &params)
            .map_err(|e| anyhow::anyhow!("load {}: {e}", config.model_path))
    }

    #[cfg(not(feature = "llm"))]
    fn load(config: &LlmConfig) -> anyhow::Result<()> {
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        anyhow::bail!("built without the `llm` feature")
    }

    /// Whether dialogue is available.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Why the engine is inactive (empty string when active).
    #[must_use]
    pub fn inactive_reason(&self) -> &str {
        &self.inactive_reason
    }

    /// One deterministic (greedy) chat completion over the conversation.
    ///
    /// # Errors
    ///
    /// Inactive engine, tokenization or inference failure.
    pub fn complete(&self, turns: &[ChatTurn]) -> anyhow::Result<String> {
        self.complete_with_usage(turns).map(|(reply, _, _)| reply)
    }

    /// [`ChatEngine::complete`] plus the llama.cpp token accounting —
    /// prompt and generated counts feed per-model metrics and the
    /// conversation call-chain spans (SPEC §3.3).
    pub fn complete_with_usage(&self, turns: &[ChatTurn]) -> anyhow::Result<(String, u64, u64)> {
        self.complete_streaming_with_usage(turns, None)
    }

    /// [`ChatEngine::complete_with_usage`] with a per-token delta
    /// callback (sentence-streaming voice pipeline, 2026-10-12 P0). The
    /// callback receives each generated piece as it is produced;
    /// returning `false` stops generation at the next token boundary and
    /// the partial reply is returned — the caller keeps whatever it
    /// already consumed. Pieces are valid UTF-8 but may split sentences
    /// or `<think>` markers arbitrarily — feed a
    /// [`crate::sentence::ThinkFilter`], not a TTS engine.
    pub fn complete_streaming_with_usage(
        &self,
        turns: &[ChatTurn],
        on_delta: Option<&mut dyn FnMut(&str) -> bool>,
    ) -> anyhow::Result<(String, u64, u64)> {
        #[cfg(feature = "llm")]
        {
            let Some(model) = &self.model else {
                anyhow::bail!("llm inactive: {}", self.inactive_reason);
            };
            let call = observability::model_call("llm", &self.model_variant());
            match self.run_completion(model, turns, on_delta) {
                Ok((reply, prompt_tokens, completion_tokens)) => {
                    call.finish_ok(Some(prompt_tokens), Some(completion_tokens));
                    Ok((reply, prompt_tokens, completion_tokens))
                }
                Err(e) => {
                    call.finish_err();
                    Err(e)
                }
            }
        }
        #[cfg(not(feature = "llm"))]
        {
            let _ = (turns, on_delta);
            anyhow::bail!("llm inactive: {}", self.inactive_reason)
        }
    }

    /// Per-model metric variant label: the loaded GGUF file stem (SPEC
    /// appendix A #39 — variant = concrete model). Public so product
    /// layers can label conversation-trace spans identically.
    #[must_use]
    pub fn model_variant(&self) -> String {
        std::path::Path::new(&self.config.model_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    }

    #[cfg(feature = "llm")]
    fn run_completion(
        &self,
        model: &llama_cpp_2::model::LlamaModel,
        turns: &[ChatTurn],
        mut on_delta: Option<&mut dyn FnMut(&str) -> bool>,
    ) -> anyhow::Result<(String, u64, u64)> {
        use llama_cpp_2::context::params::LlamaContextParams;
        use llama_cpp_2::llama_batch::LlamaBatch;
        use llama_cpp_2::model::{AddBos, LlamaChatMessage};
        use llama_cpp_2::token::LlamaToken;

        let backend = backend_shared();

        let template = model.chat_template(None)?;
        let messages: Vec<LlamaChatMessage> = turns
            .iter()
            .map(|t| {
                let content = if t.role == "user" && self.config.no_think {
                    format!("{}/no_think", t.content)
                } else {
                    t.content.clone()
                };
                LlamaChatMessage::new(t.role.clone(), content)
            })
            .collect::<Result<_, _>>()?;
        let prompt = model.apply_chat_template(&template, &messages, true)?;

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(self.config.n_ctx.max(128)))
            // 2026-10-10: the config field existed but was never applied —
            // every deployment ran llama.cpp's own default regardless.
            .with_n_threads(i32::from(
                u16::try_from(self.config.n_threads.max(1)).unwrap_or(u16::MAX),
            ));
        let mut ctx = model.new_context(backend, ctx_params)?;

        let mut tokens = model.str_to_token(&prompt, AddBos::Never)?;
        let n_prompt = tokens.len();
        let mut batch = LlamaBatch::new(n_prompt, 1);
        let last = n_prompt - 1;
        for (i, t) in tokens.clone().iter().enumerate() {
            // Logits are needed from the final prompt token onward.
            batch.add(*t, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch)?;

        // Greedy generation. get_logits_ith indexes WITHIN the last
        // decoded batch: n_prompt-1 after the prompt, 0 after each
        // single-token generation step.
        let mut reply = String::new();
        let mut generated_tokens: u64 = 0;
        let mut logits_at = n_prompt as i32 - 1;
        for pos in (n_prompt as i32..).take(self.config.max_tokens as usize) {
            generated_tokens += 1;
            let logits = ctx.get_logits_ith(logits_at);
            logits_at = 0;
            let best = logits
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, _)| i)
                .unwrap_or(0);
            let token = LlamaToken(best as i32);
            if model.is_eog_token(token) {
                break;
            }
            if let Ok(bytes) = model.token_to_piece_bytes(token, 16, false, None)
                && let Ok(piece) = String::from_utf8(bytes)
            {
                reply.push_str(&piece);
                if let Some(cb) = on_delta.as_deref_mut()
                    && !cb(&piece)
                {
                    // Consumer is done (queue closed / cancelled): stop
                    // burning tokens; the partial reply is returned.
                    break;
                }
            }
            tokens.clear();
            tokens.push(token);
            let mut next = LlamaBatch::new(1, 1);
            next.add(token, pos, &[0], true)?;
            ctx.decode(&mut next)?;
        }
        // Qwen3 chatml: /no_think still emits an empty <think> block —
        // strip think sections, keep the visible answer.
        Ok((
            strip_think_blocks(&reply),
            n_prompt as u64,
            generated_tokens,
        ))
    }
}

/// Strip `<think>…</think>` sections (Qwen3's `/no_think` still emits an
/// empty block). Shared with the VLM description path.
#[cfg_attr(not(feature = "llm"), allow(dead_code))]
pub(crate) fn strip_think_blocks(reply: &str) -> String {
    let mut cleaned = String::with_capacity(reply.len());
    let mut in_think = false;
    for line in reply.lines() {
        let t = line.trim();
        if t.starts_with("<think>") {
            in_think = true;
            continue;
        }
        if t.starts_with("</think>") {
            in_think = false;
            continue;
        }
        if !in_think {
            cleaned.push_str(line);
            cleaned.push('\n');
        }
    }
    cleaned.trim().to_string()
}

/// Deterministic offline self-test (`--selftest-llm`): one greedy reply.
///
/// # Errors
///
/// Missing/inactive model or inference failure.
pub fn selftest_llm(prompt: &str) -> anyhow::Result<serde_json::Value> {
    let config = LlmConfig {
        enabled: true,
        ..LlmConfig::default()
    };
    let engine = ChatEngine::from_config(&config);
    if !engine.is_active() {
        anyhow::bail!("llm inactive: {}", engine.inactive_reason());
    }
    let started = std::time::Instant::now();
    let turns = vec![
        ChatTurn {
            role: "system".into(),
            content: "你是家庭摄像头的语音助手，用不超过两句话的中文回答。".into(),
        },
        ChatTurn {
            role: "user".into(),
            content: prompt.to_string(),
        },
    ];
    let reply = engine.complete(&turns)?;
    Ok(serde_json::json!({
        "prompt": prompt,
        "reply": reply,
        "elapsed_s": (started.elapsed().as_secs_f64() * 100.0).round() / 100.0,
    }))
}

/// The single llama.cpp backend for the process (a second init aborts).
/// Shared by the chat engine and the VLM description engine.
#[cfg(feature = "llm")]
pub(crate) fn backend_shared() -> &'static llama_cpp_2::llama_backend::LlamaBackend {
    static BACKEND: std::sync::OnceLock<llama_cpp_2::llama_backend::LlamaBackend> =
        std::sync::OnceLock::new();
    BACKEND.get_or_init(|| {
        llama_cpp_2::llama_backend::LlamaBackend::init().expect("llama backend init")
    })
}

/// Available memory in bytes from `/proc/meminfo` (`MemAvailable`), or
/// `None` off Linux / when unreadable (the guardrail is skipped then).
pub fn available_memory_bytes() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb = line["MemAvailable:".len()..]
        .trim_end_matches(" kB")
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_memory_sane_on_linux() {
        // Sanity bounds only: a real MemAvailable is somewhere between
        // 16 MiB and 4 TiB on any machine running the test suite.
        if let Some(bytes) = available_memory_bytes() {
            assert!(bytes > 16 * 1024 * 1024);
            assert!(bytes < 4_u64 * 1024 * 1024 * 1024 * 1024);
        }
    }

    #[test]
    fn config_defaults_off() {
        let c = LlmConfig::default();
        assert!(!c.enabled, "llm is opt-in");
        assert!(c.model_path.contains("qwen3"));
        assert_eq!(c.n_ctx, 2048);
        assert!((2..=4).contains(&c.n_threads), "{}", c.n_threads);
        assert!(c.no_think);
    }

    #[test]
    fn engine_disabled_by_default() {
        let e = ChatEngine::from_config(&LlmConfig::default());
        assert!(!e.is_active());
        assert_eq!(e.inactive_reason(), "disabled by configuration");
    }

    #[test]
    fn engine_enabled_but_model_missing_fails_open() {
        let e = ChatEngine::from_config(&LlmConfig {
            enabled: true,
            ..LlmConfig::default()
        });
        assert!(!e.is_active(), "missing model must not fake activity");
    }
}
