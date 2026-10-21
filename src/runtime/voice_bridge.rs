//! Voice interaction bridge — the former inline `main()` block, moved
//! verbatim (roadmap #6, M1 slice 2). One spawned task consumes
//! [`VoiceEngine`](streaming::voice::VoiceEngine) events: persistent hearing
//! records with scene correlation (SPEC appendix A #24 / #30-C), away voice
//! control (#44), the decision → agent/LLM → TTS answer pipeline with
//! interruption support (#45), the 120 s conversation session window (#30-B)
//! and dialogue-turn records (SPEC §3.4).
//!
//! Zero behavior change: the task body is untouched; the locals the block
//! captured are struct fields now.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

use streaming::decision::DecisionEngine;
use streaming::llm::ChatEngine;
use streaming::output::SegmentSlot;
use streaming::tts::TtsEngine;
use streaming::voice::{VoiceEngine, VoiceEvent};
use web::agent::{AgentConfig, ToolRegistry};
use web::away::AwayEngine;
use web::cloud::CloudAi;
use web::conversations::ConversationLog;
use web::grounding::GroundingState;
use web::routes::events::EventBus;
use web::server::SharedTools;

/// Everything the former inline block captured, built at the same point in
/// the startup sequence. [`VoiceBridge::spawn`] consumes it.
pub struct VoiceBridge {
    pub events: broadcast::Receiver<VoiceEvent>,
    pub bridge_tx: Arc<EventBus>,
    pub chat: Arc<ChatEngine>,
    pub cloud: Arc<CloudAi>,
    pub decision: Arc<DecisionEngine>,
    pub convlog: Arc<ConversationLog>,
    pub desktop: Arc<crate::desktop::Desktop>,
    pub grounding: Arc<GroundingState>,
    pub streams: Arc<std::sync::Mutex<HashMap<String, SegmentSlot>>>,
    pub tools: SharedTools,
    pub agent_registry: Arc<ToolRegistry>,
    pub agent_config: AgentConfig,
    pub wake_word: String,
    pub voice_engine: Arc<VoiceEngine>,
    pub away: Arc<AwayEngine>,
    pub tts: Arc<TtsEngine>,
    pub pool: sqlx::SqlitePool,
    pub interrupt_enabled: bool,
    pub interrupt_confidence: f32,
}

