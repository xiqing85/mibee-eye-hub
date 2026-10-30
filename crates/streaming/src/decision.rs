//! Decision-assist engine (Laya, SPEC appendix A #26): typed single-pass
//! decisions over text — a multilingual non-autoregressive "System 1"
//! checkpoint (NandhaKishorM/laya, Apache-2.0) served through ONNX
//! Runtime with the same tokenizer protocol as the upstream runtime.
//!
//! In-product use: the voice bridge asks it to triage each transcript
//! (answer / device / ignore) before spending a local-LLM pass, and the
//! decision is surfaced as an SSE `voice_decision` event. The engine is
//! fail-open like every other optional engine — missing model files or a
//! build without the `ai` feature leave it inactive, and callers keep
//! their previous behavior.
//!
//! Wire protocol (ported from `laya/common.py::build_sequence`):
//! `[CLS] "{type} question: {instructions}" [SEP] ([MASK] " opt")×K [SEP]
//! <state> [SEP]` with per-option 48-token caps and a shared
//! `head_max_len` budget; markers are the `[MASK]` positions; `qtype` ∈
//! {choice: 0, score: 1, noul: 2}; the head output `logits` [1,K] is
//! temperature-scaled per cardinality bucket before softmax.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(feature = "ai")]
use std::path::Path;
#[cfg(feature = "ai")]
use std::sync::Mutex;

#[cfg(feature = "ai")]
use ort::session::Session;

/// `[decision]` configuration section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionConfig {
    pub enabled: bool,
    /// Laya ONNX graph (fp32 or the `.int8.onnx` quantized export).
    pub model_path: String,
    /// HuggingFace `tokenizer.json` that came with the checkpoint.
    pub tokenizer_path: String,
    /// `laya_config.json` / `rl_agent_config.json` carrying `max_len`,
    /// `head_max_len` and the calibration temperatures.
    pub config_path: String,
    /// Decisions below this `answer_confidence` are reported but not
    /// acted on (callers fail open to their previous behavior).
    pub min_confidence: f32,
    /// Inference threads.
    pub num_threads: u16,
}

impl Default for DecisionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: "models/decision/laya_multilingual.int8.onnx".into(),
            tokenizer_path: "models/decision/tokenizer.json".into(),
            config_path: "models/decision/laya_config.json".into(),
            min_confidence: 0.35,
            num_threads: 1,
        }
    }
}

/// One completed choice decision.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChoiceDecision {
    pub label: String,
    /// Calibrated max(p) — comparable across questions.
    pub confidence: f32,
    /// Per-option calibrated probabilities in input order.
    pub probabilities: Vec<(String, f32)>,
    /// Probability the question is actionable at all (`act_probs[0]`).
    pub act_probability: f32,
}

/// Question types understood by the head (`qtype` ids).
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
pub(crate) const QTYPES: [(&str, i64); 3] = [("choice", 0), ("score", 1), ("noul", 2)];

/// Per-option token cap from the upstream runtime.
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
pub(crate) const OPTION_TOKEN_CAP: usize = 48;

/// Decision engine (fail-open).
pub struct DecisionEngine {
    active: bool,
    inactive_reason: String,
    /// File stem of the loaded ONNX model (per-model metric variant,
    /// SPEC appendix A #39).
    metric_variant: String,
    min_confidence: f32,
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    max_len: usize,
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    head_max_len: usize,
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    temperature: [f32; 3],
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    temperature_by_options: HashMap<String, f32>,
    #[cfg(feature = "ai")]
    session: Option<Mutex<Session>>,
    #[cfg(feature = "ai")]
    tokenizer: Option<Mutex<tokenizers::Tokenizer>>,
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    cls_id: i64,
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    sep_id: i64,
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    mask_id: i64,
}

/// File stem label for per-model metric variants.
fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

