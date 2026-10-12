//! Conversation records (SPEC v1 §3.4): the human-readable log of every
//! dialogue turn — what the user said (HTTP prompt or ASR transcript),
//! what the device "thought" (one summary entry per internal model call
//! or routing decision, including failed fallback legs), and what the AI
//! replied with which engine. Voice interactions happen away from the
//! browser; this store is where they become visible.
//!
//! Fail-open like every other record sink: a full or busy database, or
//! a disabled config, never touches the conversation pipeline itself.

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::db;
use crate::routes::events::CameraEvent;
use crate::routes::events::EventBus;

/// Thinking notes are human-readable summaries, not verbatim prompts —
/// hard-truncated so a runaway model string can never bloat a row.
pub const NOTE_MAX_CHARS: usize = 200;

/// One internal "thinking" step inside a turn (SPEC §3.4): the model or
/// routing decision consulted, a hand-written summary of what it did,
/// and how long it took. `source` shares the §3.3 span-model namespace
/// (`decision` / `cloud.chat` / `cloud.vision` / `vlm` / `llm` /
/// `tts.zh` …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingEntry {
    pub source: String,
    pub model: String,
    pub note: String,
    pub duration_ms: u64,
}

/// One persisted dialogue turn (SPEC §3.4 turn object). `reply_text` /
/// `engine` are `None` on no-reply turns (e.g. the decision engine
/// classified the utterance as noise) — those turns are still recorded
/// so the log honestly shows why nothing was answered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationTurn {
    pub id: i64,
    pub conversation_id: String,
    /// `"voice"` | `"http"`.
    pub origin: String,
    pub started_ms: i64,
    pub user_text: Option<String>,
    pub thinking: Vec<ThinkingEntry>,
    pub reply_text: Option<String>,
    /// `"cloud"` | `"local"` | `"vlm"` | `None` (no reply).
    pub engine: Option<String>,
    /// SPEC §3.4 (2026-10-12): `true` only on turns cut short by user
    /// interruption — `reply_text` then holds the actually-spoken prefix
    /// (possibly `None`). Serialized only when true (old frontends ignore
    /// unknown fields either way).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

/// Where a turn came from. `Voice` turns share the §3.3 120 s session
/// slot's `conversation_id`; `Http` turns use the request's chat trace
/// id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOrigin {
    Voice,
    Http,
}

impl TurnOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Http => "http",
        }
    }
}

/// Truncate a thinking note on a char boundary (SPEC §3.4: 200 chars).
/// Multi-byte text is the norm here — naive slicing would panic.
#[must_use]
pub fn truncate_note(note: &str) -> String {
    if note.chars().count() <= NOTE_MAX_CHARS {
        return note.to_string();
    }
    note.chars().take(NOTE_MAX_CHARS).collect()
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Builder for one in-flight turn. The voice bridge and the HTTP chat
/// route collect entries as their legs run and hand the draft to
/// [`ConversationLog::finish`] exactly once, when the turn's outcome is
/// known (reply, no-reply decision, or failure).
#[derive(Debug, Clone)]
pub struct TurnDraft {
    conversation_id: String,
    origin: TurnOrigin,
    started_ms: i64,
    user_text: Option<String>,
    thinking: Vec<ThinkingEntry>,
    reply_text: Option<String>,
    engine: Option<String>,
    interrupted: bool,
}

impl TurnDraft {
    pub fn new(origin: TurnOrigin, conversation_id: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            origin,
            started_ms: unix_now_ms(),
            user_text: None,
            thinking: Vec::new(),
            reply_text: None,
            engine: None,
            interrupted: false,
        }
    }

    /// The heard (ASR) or typed (HTTP) user text.
    pub fn user_text(mut self, text: impl Into<String>) -> Self {
        self.user_text = Some(text.into());
        self
    }

    /// Append one internal-call summary entry.
    pub fn think(
        mut self,
        source: &str,
        model: &str,
        note: impl AsRef<str>,
        duration_ms: u64,
    ) -> Self {
        self.thinking.push(ThinkingEntry {
            source: source.to_string(),
            model: model.to_string(),
            note: truncate_note(note.as_ref()),
            duration_ms,
        });
        self
    }

    /// Record the AI reply and the engine that produced it.
    pub fn reply(mut self, text: impl Into<String>, engine: &str) -> Self {
        self.reply_text = Some(text.into());
        self.engine = Some(engine.to_string());
        self
    }

    /// Mark the turn as cut short by user interruption (SPEC §3.4
    /// `interrupted`). Call after [`Self::reply`] (prefix) or on a
    /// nothing-spoken cancel.
    pub fn interrupted(mut self) -> Self {
        self.interrupted = true;
        self
    }

    pub(crate) fn into_turn(self, id: i64) -> ConversationTurn {
        ConversationTurn {
            id,
            conversation_id: self.conversation_id,
            origin: self.origin.as_str().to_string(),
            started_ms: self.started_ms,
            user_text: self.user_text,
            thinking: self.thinking,
            reply_text: self.reply_text,
            engine: self.engine,
            interrupted: self.interrupted,
        }
    }
}

/// Shared sink for finished turns: persist (SQLite, FIFO-pruned) and
/// broadcast (SSE `conversation`). Cheap to clone into every bridge
/// task via its `Arc`.
pub struct ConversationLog {
    pool: SqlitePool,
    event_tx: Arc<EventBus>,
    /// Privacy master switch (`[conversations] enabled`, default true).
    /// Disabled → records nothing and advertises no capability.
    enabled: bool,
}

impl ConversationLog {
    pub fn new(pool: SqlitePool, event_tx: Arc<EventBus>, enabled: bool) -> Self {
        Self {
            pool,
            event_tx,
            enabled,
        }
    }