impl VoiceBridge {
    /// Spawn the bridge task. Field names destructure onto the original
    /// block-local names so the moved body compiles unchanged.
    pub fn spawn(self) {
        let VoiceBridge {
            events: mut voice_events,
            bridge_tx,
            chat: chat_for_voice,
            cloud: cloud_for_voice,
            decision: decision_for_voice,
            convlog: convlog_for_voice,
            desktop: desktop_for_voice,
            grounding: grounding_for_voice,
            streams: streams_for_voice,
            tools: tools_for_voice,
            agent_registry: agent_registry_for_voice,
            agent_config: agent_config_for_voice,
            wake_word: wake_word_for_voice,
            voice_engine: voice_engine_for_arming,
            away: away_for_voice,
            tts: tts_engine,
            pool: records_pool,
            interrupt_enabled,
            interrupt_confidence,
        } = self;
        let tts_for_away_cmd = Arc::clone(&tts_engine);
        // Conversation session (#30-B): the last 120 s of turns give
        // follow-ups ("再说详细点") their context. The slot also owns the
        // conversation trace (SPEC §3.3): all turns inside the window land
        // on one `origin:"voice"` trace; expiry closes it.
        type ConversationSlot = Arc<
            std::sync::Mutex<
                Option<(
                    std::time::Instant,
                    Vec<streaming::llm::ChatTurn>,
                    web::convtrace::ConvHandle,
                )>,
            >,
        >;
        let conversation: ConversationSlot = Arc::new(std::sync::Mutex::new(None));
        let grounding_voice = Arc::clone(&grounding_for_voice);
        // Active answer round (SPEC appendix A #45): at most one at a
        // time — this slot is what turns a transcript arriving mid-round
        // into an interruption decision instead of a racing second round.
        // `cancel` stops the streaming generation at the next token and
        // kills playback; `spoken_prefix` accumulates the sentences that
        // actually finished playing (the interrupted turn's honest
        // reply); `round_id` lets a finished round clear the slot only
        // when it still owns it.
        struct ActiveRound {
            cancel: Arc<std::sync::atomic::AtomicBool>,
            round_id: u64,
        }
        let active_round: Arc<std::sync::Mutex<Option<ActiveRound>>> =
            Arc::new(std::sync::Mutex::new(None));
        let round_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        tokio::spawn(async move {
            loop {
                match voice_events.recv().await {
                    Ok(ev) => {
                        // Persistent hearing record (SPEC appendix A #24)
                        // with the correlated scene (#30-C).
                        let camera = web::db::list_cameras(&records_pool)
                            .await
                            .ok()
                            .and_then(|cams| cams.first().map(|c| c.id.clone()));
                        let scene = camera
                            .as_deref()
                            .and_then(|id| grounding_voice.scene_summary(id, ev.timestamp_ms))
                            .unwrap_or_default();
                        let media_ref = camera
                            .as_deref()
                            .and_then(|id| {
                                web::stream_manager::StreamManager::segment_covering(
                                    &streams_for_voice,
                                    id,
                                    ev.timestamp_ms,
                                )
                            })
                            .unwrap_or_default();
                        if let Err(e) = web::db::insert_hearing_record(
                            &records_pool,
                            "voice",
                            &ev.transcript,
                            None,
                            &ev.keyword,
                            &ev.speaker,
                            &scene,
                            &media_ref,
                            ev.timestamp_ms as i64,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "hearing record: insert failed");
                        }
                        let _ = bridge_tx.send(web::routes::events::CameraEvent::VoiceTranscript {
                            keyword: ev.keyword,
                            transcript: ev.transcript.clone(),
                            speaker: ev.speaker.clone(),
                            scene: scene.clone(),
                            follow_up: ev.follow_up,
                            timestamp_ms: ev.timestamp_ms,
                        });
                        // Conversation trace (SPEC §3.3): join the 120 s
                        // slot's trace or start a fresh one; a stale slot
                        // closes its trace first.
                        let conv = {
                            let mut slot = conversation.lock().expect("conversation slot lock");
                            let fresh = slot
                                .as_ref()
                                .is_some_and(|(at, _, _)| at.elapsed().as_secs() < 120);
                            if !fresh && let Some((_, _, old)) = slot.take() {
                                old.close();
                            }
                            match slot.as_ref() {
                                Some((_, _, handle)) => {
                                    handle.add_turn();
                                    handle.clone()
                                }
                                None => {
                                    let handle = web::convtrace::convtrace().start("voice");
                                    handle.add_turn();
                                    *slot = Some((
                                        std::time::Instant::now(),
                                        Vec::new(),
                                        handle.clone(),
                                    ));
                                    handle
                                }
                            }
                        };
                        // Conversation record (SPEC §3.4): one turn per
                        // utterance — finished on every exit path below
                        // (reply, ignore decision, or failure). The draft
                        // collects the internal "thinking" entries as the
                        // legs run.
                        let mut draft = web::conversations::TurnDraft::new(
                            web::conversations::TurnOrigin::Voice,
                            conv.id(),
                        )
                        .user_text(ev.transcript.clone());
                        // Interruption gate (SPEC appendix A #45): a
                        // transcript while a round is running is an
                        // interruption candidate, not a second racing
                        // round. Stop words are rule-matched (0ms);
                        // everything else goes to the decision engine as
                        // supersede/backchannel with a conservative
                        // fail-open (ignore).
                        if interrupt_enabled
                            && active_round.lock().expect("active round lock").is_some()
                        {
                            if ev.transcript.trim().is_empty() {
                                // VAD noise blip mid-round — not worth a
                                // record; the round keeps thinking.
                                conv.close();
                                continue;
                            }
                            let interrupt_started = std::time::Instant::now();
                            let action = if streaming::decision::stop_words_match(&ev.transcript) {
                                streaming::decision::InterruptAction::Stop
                            } else {
                                let mut action = streaming::decision::InterruptAction::Backchannel;
                                if decision_for_voice.is_active() {
                                    let state_text = format!(
                                        "语音助手正在回答上一个问题时，用户又说：「{}」",
                                        ev.transcript
                                    );
                                    let engine = decision_for_voice.clone();
                                    if let Ok(d) = tokio::task::spawn_blocking(move || {
                                        engine.decide_choice(
                                            &state_text,
                                            "这句话是新的问题还是附和？",
                                            &[
                                                (
                                                    "supersede".into(),
                                                    "用户提出了新的问题或改变了话题，需要重新回答"
                                                        .into(),
                                                ),
                                                (
                                                    "backchannel".into(),
                                                    "附和、确认或无意义的声音（嗯、好、哦）".into(),
                                                ),
                                            ],
                                        )
                                    })
                                    .await
                                        && let Some(d) = d
                                    {
                                        action = streaming::decision::interrupt_policy(
                                            &d.label,
                                            d.confidence,
                                            interrupt_confidence,
                                        );
                                    }
                                }
                                action
                            };
                            match action {
                                streaming::decision::InterruptAction::Stop => {
                                    if let Some(active) =
                                        active_round.lock().expect("active round lock").take()
                                    {
                                        active
                                            .cancel
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    tracing::info!(
                                        transcript = %ev.transcript,
                                        "voice: round interrupted (stop word)"
                                    );
                                    convlog_for_voice
                                        .finish(draft.think(
                                            "interrupt",
                                            "rule",
                                            "停词 — 已取消当前回答",
                                            interrupt_started.elapsed().as_millis() as u64,
                                        ))
                                        .await;
                                    conv.close();
                                    continue;
                                }
                                streaming::decision::InterruptAction::Backchannel => {
                                    tracing::info!(
                                        transcript = %ev.transcript,
                                        "voice: mid-round utterance ignored (backchannel)"
                                    );
                                    // The VAD segment machinery re-arms the
                                    // follow-up window to now+2s on every
                                    // segment — restore the round-long
                                    // thinking window so one backchannel
                                    // blip does not end wake-free
                                    // listening for the rest of the round.
                                    voice_engine_for_arming.arm_listen(180_000);
                                    convlog_for_voice
                                        .finish(draft.think(
                                            "interrupt",
                                            "laya",
                                            "附和或低置信 — 忽略，当前回答继续",
                                            interrupt_started.elapsed().as_millis() as u64,
                                        ))
                                        .await;
                                    conv.close();
                                    continue;
                                }
                                streaming::decision::InterruptAction::Supersede => {
                                    if let Some(active) =
                                        active_round.lock().expect("active round lock").take()
                                    {
                                        active
                                            .cancel
                                            .store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                    tracing::info!(
                                        transcript = %ev.transcript,
                                        "voice: round superseded by a new question"
                                    );
                                    // Fall through: this utterance becomes
                                    // the new round below (the dying round
                                    // records itself as interrupted).
                                    draft = draft.think(
                                        "interrupt",
                                        "laya",
                                        "新问题 — 已取消上一轮回答",
                                        interrupt_started.elapsed().as_millis() as u64,
                                    );
                                }
                            }
                        }
                        // Away voice control (#44 amendment): "我要出门了"
                        // "我回来了" arm/disarm the watch directly — a
                        // built-in intent, answered without the LLM.
                        let away_cmd_started = std::time::Instant::now();
                        if let Some(want_active) = web::away::match_voice_command(&ev.transcript)
                            && away_for_voice.settings_snapshot().voice_control
                        {
                            let result = web::away::set_active(
                                &away_for_voice,
                                &records_pool,
                                &bridge_tx,
                                want_active,
                                Some(&chat_for_voice),
                            )
                            .await;
                            let (reply, note) = match &result {
                                Ok(None) if want_active => (
                                    "好的，离家模式已布防，路上注意安全。".to_string(),
                                    "已布防".to_string(),
                                ),
                                Ok(summary) => match summary {
                                    Some(s) => (
                                        format!("欢迎回家！离家模式已撤防。{s}"),
                                        format!("已撤防 · {s}"),
                                    ),
                                    None => (
                                        "欢迎回家！离家模式已撤防。".to_string(),
                                        "已撤防（无事件）".to_string(),
                                    ),
                                },
                                Err(reason) => (
                                    format!("抱歉，当前无法布防：{reason}"),
                                    format!("布防失败：{reason}"),
                                ),
                            };
                            let _ = bridge_tx.send(web::routes::events::CameraEvent::ChatReply {
                                source: "voice".into(),
                                reply: reply.clone(),
                                grounded: "none".into(),
                                timestamp_ms: unix_now_ms(),
                            });
                            if tts_for_away_cmd.is_active() {
                                voice_engine_for_arming.begin_playback_mute();
                                let tts = Arc::clone(&tts_for_away_cmd);
                                let text = reply.clone();
                                if let Err(e) =
                                    tokio::task::spawn_blocking(move || tts.speak(&text)).await
                                {
                                    tracing::warn!(error = %e, "away: command speak failed");
                                }
                                voice_engine_for_arming.end_playback_mute();
                            }
                            voice_engine_for_arming.arm_follow_up();
                            desktop_for_voice.notify_conversation(&reply);
                            convlog_for_voice
                                .finish(
                                    draft
                                        .think(
                                            "away.command",
                                            "builtin",
                                            note,
                                            away_cmd_started.elapsed().as_millis() as u64,
                                        )
                                        .reply(reply, "builtin"),
                                )
                                .await;
                            continue;
                        }
                        // Decision triage (SPEC appendix A #26): one Laya
                        // typed decision classifies the transcript before
                        // any local-LLM tokens are spent. `ignore` skips
                        // the auto-reply; inactive/low-confidence fails
                        // open to the previous behavior (always answer).
                        let mut skip_reply = false;
                        if decision_for_voice.is_active() && !ev.transcript.trim().is_empty() {
                            let state_text = format!("用户对家庭摄像头说：「{}」", ev.transcript);
                            let engine = decision_for_voice.clone();
                            let transcript_for_decision = ev.transcript.clone();
                            let tx_for_decision = bridge_tx.clone();
                            let ts = ev.timestamp_ms;
                            let mut decision_span =
                                conv.span("decision", "laya", "意图决策", vec![]);
                            let decision_started = std::time::Instant::now();
                            let decision = match decision_span
                                .run(tokio::task::spawn_blocking(move || {
                                    engine
                                        .decide_choice(
                                            &state_text,
                                            "这句话属于哪一类意图？",
                                            &[
                                                (
                                                    "answer".into(),
                                                    "用户在提问或聊天（普通话、粤语、英语都算），需要回答"
                                                        .into(),
                                                ),
                                                ("device".into(), "用户想控制设备或查询状态".into()),
                                                (
                                                    "ignore".into(),
                                                    "环境噪声、误唤醒或无意义内容".into(),
                                                ),
                                            ],
                                        )
                                        .map(|d| (d, transcript_for_decision, ts))
                                }))
                                .await
                            {
                                Ok(v) => {
                                    decision_span.finish_ok();
                                    v
                                }
                                Err(_) => {
                                    decision_span.finish_err();
                                    None
                                }
                            };
                            if let Some((d, transcript, ts)) = decision {
                                tracing::info!(
                                    choice = %d.label,
                                    confidence = d.confidence,
                                    "decision: voice triage"
                                );
                                skip_reply = streaming::decision::triage_skips_reply(
                                    &d.label,
                                    &ev.transcript,
                                );
                                let mut decision_note =
                                    format!("意图={}（置信度 {:.2}）", d.label, d.confidence);
                                if skip_reply {
                                    decision_note.push_str(" → 不回复");
                                }
                                draft = draft.think(
                                    "decision",
                                    "laya",
                                    decision_note,
                                    decision_started.elapsed().as_millis() as u64,
                                );
                                let _ = tx_for_decision.send(
                                    web::routes::events::CameraEvent::VoiceDecision {
                                        camera_id: "0".to_string(),
                                        transcript,
                                        choice: d.label.clone(),
                                        confidence: d.confidence,
                                        act_probability: d.act_probability,
                                        timestamp_ms: ts,
                                    },
                                );
                            }
                        }
                        if !skip_reply
                            && chat_for_voice.is_active()
                            && !ev.transcript.trim().is_empty()
                        {
                            // Scene grounding (SPEC #29) + local/web
                            // context (#30-A): same blocks the HTTP chat
                            // route injects — the spoken answer knows what
                            // the camera sees, the local clock, and (when
                            // asked + enabled) the weather, and follows the
                            // user's language.
                            let scene = web::db::list_cameras(&records_pool)
                                .await
                                .ok()
                                .and_then(|cams| cams.first().map(|c| c.id.clone()))
                                .and_then(|id| {
                                    grounding_for_voice.scene_summary(&id, unix_now_ms())
                                });
                            let ctx = web::routes::chat::TurnContext {
                                scene,
                                local: Some(web::routes::chat::local_block(&records_pool).await),
                                web: {
                                    let t = tools_for_voice
                                        .read()
                                        .expect("tools config lock poisoned")
                                        .clone();
                                    web::routes::chat::web_block(&t, &ev.transcript).await
                                },
                                wake_word: wake_word_for_voice.clone(),
                            };
                            let grounded =
                                if ctx.scene.is_some() { "scene" } else { "none" }.to_string();
                            // Session history: keep the last 8 turns if the
                            // conversation is younger than 120 s.
                            let history_turns = {
                                let slot = conversation.lock().expect("conversation slot lock");
                                slot.as_ref()
                                    .map_or_else(Vec::new, |(_, turns, _)| turns.clone())
                            };
                            let mut turns =
                                vec![web::routes::chat::build_system_turn(&ctx, &ev.transcript)];
                            turns.extend(history_turns.iter().take(8).cloned());
                            turns.push(streaming::llm::ChatTurn {
                                role: "user".into(),
                                content: ev.transcript.clone(),
                            });
                            let engine = chat_for_voice.clone();
                            let cloud = cloud_for_voice.clone();
                            let agent_registry = Arc::clone(&agent_registry_for_voice);
                            let agent_cfg = agent_config_for_voice.clone();
                            let agent_tx = bridge_tx.clone();
                            let tx = bridge_tx.clone();
                            let tts_for_reply = tts_engine.clone();
                            // Sentence-streaming pipeline (2026-10-12 P0):
                            // the answer block itself drives TTS for the
                            // local leg — clones captured by the answer
                            // future below (the wrapper keeps its own).
                            let tts_for_pipeline = tts_engine.clone();
                            let voice_for_pipeline = Arc::clone(&voice_engine_for_arming);
                            let grounded = grounded.clone();
                            let conversation_for_reply = Arc::clone(&conversation);
                            let voice_engine_for_reply = Arc::clone(&voice_engine_for_arming);
                            let user_turn = ev.transcript.clone();
                            let conv_for_answer = conv.clone();
                            let conv_for_reply = conv.clone();
                            let convlog_for_reply = Arc::clone(&convlog_for_voice);
                            let desktop_for_conversation = Arc::clone(&desktop_for_voice);
                            // Active-round slot (#45): this round owns the
                            // mic's thinking-phase wake-free window and the
                            // interrupt cancel flag; the wrapper clears the
                            // slot when it still holds round_id.
                            let round_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let spoken_prefix = Arc::new(std::sync::Mutex::new(String::new()));
                            let round_id =
                                round_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            {
                                let mut slot = active_round.lock().expect("active round lock");
                                *slot = Some(ActiveRound {
                                    cancel: Arc::clone(&round_cancel),
                                    round_id,
                                });
                            }
                            if interrupt_enabled {
                                // Wake-free listening for the whole round
                                // (thinking + speaking): VAD segments land
                                // in the #45 gate above. Replaced by the
                                // normal 12 s follow-up window when the
                                // wrapper finishes the round.
                                voice_engine_for_arming.arm_listen(180_000);
                            }
                            let active_round_for_reply = Arc::clone(&active_round);
                            let round_id_for_reply = round_id;
                            // Answer path (SPEC §4.10): the cloud answers
                            // first when configured — same persona/grounding
                            // system turn — and the local model is the
                            // fallback (or the only path when cloud is off).
                            // Each model call opens a conversation span
                            // (SPEC §3.3) — the fallback keeps the failed
                            // cloud span visible in the chain. The record
                            // draft rides along (SPEC §3.4): success
                            // returns it with the reply; failure returns it
                            // with the error thinking note appended.
                            let answer = async move {
                                let mut draft = draft;
                                // Agent tool loop (SPEC §3.5) — same
                                // fail-open contract as the HTTP route:
                                // any error falls through to the legacy
                                // single-shot legs below. Voice turns
                                // carry the (prefill-heavy) tool table
                                // only on tool-shaped utterances
                                // (`voice_tool_gate`) — chit-chat keeps
                                // the fast single-shot path.
                                let voice_tools = !agent_cfg.voice_tool_gate
                                    || streaming::tools::tool_intent(&ev.transcript);
                                if agent_registry.agent_ready(&agent_cfg) && voice_tools {
                                    let cloud_first = cloud.enabled();
                                    let engine_label = if cloud_first { "cloud" } else { "local" };
                                    let (think_source, model_id) = if cloud_first {
                                        ("cloud.chat", cloud.config().chat_model.clone())
                                    } else {
                                        ("llm", engine.model_variant())
                                    };
                                    let channel: Box<dyn web::agent::runner::ModelChannel> =
                                        if cloud_first {
                                            Box::new(web::agent::channels::CloudChannel {
                                                cloud: cloud.clone(),
                                                timeout_secs: cloud.config().timeout_secs,
                                            })
                                        } else {
                                            Box::new(web::agent::channels::LocalChannel {
                                                engine: engine.clone(),
                                            })
                                        };
                                    let conv_id = conv_for_answer.id().to_string();
                                    let agent_started = std::time::Instant::now();
                                    let tool_thinks =
                                        std::sync::Mutex::new(
                                            Vec::<(String, String, String, u64)>::new(),
                                        );
                                    let outcome = web::agent::runner::run_agent(
                                        &agent_registry,
                                        channel.as_ref(),
                                        &turns,
                                        engine_label,
                                        agent_cfg.max_steps,
                                        std::time::Duration::from_millis(
                                            agent_cfg.step_timeout_ms.max(1000),
                                        ),
                                        |step| {
                                            let event = match &step {
                                                web::agent::AgentStep::PhaseThinking => {
                                                    web::routes::events::CameraEvent::AgentStep {
                                                        conversation_id: conv_id.clone(),
                                                        kind: "phase".into(),
                                                        state: "thinking".into(),
                                                        tool: None,
                                                        args: None,
                                                        result: None,
                                                        duration_ms: None,
                                                        note: None,
                                                    }
                                                }
                                                web::agent::AgentStep::PhaseAnswering {
                                                    ..
                                                } => web::routes::events::CameraEvent::AgentStep {
                                                    conversation_id: conv_id.clone(),
                                                    kind: "phase".into(),
                                                    state: "answering".into(),
                                                    tool: None,
                                                    args: None,
                                                    result: None,
                                                    duration_ms: None,
                                                    note: None,
                                                },
                                                web::agent::AgentStep::ToolStarted {
                                                    name,
                                                    args,
                                                } => web::routes::events::CameraEvent::AgentStep {
                                                    conversation_id: conv_id.clone(),
                                                    kind: "tool".into(),
                                                    state: "running".into(),
                                                    tool: Some(name.clone()),
                                                    args: Some(args.clone()),
                                                    result: None,
                                                    duration_ms: None,
                                                    note: None,
                                                },
                                                web::agent::AgentStep::ToolFinished {
                                                    record,
                                                    ..
                                                } => web::routes::events::CameraEvent::AgentStep {
                                                    conversation_id: conv_id.clone(),
                                                    kind: "tool".into(),
                                                    state: if record.ok {
                                                        "done".into()
                                                    } else {
                                                        "error".into()
                                                    },
                                                    tool: Some(record.name.clone()),
                                                    args: Some(record.args.clone()),
                                                    result: Some(record.result.clone()),
                                                    duration_ms: Some(record.duration_ms),
                                                    note: None,
                                                },
                                            };
                                            let _ = agent_tx.send(event);
                                            if let web::agent::AgentStep::ToolFinished {
                                                record,
                                                ..
                                            } = step
                                            {
                                                tool_thinks
                                                    .lock()
                                                    .expect("agent thinks lock")
                                                    .push((
                                                        "tool".to_string(),
                                                        record.name.clone(),
                                                        record.result.clone(),
                                                        record.duration_ms,
                                                    ));
                                            }
                                        },
                                    )
                                    .await;
                                    match outcome {
                                        Ok(out) => {
                                            let dur = agent_started.elapsed().as_millis() as u64;
                                            for (source, model, note, ms) in std::mem::take(
                                                &mut *tool_thinks
                                                    .lock()
                                                    .expect("agent thinks lock"),
                                            ) {
                                                draft = draft.think(&source, &model, note, ms);
                                            }
                                            let note = if out.tokens.0 > 0 || out.tokens.1 > 0 {
                                                format!(
                                                    "agent 应答 · prompt {}↑ / completion {}↓ tok（含 {} 次工具调用）",
                                                    out.tokens.0,
                                                    out.tokens.1,
                                                    out.tool_calls.len()
                                                )
                                            } else {
                                                format!(
                                                    "agent 应答（含 {} 次工具调用）",
                                                    out.tool_calls.len()
                                                )
                                            };
                                            draft = draft.think(think_source, &model_id, note, dur);
                                            return Ok((
                                                draft,
                                                Some(out.reply),
                                                engine_label,
                                                false,
                                                false,
                                            ));
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "voice: agent loop failed — falling back to legacy legs");
                                        }
                                    }
                                }
                                if cloud.enabled() {
                                    let model_id = cloud.config().chat_model.clone();
                                    let mut span = conv_for_answer.span(
                                        "cloud.chat",
                                        &model_id,
                                        "云端应答",
                                        vec![],
                                    );
                                    let leg_start = std::time::Instant::now();
                                    match span
                                        .run(cloud.complete_with_usage(&turns, None, None, None))
                                        .await
                                    {
                                        Ok((reply, usage)) => {
                                            let dur = leg_start.elapsed().as_millis() as u64;
                                            span.tokens(usage.map(|u| u.0), usage.map(|u| u.1))
                                                .finish_ok();
                                            let note = match usage {
                                                Some((p, c)) => {
                                                    format!("云端应答 · {p}↑/{c}↓ tok")
                                                }
                                                None => "云端应答".to_string(),
                                            };
                                            draft = draft.think("cloud.chat", &model_id, note, dur);
                                            return Ok((draft, Some(reply), "cloud", false, false));
                                        }
                                        Err(e) => {
                                            let dur = leg_start.elapsed().as_millis() as u64;
                                            span.finish_err();
                                            tracing::warn!(error = %e, "cloud: voice reply failed — falling back to local llm");
                                            if !cloud.config().fallback_local {
                                                draft = draft.think(
                                                    "cloud.chat",
                                                    &model_id,
                                                    format!("云端失败：{e}（不回落）"),
                                                    dur,
                                                );
                                                return Err(draft);
                                            }
                                            draft = draft.think(
                                                "cloud.chat",
                                                &model_id,
                                                format!("云端失败：{e} — 回落本地"),
                                                dur,
                                            );
                                        }
                                    }
                                }
                                let variant = engine.model_variant();
                                let mut span =
                                    conv_for_answer.span("llm", &variant, "本地应答", vec![]);
                                let engine = engine.clone();
                                let turns = turns.clone();
                                let leg_start = std::time::Instant::now();
                                // Sentence-streaming pipeline (2026-10-12
                                // P0): with TTS live, the local leg
                                // streams generation deltas into a speaker
                                // task that synthesizes/plays per sentence
                                // and keeps ONE synthesis ahead of
                                // playback — first audio lands after the
                                // FIRST sentence instead of after the
                                // whole reply, and the playback self-mute
                                // latches only around actual playback
                                // (synthesis is silent), roughly halving
                                // the deaf window. `spoken` tells the
                                // wrapper this leg already spoke.
                                let tts_stream = tts_for_pipeline.is_active();
                                let mut generator = None;
                                let mut speaker: Option<
                                    tokio::task::JoinHandle<(usize, std::time::Duration)>,
                                > = None;
                                if tts_stream {
                                    let (tx_s, mut rx_s) = tokio::sync::mpsc::channel::<String>(8);
                                    let cancel_gen = Arc::clone(&round_cancel);
                                    generator = Some(tokio::task::spawn_blocking({
                                        let engine = engine.clone();
                                        let turns = turns.clone();
                                        let tx_s = tx_s.clone();
                                        move || {
                                            engine.complete_streaming_with_usage(
                                                &turns,
                                                Some(&mut |delta: &str| {
                                                    // #45: a set cancel flag
                                                    // stops generation at the
                                                    // next token; the partial
                                                    // reply is kept.
                                                    if cancel_gen
                                                        .load(std::sync::atomic::Ordering::SeqCst)
                                                    {
                                                        return false;
                                                    }
                                                    tx_s.blocking_send(delta.to_string()).is_ok()
                                                }),
                                            )
                                        }
                                    }));
                                    let cancel_speak = Arc::clone(&round_cancel);
                                    let prefix_cell = Arc::clone(&spoken_prefix);
                                    speaker = Some(tokio::spawn({
                                        let tts = Arc::clone(&tts_for_pipeline);
                                        let voice = Arc::clone(&voice_for_pipeline);
                                        let conv = conv_for_answer.clone();
                                        async move {
                                            let mut filter =
                                                streaming::sentence::ThinkFilter::new();
                                            let mut splitter =
                                                streaming::sentence::SentenceSplitter::new();
                                            let speak_span = conv.span(
                                                "tts",
                                                "vits",
                                                "语音播报（流水线）",
                                                vec![],
                                            );
                                            let started = std::time::Instant::now();
                                            let mut first = true;
                                            let mut sentences = 0_usize;
                                            let mut synth_ahead: Option<(
                                                String,
                                                tokio::task::JoinHandle<
                                                    anyhow::Result<std::path::PathBuf>,
                                                >,
                                            )> = None;
                                            while let Some(delta) = rx_s.recv().await {
                                                if cancel_speak
                                                    .load(std::sync::atomic::Ordering::SeqCst)
                                                {
                                                    break;
                                                }
                                                let visible = filter.push(&delta);
                                                for sentence in splitter.push(&visible) {
                                                    let t = Arc::clone(&tts);
                                                    let synth_text = sentence.clone();
                                                    let next =
                                                        tokio::task::spawn_blocking(move || {
                                                            t.synthesize(&synth_text)
                                                        });
                                                    if let Some((played, handle)) =
                                                        synth_ahead.take()
                                                    {
                                                        speak_pending_sentence(
                                                            &tts,
                                                            &voice,
                                                            handle,
                                                            &played,
                                                            &mut first,
                                                            &mut sentences,
                                                            &cancel_speak,
                                                            &prefix_cell,
                                                        )
                                                        .await;
                                                    }
                                                    synth_ahead = Some((sentence, next));
                                                }
                                            }
                                            if !cancel_speak
                                                .load(std::sync::atomic::Ordering::SeqCst)
                                            {
                                                if let Some(tail) = splitter.flush() {
                                                    let t = Arc::clone(&tts);
                                                    let synth_text = tail.clone();
                                                    let next =
                                                        tokio::task::spawn_blocking(move || {
                                                            t.synthesize(&synth_text)
                                                        });
                                                    if let Some((played, handle)) =
                                                        synth_ahead.take()
                                                    {
                                                        speak_pending_sentence(
                                                            &tts,
                                                            &voice,
                                                            handle,
                                                            &played,
                                                            &mut first,
                                                            &mut sentences,
                                                            &cancel_speak,
                                                            &prefix_cell,
                                                        )
                                                        .await;
                                                    }
                                                    synth_ahead = Some((tail, next));
                                                }
                                                if let Some((played, handle)) = synth_ahead.take() {
                                                    speak_pending_sentence(
                                                        &tts,
                                                        &voice,
                                                        handle,
                                                        &played,
                                                        &mut first,
                                                        &mut sentences,
                                                        &cancel_speak,
                                                        &prefix_cell,
                                                    )
                                                    .await;
                                                }
                                            }
                                            if !first {
                                                voice.end_playback_mute();
                                            }
                                            if sentences > 0 {
                                                speak_span.finish_ok();
                                            } else {
                                                speak_span.finish_err();
                                            }
                                            (sentences, started.elapsed())
                                        }
                                    }));
                                }
                                let gen_result = match generator {
                                    Some(g) => span.run(g).await,
                                    None => {
                                        let e = engine.clone();
                                        let t = turns.clone();
                                        span.run(tokio::task::spawn_blocking(move || {
                                            e.complete_with_usage(&t)
                                        }))
                                        .await
                                    }
                                };
                                // Finish the generation span/elapsed BEFORE
                                // awaiting the speaker — the LLM thinking
                                // entry must not absorb playback time (the
                                // pipeline overlaps the two by design).
                                let gen_elapsed_ms = leg_start.elapsed().as_millis() as u64;
                                match &gen_result {
                                    Ok(Ok((_, p, c))) => {
                                        span.tokens(Some(*p), Some(*c)).finish_ok();
                                    }
                                    _ => {
                                        span.finish_err();
                                    }
                                }
                                let mut spoken = 0_usize;
                                let mut speak_ms = 0_u64;
                                if let Some(sp) = speaker {
                                    match sp.await {
                                        Ok((n, dur)) => {
                                            spoken = n;
                                            speak_ms = dur.as_millis() as u64;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                "voice: tts speaker task failed"
                                            );
                                        }
                                    }
                                }
                                match gen_result {
                                    Ok(Ok((reply, prompt_tokens, completion_tokens))) => {
                                        draft = draft.think(
                                            "llm",
                                            &variant,
                                            format!(
                                                "本地应答 · prompt {prompt_tokens}↑ / completion {completion_tokens}↓ tok"
                                            ),
                                            gen_elapsed_ms,
                                        );
                                        if spoken > 0 {
                                            draft = draft.think(
                                                "tts",
                                                "vits",
                                                format!("已播报 · {spoken} 句（流水线）"),
                                                speak_ms,
                                            );
                                        }
                                        // #45: a cancelled round reports
                                        // the actually-spoken prefix (None
                                        // when nothing got out) plus the
                                        // interrupted marker.
                                        let cancelled =
                                            round_cancel.load(std::sync::atomic::Ordering::SeqCst);
                                        let final_reply = if cancelled {
                                            let prefix = spoken_prefix
                                                .lock()
                                                .expect("spoken prefix lock")
                                                .trim()
                                                .to_string();
                                            if prefix.is_empty() {
                                                None
                                            } else {
                                                Some(prefix)
                                            }
                                        } else {
                                            Some(reply)
                                        };
                                        if cancelled {
                                            draft = draft.think(
                                                "interrupt",
                                                "rule",
                                                "回答被打断 — 仅保留已播报部分",
                                                0,
                                            );
                                        }
                                        Ok((draft, final_reply, "local", spoken > 0, cancelled))
                                    }
                                    Ok(Err(e)) => {
                                        draft = draft.think(
                                            "llm",
                                            &variant,
                                            format!("本地应答失败：{e}"),
                                            gen_elapsed_ms,
                                        );
                                        Err(draft)
                                    }
                                    Err(e) => {
                                        draft = draft.think(
                                            "llm",
                                            &variant,
                                            format!("本地应答任务失败：{e}"),
                                            gen_elapsed_ms,
                                        );
                                        Err(draft)
                                    }
                                }
                            };
                            tokio::spawn(async move {
                                let outcome = answer.await;
                                // Release the #45 active-round slot when this
                                // round still owns it (a superseding round
                                // already took it otherwise) — only AFTER the
                                // round finished: clearing earlier would
                                // empty the slot mid-round and deafen the
                                // interrupt gate.
                                {
                                    let mut slot =
                                        active_round_for_reply.lock().expect("active round lock");
                                    if slot
                                        .as_ref()
                                        .is_some_and(|r| r.round_id == round_id_for_reply)
                                    {
                                        slot.take();
                                    }
                                }
                                match outcome {
                                    Ok((mut draft, reply, engine_label, spoken, cancelled)) => {
                                        // Extend the conversation session with
                                        // this Q/A pair (trimmed to the last 4
                                        // pairs = 8 turns). An interrupted
                                        // round contributes its SPOKEN prefix
                                        // — the model's unspoken tail never
                                        // happened as far as the dialogue is
                                        // concerned.
                                        {
                                            let mut slot = conversation_for_reply
                                                .lock()
                                                .expect("conversation slot lock");
                                            let entry = slot.get_or_insert_with(|| {
                                                (
                                                    std::time::Instant::now(),
                                                    Vec::new(),
                                                    conv_for_reply.clone(),
                                                )
                                            });
                                            entry.0 = std::time::Instant::now();
                                            entry.1.push(streaming::llm::ChatTurn {
                                                role: "user".into(),
                                                content: user_turn,
                                            });
                                            if let Some(reply) = &reply {
                                                entry.1.push(streaming::llm::ChatTurn {
                                                    role: "assistant".into(),
                                                    content: reply.clone(),
                                                });
                                            }
                                            let keep = entry.1.len().saturating_sub(8);
                                            entry.1.drain(..keep);
                                        }
                                        if let Some(reply) = &reply {
                                            let _ = tx.send(
                                                web::routes::events::CameraEvent::ChatReply {
                                                    source: "voice".into(),
                                                    reply: reply.clone(),
                                                    grounded,
                                                    timestamp_ms: unix_now_ms(),
                                                },
                                            );
                                        }
                                        // Playback self-mute: while this reply
                                        // is on the speaker the wake-word
                                        // worker stays deaf — a spoken 小蜜蜂
                                        // in the reply would re-wake the
                                        // device into a self-talk loop.
                                        // The sentence-streaming local leg
                                        // already spoke (and managed the
                                        // mute itself); this one-shot path
                                        // covers the cloud/agent legs.
                                        if tts_for_reply.is_active() && !spoken && !cancelled {
                                            // One-shot path (cloud/agent legs);
                                            // `reply` is Some here — the local
                                            // pipeline leg set spoken=true or
                                            // was cancelled into None.
                                            let reply = reply.clone().unwrap_or_default();
                                            let tts_model = match streaming::lang::detect(&reply) {
                                                streaming::lang::SpokenLang::Cantonese => "tts.yue",
                                                streaming::lang::SpokenLang::English => "tts.en",
                                                streaming::lang::SpokenLang::Mandarin => "tts.zh",
                                            };
                                            let mut span = conv_for_reply.span(
                                                tts_model,
                                                "vits",
                                                "语音播报",
                                                vec![],
                                            );
                                            voice_engine_for_reply.begin_playback_mute();
                                            let tts_started = std::time::Instant::now();
                                            match span
                                                .run(async { tts_for_reply.speak(&reply) })
                                                .await
                                            {
                                                Ok(_) => {
                                                    span.finish_ok();
                                                    draft = draft.think(
                                                        tts_model,
                                                        "vits",
                                                        "已播报",
                                                        tts_started.elapsed().as_millis() as u64,
                                                    );
                                                }
                                                Err(e) => {
                                                    span.finish_err();
                                                    tracing::warn!(error = %e, "tts: speak failed");
                                                    draft = draft.think(
                                                        tts_model,
                                                        "vits",
                                                        format!("播报失败：{e}"),
                                                        tts_started.elapsed().as_millis() as u64,
                                                    );
                                                }
                                            }
                                            voice_engine_for_reply.end_playback_mute();
                                        }
                                        // Continuous dialogue (#30-B): arm the
                                        // VAD follow-up window only after the
                                        // reply finished playing — earlier would
                                        // let the mic capture our own TTS. Also
                                        // replaces the round-long #45 listening
                                        // window with the normal 12 s one.
                                        voice_engine_for_reply.arm_follow_up();
                                        // Desktop notification for the finished
                                        // reply (opt-in, SPEC appendix A #42).
                                        if let Some(reply) = &reply {
                                            desktop_for_conversation.notify_conversation(reply);
                                        }
                                        // Record the completed turn (SPEC §3.4)
                                        // — persist + SSE `conversation`.
                                        // Interrupted turns carry the spoken
                                        // prefix (or no reply at all) plus the
                                        // interrupted marker (#45).
                                        let mut draft = match &reply {
                                            Some(r) => draft.reply(r.clone(), engine_label),
                                            None => draft,
                                        };
                                        if cancelled {
                                            draft = draft.interrupted();
                                        }
                                        convlog_for_reply.finish(draft).await;
                                    }
                                    Err(draft) => {
                                        tracing::warn!("voice auto-reply failed");
                                        conv_for_reply.close();
                                        // The failed legs are already thinking
                                        // notes on the draft — record the
                                        // no-reply turn (SPEC §3.4).
                                        convlog_for_reply.finish(draft).await;
                                    }
                                }
                            });
                        } else {
                            // No reply path ran (decision said ignore, or
                            // the chat engine is inactive): the trace — at
                            // most a decision span — ends here. The record
                            // keeps the decision note — no-reply turns are
                            // honestly logged (SPEC §3.4).
                            convlog_for_voice.finish(draft).await;
                            conv.close();
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "voice: SSE bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
/// Voice pipeline P0 (2026-10-12): finish one in-flight sentence
/// synthesis — wait for its WAV, latch the playback self-mute on the
/// FIRST actual playback (synthesis is silent and stays audible-to-KWS),
/// then play it to completion. The next sentence's synthesis runs
/// concurrently (the caller started it before awaiting this).
#[allow(clippy::too_many_arguments)]
async fn speak_pending_sentence(
    tts: &Arc<streaming::tts::TtsEngine>,
    voice: &Arc<streaming::voice::VoiceEngine>,
    pending: tokio::task::JoinHandle<anyhow::Result<std::path::PathBuf>>,
    sentence: &str,
    first: &mut bool,
    sentences: &mut usize,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    spoken_prefix: &Arc<std::sync::Mutex<String>>,
) {
    let Ok(Ok(wav)) = pending.await else {
        return;
    };
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if *first {
        voice.begin_playback_mute();
        *first = false;
    }
    // #45: poll the player so an interruption kills it mid-sentence.
    let played = match tts.spawn_play(&wav) {
        Ok(Some(mut player)) => loop {
            match player.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) => {
                    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        player.kill();
                        break false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                // `PlayerProcess` kills on drop, so an error here (or a
                // cancellation of this future mid-sleep) cannot leave the
                // speaker talking after the self-mute lifts.
                Err(_) => break false,
            }
        },
        // No player configured: "played" by definition (selftest mode).
        Ok(None) => true,
        Err(e) => {
            tracing::warn!(error = %e, "tts: play spawn failed");
            false
        }
    };
    if played {
        *sentences += 1;
        let mut prefix = spoken_prefix.lock().expect("spoken prefix lock");
        prefix.push_str(sentence);
        prefix.push(' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration harness: the real bridge loop with fail-open (inactive)
    /// engines and an in-memory database, fed real [`VoiceEvent`]s through
    /// a broadcast channel. Covers the record/SSE/session/away-command
    /// paths that the spawn-time wiring owns — the answer pipeline's
    /// active-engine branches stay to the live rig.
    struct Harness {
        tx: broadcast::Sender<VoiceEvent>,
        #[allow(dead_code)]
        bus_tx: Arc<EventBus>,
        bus_rx: broadcast::Receiver<web::routes::events::CameraEvent>,
        pool: sqlx::SqlitePool,
    }

    async fn harness() -> Harness {
        // A temp-file pool: init_pool runs the real migrations and sizes
        // the pool for concurrency (an in-memory DB would give every
        // connection its own empty database).
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mibee-voice-bridge-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let pool = web::db::init_pool(path.to_str().unwrap())
            .await
            .expect("test pool + migrations");
        let bus_tx = Arc::new(web::routes::events::new_event_bus());
        let (tx, events_rx) = broadcast::channel(64);
        // An available away engine: arming is gated on an active AI
        // detector, so the harness injects a fake one (the real models are
        // deployment assets CI does not have).
        struct FakeDetector;
        impl streaming::ai::AiDetector for FakeDetector {
            fn detect(&self, _jpeg: &[u8]) -> anyhow::Result<Vec<streaming::ai::Detection>> {
                Ok(vec![])
            }
            fn model_name(&self) -> &str {
                "fake"
            }
        }
        let away = Arc::new(web::away::AwayEngine::new(
            web::away::AwayConfig::default(),
            Arc::new(streaming::ai::AiEngine::from_parts(
                streaming::ai::AiConfig::default(),
                Some(Arc::new(FakeDetector)),
            )),
            false,
        ));
        VoiceBridge {
            events: events_rx,
            bridge_tx: Arc::clone(&bus_tx),
            chat: Arc::new(ChatEngine::from_config(
                &streaming::llm::LlmConfig::default(),
            )),
            cloud: Arc::new(CloudAi::new(web::cloud::CloudConfig::default())),
            decision: Arc::new(DecisionEngine::from_config(
                &streaming::decision::DecisionConfig::default(),
            )),
            convlog: Arc::new(web::conversations::ConversationLog::new(
                pool.clone(),
                Arc::clone(&bus_tx),
                true,
            )),
            desktop: crate::desktop::Desktop::new(&crate::config::DesktopConfig::default()),
            grounding: Arc::new(web::grounding::GroundingState::new()),
            streams: Arc::new(std::sync::Mutex::new(HashMap::new())),
            tools: Arc::new(std::sync::RwLock::new(
                streaming::tools::ToolsConfig::default(),
            )),
            agent_registry: web::server::AppRouterState::agent_for_tests().0,
            agent_config: web::agent::AgentConfig::default(),
            wake_word: streaming::voice::DEFAULT_WAKE_WORD.to_string(),
            voice_engine: Arc::new(VoiceEngine::from_config(
                &streaming::voice::VoiceConfig::default(),
            )),
            away: Arc::clone(&away),
            tts: Arc::new(TtsEngine::from_config(&streaming::tts::TtsConfig::default())),
            pool: pool.clone(),
            interrupt_enabled: true,
            interrupt_confidence: 0.6,
        }
        .spawn();
        let bus_rx = bus_tx.subscribe();
        Harness {
            tx,
            bus_tx,
            bus_rx,
            pool,
        }
    }

    fn transcript_event(text: &str) -> VoiceEvent {
        VoiceEvent {
            keyword: streaming::voice::DEFAULT_WAKE_WORD.to_string(),
            transcript: text.to_string(),
            timestamp_ms: 1_700_000_000_000,
            speaker: "米基".to_string(),
            follow_up: false,
        }
    }

    async fn drain_until<T>(
        rx: &mut broadcast::Receiver<web::routes::events::CameraEvent>,
        deadline_ms: u64,
        pred: impl Fn(&web::routes::events::CameraEvent) -> Option<T>,
    ) -> Option<T> {
        let start = std::time::Instant::now();
        loop {
            if start.elapsed().as_millis() as u64 > deadline_ms {
                return None;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(deadline_ms), rx.recv())
                .await
            {
                Ok(Ok(ev)) => {
                    if let Some(v) = pred(&ev) {
                        return Some(v);
                    }
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn transcript_persists_hearing_record_and_broadcasts_sse() {
        let mut h = harness().await;
        h.tx.send(transcript_event("现在几点了")).expect("send");

        // SSE: the transcript reaches the wire with keyword + speaker.
        let seen = drain_until(&mut h.bus_rx, 5_000, |ev| match ev {
            web::routes::events::CameraEvent::VoiceTranscript {
                transcript,
                speaker,
                ..
            } if transcript == "现在几点了" => Some(speaker.clone()),
            _ => None,
        })
        .await;
        assert_eq!(seen.as_deref(), Some("米基"));

        // DB: the hearing record landed with kind=voice and the text.
        let mut rows = None;
        for _ in 0..50 {
            rows = web::db::list_hearing_records(&h.pool, 5, Some("voice"))
                .await
                .ok();
            if rows.as_ref().is_some_and(|r| !r.is_empty()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let rows = rows.expect("query ok");
        assert!(
            rows.iter()
                .any(|r| r.text == "现在几点了" && r.keyword == "小蜜蜂"),
            "{rows:?}"
        );
    }

    #[tokio::test]
    async fn transcript_without_engines_still_records_the_turn() {
        let h = harness().await;
        h.tx.send(transcript_event("随便聊聊")).expect("send");
        // The no-engine path finishes the draft (user text, no reply) —
        // the honest "why nothing answered" record.
        let mut turns = Vec::new();
        for _ in 0..60 {
            if let Ok(t) = web::db::list_conversation_turns(&h.pool, 10).await
                && !t.is_empty()
            {
                turns = t;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            turns
                .iter()
                .any(|t| t.user_text.as_deref() == Some("随便聊聊")),
            "{turns:?}"
        );
    }

    #[tokio::test]
    async fn away_voice_command_arms_and_answers_by_voice() {
        let mut h = harness().await;
        h.tx.send(transcript_event("我要出门了")).expect("send");

        // The away engine flips to armed and announces the state change.
        let armed = drain_until(&mut h.bus_rx, 5_000, |ev| match ev {
            web::routes::events::CameraEvent::AwayState { active, .. } => Some(*active),
            _ => None,
        })
        .await;
        assert_eq!(armed, Some(true), "voice arm/disarm path fired");
    }

    #[test]
    fn unix_now_ms_is_monotonic_nonzero() {
        let a = unix_now_ms();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = unix_now_ms();
        assert!(b >= a && b > 0, "a={a} b={b}");
    }

    #[allow(clippy::too_many_arguments)]
    async fn speak(
        tts: &Arc<TtsEngine>,
        voice: &Arc<VoiceEngine>,
        pending: tokio::task::JoinHandle<anyhow::Result<std::path::PathBuf>>,
        sentence: &str,
        first: &mut bool,
        sentences: &mut usize,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
        spoken_prefix: &Arc<std::sync::Mutex<String>>,
    ) {
        speak_pending_sentence(
            tts,
            voice,
            pending,
            sentence,
            first,
            sentences,
            cancel,
            spoken_prefix,
        )
        .await
    }

    #[tokio::test]
    async fn speak_pending_sentence_counts_played_sentence_no_player() {
        // Player pinned to "" — TtsConfig::default() ships "aplay -q", which
        // exists on dev rigs but not CI runners, so the default would make
        // this test environment-dependent. With no player, spawn_play
        // returns Ok(None) — "played" by definition — and the sentence
        // lands in the spoken prefix (the interrupted turn's honest reply,
        // SPEC appendix A #45).
        let tts_cfg = streaming::tts::TtsConfig {
            player: String::new(),
            ..std::default::Default::default()
        };
        let tts = Arc::new(TtsEngine::from_config(&tts_cfg));
        let voice = Arc::new(VoiceEngine::from_config(
            &streaming::voice::VoiceConfig::default(),
        ));
        let pending = tokio::spawn(async { Ok(std::path::PathBuf::from("/tmp/x.wav")) });
        let mut first = true;
        let mut sentences = 0;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prefix = Arc::new(std::sync::Mutex::new(String::new()));
        speak(
            &tts,
            &voice,
            pending,
            "你好。",
            &mut first,
            &mut sentences,
            &cancel,
            &prefix,
        )
        .await;
        assert_eq!(sentences, 1);
        assert!(!first, "first-playback self-mute latch consumed");
        assert_eq!(*prefix.lock().unwrap(), "你好。 ");
    }

    #[tokio::test]
    async fn speak_pending_sentence_cancelled_before_playback_is_dropped() {
        let tts = Arc::new(TtsEngine::from_config(&streaming::tts::TtsConfig::default()));
        let voice = Arc::new(VoiceEngine::from_config(
            &streaming::voice::VoiceConfig::default(),
        ));
        let pending = tokio::spawn(async { Ok(std::path::PathBuf::from("/tmp/x.wav")) });
        let mut first = true;
        let mut sentences = 0;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let prefix = Arc::new(std::sync::Mutex::new(String::new()));
        speak(
            &tts,
            &voice,
            pending,
            "你好。",
            &mut first,
            &mut sentences,
            &cancel,
            &prefix,
        )
        .await;
        assert_eq!(sentences, 0, "cancelled sentence must not count");
        assert!(first, "self-mute latch untouched when cancelled");
        assert_eq!(*prefix.lock().unwrap(), "");
    }

    #[tokio::test]
    async fn speak_pending_sentence_failed_synthesis_is_skipped() {
        let tts = Arc::new(TtsEngine::from_config(&streaming::tts::TtsConfig::default()));
        let voice = Arc::new(VoiceEngine::from_config(
            &streaming::voice::VoiceConfig::default(),
        ));
        let pending = tokio::spawn(async { Err(anyhow::anyhow!("synth failed")) });
        let mut first = true;
        let mut sentences = 0;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let prefix = Arc::new(std::sync::Mutex::new(String::new()));
        speak(
            &tts,
            &voice,
            pending,
            "你好。",
            &mut first,
            &mut sentences,
            &cancel,
            &prefix,
        )
        .await;
        assert_eq!(sentences, 0, "failed synthesis must not count");
        assert!(first);
    }
}