impl DecisionEngine {
    /// Build from config (fail-open on every load failure).
    #[must_use]
    pub fn from_config(config: &DecisionConfig) -> Self {
        match Self::load(config) {
            #[cfg(feature = "ai")]
            Ok(loaded) => {
                tracing::info!(model = %config.model_path, "decision: engine loaded");
                Self {
                    active: true,
                    inactive_reason: String::new(),
                    metric_variant: file_stem(&config.model_path),
                    min_confidence: config.min_confidence.clamp(0.0, 1.0),
                    max_len: loaded.max_len,
                    head_max_len: loaded.head_max_len,
                    temperature: loaded.temperature,
                    temperature_by_options: loaded.temperature_by_options,
                    session: Some(Mutex::new(loaded.session)),
                    tokenizer: Some(Mutex::new(loaded.tokenizer)),
                    cls_id: loaded.cls_id,
                    sep_id: loaded.sep_id,
                    mask_id: loaded.mask_id,
                }
            }
            #[cfg(not(feature = "ai"))]
            Ok(()) => unreachable!("non-ai load never succeeds"),
            Err(reason) => {
                tracing::info!(%reason, "decision: disabled");
                Self {
                    active: false,
                    inactive_reason: format!("{reason:#}"),
                    metric_variant: file_stem(&config.model_path),
                    min_confidence: config.min_confidence,
                    max_len: 512,
                    head_max_len: 192,
                    temperature: [1.0; 3],
                    temperature_by_options: HashMap::new(),
                    #[cfg(feature = "ai")]
                    session: None,
                    #[cfg(feature = "ai")]
                    tokenizer: None,
                    cls_id: 101,
                    sep_id: 102,
                    mask_id: 103,
                }
            }
        }
    }

