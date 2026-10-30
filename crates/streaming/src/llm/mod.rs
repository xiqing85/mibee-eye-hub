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
///
/// The loaded model lives in an interior-mutable slot so the elasticity
/// controller can hot-swap tiers (L2) without a restart: completions
/// clone the `Arc` out per call, an in-flight generation finishes on the
/// model it started with, and the swap itself only waits for the
/// momentary read-guard release.
pub struct ChatEngine {
    #[cfg_attr(not(feature = "llm"), allow(dead_code))]
    config: LlmConfig,
    slot: parking_lot::RwLock<ChatSlot>,
}

struct ChatSlot {
    #[cfg(feature = "llm")]
    model: Option<std::sync::Arc<llama_cpp_2::model::LlamaModel>>,
    active: bool,
    inactive_reason: String,
    /// The model file currently loaded — `config.model_path` at boot, the
    /// live-swap target afterwards.
    #[cfg_attr(not(feature = "llm"), allow(dead_code))]
    model_path: String,
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
                    slot: parking_lot::RwLock::new(ChatSlot {
                        model: Some(std::sync::Arc::new(model)),
                        active: true,
                        inactive_reason: String::new(),
                        model_path: config.model_path.clone(),
                    }),
                }
            }
            #[cfg(not(feature = "llm"))]
            Ok(()) => unreachable!("non-voice load never succeeds"),
            Err(reason) => {
                tracing::info!(%reason, "llm: disabled");
                Self {
                    config: config.clone(),
                    slot: parking_lot::RwLock::new(ChatSlot {
                        #[cfg(feature = "llm")]
                        model: None,
                        active: false,
                        inactive_reason: format!("{reason:#}"),
                        model_path: config.model_path.clone(),
                    }),
                }
            }
        }
    }

    #[cfg(feature = "llm")]
    fn load(config: &LlmConfig) -> anyhow::Result<llama_cpp_2::model::LlamaModel> {
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        ensure_model_fits_memory(&config.model_path)?;
        Self::load_model_file(&config.model_path)
    }

    #[cfg(not(feature = "llm"))]
    fn load(config: &LlmConfig) -> anyhow::Result<()> {
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        anyhow::bail!("built without the `llm` feature")
    }

    /// Load a GGUF file with default model params (no per-tier tuning —
    /// context/threads come from the boot config at completion time).
    #[cfg(feature = "llm")]
    fn load_model_file(path: &str) -> anyhow::Result<llama_cpp_2::model::LlamaModel> {
        use llama_cpp_2::model::LlamaModel;
        use llama_cpp_2::model::params::LlamaModelParams;
        if !std::path::Path::new(path).exists() {
            anyhow::bail!("model file not found: {path}");
        }
        // Process-wide backend (shared with run_completion via backend()).
        let backend = backend_shared();
        let params = LlamaModelParams::default();
        LlamaModel::load_from_file(backend, std::path::Path::new(path), &params)
            .map_err(|e| anyhow::anyhow!("load {path}: {e}"))
    }

    /// The model file currently loaded (boot path or live-swap target).
    #[must_use]
    pub fn loaded_model_path(&self) -> String {
        self.slot.read().model_path.clone()
    }

    /// L2 elasticity: live-swap the model file without a restart.
    ///
    /// The new model loads on a blocking thread; only then does the slot
    /// flip — a load failure leaves the current model untouched (fail
    /// closed). In-flight completions finish on the model they started
    /// with. Swapping in the currently loaded path is a no-op `Ok`.
    ///
    /// Downgrades (strictly smaller file) park the old model out of the
    /// slot first (issue #12): the load-first default demands headroom
    /// the emergency path never has — a smaller model must be loadable
    /// exactly when memory ran out. In-flight completions keep their own
    /// `Arc`, so a pinned old model simply fails the guard and is
    /// restored — still fail-closed.
    ///
    /// # Errors
    ///
    /// Missing file, the available-memory guardrail, or a load failure.
    pub async fn swap_model(&self, new_path: &str) -> anyhow::Result<SwapOutcome> {
        // Same-path short-circuit regardless of build features: a tier that
        // resolves to the loaded model needs no swap at all.
        if new_path == self.loaded_model_path() {
            tracing::info!(
                path = %new_path,
                "llm: swap target equals the loaded model — no-op (tier slot unconfigured or mirrored)"
            );
            return Ok(SwapOutcome::NoopSamePath {
                path: new_path.to_string(),
            });
        }
        #[cfg(feature = "llm")]
        {
            if !std::path::Path::new(new_path).exists() {
                anyhow::bail!("llm swap: file not found: {new_path}");
            }
            if is_downgrade(&self.loaded_model_path(), new_path) {
                self.swap_downgrade(new_path).await
            } else {
                ensure_model_fits_memory(new_path)?;
                let path = new_path.to_string();
                let loaded = tokio::task::spawn_blocking(move || Self::load_model_file(&path))
                    .await
                    .map_err(|e| anyhow::anyhow!("llm swap: load task failed: {e}"))??;
                let mut slot = self.slot.write();
                let from = slot.model_path.clone();
                slot.model = Some(std::sync::Arc::new(loaded));
                slot.active = true;
                slot.inactive_reason.clear();
                slot.model_path = new_path.to_string();
                drop(slot);
                tracing::info!(from = %from, to = %new_path, "llm: live model swap applied");
                Ok(SwapOutcome::Swapped {
                    from,
                    to: new_path.to_string(),
                })
            }
        }
        #[cfg(not(feature = "llm"))]
        {
            let _ = new_path;
            anyhow::bail!("llm swap: built without the `llm` feature")
        }
    }

    /// Downgrade swap (issue #12): park the old model so its pages are
    /// reclaimable for the load, then guard + load the smaller file. Any
    /// failure restores the parked model — the engine never ends up
    /// empty because a swap was refused.
    #[cfg(feature = "llm")]
    async fn swap_downgrade(&self, new_path: &str) -> anyhow::Result<SwapOutcome> {
        let parked = {
            let mut slot = self.slot.write();
            let old = slot.model.take();
            slot.active = false;
            slot.inactive_reason = format!("downgrading to {new_path}");
            old
        };
        let loaded = {
            let result = async {
                ensure_model_fits_memory(new_path)?;
                let path = new_path.to_string();
                let loaded = tokio::task::spawn_blocking(move || Self::load_model_file(&path))
                    .await
                    .map_err(|e| anyhow::anyhow!("llm swap: load task failed: {e}"))??;
                anyhow::Ok(loaded)
            }
            .await;
            match result {
                Ok(loaded) => loaded,
                Err(e) => {
                    let mut slot = self.slot.write();
                    if slot.model.is_none() {
                        slot.model = parked;
                    }
                    slot.active = true;
                    slot.inactive_reason.clear();
                    drop(slot);
                    anyhow::bail!(
                        "llm swap: downgrade to {new_path} failed — old model restored: {e:#}"
                    );
                }
            }
        };
        let mut slot = self.slot.write();
        let from = slot.model_path.clone();
        slot.model = Some(std::sync::Arc::new(loaded));
        slot.active = true;
        slot.inactive_reason.clear();
        slot.model_path = new_path.to_string();
        drop(slot);
        tracing::info!(
            from = %from,
            to = %new_path,
            "llm: live model swap applied (downgrade; old model parked for release)"
        );
        // Drop the parked reference after the flip: with no in-flight
        // completion holding the old Arc, its memory frees here.
        drop(parked);
        Ok(SwapOutcome::Swapped {
            from,
            to: new_path.to_string(),
        })
    }

    /// Whether dialogue is available.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.slot.read().active
    }

    /// Why the engine is inactive (empty string when active).
    #[must_use]
    pub fn inactive_reason(&self) -> String {
        self.slot.read().inactive_reason.clone()
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
            // Clone the Arc out and drop the read guard before the
            // (seconds-long) generation: a concurrent live swap only waits
            // for the guard release, and this generation finishes on the
            // model it started with (L2 swap semantics).
            let (model, reason) = {
                let slot = self.slot.read();
                (slot.model.clone(), slot.inactive_reason.clone())
            };
            let Some(model) = model else {
                anyhow::bail!("llm inactive: {reason}");
            };
            let call = observability::model_call("llm", &self.model_variant());
            match self.run_completion(&model, turns, on_delta) {
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
            anyhow::bail!("llm inactive: {}", self.inactive_reason())
        }
    }

    /// Per-model metric variant label: the loaded GGUF file stem (SPEC
    /// appendix A #39 — variant = concrete model). Public so product
    /// layers can label conversation-trace spans identically. Reflects
    /// live swaps (the slot path, not the boot config).
    #[must_use]
    pub fn model_variant(&self) -> String {
        let path = self.loaded_model_path();
        std::path::Path::new(&path)
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
        use llama_cpp_2::model::LlamaChatMessage;
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
        let mut prompt = model.apply_chat_template(&template, &messages, true)?;
        // llama.cpp 0.1.160's chat-template application strips the
        // template's own <think> scaffolding (reasoning-format
        // handling), leaving a bare `assistant\n` turn — hybrid-
        // thinking models (Qwen3.5) then reopen a fresh <think> and
        // burn the whole completion budget on hidden reasoning
        // (measured 2026-10-30: every generated token landed inside
        // <think>, the visible reply came back empty). Restore the
        // closed-think prefill the Qwen3.5 template intends; models
        // whose templates carry their own prefill are left alone.
        if prompt.ends_with("assistant\n") || prompt.ends_with("assistant\n\n") {
            prompt.push_str("<think>\n\n</think>\n\n");
        }

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(self.config.n_ctx.max(128)))
            // 2026-10-10: the config field existed but was never applied —
            // every deployment ran llama.cpp's own default regardless.
            .with_n_threads(i32::from(
                u16::try_from(self.config.n_threads.max(1)).unwrap_or(u16::MAX),
            ));
        let mut ctx = model.new_context(backend, ctx_params)?;

        // llama-cpp-2 0.1.160 moved tokenization onto `vocab()`;
        // (add_special=false, parse_special=true) preserves the old
        // `str_to_token(…, AddBos::Never)` semantics — the chat-template
        // output contains special markers that must tokenize as such.
        let mut tokens = model.vocab().tokenize(prompt.as_bytes(), false, true);
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
            if model.vocab().is_eog(token) {
                break;
            }
            {
                let bytes = model.vocab().token_to_piece(token, false, None);
                let piece = String::from_utf8_lossy(&bytes);
                if !piece.is_empty() {
                    reply.push_str(&piece);
                    if let Some(cb) = on_delta.as_deref_mut()
                        && !cb(&piece)
                    {
                        // Consumer is done (queue closed / cancelled): stop
                        // burning tokens; the partial reply is returned.
                        break;
                    }
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
        let stripped = strip_think_blocks(&reply);
        if stripped.trim().is_empty() && !reply.trim().is_empty() {
            // The model spent its whole budget on hidden reasoning —
            // an operational warning worth surfacing, not debug noise.
            tracing::warn!(
                generated_tokens,
                "llm: reply stripped to empty — generation was all <think>"
            );
        }
        Ok((stripped, n_prompt as u64, generated_tokens))
    }
}

