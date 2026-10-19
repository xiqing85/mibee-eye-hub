//! AI detection bridge — the former inline `main()` block, moved
//! verbatim (roadmap #6, M1 slice 3). One spawned task consumes the
//! AI engine's detection broadcast: grounding feed updates (detections +
//! face labels + VLM descriptions), SSE `ai_detection` events, rising-edge
//! alarms (SPEC §6 `alarm` + desktop notification + ONVIF MotionAlarm +
//! GB28181 Alarm NOTIFY behind the runtime gate) and single-flight VLM
//! descriptions of the triggering frame (SPEC appendix A #23).
//!
//! Zero behavior change: the loop body is untouched except the face-label
//! aggregation, which moved into the pure function [`face_label_text`]
//! (same output, now unit-tested).

use std::sync::Arc;

use streaming::ai::DetectionEvent;
use streaming::face::FaceEngine;
use streaming::face::FaceHit;
use streaming::vlm::VlmEngine;
use web::grounding::GroundingState;
use web::routes::events::EventBus;

use crate::desc_gate::{self, DescDecision};
use crate::desktop::Desktop;

/// VLM description admission gate. See `desc_gate` for the full rationale:
/// single-flight plus a start-to-start floor, with the lease released by
/// `Drop` so a panicking inference cannot wedge the feature off for the life
/// of the process — which is exactly what the previous inline
/// `VLM_DESC_IN_FLIGHT.store(false)` after the call did.
static VLM_DESC_GATE: desc_gate::DescGate = desc_gate::DescGate::new(VLM_DESC_MIN_INTERVAL_MS);
/// Minimum spacing between description starts — a description takes
/// 35–60 s of near-full CPU; this keeps headroom for everything else.
const VLM_DESC_MIN_INTERVAL_MS: u64 = 120_000;

/// Everything the former inline block captured, built at the same point in
/// the startup sequence. [`AiBridge::spawn`] consumes it.
pub struct AiBridge {
    pub events: tokio::sync::broadcast::Receiver<DetectionEvent>,
    pub vlm: Arc<VlmEngine>,
    pub grounding: Arc<GroundingState>,
    pub face: Arc<FaceEngine>,
    pub bridge_tx: Arc<EventBus>,
    pub alarm_cooldown_secs: u64,
    pub notifier_slot: Arc<std::sync::Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>>,
    pub alarm_notify_gate: Arc<std::sync::atomic::AtomicBool>,
    pub onvif_events_slot:
        Arc<std::sync::Mutex<Option<Arc<onvif_device_rs::events::EventsService>>>>,
    pub desktop: Arc<Desktop>,
}