    #[cfg(feature = "ai")]
    fn load(config: &DecisionConfig) -> anyhow::Result<Loaded> {
        use anyhow::{Context, bail};
        if !config.enabled {
            bail!("disabled by configuration");
        }
        for (label, path) in [
            ("model", &config.model_path),
            ("tokenizer", &config.tokenizer_path),
        ] {
            if !Path::new(path.as_str()).exists() {
                bail!("decision.{label} file not found: {path}");
            }
        }
        let tokenizer = tokenizers::Tokenizer::from_file(&config.tokenizer_path)
            .map_err(|e| anyhow::anyhow!("tokenizer parse failed: {e}"))?;
        let (cls_id, sep_id, mask_id) = special_ids(&tokenizer)?;

        // Calibration + budget from the checkpoint config when present.
        let (max_len, head_max_len, temperature, temps_by_options) =
            if Path::new(config.config_path.as_str()).exists() {
                let raw = std::fs::read_to_string(&config.config_path)
                    .with_context(|| format!("read {}", config.config_path))?;
                let cfg: serde_json::Value = serde_json::from_str(&raw)
                    .with_context(|| format!("parse {}", config.config_path))?;
                let max_len = cfg.get("max_len").and_then(|v| v.as_u64()).unwrap_or(512) as usize;
                let head_max_len = cfg
                    .get("head_max_len")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(192) as usize;
                let temperature = [
                    clamp_temperature(cfg_get_f32(&cfg, 0)),
                    clamp_temperature(cfg_get_f32(&cfg, 1)),
                    clamp_temperature(cfg_get_f32(&cfg, 2)),
                ];
                let mut by_options = HashMap::new();
                if let Some(map) = cfg
                    .get("temperature_by_options")
                    .and_then(|v| v.as_object())
                {
                    for (k, v) in map {
                        if let Some(t) = v.as_f64() {
                            by_options.insert(k.clone(), clamp_temperature(t as f32));
                        }
                    }
                }
                (max_len, head_max_len, temperature, by_options)
            } else {
                (512, 192, [1.0; 3], HashMap::new())
            };

        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("session builder: {e}"))?
            .with_intra_threads(usize::from(config.num_threads.max(1)))
            .map_err(|e| anyhow::anyhow!("intra threads: {e}"))?
            .commit_from_file(&config.model_path)
            .map_err(|e| anyhow::anyhow!("load {}: {e}", config.model_path))?;
        Ok(Loaded {
            session,
            tokenizer,
            cls_id,
            sep_id,
            mask_id,
            max_len: max_len.max(64),
            head_max_len: head_max_len.max(32),
            temperature,
            temperature_by_options: temps_by_options,
        })
    }

    #[cfg(not(feature = "ai"))]
    fn load(config: &DecisionConfig) -> anyhow::Result<()> {
        if !config.enabled {
            anyhow::bail!("disabled by configuration");
        }
        anyhow::bail!("built without the `ai` feature")
    }

    /// Whether decisions can run.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Why the engine is inactive ("" when active).
    #[must_use]
    pub fn inactive_reason(&self) -> &str {
        &self.inactive_reason
    }

    /// Decide one choice question over `state`. Returns `None` when the
    /// engine is inactive or the confidence sits below `min_confidence`
    /// (callers fail open).
    pub fn decide_choice(
        &self,
        state: &str,
        instructions: &str,
        options: &[(String, String)],
    ) -> Option<ChoiceDecision> {
        if !self.active || options.len() < 2 || options.len() > 10 {
            return None;
        }
        let call = observability::model_call("decision", &self.metric_variant());
        let inner = self.decide_choice_inner(state, instructions, options);
        match &inner {
            // A low-confidence decision is a successful inference whose
            // output the caller fails open on — not an engine error.
            Some(_) => call.finish_ok(None, None),
            None => call.finish_err(),
        }
        inner.and_then(|d| (d.confidence >= self.min_confidence).then_some(d))
    }

    /// Per-model metric variant label: the ONNX file stem (SPEC appendix
    /// A #39).
    fn metric_variant(&self) -> String {
        self.metric_variant.clone()
    }

    #[cfg(feature = "ai")]
    fn decide_choice_inner(
        &self,
        state: &str,
        instructions: &str,
        options: &[(String, String)],
    ) -> Option<ChoiceDecision> {
        use ort::value::Tensor;

        let rendered = render_choice_options(options);
        let tokenizer = self.tokenizer.as_ref()?.lock().ok()?;
        let head_ids = encode_ids(&tokenizer, &format!("choice question: {instructions}"))?;
        let mut opt_ids = Vec::with_capacity(options.len());
        for text in &rendered {
            let ids = encode_ids(&tokenizer, &format!(" {text}"))?;
            opt_ids.push(ids);
        }
        let state_ids = encode_ids(&tokenizer, state)?;
        drop(tokenizer);

        let (ids, markers) = assemble_sequence(
            SpecialTokens {
                cls: self.cls_id,
                sep: self.sep_id,
                mask: self.mask_id,
            },
            &head_ids,
            &opt_ids,
            &state_ids,
            self.max_len,
            self.head_max_len,
        );
        if markers.is_empty() {
            return None;
        }
        let attention: Vec<i64> = vec![1; ids.len()];
        let marker_mask: Vec<bool> = vec![true; markers.len()];
        let qtype = [QTYPES[0].1];

        let input_tensor = |v: Vec<i64>| -> Option<Tensor<i64>> {
            Tensor::from_array((vec![1_i64, v.len() as i64], v)).ok()
        };
        let ids_t = input_tensor(ids)?;
        let attn_t = input_tensor(attention)?;
        let markers_t =
            Tensor::from_array((vec![1_i64, markers.len() as i64], markers.clone())).ok()?;
        let marker_mask_t =
            Tensor::from_array((vec![1_i64, marker_mask.len() as i64], marker_mask)).ok()?;
        let qtype_t = Tensor::from_array((vec![1_i64], qtype.to_vec())).ok()?;

        let mut session = self.session.as_ref()?.lock().ok()?;
        let outputs = session
            .run(ort::inputs! {
                "input_ids" => ids_t,
                "attention_mask" => attn_t,
                "marker_pos" => markers_t,
                "marker_mask" => marker_mask_t,
                "qtype" => qtype_t,
            })
            .ok()?;
        // Output names drift between export vintages ("act_probs" vs
        // "act_logits"); address positionally, decode by name.
        let mut out = outputs.into_iter();
        let (logits_name, logits_val) = out.next()?;
        let (act_name, act_val) = out.next()?;
        let act_is_prob = act_name.contains("act_probs");
        let _ = logits_name;
        let (_, logits_raw) = logits_val.try_extract_tensor::<f32>().ok()?;
        let k = markers.len().min(logits_raw.len());
        let logits: Vec<f32> = logits_raw[..k].to_vec();

        let (_, act_raw) = act_val.try_extract_tensor::<f32>().ok()?;
        let act_probability = if act_is_prob || act_raw.len() < 2 {
            act_raw.first().copied().unwrap_or(0.0)
        } else {
            // Softmax over the two act logits, keep the "actionable" side.
            let (a, b) = (act_raw[0], act_raw[1]);
            let ea = (a - a.max(b)).exp();
            let eb = (b - a.max(b)).exp();
            ea / (ea + eb)
        };

        let t = pick_temperature(0, k, &self.temperature_by_options, self.temperature[0]);
        let probs = tempered_softmax(&logits, t);
        let (best_idx, _) = probs
            .iter()
            .enumerate()
            .reduce(|(i, p), (j, q)| if *q > *p { (j, q) } else { (i, p) })?;
        let label = options.get(best_idx).map(|(l, _)| l.clone())?;
        let confidence = probs[best_idx];
        Some(ChoiceDecision {
            label,
            confidence,
            probabilities: options
                .iter()
                .zip(probs.iter())
                .map(|((l, _), p)| (l.clone(), *p))
                .collect(),
            act_probability,
        })
    }

    #[cfg(not(feature = "ai"))]
    fn decide_choice_inner(
        &self,
        _state: &str,
        _instructions: &str,
        _options: &[(String, String)],
    ) -> Option<ChoiceDecision> {
        None
    }
}