/// The model file a rung of the LLM ladder (`full`/`mid`/`lite`) resolves
/// to, mirroring [`crate::tools::resolve_llm_tier`]'s fallback (an empty
/// mid/lite slot reuses `model_path`). Unknown tiers resolve to
/// `None` — callers treat that as "no live swap".
#[must_use]
pub fn ladder_path(tier: &str, config: &LlmConfig) -> Option<String> {
    match tier {
        "full" => Some(config.model_path.clone()),
        "mid" => Some(if config.model_path_mid.is_empty() {
            config.model_path.clone()
        } else {
            config.model_path_mid.clone()
        }),
        "lite" => Some(if config.model_path_lite.is_empty() {
            config.model_path.clone()
        } else {
            config.model_path_lite.clone()
        }),
        _ => None,
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

/// What a live swap (`ChatEngine::swap_model`) actually did. The elastic
/// controller reports these differently (issue #12): a same-path "Ok"
/// that used to be logged as a swap freed nothing and hid the fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapOutcome {
    /// A different model file was loaded and installed.
    Swapped { from: String, to: String },
    /// The target resolves to the already-loaded model — nothing changed
    /// (tier slot unconfigured, or mirrored onto the same file).
    NoopSamePath { path: String },
}

/// Strict downgrade check by on-disk file size (issue #12): only a
/// strictly smaller file may take the park-first swap path. Missing
/// metadata (either side) conservatively reports `false` — the caller
/// then keeps the load-first semantics.
#[cfg_attr(not(feature = "llm"), allow(dead_code))]
fn is_downgrade(old_path: &str, new_path: &str) -> bool {
    match (std::fs::metadata(old_path), std::fs::metadata(new_path)) {
        (Ok(old), Ok(new)) => new.len() < old.len(),
        _ => false,
    }
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

/// Memory guardrail shared by boot load and live swap: refuse a model
/// larger than 2/3 of available memory — hosts without swap get
/// OOM-killed otherwise (fail-open keeps chat on the current model).
#[cfg_attr(not(feature = "llm"), allow(dead_code))]
fn ensure_model_fits_memory(path: &str) -> anyhow::Result<()> {
    let model_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if let Some(avail) = available_memory_bytes()
        && model_bytes > avail * 2 / 3
    {
        anyhow::bail!(
            "model {} MiB exceeds 2/3 of available memory ({} MiB) — refusing to load",
            model_bytes / 1_048_576,
            avail / 1_048_576,
        );
    }
    Ok(())
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

    #[test]
    fn ladder_path_mirrors_tier_fallbacks() {
        let cfg = LlmConfig {
            model_path: "a.gguf".into(),
            model_path_mid: "b.gguf".into(),
            model_path_lite: String::new(),
            ..LlmConfig::default()
        };
        assert_eq!(ladder_path("full", &cfg).as_deref(), Some("a.gguf"));
        assert_eq!(ladder_path("mid", &cfg).as_deref(), Some("b.gguf"));
        // Empty lite slot falls back to the full-tier model (no swap).
        assert_eq!(ladder_path("lite", &cfg).as_deref(), Some("a.gguf"));
        assert_eq!(ladder_path("bogus", &cfg), None);
    }

    #[tokio::test]
    async fn swap_missing_file_fails_closed_and_keeps_current() {
        let e = ChatEngine::from_config(&LlmConfig {
            enabled: true,
            model_path: "definitely-not-here.gguf".into(),
            ..LlmConfig::default()
        });
        assert!(!e.is_active());
        assert_eq!(e.loaded_model_path(), "definitely-not-here.gguf");
        let _err = e.swap_model("also-not-here.gguf").await.unwrap_err();
        #[cfg(feature = "llm")]
        assert!(_err.to_string().contains("not found"), "{_err}");
        // Fail-closed: slot untouched after the failed swap.
        assert!(!e.is_active());
        assert_eq!(e.loaded_model_path(), "definitely-not-here.gguf");
    }

    #[tokio::test]
    async fn swap_to_current_path_is_a_reported_no_op() {
        let e = ChatEngine::from_config(&LlmConfig::default());
        // Even an inactive engine short-circuits on the same path — now
        // as an explicit NoopSamePath, not a silent Ok(()) (issue #12).
        let outcome = e
            .swap_model(&LlmConfig::default().model_path)
            .await
            .expect("same-path swap is a no-op");
        assert_eq!(
            outcome,
            SwapOutcome::NoopSamePath {
                path: LlmConfig::default().model_path
            }
        );
        assert!(!e.is_active());
    }

    #[test]
    fn is_downgrade_compares_file_sizes() {
        let dir = std::env::temp_dir();
        let big = dir.join(format!("mibee-llm-big-{}.gguf", std::process::id()));
        let small = dir.join(format!("mibee-llm-small-{}.gguf", std::process::id()));
        std::fs::write(&big, vec![0_u8; 2048]).expect("big");
        std::fs::write(&small, vec![0_u8; 512]).expect("small");
        let (big, small) = (big.to_str().unwrap(), small.to_str().unwrap());
        assert!(is_downgrade(big, small), "smaller file is a downgrade");
        assert!(!is_downgrade(small, big), "larger file is not");
        assert!(!is_downgrade(big, big), "equal size is not a downgrade");
        assert!(
            !is_downgrade(big, "definitely-not-here.gguf"),
            "missing metadata is conservatively not a downgrade"
        );
        let _ = std::fs::remove_file(big);
        let _ = std::fs::remove_file(small);
    }
}
