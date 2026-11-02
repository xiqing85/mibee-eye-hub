//! The two production `ModelChannel` implementations: the OpenAI-
//! compatible cloud (SPEC §4.10, native `tools`/`tool_calls`) and the
//! local Qwen3 engine (`<tools>` system section + `<tool_call>` parse).

use super::runner::{ModelChannel, ModelOutput};
use super::{ToolSpec, qwen_tools_section};
use crate::cloud::CloudAi;
use std::sync::Arc;
use streaming::llm::ChatTurn;

// ─── Cloud (OpenAI-compatible tool calling) ──────────────────────────

pub struct CloudChannel {
    pub cloud: Arc<CloudAi>,
    pub timeout_secs: u64,
    /// Reply length cap riding every agent call — voice rounds pass a
    /// tight cap (long monologues cost 20 s+ of TTS, 2026-11-02).
    pub max_tokens: u32,
}

impl ModelChannel for CloudChannel {
    fn complete(
        &self,
        turns: &[ChatTurn],
        tools: &[ToolSpec],
        with_tools: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ModelOutput>> + Send + '_>>
    {
        let cloud = self.cloud.clone();
        let timeout = self.timeout_secs;
        let max_tokens = self.max_tokens;
        let turns = turns.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            let (text, calls, usage) = cloud
                .agent_complete(&turns, &tools, with_tools, timeout, max_tokens)
                .await
                .map_err(|e| anyhow::anyhow!("cloud agent: {e:?}"))?;
            Ok(ModelOutput {
                text,
                tool_calls: calls,
                tokens: usage,
            })
        })
    }
}

// ─── Local Qwen3 ─────────────────────────────────────────────────────

pub struct LocalChannel {
    pub engine: Arc<streaming::llm::ChatEngine>,
}

impl ModelChannel for LocalChannel {
    fn complete(
        &self,
        turns: &[ChatTurn],
        tools: &[ToolSpec],
        with_tools: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ModelOutput>> + Send + '_>>
    {
        let engine = self.engine.clone();
        let mut turns = turns.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            if with_tools && !tools.is_empty() {
                // The tool list rides the system turn (llama-cpp-2 has no
                // tools-aware chat template; the Qwen3 template format is
                // what the model was trained on).
                if let Some(first) = turns.first_mut()
                    && first.role == "system"
                {
                    first.content.push_str(&qwen_tools_section(&tools));
                }
            }
            let (reply, prompt, completion) =
                tokio::task::spawn_blocking(move || engine.complete_with_usage(&turns))
                    .await
                    .map_err(|e| anyhow::anyhow!("llm task join failed: {e}"))??;
            let calls = super::parse_tool_calls(&reply);
            // The text alongside/besides markers is prose the loop should
            // ignore while calls are pending; keep it for the final answer.
            let text = if calls.is_empty() {
                Some(reply)
            } else {
                strip_tool_markers(&reply)
            };
            Ok(ModelOutput {
                text,
                tool_calls: calls,
                tokens: Some((prompt, completion)),
            })
        })
    }
}

/// Remove `<tool_call>…</tool_call>` spans so leftover prose can serve as
/// the reply when the loop is forced to answer in text.
pub(crate) fn strip_tool_markers(output: &str) -> Option<String> {
    let mut out = String::with_capacity(output.len());
    let mut rest = output;
    while let Some(start) = rest.find("<tool_call>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "<tool_call>".len()..];
        match after.find("</tool_call>") {
            Some(end) => rest = &after[end + "</tool_call>".len()..],
            None => {
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_markers_keeps_prose_around_calls() {
        let out = "先查一下。<tool_call>{\"name\":\"a\"}</tool_call>稍等";
        assert_eq!(strip_tool_markers(out).as_deref(), Some("先查一下。稍等"));
        assert_eq!(strip_tool_markers("<tool_call>{}</tool_call>"), None);
        assert_eq!(strip_tool_markers("直接回答").as_deref(), Some("直接回答"));
    }
}