#[cfg(feature = "ai")]
struct Loaded {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
    cls_id: i64,
    sep_id: i64,
    mask_id: i64,
    max_len: usize,
    head_max_len: usize,
    temperature: [f32; 3],
    temperature_by_options: HashMap<String, f32>,
}

#[cfg(feature = "ai")]
fn special_ids(tokenizer: &tokenizers::Tokenizer) -> anyhow::Result<(i64, i64, i64)> {
    let cls = tokenizer
        .token_to_id("[CLS]")
        .or_else(|| tokenizer.token_to_id("<s>"))
        .ok_or_else(|| anyhow::anyhow!("tokenizer has no CLS token"))?;
    let sep = tokenizer
        .token_to_id("[SEP]")
        .or_else(|| tokenizer.token_to_id("</s>"))
        .ok_or_else(|| anyhow::anyhow!("tokenizer has no SEP token"))?;
    let mask = tokenizer
        .token_to_id("[MASK]")
        .or_else(|| tokenizer.token_to_id("<mask>"))
        .ok_or_else(|| {
            anyhow::anyhow!("tokenizer has no MASK token — not a Laya checkpoint tokenizer")
        })?;
    Ok((i64::from(cls), i64::from(sep), i64::from(mask)))
}

#[cfg(feature = "ai")]
fn encode_ids(tokenizer: &tokenizers::Tokenizer, text: &str) -> Option<Vec<i64>> {
    let enc = tokenizer
        .encode(text.replace("[MASK]", " ").as_str(), false)
        .map_err(|e| tracing::warn!(error = %e, "decision: encode failed"))
        .ok()?;
    Some(enc.get_ids().iter().map(|&id| i64::from(id)).collect())
}

/// Render choice options exactly like the upstream runtime: `label:
/// description` when a description exists, the bare label otherwise.
pub(crate) fn render_choice_options(options: &[(String, String)]) -> Vec<String> {
    options
        .iter()
        .map(|(label, desc)| {
            if desc.trim().is_empty() {
                label.clone()
            } else {
                format!("{label}: {desc}")
            }
        })
        .collect()
}

/// The three special tokens the sequence layout is built from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SpecialTokens {
    pub cls: i64,
    pub sep: i64,
    pub mask: i64,
}

/// Assemble the decision sequence (port of `build_sequence`):
/// `[CLS] head [SEP] [MASK]opt0 [MASK]opt1 … [SEP] state [SEP]`, with the
/// shared head budget squeezing instructions first and options evenly
/// only when the budget cannot fit them. Returns `(ids, marker_pos)`.
pub(crate) fn assemble_sequence(
    sp: SpecialTokens,
    head_ids: &[i64],
    opt_ids: &[Vec<i64>],
    state_ids: &[i64],
    max_len: usize,
    head_max_len: usize,
) -> (Vec<i64>, Vec<i64>) {
    let SpecialTokens { cls, sep, mask } = sp;
    let mut opt_ids: Vec<Vec<i64>> = opt_ids
        .iter()
        .map(|o| {
            let mut v = vec![mask];
            v.extend_from_slice(&o[..o.len().min(OPTION_TOKEN_CAP)]);
            v
        })
        .collect();
    let mut budget = head_max_len as i64 - opt_ids.iter().map(|o| o.len() as i64).sum::<i64>();
    if budget < 16 {
        let per = ((head_max_len as i64 - 16) / opt_ids.len().max(1) as i64).max(4) as usize;
        for o in &mut opt_ids {
            o.truncate(per);
        }
        budget = head_max_len as i64 - opt_ids.iter().map(|o| o.len() as i64).sum::<i64>();
    }
    let head_keep = (head_ids.len() as i64).clamp(0, budget.max(8)) as usize;
    let mut ids = vec![cls];
    ids.extend_from_slice(&head_ids[..head_keep]);
    ids.push(sep);
    let mut markers = Vec::with_capacity(opt_ids.len());
    for o in &opt_ids {
        markers.push(ids.len() as i64);
        ids.extend_from_slice(o);
    }
    ids.push(sep);
    let room = max_len.saturating_sub(ids.len() + 1);
    ids.extend_from_slice(&state_ids[..state_ids.len().min(room)]);
    ids.push(sep);
    ids.truncate(max_len);
    markers.retain(|m| (*m as usize) < ids.len());
    (ids, markers)
}

