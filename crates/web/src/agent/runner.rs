//! The agent tool-calling loop (SPEC §3.5): model → tool calls →
//! execution → observation feedback → final answer, bounded by
//! `max_steps`. Two model channels exist — the cloud (OpenAI-compatible
//! `tools`/`tool_calls`/`role:"tool"`, SPEC §4.10) and the local Qwen3
//! engine (`<tools>` JSON in a system section, `<tool_call>` markers in
//! the output, `<tool_response>` feedback — the format the model was
//! trained on; llama-cpp-2 0.1.157 has no tools-aware chat template so
//! we construct it ourselves).
//!
//! Fail-open: a model reply without parseable tool calls is the final
//! answer (identical to the non-agent path); a tool failure is fed back
//! as honest error text; the last iteration drops the tool table so the
//! model must answer in text.

use super::{AgentStep, ToolCallRecord, ToolRegistry, ToolSpec, truncate_for_model};
use crate::conversations::truncate_note;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use streaming::llm::ChatTurn;

/// What a model channel returns per iteration.
#[derive(Debug)]
pub struct ModelOutput {
    pub text: Option<String>,
    pub tool_calls: Vec<ParsedToolCall>,
    /// Cumulative token usage when the channel reports it.
    pub tokens: Option<(u64, u64)>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The model backend the loop talks to. `with_tools == false` on the
/// final forced iteration (no tool table — text answer required).
pub trait ModelChannel: Send + Sync {
    fn complete(
        &self,
        turns: &[ChatTurn],
        tools: &[ToolSpec],
        with_tools: bool,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ModelOutput>> + Send + '_>>;
}

/// The loop's final result.
pub struct AgentOutcome {
    pub reply: String,
    pub engine: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens: (u64, u64),
}

/// Run the agent loop. Emits step events (SSE + caller-side thinking
/// entries) through `on_step`. `base_turns` must already carry the
/// grounding system turn; the tool section is merged in for local
/// channels by the channel itself or here (see `LocalChannel`).
pub async fn run_agent(
    registry: &Arc<ToolRegistry>,
    channel: &dyn ModelChannel,
    base_turns: &[ChatTurn],
    engine_label: &str,
    max_steps: u32,
    step_timeout: std::time::Duration,
    mut on_step: impl FnMut(AgentStep),
) -> anyhow::Result<AgentOutcome> {
    let specs = registry.cached_specs();
    let mut turns: Vec<ChatTurn> = base_turns.to_vec();
    let mut executed: Vec<ToolCallRecord> = Vec::new();
    let mut tokens_acc = (0u64, 0u64);
    let steps = max_steps.max(1);

    for step in 0..steps {
        // The final iteration drops the tool table once tools were already
        // offered, forcing a text answer (a first-and-only call stays
        // tooled so one-shot tool answers work with max_steps=1).
        let final_forced = step + 1 >= steps && step > 0;
        let with_tools = !final_forced;

        on_step(AgentStep::PhaseThinking);
        let out = channel.complete(&turns, &specs, with_tools).await?;
        if let Some((p, c)) = out.tokens {
            tokens_acc.0 += p;
            tokens_acc.1 += c;
        }
        if out.tool_calls.is_empty() {
            let reply = out
                .text
                .ok_or_else(|| anyhow::anyhow!("model returned neither text nor tool calls"))?;
            on_step(AgentStep::PhaseAnswering {
                engine: engine_label.to_string(),
            });
            return Ok(AgentOutcome {
                reply,
                engine: engine_label.to_string(),
                tool_calls: executed,
                tokens: tokens_acc,
            });
        }
        if final_forced {
            // Tools were dropped but the model still "called" something —
            // prefer any raw text over looping; without text this falls
            // through to tool execution and the loop's honest error.
            if let Some(reply) = out.text.clone().filter(|s| !s.trim().is_empty()) {
                on_step(AgentStep::PhaseAnswering {
                    engine: engine_label.to_string(),
                });
                return Ok(AgentOutcome {
                    reply,
                    engine: engine_label.to_string(),
                    tool_calls: executed,
                    tokens: tokens_acc,
                });
            }
        }
        // Qwen3 template fidelity: the assistant turn that *issued* the
        // tool calls must be in the history before the <tool_response>
        // feedback, otherwise the observation is orphaned (the model
        // never "said" it wanted a tool).
        let mut assistant_turn = out.text.clone().unwrap_or_default();
        for call in &out.tool_calls {
            if !assistant_turn.is_empty() {
                assistant_turn.push('\n');
            }
            assistant_turn.push_str(&format!(
                "<tool_call>{{\"name\": {}, \"arguments\": {}}}</tool_call>",
                serde_json::to_string(&call.name).unwrap_or_default(),
                serde_json::to_string(&call.arguments).unwrap_or_default()
            ));
        }
        turns.push(ChatTurn {
            role: "assistant".into(),
            content: assistant_turn,
        });

        // Execute every requested call, feed observations back.
        for call in &out.tool_calls {
            on_step(AgentStep::ToolStarted {
                name: call.name.clone(),
                args: call.arguments.clone(),
            });
            let started = Instant::now();
            let result = registry
                .call(&call.name, call.arguments.clone(), step_timeout)
                .await;
            let duration_ms = started.elapsed().as_millis() as u64;
            let record = match result {
                Ok(output) => {
                    let text = truncate_for_model(&output.text);
                    feedback(&mut turns, &call.name, &text);
                    ToolCallRecord {
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        ok: true,
                        result: truncate_note(&text),
                        media_url: output.media_url,
                        duration_ms,
                    }
                }
                Err(e) => {
                    let msg = format!("工具执行失败：{e:#}");
                    feedback(&mut turns, &call.name, &msg);
                    ToolCallRecord {
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        ok: false,
                        result: truncate_note(&msg),
                        media_url: None,
                        duration_ms,
                    }
                }
            };
            on_step(AgentStep::ToolFinished {
                name: record.name.clone(),
                args: record.args.clone(),
                record: record.clone(),
            });
            executed.push(record);
        }
    }
    // Unreachable in practice (the last iteration has no tools and must
    // return text); kept as an honest error rather than a silent empty.
    Err(anyhow::anyhow!(
        "agent loop exhausted without a text answer"
    ))
}

/// Append the observation feedback as a Qwen3 `<tool_response>` user
/// turn — the unified format both channels consume (cloud models read
/// it as an ordinary user message; native `role:"tool"` structuring is a
/// documented follow-up).
fn feedback(turns: &mut Vec<ChatTurn>, name: &str, text: &str) {
    turns.push(ChatTurn {
        role: "user".into(),
        content: super::tool_feedback_text(name, text),
    });
}

// ─── Qwen3 local formatting/parsing ──────────────────────────────────

/// The `# Tools` system-section fragment per the Qwen chat template
/// convention: JSON tool definitions inside <tools>, instructions to
/// emit `<tool_call>`. Qwen3.5 moved the *call* dialect to the
/// Qwen3-Coder XML form (`<function=…><parameter=…>`); the instruction
/// shows that form, and [`parse_tool_calls`] accepts both it and the
/// older JSON body — a model trained on either dialect works.
pub fn qwen_tools_section(specs: &[ToolSpec]) -> String {
    // Prefill dominates CPU inference (~28 ms/token on a laptop-class
    // core), so the section is kept minimal: single-line JSON and a
    // terse instruction — the Qwen3 template convention still holds.
    let defs: Vec<serde_json::Value> = specs
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();
    format!(
        "\n\n# Tools\n\n如需外部信息或设备能力，可调用下列函数（签名见 <tools>）：\n<tools>\n{}\n</tools>\n\n调用时仅输出：\n<tool_call>\n<function=名称>\n<parameter=参数名>值</parameter>\n</function>\n</tool_call>",
        serde_json::to_string(&defs).unwrap_or_default()
    )
}

/// Parse one `<tool_call>` body in the Qwen3.5/Qwen3-Coder XML dialect:
/// `<function=name>…<parameter=key>value</parameter>…</function>`.
/// Tolerant of newlines and stray whitespace anywhere; a `function`
/// header without parameters yields an empty argument object.
fn parse_xml_tool_call(body: &str) -> Option<ParsedToolCall> {
    let start = body.find("<function=")?;
    let after = &body[start + "<function=".len()..];
    let name_end = after.find('>')?;
    let name = after[..name_end].trim();
    if name.is_empty() || name.contains('<') {
        return None;
    }
    let mut arguments = serde_json::Map::new();
    let mut rest = &after[name_end + 1..];
    while let Some(p) = rest.find("<parameter=") {
        let after_p = &rest[p + "<parameter=".len()..];
        let Some(k_end) = after_p.find('>') else {
            break;
        };
        let key = after_p[..k_end].trim().to_string();
        let Some(v_close) = after_p[k_end + 1..].find("</parameter>") else {
            break;
        };
        let value = after_p[k_end + 1..k_end + 1 + v_close].trim();
        if !key.is_empty() && !key.contains('<') {
            // Values arrive as raw text; keep them as JSON strings —
            // tool schemas here take stringly-typed inputs, and a
            // non-string would need the schema to disambiguate.
            arguments.insert(key, serde_json::Value::String(value.to_string()));
        }
        rest = &after_p[k_end + 1 + v_close + "</parameter>".len()..];
    }
    Some(ParsedToolCall {
        name: name.to_string(),
        arguments: serde_json::Value::Object(arguments),
    })
}

/// Parse one `<tool_call>` body in the Qwen3 JSON dialect:
/// `{"name":…,"arguments":{…}}`.
fn parse_json_tool_call(body: &str) -> Option<ParsedToolCall> {
    let v = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name.is_empty() {
        return None;
    }
    Some(ParsedToolCall {
        name: name.to_string(),
        arguments: v.get("arguments").cloned().unwrap_or(serde_json::json!({})),
    })
}

/// Extract `<tool_call>` blocks from a model reply. Two bodies are
/// accepted: the Qwen3 JSON form (`{"name":…,"arguments":{…}}`) and
/// the Qwen3.5/Qwen3-Coder XML form (`<function=…><parameter=…>…`).
/// Tolerant of surrounding prose/think blocks; malformed content
/// inside a marker is skipped (that call is lost, the rest still
/// parse). Empty output = no calls.
pub fn parse_tool_calls(output: &str) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();
    let mut rest = output;
    while let Some(start) = rest.find("<tool_call>") {
        let after = &rest[start + "<tool_call>".len()..];
        let Some(end) = after.find("</tool_call>") else {
            break; // unterminated marker — nothing more to parse
        };
        let body = after[..end].trim();
        let call = if body.contains("<function=") {
            // Qwen3.5 XML dialect first, JSON as a fallback so a
            // degenerate XML body carrying a JSON payload still parses.
            parse_xml_tool_call(body).or_else(|| parse_json_tool_call(body))
        } else {
            parse_json_tool_call(body)
        };
        if let Some(call) = call {
            calls.push(call);
        }
        rest = &after[end + "</tool_call>".len()..];
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_tools_section_lists_every_tool_as_function() {
        let specs = vec![ToolSpec {
            name: "weather.current".into(),
            description: "查天气".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            source: "builtin".into(),
        }];
        let s = qwen_tools_section(&specs);
        assert!(s.contains("# Tools"));
        assert!(s.contains("<tools>"));
        assert!(s.contains("\"weather.current\""));
        assert!(s.contains("<tool_call>"));
        assert!(s.contains("<function="));
    }

    #[test]
    fn parse_extracts_qwen35_xml_call_with_parameters() {
        // Qwen3.5 / Qwen3-Coder dialect as emitted raw (newlines and
        // all) — verified against field reports of llama.cpp issue
        // #20837 and the froggeric template write-ups.
        let out = "我查一下天气。\n<tool_call>\n<function=weather.current>\n<parameter=city>广州</parameter>\n</function>\n</tool_call>";
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "weather.current");
        assert_eq!(calls[0].arguments["city"], "广州");
    }

