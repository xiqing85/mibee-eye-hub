//! Server-Sent Events (SSE) endpoint for real-time camera events.
//!
//! `GET /api/events` opens a persistent SSE connection that streams
//! camera hot-plug events (`camera_added`, `camera_offline`) to the
//! browser. The browser uses this to refresh the camera list without
//! polling.
//!
//! # Event format
//!
//! Each SSE event is a UTF-8 text block terminated by `\n\n`:
//!
//! ```text
//! event: camera_added
//! data: {"camera_id":"...","device_index":0,"name":"..."}
//!
//! event: camera_offline
//! data: {"camera_id":"...","device_index":0}
//! ```

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Extension;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::errors::ApiError;
use security::middleware::AuthenticatedUser;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Events broadcast to SSE clients when camera state changes.
///
/// These are produced by the hot-plug monitor (via main.rs) and consumed
/// by the SSE endpoint.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum CameraEvent {
    /// An LLM reply (SPEC appendix A notebook dialect: `chat_reply`) —
    /// either the voice auto-reply or a POST /api/chat answer.
    ChatReply {
        /// `"voice"` (auto-reply to a transcript) — HTTP replies return
        /// directly and do not ride the SSE bus.
        source: String,
        reply: String,
        /// Grounding mode that produced the reply (SPEC appendix A
        /// #29): `"scene"` (live context injected) or `"none"`.
        grounded: String,
        timestamp_ms: u64,
    },
    /// A voice interaction completed (SPEC appendix A notebook dialect:
    /// `voice_transcript`): wake word detected + utterance transcribed.
    VoiceTranscript {
        keyword: String,
        transcript: String,
        /// Best-matching enrolled speaker ("" = unknown) — SPEC appendix A
        /// #25 additive field.
        speaker: String,
        /// 【画面】 summary when the utterance fired (#30-C; "" = none).
        scene: String,
        /// Captured inside a follow-up window (#30-B).
        follow_up: bool,
        timestamp_ms: u64,
    },
    /// One Laya typed decision over a voice transcript (SPEC appendix A
    /// #26): how the device classified the utterance before answering.
    VoiceDecision {
        camera_id: String,
        transcript: String,
        choice: String,
        confidence: f32,
        act_probability: f32,
        timestamp_ms: u64,
    },
    /// Meeting-mode lifecycle change (SPEC appendix A notebook dialect
    /// #27: `meeting_state`): recording started, processing started,
    /// pipeline done/failed.
    MeetingState {
        /// Device-level: notebook meetings are not per-camera ("all").
        camera_id: String,
        meeting_id: i64,
        status: String,
        timestamp_ms: u64,
    },
    /// A zone event fired (SPEC appendix A notebook dialect:
    /// `zone_event`). Produced by the zone engine in main.rs from tracked
    /// detections crossing user-drawn zones.
    ZoneEvent {
        camera_id: String,
        zone: String,
        /// `intrusion` | `loiter` | `line_cross_forward` | `line_cross_backward`
        event: String,
        track_id: u64,
        label: String,
        timestamp_ms: u64,
    },
    /// Microphone level for the real-time voice waveform (SPEC §6
    /// `audio_level`, notebook dialect #36): smoothed RMS 0..1 at ≤10 Hz
    /// while the audio monitor runs. Zero = idle floor.
    AudioLevel {
        level: f32,
        timestamp_ms: u64,
        /// SPEC appendix A #46: passive (ambience) vs active (functional
        /// capture window). Additive — old consumers ignore it.
        capture_mode: String,
        /// SPEC #46: "self" | "speaker:<name>" | "others" | "ambient".
        source: String,
    },
    /// A model download task changed state (SPEC §4.9 `model_task`):
    /// progress ticks (throttled ≥0.5s) and terminal statuses, forwarded
    /// from the download manager by main.rs.
    ModelTask {
        #[serde(flatten)]
        task: streaming::models::TaskSnapshot,
    },
    /// A new camera was discovered (plugged in or detected on startup).
    CameraAdded {
        camera_id: String,
        device_index: u32,
        name: String,
    },
    /// A previously-known camera went offline (unplugged).
    CameraOfflined {
        camera_id: String,
        device_index: u32,
    },
    /// The active AI model was hot-switched (SPEC v1 §6 `ai_model_changed`).
    /// Device-wide on this multi-camera device: `camera_id` is `"all"`
    /// (SPEC appendix A notebook dialect).
    AiModelChanged { model: String },
    /// An AI inference completed on a camera (SPEC v1 §6 `ai_detection`).
    /// Produced by the AI engine's per-camera workers and bridged into
    /// this bus by main.rs.
    AiDetection {
        camera_id: String,
        detections: Vec<streaming::ai::Detection>,
        frame_number: u64,
    },
    /// An alarm fired (SPEC v1 §6 `alarm`). Produced by the alarm bridge
    /// in main.rs on a detection rising edge (source `"ai"`) or by the
    /// audio-event engine on a voted sound-class rising edge
    /// (source `"audio"`, carrying the class and its score).
    Alarm {
        camera_id: String,
        targets: usize,
        timestamp_ms: u64,
        /// `"ai"` (visual detection) or `"audio"` (sound event).
        source: String,
        /// Sound class (YAMNet display name) — audio alarms only.
        class: Option<String>,
        /// Voted sound-class score — audio alarms only.
        score: Option<f32>,
    },
    /// A VLM description of an alarm frame arrived (SPEC appendix A #23
    /// `alarm_description`). Emitted asynchronously after the visual
    /// alarm — the alarm itself never waits for the description.
    AlarmDescription {
        camera_id: String,
        /// Timestamp of the alarm this description belongs to (join key
        /// on the browser side).
        alarm_timestamp_ms: u64,
        description: String,
        elapsed_s: f64,
    },
    /// A dialogue turn finished (SPEC v1 §3.4 `conversation`): the full
    /// record — heard/input text, internal thinking entries, reply and
    /// engine. No-reply turns ride the same event with `reply_text:null`.
    ConversationRecord {
        #[serde(flatten)]
        turn: crate::conversations::ConversationTurn,
    },
    /// An away-mode event record changed (SPEC v1 §6 `away_event`):
    /// created, or state-migrated (VLM description landed, visitor
    /// answered, listen window expired silent). Same record shape as
    /// the API list — the browser upserts by `id`.
    AwayEvent {
        #[serde(flatten)]
        event: crate::away::AwayEventRecord,
    },
    /// Away mode armed/disarmed by any client (SPEC v1 §6 `away_state`)
    /// — other tabs sync their badge from this.
    AwayState { active: bool, since_ms: Option<u64> },
    /// A live agent step (SPEC v1 §6 `agent_step`, §3.5): tool executions
    /// (running → done/error) and phase switches (thinking/answering) —
    /// the frontend hero state and live thinking panel ride on this.
    /// Elasticity decision (SPEC appendix A #48): tier action, reason,
    /// and the ladder endpoints ("" for advice actions).
    Scale {
        action: String,
        from: String,
        to: String,
        reason: String,
        timestamp_ms: u64,
    },
    /// Plugin trigger invocation (SPEC appendix A #47): one rule action's
    /// execution — outcome ok | error:<msg> | skipped:<reason>.
    PluginInvocation {
        plugin: String,
        rule: usize,
        action: String,
        outcome: String,
        timestamp_ms: u64,
    },
    AgentStep {
        conversation_id: String,
        /// `"tool"` | `"phase"`.
        kind: String,
        /// tool: `"running"|"done"|"error"`; phase: `"thinking"|"answering"`.
        state: String,
        tool: Option<String>,
        args: Option<serde_json::Value>,
        result: Option<String>,
        duration_ms: Option<u64>,
        note: Option<String>,
    },
}