/// Temperature bucket key (`"{type}:{size}"`, size ∈ 2/3-5/6-10/11+).
pub(crate) fn temp_bucket(qtype: usize, k: usize) -> String {
    let name = QTYPES.get(qtype).map_or("choice", |(n, _)| *n);
    let size = if k <= 2 {
        "2"
    } else if k <= 5 {
        "3-5"
    } else if k <= 10 {
        "6-10"
    } else {
        "11+"
    };
    format!("{name}:{size}")
}

/// Calibration temperatures are clamped to a sane range (a fitted value
/// below the floor would sharpen logits ~10× and publish a 0.24 top
/// probability as near-certain — upstream clamps for the same reason).
pub(crate) fn clamp_temperature(t: f32) -> f32 {
    if t.is_finite() && t > 0.0 {
        t.clamp(0.25, 4.0)
    } else {
        1.0
    }
}

/// Pick the calibration temperature for a question: the per-cardinality
/// bucket when fitted, else the per-type base.
pub(crate) fn pick_temperature(
    qtype: usize,
    k: usize,
    by_options: &HashMap<String, f32>,
    base: f32,
) -> f32 {
    by_options
        .get(&temp_bucket(qtype, k))
        .copied()
        .unwrap_or(base)
}

/// Numerically stable softmax with a temperature scale.
pub(crate) fn tempered_softmax(logits: &[f32], t: f32) -> Vec<f32> {
    if logits.is_empty() {
        return Vec::new();
    }
    let t = clamp_temperature(t);
    let max = logits.iter().cloned().fold(f32::MIN, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| ((l - max) / t).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum <= 0.0 || !sum.is_finite() {
        return vec![1.0 / logits.len() as f32; logits.len()];
    }
    exps.iter().map(|e| e / sum).collect()
}

#[cfg(feature = "ai")]
fn cfg_get_f32(cfg: &serde_json::Value, idx: usize) -> f32 {
    cfg.get("temperature")
        .and_then(|t| t.get(idx))
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .unwrap_or(1.0)
}

/// Filler syllables discounted by [`triage_substantive`]: ambient-noise
/// ASR transcribes to chains of interjections ("嗯嗯啊啊的背景声"),
/// while real speech addressed to the device carries content words.
const TRIAGE_FILLERS: &[&str] = &[
    "嗯", "啊", "哦", "呃", "诶", "唉", "呀", "哈", "嘿", "blah", "haha",
];

/// Whether a transcript carries substantive content for the triage
/// ignore guard (2026-10-30): ≥[`INTERRUPT_CONTENT_MIN_CHARS`] non-space
/// characters after filler interjections are discounted.
#[must_use]
pub fn triage_substantive(transcript: &str) -> bool {
    let mut rest = transcript.to_string();
    for f in TRIAGE_FILLERS {
        rest = rest.replace(f, "");
    }
    rest.chars().filter(|c| !c.is_whitespace()).count() >= INTERRUPT_CONTENT_MIN_CHARS
}

/// Voice-triage ignore guard. The triage options are written in
/// Mandarin and the multilingual classifier is out-of-domain on
/// written Cantonese — it confidently labels real Cantonese
/// questions as noise (0.90/0.70 on 而家…点嘛, 2026-10-02), which
/// silently drops the spoken reply. An "ignore" verdict on a
/// Cantonese transcript is not trustworthy; fail open to a reply,
/// like every other uncertain path (speaker_verify, weather).
///
/// The same distrust now extends to *substantive* transcripts in any
/// language (2026-10-30 field data): the wake word fired, so the user
/// deliberately addressed the device — yet the classifier labeled real
/// conversational speech "ignore" at up to 0.91 confidence ("干吗没听到
/// 你的声音呐", "你的反应很慢呢"), producing the "说话没反应" experience.
/// An "ignore" verdict may now only drop filler-like transcripts; the
/// answer category covers chit-chat and complaints by definition.
#[must_use]
pub fn triage_skips_reply(label: &str, transcript: &str) -> bool {
    label == "ignore"
        && crate::lang::detect(transcript) != crate::lang::SpokenLang::Cantonese
        && !triage_substantive(transcript)
}

/// What to do with an utterance that arrives while an answer round is
/// still running (SPEC appendix A #45, conservative direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptAction {
    /// A stop word ("停/别说了/闭嘴/算了…") — cancel the round, answer
    /// nothing. Rule-matched, zero model latency.
    Stop,
    /// A new question — cancel the old round and answer this instead.
    Supersede,
    /// Backchannel ("嗯/好/哦") or anything unclassified — ignore, the
    /// old round keeps going. The fail-open direction: rather miss an
    /// interruption than falsely kill a good answer.
    Backchannel,
}