    /// Whether the record surface is on (drives the capability and the
    /// SSE event advertisement).
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Persist one finished turn and broadcast it as the SSE
    /// `conversation` event. Fail-open: DB errors are logged and
    /// dropped — recording must never break a conversation.
    pub async fn finish(&self, draft: TurnDraft) {
        if !self.enabled {
            return;
        }
        let turn = match db::insert_conversation_turn(&self.pool, draft).await {
            Ok(turn) => turn,
            Err(e) => {
                tracing::warn!(error = %e, "conversation record: insert failed");
                return;
            }
        };
        // SSE is lossy by design; a send error just means no client is
        // listening right now.
        let _ = self.event_tx.send(CameraEvent::ConversationRecord { turn });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_note_cuts_on_char_boundary() {
        assert_eq!(truncate_note("short"), "short");
        let long: String = "蜜".repeat(NOTE_MAX_CHARS + 5);
        let cut = truncate_note(&long);
        assert_eq!(cut.chars().count(), NOTE_MAX_CHARS);
        // No panic on multi-byte input, and the prefix is preserved.
        assert!(cut.starts_with('蜜'));
    }

    #[test]
    fn draft_builds_turn_in_order() {
        let draft = TurnDraft::new(TurnOrigin::Voice, "c123")
            .user_text("小蜜蜂，几点了")
            .think("decision", "laya", "意图=answer（置信度 0.93）", 41)
            .think(
                "llm",
                "qwen3",
                "本地应答 · prompt 12 / completion 3 tok",
                900,
            )
            .reply("现在是下午三点。", "local");
        let turn = draft.into_turn(7);
        assert_eq!(turn.id, 7);
        assert_eq!(turn.origin, "voice");
        assert_eq!(turn.conversation_id, "c123");
        assert_eq!(turn.user_text.as_deref(), Some("小蜜蜂，几点了"));
        assert_eq!(turn.thinking.len(), 2);
        assert_eq!(turn.thinking[0].source, "decision");
        assert_eq!(turn.engine.as_deref(), Some("local"));
        assert_eq!(turn.reply_text.as_deref(), Some("现在是下午三点。"));
    }

    #[test]
    fn no_reply_turn_keeps_thinking_only() {
        let turn = TurnDraft::new(TurnOrigin::Voice, "c9")
            .user_text("（电视声）")
            .think("decision", "laya", "意图=ignore（置信度 0.88）→ 不回复", 35)
            .into_turn(1);
        assert_eq!(turn.reply_text, None);
        assert_eq!(turn.engine, None);
        assert_eq!(turn.thinking.len(), 1);
    }

    #[tokio::test]
    async fn finish_persists_prunes_and_broadcasts() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("migrations for test pool");
        let event_tx = Arc::new(crate::routes::events::new_event_bus());
        let mut rx = event_tx.subscribe();
        let log = ConversationLog::new(pool.clone(), Arc::clone(&event_tx), true);

        log.finish(
            TurnDraft::new(TurnOrigin::Http, "c1")
                .user_text("你好")
                .think("llm", "qwen3", "本地应答", 10)
                .reply("你好呀！", "local"),
        )
        .await;

        let listed = crate::db::list_conversation_turns(&pool, 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].origin, "http");
        assert_eq!(listed[0].thinking.len(), 1);
        assert_eq!(listed[0].reply_text.as_deref(), Some("你好呀！"));
        assert!(!listed[0].interrupted);

        // Interrupted turn (SPEC §3.4, #45): spoken prefix + marker; the
        // flag serializes only when true (old frontends never see it).
        log.finish(
            TurnDraft::new(TurnOrigin::Voice, "c1")
                .user_text("介绍你自己")
                .think("llm", "qwen3", "回答被打断", 10)
                .reply("你好，我是小蜜蜂。", "local")
                .interrupted(),
        )
        .await;
        let listed = crate::db::list_conversation_turns(&pool, 10).await.unwrap();
        let interrupted = listed.iter().find(|t| t.interrupted).unwrap();
        assert_eq!(
            interrupted.reply_text.as_deref(),
            Some("你好，我是小蜜蜂。")
        );
        let json = serde_json::to_value(interrupted).unwrap();
        assert_eq!(json["interrupted"], serde_json::json!(true));
        let plain = serde_json::to_value(&listed[1]).unwrap();
        assert!(
            plain.get("interrupted").is_none(),
            "flag omitted when false"
        );

        match rx.try_recv().expect("SSE event broadcast") {
            CameraEvent::ConversationRecord { turn } => {
                assert_eq!(turn.id, listed[1].id);
                assert_eq!(turn.engine.as_deref(), Some("local"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Disabled log records nothing (still the two turns above).
        let off = ConversationLog::new(pool.clone(), event_tx, false);
        off.finish(TurnDraft::new(TurnOrigin::Http, "c2").user_text("x"))
            .await;
        assert_eq!(
            crate::db::list_conversation_turns(&pool, 10)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn insert_prunes_to_fifo_cap() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("migrations for test pool");
        for i in 0..5 {
            let draft = TurnDraft::new(TurnOrigin::Http, format!("c{i}")).user_text("t");
            crate::db::insert_conversation_turn_with_cap(&pool, draft, 3)
                .await
                .unwrap();
        }
        let listed = crate::db::list_conversation_turns(&pool, 10).await.unwrap();
        assert_eq!(listed.len(), 3, "FIFO cap keeps the newest 3");
        assert_eq!(listed[0].conversation_id, "c4");
        assert_eq!(listed[2].conversation_id, "c2");
    }
}