/// Type alias for the broadcast sender used to fan out camera events.
pub type EventBus = broadcast::Sender<CameraEvent>;

/// Create a new event bus with a reasonable buffer size.
pub fn new_event_bus() -> EventBus {
    broadcast::channel::<CameraEvent>(64).0
}

// ---------------------------------------------------------------------------
// SSE handler
// ---------------------------------------------------------------------------

/// `GET /api/events` — Server-Sent Events stream for camera hot-plug events.
///
/// Returns a persistent SSE connection. The browser receives
/// `camera_added` / `camera_offlined` events as they happen.
///
/// Authenticated via session cookie (same as all other API endpoints).
#[tracing::instrument(skip_all)]
pub async fn sse_events(
    Extension(event_tx): Extension<Arc<EventBus>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> std::result::Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    // Subscribe to the broadcast channel.
    let rx = event_tx.subscribe();

    // Convert the broadcast receiver into an async stream.
    let event_stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(event) => Some(Ok(event_to_sse(event))),
            // Lagged errors are expected when events arrive faster than
            // the client can consume; just skip them.
            Err(_lagged) => None,
        }
    });

    // Wrap with keep-alive pings every 15s.
    Ok(Sse::new(event_stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

/// Convert a [`CameraEvent`] into its SSE wire pair: the event name and
/// the JSON payload string (SPEC §6). Pure — the thin wrapper below is
/// the only Axum-aware part, so the wire contract is unit-testable.
fn event_wire(event: &CameraEvent) -> (&'static str, String) {
    match event {
        CameraEvent::ChatReply {
            source,
            reply,
            grounded,
            timestamp_ms,
        } => (
            "chat_reply",
            serde_json::json!({
                "source": source,
                "reply": reply,
                "grounded": grounded,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::VoiceTranscript {
            keyword,
            transcript,
            speaker,
            scene,
            follow_up,
            timestamp_ms,
        } => (
            "voice_transcript",
            serde_json::json!({
                "keyword": keyword,
                "transcript": transcript,
                "speaker": speaker,
                "scene": scene,
                "follow_up": follow_up,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::VoiceDecision {
            camera_id,
            transcript,
            choice,
            confidence,
            act_probability,
            timestamp_ms,
        } => (
            "voice_decision",
            serde_json::json!({
                "camera_id": camera_id,
                "transcript": transcript,
                "choice": choice,
                "confidence": confidence,
                "act_probability": act_probability,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::MeetingState {
            camera_id,
            meeting_id,
            status,
            timestamp_ms,
        } => (
            "meeting_state",
            serde_json::json!({
                "camera_id": camera_id,
                "meeting_id": meeting_id,
                "status": status,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::ZoneEvent {
            camera_id,
            zone,
            event,
            track_id,
            label,
            timestamp_ms,
        } => (
            "zone_event",
            serde_json::json!({
                "camera_id": camera_id,
                "zone": zone,
                "event": event,
                "track_id": track_id,
                "label": label,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::AudioLevel {
            level,
            timestamp_ms,
            capture_mode,
            source,
        } => (
            "audio_level",
            serde_json::json!({
                "level": level,
                "timestamp": timestamp_ms,
                "capture_mode": capture_mode,
                "source": source,
            })
            .to_string(),
        ),
        CameraEvent::ModelTask { task } => (
            "model_task",
            serde_json::to_value(task).unwrap_or_default().to_string(),
        ),
        CameraEvent::CameraAdded {
            camera_id,
            device_index,
            name,
        } => (
            "camera_added",
            serde_json::json!({
                "camera_id": camera_id,
                "device_index": device_index,
                "name": name,
            })
            .to_string(),
        ),
        CameraEvent::CameraOfflined {
            camera_id,
            device_index,
        } => (
            "camera_offlined",
            serde_json::json!({
                "camera_id": camera_id,
                "device_index": device_index,
            })
            .to_string(),
        ),
        CameraEvent::AiModelChanged { model } => (
            "ai_model_changed",
            serde_json::json!({
                "camera_id": "all",
                "model": model,
            })
            .to_string(),
        ),
        CameraEvent::AiDetection {
            camera_id,
            detections,
            frame_number,
        } => (
            "ai_detection",
            serde_json::json!({
                "camera_id": camera_id,
                "detections": detections,
                "frame_number": frame_number,
            })
            .to_string(),
        ),
        CameraEvent::Alarm {
            camera_id,
            targets,
            timestamp_ms,
            source,
            class,
            score,
        } => {
            let mut payload = serde_json::json!({
                "camera_id": camera_id,
                "active": true,
                "source": source,
                "targets": targets,
                "timestamp": timestamp_ms,
            });
            if let Some(class) = class {
                payload["class"] = serde_json::json!(class);
            }
            if let Some(score) = score {
                payload["score"] = serde_json::json!(score);
            }
            ("alarm", payload.to_string())
        }
        CameraEvent::AlarmDescription {
            camera_id,
            alarm_timestamp_ms,
            description,
            elapsed_s,
        } => (
            "alarm_description",
            serde_json::json!({
                "camera_id": camera_id,
                "alarm_timestamp": alarm_timestamp_ms,
                "description": description,
                "elapsed_s": elapsed_s,
            })
            .to_string(),
        ),
        CameraEvent::ConversationRecord { turn } => (
            "conversation",
            serde_json::to_value(turn).unwrap_or_default().to_string(),
        ),
        CameraEvent::AwayEvent { event } => (
            "away_event",
            serde_json::to_value(event).unwrap_or_default().to_string(),
        ),
        CameraEvent::AwayState { active, since_ms } => (
            "away_state",
            serde_json::json!({
                "active": active,
                "since_ms": since_ms,
            })
            .to_string(),
        ),
        CameraEvent::Scale {
            action,
            from,
            to,
            reason,
            timestamp_ms,
        } => (
            "scale",
            serde_json::json!({
                "action": action,
                "from": from,
                "to": to,
                "reason": reason,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::PluginInvocation {
            plugin,
            rule,
            action,
            outcome,
            timestamp_ms,
        } => (
            "plugin_invocation",
            serde_json::json!({
                "plugin": plugin,
                "rule": rule,
                "action": action,
                "outcome": outcome,
                "timestamp": timestamp_ms,
            })
            .to_string(),
        ),
        CameraEvent::AgentStep {
            conversation_id,
            kind,
            state,
            tool,
            args,
            result,
            duration_ms,
            note,
        } => {
            let mut payload = serde_json::json!({
                "conversation_id": conversation_id,
                "kind": kind,
                "state": state,
            });
            if let Some(t) = tool {
                payload["tool"] = serde_json::json!(t);
            }
            if let Some(a) = args {
                payload["args"] = a.clone();
            }
            if let Some(r) = result {
                payload["result"] = serde_json::json!(r);
            }
            if let Some(d) = duration_ms {
                payload["duration_ms"] = serde_json::json!(d);
            }
            if let Some(n) = note {
                payload["note"] = serde_json::json!(n);
            }
            ("agent_step", payload.to_string())
        }
    }
}

/// Convert a [`CameraEvent`] into an Axum SSE [`Event`].
fn event_to_sse(event: CameraEvent) -> Event {
    let (name, data) = event_wire(&event);
    Event::default().event(name).data(data)
}
/// Returns the SSE-friendly headers for `text/event-stream`.
///
/// Currently unused directly (Axum's `Sse` handles this), but documented
/// here for reference.
#[allow(dead_code)]
pub fn sse_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "text/event-stream".parse().unwrap());
    headers.insert("cache-control", "no-cache".parse().unwrap());
    headers.insert("connection", "keep-alive".parse().unwrap());
    headers
}

#[cfg(test)]
mod wire_tests {
    //! SPEC §6 wire contract: every CameraEvent variant's SSE name and
    //! JSON shape, pinned per variant through the pure [`event_wire`].

    use super::*;

    fn wire(event: CameraEvent) -> (&'static str, serde_json::Value) {
        let (name, data) = event_wire(&event);
        let json = serde_json::from_str(&data).expect("payload is valid JSON");
        (name, json)
    }

    #[test]
    fn dialogue_family_events() {
        let (n, j) = wire(CameraEvent::ChatReply {
            source: "voice".into(),
            reply: "你好".into(),
            grounded: String::new(),
            timestamp_ms: 7,
        });
        assert_eq!(n, "chat_reply");
        assert_eq!(j["source"], "voice");
        assert_eq!(j["reply"], "你好");
        assert_eq!(j["grounded"], "");
        assert_eq!(j["timestamp"], 7);

        let (n, j) = wire(CameraEvent::VoiceTranscript {
            keyword: "小蜜蜂".into(),
            transcript: "几点了".into(),
            speaker: String::new(),
            scene: "室内".into(),
            follow_up: false,
            timestamp_ms: 8,
        });
        assert_eq!(n, "voice_transcript");
        assert_eq!(j["keyword"], "小蜜蜂");
        assert_eq!(j["transcript"], "几点了");
        assert_eq!(j["speaker"], "");
        assert_eq!(j["scene"], "室内");
        assert_eq!(j["follow_up"], false);

        let (n, j) = wire(CameraEvent::VoiceDecision {
            camera_id: "0".into(),
            transcript: "天气".into(),
            choice: "answer".into(),
            confidence: 0.84,
            act_probability: 0.9,
            timestamp_ms: 9,
        });
        assert_eq!(n, "voice_decision");
        assert_eq!(j["choice"], "answer");
        assert_eq!(j["confidence"].as_f64().unwrap() as f32, 0.84);

        let (n, j) = wire(CameraEvent::AgentStep {
            conversation_id: "c1".into(),
            kind: "tool".into(),
            state: "done".into(),
            tool: Some("weather.current".into()),
            args: Some(serde_json::json!({"city": "广州"})),
            result: Some("晴".into()),
            duration_ms: Some(1041),
            note: None,
        });
        assert_eq!(n, "agent_step");
        assert_eq!(j["kind"], "tool");
        assert_eq!(j["tool"], "weather.current");
        assert_eq!(j["args"]["city"], "广州");
        assert_eq!(j["result"], "晴");
        assert_eq!(j["duration_ms"], 1041);
        assert!(
            j.get("note").is_none(),
            "None optionals are omitted, not null"
        );

        let turn = crate::conversations::ConversationTurn {
            id: 1,
            conversation_id: "c1".into(),
            origin: "voice".into(),
            started_ms: 2,
            user_text: Some("问".into()),
            thinking: vec![],
            reply_text: Some("答".into()),
            engine: Some("local".into()),
            interrupted: false,
        };
        let (n, j) = wire(CameraEvent::ConversationRecord { turn });
        assert_eq!(n, "conversation");
        assert_eq!(j["origin"], "voice");
        assert_eq!(j["reply_text"], "答");
        assert_eq!(j["engine"], "local");
    }

    #[test]
    fn camera_and_detection_events() {
        let (n, j) = wire(CameraEvent::CameraAdded {
            camera_id: "c1".into(),
            device_index: 2,
            name: "USB Cam".into(),
        });
        assert_eq!(n, "camera_added");
        assert_eq!(j["camera_id"], "c1");
        assert_eq!(j["device_index"], 2);
        assert_eq!(j["name"], "USB Cam");

        let (n, j) = wire(CameraEvent::CameraOfflined {
            camera_id: "c1".into(),
            device_index: 2,
        });
        assert_eq!(n, "camera_offlined");
        assert_eq!(j["device_index"], 2);

        let det = streaming::ai::Detection {
            label: "person".into(),
            confidence: 0.9,
            bbox: [1, 2, 3, 4],
        };
        let (n, j) = wire(CameraEvent::AiDetection {
            camera_id: "c1".into(),
            detections: vec![det],
            frame_number: 42,
        });
        assert_eq!(n, "ai_detection");
        assert_eq!(j["frame_number"], 42);
        assert_eq!(j["detections"][0]["label"], "person");
        assert_eq!(
            j["detections"][0]["confidence"].as_f64().unwrap() as f32,
            0.9
        );
        assert_eq!(j["detections"][0]["bbox"], serde_json::json!([1, 2, 3, 4]));

        let (n, j) = wire(CameraEvent::AiModelChanged {
            model: "nanodet-x".into(),
        });
        assert_eq!(n, "ai_model_changed");
        assert_eq!(
            j["camera_id"], "all",
            "model change is fleet-wide by convention"
        );
        assert_eq!(j["model"], "nanodet-x");

        let (n, j) = wire(CameraEvent::ZoneEvent {
            camera_id: "c1".into(),
            zone: "前门".into(),
            event: "intrusion".into(),
            track_id: 5,
            label: "person".into(),
            timestamp_ms: 10,
        });
        assert_eq!(n, "zone_event");
        assert_eq!(j["zone"], "前门");
        assert_eq!(j["event"], "intrusion");
        assert_eq!(j["track_id"], 5);
    }

    #[test]
    fn alarm_events_pin_the_pinned_triple_and_optionals() {
        let (n, j) = wire(CameraEvent::Alarm {
            camera_id: "c1".into(),
            targets: 2,
            timestamp_ms: 11,
            source: "ai".into(),
            class: None,
            score: None,
        });
        assert_eq!(n, "alarm");
        assert_eq!(j["active"], true, "rising-edge convention");
        assert_eq!(j["source"], "ai");
        assert_eq!(j["targets"], 2);
        assert!(j.get("class").is_none());
        assert!(j.get("score").is_none());

        let (_n, j) = wire(CameraEvent::Alarm {
            camera_id: "0".into(),
            targets: 1,
            timestamp_ms: 12,
            source: "audio".into(),
            class: Some("Siren".into()),
            score: Some(0.82),
        });
        assert_eq!(j["class"], "Siren");
        assert_eq!(j["score"].as_f64().unwrap() as f32, 0.82);

        let (n, j) = wire(CameraEvent::AlarmDescription {
            camera_id: "c1".into(),
            alarm_timestamp_ms: 11,
            description: "一个人经过".into(),
            elapsed_s: 41.5,
        });
        assert_eq!(n, "alarm_description");
        assert_eq!(j["alarm_timestamp"], 11);
        assert_eq!(j["description"], "一个人经过");
        assert_eq!(j["elapsed_s"], 41.5);
    }

    #[test]
    fn audio_meeting_away_model_events() {
        let (n, j) = wire(CameraEvent::AudioLevel {
            level: 0.42,
            timestamp_ms: 13,
            capture_mode: "active".into(),
            source: "speaker:alice".into(),
        });
        assert_eq!(n, "audio_level");
        assert_eq!(j["level"].as_f64().unwrap() as f32, 0.42);
        assert_eq!(j["timestamp"], 13);
        assert_eq!(j["capture_mode"], "active", "SPEC #46 additive field");
        assert_eq!(j["source"], "speaker:alice");

        let (n, j) = wire(CameraEvent::MeetingState {
            camera_id: "0".into(),
            meeting_id: 7,
            status: "recording".into(),
            timestamp_ms: 14,
        });
        assert_eq!(n, "meeting_state");
        assert_eq!(j["meeting_id"], 7);
        assert_eq!(j["status"], "recording");

        let task = streaming::models::TaskSnapshot {
            task_id: "t1".into(),
            capability: "llm".into(),
            model_id: "qwen".into(),
            model_name: "Qwen".into(),
            status: streaming::models::TaskStatus::Downloading,
            progress: 0.5,
            downloaded_bytes: 10,
            total_bytes: 20,
            error: None,
        };
        let (n, j) = wire(CameraEvent::ModelTask { task });
        assert_eq!(n, "model_task");
        assert_eq!(j["task_id"], "t1");
        assert_eq!(j["progress"], 0.5);
        assert_eq!(j["total_bytes"], 20);

        let rec = crate::away::AwayEventRecord {
            id: 3,
            camera_id: "0".into(),
            kind: "person".into(),
            started_ms: 15,
            labels: "person×1".into(),
            face_name: None,
            description: None,
            visitor_reply: None,
            snapshot: None,
            state: "silent".into(),
        };
        let (n, j) = wire(CameraEvent::AwayEvent { event: rec });
        assert_eq!(n, "away_event");
        assert_eq!(j["kind"], "person");
        assert_eq!(j["state"], "silent");

        let (n, j) = wire(CameraEvent::AwayState {
            active: true,
            since_ms: Some(16),
        });
        assert_eq!(n, "away_state");
        assert_eq!(j["active"], true);
        assert_eq!(j["since_ms"], 16);
    }
}