/// Rule-level stop-word match (the ① tier of the #45 policy — no model
/// involved). Only short utterances match: a stop word buried in a
/// longer sentence is content, not a command.
#[must_use]
pub fn stop_words_match(text: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "停",
        "别说了",
        "闭嘴",
        "不用说了",
        "不要说了",
        "算了",
        "收声",
        "唔使讲",
        "不用讲",
        "stop",
        "quiet",
        "shh",
    ];
    let t = text.trim();
    if t.chars().count() > 6 {
        return false;
    }
    let lower = t.to_lowercase();
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Map a #45 classifier verdict onto an action. Anything below the
/// confidence threshold fails open to [`InterruptAction::Backchannel`].
#[must_use]
pub fn interrupt_policy(label: &str, confidence: f32, threshold: f32) -> InterruptAction {
    if confidence < threshold {
        return InterruptAction::Backchannel;
    }
    match label {
        "supersede" => InterruptAction::Supersede,
        _ => InterruptAction::Backchannel,
    }
}

/// Minimum non-whitespace chars for a mid-round transcript to count as
/// substantive content in the [`interrupt_content_gate`] (2026-10-30).
/// Real backchannels are ≤4 chars (嗯 / 对 / 好的 / 嗯嗯嗯嗯 / "yeah");
/// everything longer deserves a hearing.
pub const INTERRUPT_CONTENT_MIN_CHARS: usize = 5;

