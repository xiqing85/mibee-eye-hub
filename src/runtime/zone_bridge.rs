//! Zone-event bridge — the former inline `main()` block, moved verbatim
//! (roadmap #6, M1 slice 4). One spawned task consumes the AI engine's
//! track broadcast: tracked detections × user-drawn zones → intrusion /
//! loiter / line-cross events → grounding records, SSE `zone_event`,
//! desktop notification and GB28181 Alarm NOTIFY (same pinned triple;
//! description names the zone).

use std::collections::HashMap;
use std::sync::Arc;

use streaming::ai::TrackEvent;
use web::grounding::GroundingState;
use web::routes::events::EventBus;
use web::zones::SharedZones;

use crate::desktop::Desktop;

/// Everything the former inline block captured.
pub struct ZoneBridge {
    pub events: tokio::sync::broadcast::Receiver<TrackEvent>,
    pub bridge_tx: Arc<EventBus>,
    pub zones: SharedZones,
    pub grounding: Arc<GroundingState>,
    pub notifier_slot: Arc<std::sync::Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>>,
    pub alarm_notify_gate: Arc<std::sync::atomic::AtomicBool>,
    pub desktop: Arc<Desktop>,
}

impl ZoneBridge {
    /// Spawn the bridge task. Fields destructure onto the original
    /// block-local names so the moved body compiles unchanged.
    pub fn spawn(self) {
        let ZoneBridge {
            events: mut track_events,
            bridge_tx,
            zones: zones_ref,
            grounding: grounding_zones,
            notifier_slot,
            alarm_notify_gate,
            desktop: desktop_zone_alarm,
        } = self;
        tokio::spawn(async move {
            let mut engines: HashMap<String, streaming::ai::zones::ZoneEngine> = HashMap::new();
            loop {
                match track_events.recv().await {
                    Ok(ev) => {
                        let zones = zones_ref.read().clone();
                        if zones.is_empty() {
                            continue;
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let engine = engines.entry(ev.camera_id.clone()).or_default();
                        for ze in engine.update(
                            &ev.camera_id,
                            zones.get(&ev.camera_id).map_or(&[], Vec::as_slice),
                            &ev.tracks,
                            now_ms,
                        ) {
                            let kind = zone_event_kind(ze.event);
                            tracing::info!(
                                camera = %ze.camera_id,
                                zone = %ze.zone,
                                kind,
                                track = ze.track_id,
                                "zones: event"
                            );
                            grounding_zones.record_zone_event(
                                &ze.camera_id,
                                ze.timestamp_ms,
                                &ze.zone,
                            );
                            let _ = bridge_tx.send(web::routes::events::CameraEvent::ZoneEvent {
                                camera_id: ze.camera_id.clone(),
                                zone: ze.zone.clone(),
                                event: kind.to_string(),
                                track_id: ze.track_id,
                                label: ze.label.clone(),
                                timestamp_ms: ze.timestamp_ms,
                            });
                            // Desktop notification (SPEC appendix A #42).
                            desktop_zone_alarm.notify_alarm("zone", 1, Some(&ze.zone));
                            if alarm_notify_gate.load(std::sync::atomic::Ordering::SeqCst) {
                                let notifier =
                                    notifier_slot.lock().expect("gb notifier slot lock").clone();
                                if let Some(n) = notifier {
                                    let description = format!("zone event: {} ({kind})", ze.zone);
                                    let sent = n.send_alarm(
                                        "4",
                                        "5",
                                        &gb28181_rs::client::format_gb_time_ms(ze.timestamp_ms),
                                        "2",
                                        &description,
                                    );
                                    if !sent {
                                        tracing::debug!("alarm notify: no active subscription");
                                    }
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "zones: bridge lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// SSE/wire name for one zone event (line crossings split by direction).
fn zone_event_kind(kind: streaming::ai::zones::ZoneEventKind) -> &'static str {
    match kind {
        streaming::ai::zones::ZoneEventKind::Intrusion => "intrusion",
        streaming::ai::zones::ZoneEventKind::Loiter => "loiter",
        streaming::ai::zones::ZoneEventKind::LineCross { forward: true } => "line_cross_forward",
        streaming::ai::zones::ZoneEventKind::LineCross { forward: false } => "line_cross_backward",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_event_kind_covers_all_variants() {
        use streaming::ai::zones::ZoneEventKind;
        assert_eq!(zone_event_kind(ZoneEventKind::Intrusion), "intrusion");
        assert_eq!(zone_event_kind(ZoneEventKind::Loiter), "loiter");
        assert_eq!(
            zone_event_kind(ZoneEventKind::LineCross { forward: true }),
            "line_cross_forward"
        );
        assert_eq!(
            zone_event_kind(ZoneEventKind::LineCross { forward: false }),
            "line_cross_backward"
        );
    }
}
