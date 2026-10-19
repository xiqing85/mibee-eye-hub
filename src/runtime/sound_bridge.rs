//! Sound-event bridge — the former inline `main()` block, moved verbatim
//! (roadmap #6, M1 slice 6). One spawned task consumes the audio-AI
//! engine's sound events: persistent hearing records with scene + media
//! correlation (SPEC appendix A #24 / #30-C), SSE `alarm` with
//! source:"audio", desktop notification, the away audio link (#44) and
//! the GB Alarm NOTIFY with the same pinned 2022 standard triple. The
//! microphone is device-level hardware, so events are attributed to
//! camera "0".

use std::collections::HashMap;
use std::sync::Arc;

use streaming::audio_ai::SoundEvent;
use streaming::output::SegmentSlot;
use web::away::AwayEngine;
use web::grounding::GroundingState;
use web::routes::events::EventBus;

use crate::desktop::Desktop;

/// Everything the former inline block captured.
pub struct SoundBridge {
    pub events: tokio::sync::broadcast::Receiver<SoundEvent>,
    pub bridge_tx: Arc<EventBus>,
    pub notifier_slot: Arc<std::sync::Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>>,
    pub alarm_notify_gate: Arc<std::sync::atomic::AtomicBool>,
    pub pool: sqlx::SqlitePool,
    pub grounding: Arc<GroundingState>,
    pub away: Arc<AwayEngine>,
    pub streams: Arc<std::sync::Mutex<HashMap<String, SegmentSlot>>>,
    pub desktop: Arc<Desktop>,
}

impl SoundBridge {
    /// Spawn the bridge task. Fields destructure onto the original
    /// block-local names so the moved body compiles unchanged.
    pub fn spawn(self) {
        let SoundBridge {
            events: mut sound_events,
            bridge_tx,
            notifier_slot,
            alarm_notify_gate,
            pool: records_pool,
            grounding: grounding_sound,
            away: away_for_sound,
            streams: streams_for_sound,
            desktop: desktop_sound_alarm,
        } = self;
        let sound_pool_for_scene = records_pool.clone();
        tokio::spawn(async move {
            loop {
                match sound_events.recv().await {
                    Ok(ev) => {
                        // Persistent hearing record (SPEC appendix A #24) —
                        // fail-open: a DB hiccup never touches the alarm
                        // pipeline. Scene = what the camera saw when the
                        // sound fired (#30-C).
                        let camera = web::db::list_cameras(&sound_pool_for_scene)
                            .await
                            .ok()
                            .and_then(|cams| cams.first().map(|c| c.id.clone()));
                        let scene = camera
                            .as_deref()
                            .and_then(|id| grounding_sound.scene_summary(id, ev.timestamp_ms))
                            .unwrap_or_default();
                        // Video dimension (#30-C): the MP4 segment covering
                        // this sound, when the camera is recording.
                        let media_ref = camera
                            .as_deref()
                            .and_then(|id| {
                                web::stream_manager::StreamManager::segment_covering(
                                    &streams_for_sound,
                                    id,
                                    ev.timestamp_ms,
                                )
                            })
                            .unwrap_or_default();
                        if let Err(e) = web::db::insert_hearing_record(
                            &records_pool,
                            "sound",
                            &ev.class,
                            Some(ev.score as f64),
                            "",
                            "",
                            &scene,
                            &media_ref,
                            ev.timestamp_ms as i64,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "hearing record: insert failed");
                        }
                        let _ = bridge_tx.send(web::routes::events::CameraEvent::Alarm {
                            camera_id: "0".to_string(),
                            targets: 1,
                            timestamp_ms: ev.timestamp_ms,
                            source: "audio".to_string(),
                            class: Some(ev.class.clone()),
                            score: Some(ev.score),
                        });
                        // Desktop notification (SPEC appendix A #42).
                        desktop_sound_alarm.notify_alarm("audio", 1, Some(&ev.class));
                        // Away audio link (#44): one activity record per
                        // cooldown while armed (observe_audio gates the
                        // audio_link setting + cooldown internally).
                        if away_for_sound.is_armed()
                            && let Some(labels) =
                                away_for_sound.observe_audio(&ev.class, ev.score, ev.timestamp_ms)
                        {
                            crate::away_monitor::record_audio_event(
                                &away_for_sound,
                                &sound_pool_for_scene,
                                &bridge_tx,
                                &labels,
                            )
                            .await;
                        }
                        if alarm_notify_gate.load(std::sync::atomic::Ordering::SeqCst) {
                            let notifier =
                                notifier_slot.lock().expect("gb notifier slot lock").clone();
                            if let Some(n) = notifier {
                                let description =
                                    audio_alarm_description(&ev.class, &ev.label_zh, ev.score);
                                let sent = n.send_alarm(
                                    "4",
                                    "5",
                                    &gb28181_rs::client::format_gb_time_ms(ev.timestamp_ms),
                                    "2",
                                    &description,
                                );
                                if !sent {
                                    tracing::debug!("alarm notify: no active subscription");
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "audio_ai: SSE bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// GB Alarm NOTIFY description for a sound event: the Chinese label when
/// a mapping exists, the canonical class otherwise, score to 2 decimals.
fn audio_alarm_description(class: &str, label_zh: &str, score: f32) -> String {
    format!(
        "audio event: {} ({:.2})",
        if label_zh.is_empty() { class } else { label_zh },
        score
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_alarm_description_prefers_zh_label_and_falls_back() {
        assert_eq!(
            audio_alarm_description("Dog", "狗叫", 0.876),
            "audio event: 狗叫 (0.88)".to_string()
        );
        assert_eq!(
            audio_alarm_description("Siren", "", 0.5),
            "audio event: Siren (0.50)".to_string()
        );
    }
}