/// The ②.5 tier of the #45 policy (2026-10-30): a *substantive* transcript
/// is content, not a backchannel, regardless of classifier confidence.
///
/// Field data from the workstation 2026-10-30: one two-minute round ignored
/// ten follow-up utterances — including clear questions (问我是谁嘛 /
/// 唉呀对不起啊我不应该骂你) — because noisy ASR kept Laya below the
/// fail-open threshold. Waiting out a stale answer with no way to redirect
/// it is the worst failure mode of the interrupt gate, worse than a false
/// supersede (which just re-answers).
///
/// The veto: a *confident* backchannel verdict (`label == "backchannel"` at
/// or above `threshold`) still ignores the utterance, so genuine
/// acknowledgements (好的没问题 / 哈哈哈哈) do not cancel a reply the user
/// wants to hear. `label` is `None` when the classifier was unavailable.
#[must_use]
pub fn interrupt_content_gate(
    action: InterruptAction,
    transcript: &str,
    label: Option<&str>,
    confidence: f32,
    threshold: f32,
) -> InterruptAction {
    use InterruptAction::Backchannel;
    if !matches!(action, Backchannel) {
        return action;
    }
    let chars = transcript.chars().filter(|c| !c.is_whitespace()).count();
    if chars < INTERRUPT_CONTENT_MIN_CHARS {
        return action;
    }
    if label == Some("backchannel") && confidence >= threshold {
        return action;
    }
    InterruptAction::Supersede
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triage_ignore_fails_open_for_cantonese() {
        // Measured on the deployed workstation 2026-10-02: the triage
        // options are Mandarin and the classifier confidently labeled
        // real Cantonese questions (而家…点嘛 / …变嘛) as "ignore"
        // (0.90/0.70) — the spoken reply was silently dropped.
        assert!(!triage_skips_reply("ignore", "而家广州天气变嘛"));
        assert!(!triage_skips_reply("ignore", "而家天气广州天气点嘛"));
    }

    #[test]
    fn stop_words_match_short_commands_in_three_languages() {
        assert!(stop_words_match("停"));
        assert!(stop_words_match("别说了"));
        assert!(stop_words_match(" 闭嘴 "));
        assert!(stop_words_match("算了"));
        assert!(stop_words_match("收声啦"));
        assert!(stop_words_match("唔使讲"));
        assert!(stop_words_match("stop"));
        // Content, not commands: long sentences never match even when
        // they contain a stop word.
        assert!(!stop_words_match("请问停车场在哪里"));
        assert!(!stop_words_match("别说了这么多，再讲讲你刚才提到的算法"));
        assert!(!stop_words_match("现在几点了"));
        assert!(!stop_words_match(""));
    }

    #[test]
    fn interrupt_policy_fails_open_below_threshold() {
        assert_eq!(
            interrupt_policy("supersede", 0.9, 0.6),
            InterruptAction::Supersede
        );
        assert_eq!(
            interrupt_policy("supersede", 0.5, 0.6),
            InterruptAction::Backchannel,
            "low confidence fails open"
        );
        assert_eq!(
            interrupt_policy("backchannel", 0.99, 0.6),
            InterruptAction::Backchannel
        );
        assert_eq!(
            interrupt_policy("anything-else", 0.99, 0.6),
            InterruptAction::Backchannel,
            "unknown label is never an interrupt"
        );
    }

    #[test]
    fn content_gate_promotes_substantive_utterances() {
        // Field data 2026-10-30: real questions that sat ignored for a
        // whole two-minute round because Laya stayed under threshold.
        for (utterance, label, conf) in [
            ("问我是谁嘛", Some("backchannel"), 0.55),
            ("唉呀对不起啊我不应该骂你", None, 0.0),
            ("这个给你看吧", Some("supersede"), 0.4),
            ("well well what about this", None, 0.0),
        ] {
            let action = interrupt_policy(label.unwrap_or("backchannel"), conf, 0.6);
            assert_eq!(
                interrupt_content_gate(action, utterance, label, conf, 0.6),
                InterruptAction::Supersede,
                "should supersede on: {utterance}"
            );
        }
    }

    #[test]
    fn content_gate_keeps_confident_backchannel_verdicts() {
        assert_eq!(
            interrupt_content_gate(
                InterruptAction::Backchannel,
                "好的没问题",
                Some("backchannel"),
                0.9,
                0.6
            ),
            InterruptAction::Backchannel
        );
        assert_eq!(
            interrupt_content_gate(
                InterruptAction::Backchannel,
                "哈哈哈哈哈",
                Some("backchannel"),
                0.75,
                0.6
            ),
            InterruptAction::Backchannel
        );
    }

    #[test]
    fn content_gate_keeps_short_and_non_backchannel_actions() {
        // ≤4 chars (whitespace excluded) is backchannel territory.
        for short in ["嗯嗯嗯嗯", "这么大了", "they", "  好 的  "] {
            assert_eq!(
                interrupt_content_gate(InterruptAction::Backchannel, short, None, 0.0, 0.6),
                InterruptAction::Backchannel,
                "short utterance stays ignored: {short}"
            );
        }
        // Stop/Supersede verdicts pass through untouched.
        assert_eq!(
            interrupt_content_gate(
                InterruptAction::Stop,
                "好的没问题",
                Some("backchannel"),
                0.9,
                0.6
            ),
            InterruptAction::Stop
        );
        assert_eq!(
            interrupt_content_gate(InterruptAction::Supersede, "嗯", None, 0.0, 0.6),
            InterruptAction::Supersede
        );
    }

    #[test]
    fn triage_ignore_still_skips_mandarin_and_english_noise() {
        assert!(triage_skips_reply("ignore", "嗯嗯啊啊的背景声"));
        assert!(triage_skips_reply("ignore", "blah blah"));
        // Short/wake-only fragments stay droppable.
        assert!(triage_skips_reply("ignore", "小蜜蜂"));
        assert!(triage_skips_reply("ignore", "嗯嗯嗯嗯"));
    }

    #[test]
    fn triage_never_skips_answer_or_device() {
        assert!(!triage_skips_reply("answer", "现在几点了"));
        assert!(!triage_skips_reply("device", "打开录像"));
        assert!(!triage_skips_reply("answer", "而家几点"));
    }

    #[test]
    fn triage_ignore_fails_open_for_substantive_speech() {
        // Field data 2026-10-30: the classifier labeled these real,
        // deliberately-woken utterances "ignore" at up to 0.91 — the
        // user experienced them as "说话没反应".
        for said in ["干吗没听到你的声音呐", "你的反应很慢呢", "问我是谁嘛"] {
            assert!(
                !triage_skips_reply("ignore", said),
                "substantive speech must not be silently dropped: {said}"
            );
        }
    }

    #[test]
    fn config_defaults_off() {
        let c = DecisionConfig::default();
        assert!(!c.enabled, "decision is opt-in");
        assert!(c.model_path.contains("decision"));
        assert!((c.min_confidence - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn engine_disabled_by_default() {
        let e = DecisionEngine::from_config(&DecisionConfig::default());
        assert!(!e.is_active());
        assert_eq!(e.inactive_reason(), "disabled by configuration");
        assert!(
            e.decide_choice(
                "s",
                "i",
                &[("a".into(), "".into()), ("b".into(), "".into())]
            )
            .is_none(),
            "inactive engine decides nothing"
        );
    }

    #[test]
    fn engine_enabled_but_files_missing_fails_open() {
        let e = DecisionEngine::from_config(&DecisionConfig {
            enabled: true,
            ..DecisionConfig::default()
        });
        assert!(!e.is_active(), "missing model files must not fake activity");
    }

    #[test]
    fn options_render_like_upstream() {
        let opts = vec![
            (
                "answer".to_string(),
                "用户在提问或聊天，需要回答".to_string(),
            ),
            ("ignore".to_string(), String::new()),
        ];
        assert_eq!(
            render_choice_options(&opts),
            vec![
                "answer: 用户在提问或聊天，需要回答".to_string(),
                "ignore".to_string(),
            ]
        );
    }

    #[test]
    fn sequence_layout_matches_protocol() {
        // [CLS]=1 [SEP]=2 [MASK]=0; synthetic ids spell out each segment.
        let head = vec![10, 11, 12];
        let opts = vec![vec![20, 21], vec![30], vec![40, 41, 42]];
        let state = vec![50, 51, 52, 53];
        let sp = SpecialTokens {
            cls: 1,
            sep: 2,
            mask: 0,
        };
        let (ids, markers) = assemble_sequence(sp, &head, &opts, &state, 512, 192);
        assert_eq!(
            ids,
            vec![
                1, 10, 11, 12, 2, 0, 20, 21, 0, 30, 0, 40, 41, 42, 2, 50, 51, 52, 53, 2
            ],
            "[CLS] head [SEP] [MASK]opt… [SEP] state [SEP]"
        );
        assert_eq!(markers, vec![5, 8, 10], "markers sit on every [MASK]");
    }

    #[test]
    fn head_budget_squeezes_instructions_before_options() {
        let head = vec![7; 100];
        let opts = vec![vec![3; 4], vec![4; 4]];
        // head_max_len 24: options become 5 ids each (MASK + 4) = 10,
        // budget 14 < 16 → options trim to (24-16)/2 = 4 ids each = 8,
        // budget recovers to 16 → instructions keep exactly 16.
        let sp = SpecialTokens {
            cls: 1,
            sep: 2,
            mask: 0,
        };
        let (ids, markers) = assemble_sequence(sp, &head, &opts, &[], 512, 24);
        let first_sep = ids.iter().position(|&t| t == 2).expect("head [SEP]");
        let head_len = first_sep - 1;
        assert_eq!(head_len, 16, "instructions cut to the remaining budget");
        assert_eq!(
            ids.iter().filter(|&&t| t == 0).count(),
            2,
            "options survive the squeeze (one MASK each)"
        );
        assert_eq!(markers.len(), 2);
    }

    #[test]
    fn max_len_truncates_state_not_head() {
        let head = vec![7; 20];
        let opts = vec![vec![3; 4], vec![4; 4]];
        let state = vec![9; 500];
        let sp = SpecialTokens {
            cls: 1,
            sep: 2,
            mask: 0,
        };
        let (ids, _) = assemble_sequence(sp, &head, &opts, &state, 40, 192);
        assert_eq!(ids.len(), 40, "sequence respects max_len");
        assert_eq!(*ids.last().unwrap(), 2, "state end still carries [SEP]");
    }

    #[test]
    fn buckets_and_temperature_selection() {
        assert_eq!(temp_bucket(0, 2), "choice:2");
        assert_eq!(temp_bucket(0, 5), "choice:3-5");
        assert_eq!(temp_bucket(1, 10), "score:6-10");
        assert_eq!(temp_bucket(2, 30), "noul:11+");
        let mut by_options = HashMap::new();
        by_options.insert("choice:3-5".to_string(), 1.5_f32);
        assert!((pick_temperature(0, 4, &by_options, 1.0) - 1.5).abs() < f32::EPSILON);
        assert!((pick_temperature(0, 2, &by_options, 1.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn softmax_is_stable_and_calibrated() {
        let p = tempered_softmax(&[2.0, 1.0, 0.0], 1.0);
        let sum: f32 = p.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        assert!(p[0] > p[1] && p[1] > p[2]);
        // Higher temperature flattens.
        let hot = tempered_softmax(&[2.0, 1.0, 0.0], 4.0);
        assert!(hot[0] - hot[2] < p[0] - p[2]);
        // Extreme logits stay finite.
        let big = tempered_softmax(&[1e30, -1e30], 1.0);
        assert!(big[0].is_finite() && big[0] > 0.99);
        assert_eq!(tempered_softmax(&[], 1.0).len(), 0);
    }

    #[test]
    fn temperatures_clamp_away_from_nonsense() {
        assert!(
            (clamp_temperature(0.05) - 0.25).abs() < f32::EPSILON,
            "floor"
        );
        assert!(
            (clamp_temperature(99.0) - 4.0).abs() < f32::EPSILON,
            "ceiling"
        );
        assert_eq!(clamp_temperature(f32::NAN), 1.0);
        assert_eq!(clamp_temperature(0.0), 1.0);
    }
}