impl AiBridge {
    /// Spawn the bridge task. Fields destructure onto the original
    /// block-local names so the moved body compiles unchanged.
    pub fn spawn(self) {
        let AiBridge {
            events: mut ai_events,
            vlm: vlm_engine,
            grounding: grounding_feed,
            face: face_for_ai,
            bridge_tx,
            alarm_cooldown_secs,
            notifier_slot,
            alarm_notify_gate,
            onvif_events_slot,
            desktop: desktop_ai_alarm,
        } = self;
        let mut alarm_bridge =
            web::alarm::AlarmBridge::new(std::time::Duration::from_secs(alarm_cooldown_secs));
        tokio::spawn(async move {
            loop {
                match ai_events.recv().await {
                    Ok(ev) => {
                        let targets = ev.detections.len();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let frame_w = ev
                            .jpeg
                            .as_deref()
                            .and_then(web::grounding::jpeg_dimensions)
                            .map_or(0, |(w, _)| w);
                        grounding_feed.record_detections(
                            &ev.camera_id,
                            now_ms,
                            frame_w,
                            &ev.detections,
                        );
                        // Face recognition (#33): names + enrollment feed
                        // on the same frame the detection ran on.
                        if face_for_ai.is_active()
                            && let Some(jpeg) = ev.jpeg.clone()
                        {
                            let face = Arc::clone(&face_for_ai);
                            let cam_id = ev.camera_id.clone();
                            let feed = Arc::clone(&grounding_feed);
                            tokio::task::spawn_blocking(move || {
                                let hits = face.match_jpeg(&jpeg);
                                if let Some(label) = face_label_text(&hits) {
                                    let ts = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    feed.record_face_labels(&cam_id, ts, &label);
                                }
                                // Enrollment feed: any face in frame
                                // advances the pending session.
                                let in_session = face.with_registry(|r| {
                                    r.enrollment_status(face.enroll_frames()).is_some()
                                });
                                if in_session {
                                    let needed = face.enroll_frames();
                                    if let Ok(emb) = face.embed_jpeg(&jpeg) {
                                        face.with_registry(|r| {
                                            r.feed_enroll(emb, needed);
                                        });
                                    }
                                }
                            });
                        }
                        let _ = bridge_tx.send(web::routes::events::CameraEvent::AiDetection {
                            camera_id: ev.camera_id.clone(),
                            detections: ev.detections,
                            frame_number: ev.frame_number,
                        });

                        if let Some(sig) = alarm_bridge.observe(
                            &ev.camera_id,
                            targets,
                            std::time::Instant::now(),
                            now_ms,
                        ) {
                            let _ = bridge_tx.send(web::routes::events::CameraEvent::Alarm {
                                camera_id: sig.camera_id.clone(),
                                targets: sig.targets,
                                timestamp_ms: sig.timestamp_ms,
                                source: "ai".to_string(),
                                class: None,
                                score: None,
                            });
                            // Desktop notification rides the same accepted
                            // edge (SPEC appendix A #42; no-op headless).
                            desktop_ai_alarm.notify_alarm("ai", sig.targets, None);
                            // VLM description of the triggering frame (SPEC
                            // appendix A #23): the alarm never waits for it —
                            // the description arrives as its own SSE event.
                            // Single-flight + a floor between starts: a
                            // flickering false-positive detection fires rising
                            // edges every cooldown (30 s) while one
                            // description takes 35–60 s — without the guard
                            // they stack into concurrent contexts that
                            // saturate the CPU (and can OOM-abort the
                            // process). New triggers during a run or inside
                            // the floor are dropped, not queued.
                            if vlm_engine.is_active()
                                && let Some(jpeg) = ev.jpeg.clone()
                            {
                                let (decision, lease) = VLM_DESC_GATE.try_admit(now_ms);
                                match (decision, lease) {
                                    (DescDecision::Busy, _) => {
                                        // Another description is still running —
                                        // drop this frame (never queue).
                                        tracing::debug!(
                                            "vlm: description in flight — alarm frame skipped"
                                        );
                                    }
                                    (DescDecision::Throttled, _) => {
                                        tracing::debug!(
                                            "vlm: description throttled — alarm frame skipped"
                                        );
                                    }
                                    (DescDecision::Admitted, Some(lease)) => {
                                        let vlm = Arc::clone(&vlm_engine);
                                        let tx = bridge_tx.clone();
                                        let camera_id = sig.camera_id.clone();
                                        let alarm_ts = sig.timestamp_ms;
                                        let grounding_feed = Arc::clone(&grounding_feed);
                                        tokio::task::spawn_blocking(move || {
                                            // The lease is moved in and dropped
                                            // when this closure ends — including
                                            // on unwind, which is the whole point:
                                            // the old code released the flag with a
                                            // `store(false)` *after* the call, so a
                                            // panicking inference left the gate shut
                                            // and every later frame was silently
                                            // skipped for the life of the process.
                                            let _lease = lease;
                                            let started = std::time::Instant::now();
                                            let result = vlm.describe_jpeg(&jpeg);
                                            match result {
                                                Ok(description) => {
                                                    grounding_feed.record_vlm_description(
                                                        &camera_id,
                                                        alarm_ts,
                                                        &description,
                                                    );
                                                    let _ = tx.send(
                                                    web::routes::events::CameraEvent::AlarmDescription {
                                                        camera_id,
                                                        alarm_timestamp_ms: alarm_ts,
                                                        description,
                                                        elapsed_s: (started
                                                            .elapsed()
                                                            .as_secs_f64()
                                                            * 100.0)
                                                            .round()
                                                            / 100.0,
                                                    },
                                                );
                                                }
                                                Err(e) => tracing::warn!(
                                                    error = %e,
                                                    "vlm: description failed"
                                                ),
                                            }
                                        });
                                    }
                                    // `Admitted` without a lease is
                                    // unreachable (the gate hands one out
                                    // with the decision), but matching it
                                    // keeps this arm total rather than
                                    // relying on an `unreachable!`.
                                    (DescDecision::Admitted, None) => {
                                        tracing::warn!(
                                            "vlm: gate admitted without a lease;                                              frame skipped"
                                        );
                                    }
                                }
                            }
                            // ONVIF MotionAlarm rides the same accepted
                            // edge (no NVR subscribed = no-op).
                            if let Some(events) = onvif_events_slot
                                .lock()
                                .expect("onvif events slot lock")
                                .clone()
                            {
                                events.publish_event(web::onvif_alarm::motion_alarm_event(
                                    &sig.camera_id,
                                    sig.targets,
                                ));
                            }
                            if alarm_notify_gate.load(std::sync::atomic::Ordering::SeqCst) {
                                let notifier =
                                    notifier_slot.lock().expect("gb notifier slot lock").clone();
                                if let Some(n) = notifier {
                                    // 2022 standard table: priority 4, video
                                    // alarm (method 5), motion target (type 2).
                                    let sent = n.send_alarm(
                                        "4",
                                        "5",
                                        &gb28181_rs::client::format_gb_time_ms(now_ms),
                                        "2",
                                        "motion target detected",
                                    );
                                    if !sent {
                                        tracing::debug!("alarm notify: no active subscription");
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "ai: SSE bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// Aggregate one frame's face hits into the grounding-feed label text:
/// named faces counted per name (`甲×2`), unknown faces as one
/// `未识别×N` entry, joined with `、`. `None` when the frame had no
/// faces (nothing to record).
fn face_label_text(hits: &[FaceHit]) -> Option<String> {
    if hits.is_empty() {
        return None;
    }
    let mut named: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut unknown = 0usize;
    for h in hits {
        match &h.name {
            Some(n) => {
                *named.entry(n.clone()).or_default() += 1;
            }
            None => unknown += 1,
        }
    }
    let mut text: Vec<String> = named.into_iter().map(|(n, c)| format!("{n}×{c}")).collect();
    if unknown > 0 {
        text.push(format!("未识别×{unknown}"));
    }
    Some(text.join("、"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(name: Option<&str>) -> FaceHit {
        FaceHit {
            name: name.map(String::from),
            score: 0.9,
            bbox: (0.0, 0.0, 1.0, 1.0),
        }
    }

    #[test]
    fn face_label_text_counts_named_and_unknown() {
        assert_eq!(
            face_label_text(&[hit(Some("甲")), hit(Some("甲")), hit(Some("乙")), hit(None)]),
            Some("乙×1、甲×2、未识别×1".to_string())
        );
    }

    #[test]
    fn face_label_text_known_only_and_unknown_only() {
        assert_eq!(
            face_label_text(&[hit(Some("甲"))]),
            Some("甲×1".to_string())
        );
        assert_eq!(
            face_label_text(&[hit(None), hit(None)]),
            Some("未识别×2".to_string())
        );
    }

    #[test]
    fn face_label_text_empty_frame_is_none() {
        assert_eq!(face_label_text(&[]), None);
    }
}
