//! Sentence streaming support for the voice pipeline (2026-10-12
//! full-duplex design, phase P0): turn a token-by-token LLM delta stream
//! into speakable sentences, hiding Qwen3 `<think>` blocks from the
//! speaker — the final reply strips them at the end, but a streaming
//! consumer would otherwise synthesize the model's private reasoning.
//!
//! Two stages, both pure state machines:
//! - [`ThinkFilter`] strips `<think>…</think>` spans from a delta stream,
//!   holding back a tail that could be a marker prefix split across
//!   deltas.
//! - [`SentenceSplitter`] accumulates filtered text and emits complete
//!   sentences on terminal punctuation (。！？!?；; and newlines); short
//!   segments without punctuation buffer until a later boundary or the
//!   final flush.

/// State machine stripping `<think>…</think>` spans from streamed deltas.
///
/// Deltas are valid UTF-8 pieces; the markers may straddle delta
/// boundaries, so a tail that is a strict prefix of either marker is held
/// back until the ambiguity resolves.
#[derive(Debug, Default)]
pub struct ThinkFilter {
    in_think: bool,
    pending: String,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl ThinkFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a `<think>` block is currently open (all output suppressed).
    pub fn in_think(&self) -> bool {
        self.in_think
    }

    /// Feed one delta; returns the visible text outside think spans.
    pub fn push(&mut self, delta: &str) -> String {
        self.pending.push_str(delta);
        let mut out = String::new();
        loop {
            if self.in_think {
                if let Some(at) = self.pending.find(THINK_CLOSE) {
                    // Skip past the closing marker; anything after it is
                    // visible (usually the answer starts right after).
                    self.pending.drain(..at + THINK_CLOSE.len());
                    self.in_think = false;
                    continue;
                }
                // No close yet: hold back a possible marker prefix, drop
                // the rest (it is thinking content, never spoken).
                let keep = marker_prefix_len(&self.pending, THINK_CLOSE);
                let drop_to = self.pending.len() - keep;
                self.pending.drain(..drop_to);
                return out;
            }
            if let Some(at) = self.pending.find(THINK_OPEN) {
                out.push_str(&self.pending[..at]);
                self.pending.drain(..at + THINK_OPEN.len());
                self.in_think = true;
                continue;
            }
            // No marker: emit everything except a possible marker prefix
            // straddling into the next delta.
            let hold = marker_prefix_len(&self.pending, THINK_OPEN);
            let emit_to = self.pending.len() - hold;
            out.push_str(&self.pending[..emit_to]);
            self.pending.drain(..emit_to);
            return out;
        }
    }
}

/// Length of the longest suffix of `text` that is a strict prefix of
/// `marker` (0 when the text does not end inside a partial marker).
fn marker_prefix_len(text: &str, marker: &str) -> usize {
    let t = text.as_bytes();
    let m = marker.as_bytes();
    let max = t.len().min(m.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if t[t.len() - len..].eq_ignore_ascii_case(&m[..len]) {
            return len;
        }
    }
    0
}

/// Accumulates text and emits complete sentences.
///
/// Boundaries: 。！？!?；; plus newlines. A trailing boundary character is
/// kept un-emitted only until the next push decides whether it is part of
/// a multi-character terminator (e.g. `！？`); in practice one boundary
/// ends the sentence and the rest flushes with [`SentenceSplitter::flush`].
#[derive(Debug, Default)]
pub struct SentenceSplitter {
    buf: String,
}

impl SentenceSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed text; returns every sentence completed by this push.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some((idx, c)) = self
            .buf
            .char_indices()
            .find(|&(_, c)| matches!(c, '。' | '！' | '？' | '!' | '?' | ';' | '；' | '\n'))
        {
            // Include the punctuation itself in the sentence.
            let end = idx + c.len_utf8();
            let sentence: String = self.buf.drain(..end).collect();
            let trimmed = sentence.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
        out
    }

    /// Flush any buffered remainder as a final sentence (empty string
    /// when nothing is left).
    pub fn flush(&mut self) -> Option<String> {
        let rest = self.buf.trim();
        if rest.is_empty() {
            self.buf.clear();
            return None;
        }
        Some(std::mem::take(&mut self.buf).trim().to_string())
    }

    /// Buffered-but-unemitted text length (for tests/diagnostics).
    pub fn pending_len(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a filter+splitter pair one delta at a time.
    fn stream(deltas: &[&str]) -> Vec<String> {
        let mut tf = ThinkFilter::new();
        let mut sp = SentenceSplitter::new();
        let mut out = Vec::new();
        for d in deltas {
            out.extend(sp.push(&tf.push(d)));
        }
        if let Some(tail) = sp.flush() {
            out.push(tail);
        }
        out
    }

    #[test]
    fn splits_chinese_and_english_sentences() {
        let s = stream(&["现在几点了？", "现在是下午三点。", "还有什么想问的吗？"]);
        assert_eq!(
            s,
            vec!["现在几点了？", "现在是下午三点。", "还有什么想问的吗？"]
        );
    }

    #[test]
    fn markers_split_across_deltas_are_never_spoken() {
        // The pathological case: `<think>` and `</think>` each straddle
        // three deltas; the answer after the close must survive intact.
        let s = stream(&[
            "<th",
            "ink>让我想想",
            "这个问题</th",
            "ink>答案是四十二。",
            "再见！",
        ]);
        assert_eq!(s, vec!["答案是四十二。", "再见！"]);
    }

    #[test]
    fn plain_text_containing_marker_like_prefixes_flows() {
        // "< t h" partials that never complete a marker must not stall the
        // stream — the held-back tail resolves once a non-matching byte
        // arrives.
        let s = stream(&["三点<", "a>", "开饭。"]);
        assert_eq!(s, vec!["三点<a>开饭。"]);
    }

    #[test]
    fn no_punctuation_text_flushes_at_end() {
        let s = stream(&["好的", "没问题", "这就去"]);
        assert_eq!(s, vec!["好的没问题这就去"]);
    }

    #[test]
    fn newline_also_ends_a_sentence() {
        let s = stream(&["第一行\n第二行\n"]);
        assert_eq!(s, vec!["第一行", "第二行"]);
    }

    #[test]
    fn empty_deltas_and_whitespace_only_are_noops() {
        let s = stream(&["", "   ", "嗯。", " "]);
        assert_eq!(s, vec!["嗯。"]);
    }

    #[test]
    fn think_filter_tracks_state() {
        let mut tf = ThinkFilter::new();
        assert!(tf.push("<think>abc").is_empty());
        assert!(tf.in_think());
        assert_eq!(tf.push("def</think>123"), "123");
        assert!(!tf.in_think());
    }

    #[test]
    fn splitter_pending_length_tracks_buffer() {
        let mut sp = SentenceSplitter::new();
        sp.push("还没");
        sp.push("说完");
        assert_eq!(sp.pending_len(), 12);
        assert_eq!(sp.push("呢。"), vec!["还没说完呢。"]);
        assert_eq!(sp.pending_len(), 0);
        assert_eq!(sp.flush(), None);
    }
}