    #[test]
    fn parse_extracts_xml_call_without_parameters() {
        let out = "<tool_call><function=time.now></function></tool_call>";
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "time.now");
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn parse_accepts_mixed_json_and_xml_dialects() {
        // A Qwen3-4B turn (JSON) and a Qwen3.5 turn (XML) can both
        // appear when models are swapped mid-conversation history.
        let out = concat!(
            "<tool_call>{\"name\": \"time.now\", \"arguments\": {}}</tool_call>",
            "，还有：",
            "<tool_call>\n<function=weather.current>\n<parameter=city>广州</parameter>\n</function>\n</tool_call>",
        );
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "time.now");
        assert_eq!(calls[1].name, "weather.current");
        assert_eq!(calls[1].arguments["city"], "广州");
    }

    #[test]
    fn parse_skips_malformed_xml_but_keeps_later_calls() {
        let out = concat!(
            "<tool_call><function=broken name</function></tool_call>",
            "<tool_call><function=time.now></function></tool_call>",
        );
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "time.now");
    }

    #[test]
    fn parse_xml_multi_parameter_call() {
        let out = "<tool_call>\n<function=lamp.switch>\n<parameter=room>客厅</parameter>\n<parameter=on>true</parameter>\n</function>\n</tool_call>";
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "lamp.switch");
        assert_eq!(calls[0].arguments["room"], "客厅");
        assert_eq!(calls[0].arguments["on"], "true");
    }

    #[test]
    fn parse_extracts_single_call_with_arguments() {
        let out = "我需要查一下天气。\n<tool_call>\n{\"name\": \"weather.current\", \"arguments\": {\"city\": \"Guangzhou\"}}\n</tool_call>";
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "weather.current");
        assert_eq!(calls[0].arguments["city"], "Guangzhou");
    }

    #[test]
    fn parse_handles_multiple_and_skips_malformed() {
        let out = "<tool_call>{\"name\": \"a\", \"arguments\": {}}</tool_call> 中间说明文字 <tool_call>{broken json}</tool_call><tool_call>{\"name\":\"b\"}</tool_call>";
        let calls = parse_tool_calls(out);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].name, "b");
        assert!(calls[1].arguments.is_object());
    }

    #[test]
    fn parse_returns_empty_without_markers() {
        assert!(parse_tool_calls("今天天气很好，适合出行。").is_empty());
        assert!(parse_tool_calls("<tool_call>未闭合").is_empty());
        assert!(parse_tool_calls("").is_empty());
    }

    #[test]
    fn parse_tolerates_think_blocks_around_calls() {
        let out =
            "<think>用户问天气</think>\n<tool_call>{\"name\": \"weather.current\"}</tool_call>";
        assert_eq!(parse_tool_calls(out).len(), 1);
    }

    // ─── loop tests with a scripted channel + real registry ──────────

    use super::super::{AgentConfig, ToolRegistry};
    use crate::agent::runner::ParsedToolCall;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use streaming::llm::ChatTurn;

    /// Scripted channel: returns queued outputs in order.
    struct FakeChannel {
        outputs: StdMutex<Vec<ModelOutput>>,
        saw_tools_off: StdMutex<Vec<bool>>,
    }

    impl ModelChannel for FakeChannel {
        fn complete(
            &self,
            _turns: &[ChatTurn],
            _tools: &[ToolSpec],
            with_tools: bool,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<ModelOutput>> + Send + '_>> {
            self.saw_tools_off.lock().unwrap().push(with_tools);
            let next = self.outputs.lock().unwrap().remove(0);
            Box::pin(async move { Ok(next) })
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        {
            let r = Arc::new(ToolRegistry::new(
                Arc::new(std::sync::RwLock::new(
                    streaming::tools::ToolsConfig::default(),
                )),
                &AgentConfig::default(),
            ));
            r.attach_streams(Arc::new(crate::stream_manager::StreamManager::new()));
            r
        }
    }

    fn base_turns() -> Vec<ChatTurn> {
        vec![
            ChatTurn {
                role: "system".into(),
                content: "你是助手".into(),
            },
            ChatTurn {
                role: "user".into(),
                content: "现在几点".into(),
            },
        ]
    }

    #[tokio::test]
    async fn loop_executes_tool_then_answers_with_feedback_turns() {
        let channel = FakeChannel {
            outputs: StdMutex::new(vec![
                ModelOutput {
                    text: Some("我查一下。".into()),
                    tool_calls: vec![ParsedToolCall {
                        name: "time.now".into(),
                        arguments: serde_json::json!({}),
                    }],
                    tokens: Some((10, 5)),
                },
                ModelOutput {
                    text: Some("现在是下午。".into()),
                    tool_calls: vec![],
                    tokens: Some((30, 8)),
                },
            ]),
            saw_tools_off: StdMutex::new(vec![]),
        };
        let reg = registry();
        let mut steps = Vec::new();
        let out = run_agent(
            &reg,
            &channel,
            &base_turns(),
            "local",
            3,
            std::time::Duration::from_secs(5),
            |s| steps.push(format!("{s:?}")),
        )
        .await
        .unwrap();
        assert_eq!(out.reply, "现在是下午。");
        assert_eq!(out.engine, "local");
        assert_eq!(out.tokens, (40, 13));
        assert_eq!(out.tool_calls.len(), 1);
        assert_eq!(out.tool_calls[0].name, "time.now");
        assert!(out.tool_calls[0].ok);
        assert!(
            out.tool_calls[0].result.contains("星期"),
            "{}",
            out.tool_calls[0].result
        );
        // Steps: thinking → tool started → finished → thinking → answering.
        assert!(steps.iter().any(|s| s.contains("ToolStarted")));
        assert!(steps.iter().any(|s| s.contains("ToolFinished")));
        assert!(steps.iter().any(|s| s.contains("PhaseAnswering")));
        // Both iterations ran with tools (3-step budget, answered on 2nd).
        assert_eq!(*channel.saw_tools_off.lock().unwrap(), vec![true, true]);
    }

    #[tokio::test]
    async fn plain_text_first_reply_skips_tools_entirely() {
        let channel = FakeChannel {
            outputs: StdMutex::new(vec![ModelOutput {
                text: Some("直接回答".into()),
                tool_calls: vec![],
                tokens: None,
            }]),
            saw_tools_off: StdMutex::new(vec![]),
        };
        let out = run_agent(
            &registry(),
            &channel,
            &base_turns(),
            "cloud",
            3,
            std::time::Duration::from_secs(5),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(out.reply, "直接回答");
        assert!(out.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn failing_tool_feeds_honest_error_and_still_answers() {
        let channel = FakeChannel {
            outputs: StdMutex::new(vec![
                ModelOutput {
                    text: None,
                    tool_calls: vec![ParsedToolCall {
                        name: "no.such.tool".into(),
                        arguments: serde_json::json!({}),
                    }],
                    tokens: None,
                },
                ModelOutput {
                    text: Some("该工具不可用。".into()),
                    tool_calls: vec![],
                    tokens: None,
                },
            ]),
            saw_tools_off: StdMutex::new(vec![]),
        };
        let out = run_agent(
            &registry(),
            &channel,
            &base_turns(),
            "cloud",
            3,
            std::time::Duration::from_secs(5),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(out.reply, "该工具不可用。");
        assert_eq!(out.tool_calls.len(), 1);
        assert!(!out.tool_calls[0].ok);
        assert!(out.tool_calls[0].result.contains("unknown tool"));
    }

    #[tokio::test]
    async fn forced_final_iteration_drops_the_tool_table() {
        // The model never answers in text: three tool-call iterations hit
        // the budget; the last call must have gone out WITHOUT tools.
        let scripted: Vec<ModelOutput> = (0..3)
            .map(|_| ModelOutput {
                text: None,
                tool_calls: vec![ParsedToolCall {
                    name: "time.now".into(),
                    arguments: serde_json::json!({}),
                }],
                tokens: None,
            })
            .collect();
        let channel = FakeChannel {
            outputs: StdMutex::new(scripted),
            saw_tools_off: StdMutex::new(vec![]),
        };
        let result = run_agent(
            &registry(),
            &channel,
            &base_turns(),
            "cloud",
            3,
            std::time::Duration::from_secs(5),
            |_| {},
        )
        .await;
        // Tool-spam with no text anywhere ends in the honest error…
        assert!(result.is_err());
        // …and the third call dropped tools.
        assert_eq!(
            *channel.saw_tools_off.lock().unwrap(),
            vec![true, true, false]
        );
    }
}
