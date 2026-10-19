//! Hot-plug watch — the former inline `main()` block, moved verbatim
//! (roadmap #6, M1 slice 5). Listens to kernel netlink uevents for the
//! video4linux subsystem: ADD re-runs auto-discovery (never auto-starts
//! the stream); REMOVE marks the camera offline in the DB, gracefully
//! stops its active stream (flushing any in-progress recording segment)
//! and broadcasts the SSE event. Unlike the bridges, this task returns
//! its JoinHandle for the shutdown join set.

use std::sync::Arc;

use sqlx::SqlitePool;
use web::routes::events::EventBus;

use web::stream_manager::StreamManager;

/// Everything the former inline block captured.
pub struct HotplugWatch {
    pub bridge_tx: Arc<EventBus>,
    pub pool: SqlitePool,
    pub stream_manager: Arc<StreamManager>,
}

impl HotplugWatch {
    /// Spawn the watch task; the handle belongs in `protocol_handles`.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let HotplugWatch {
            bridge_tx: hotplug_tx,
            pool: hotplug_db,
            stream_manager: hotplug_stream_manager,
        } = self;
        tokio::spawn(async move {
            // Diagnostic escape hatch: MIBEE_DISABLE_HOTPLUG=1 skips the netlink
            // monitor entirely (used to bisect runtime starvation).
            if std::env::var_os("MIBEE_DISABLE_HOTPLUG").is_some_and(|v| v != "0") {
                tracing::warn!("hot-plug monitor disabled via MIBEE_DISABLE_HOTPLUG");
                return;
            }
            let monitor = match capture::hotplug::HotplugMonitor::new() {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to start hot-plug monitor; camera plug/unplug will not be detected");
                    return;
                }
            };

            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
            tokio::spawn(async move {
                monitor.run(tx).await;
            });

            while let Some(event) = rx.recv().await {
                match event {
                    capture::hotplug::HotplugEvent::Added { device_index, .. } => {
                        tracing::info!(device_index, "camera plugged in, running auto-discovery");
                        // Re-run discovery. Does NOT auto-start (user decision).
                        match web::db::auto_discover_cameras(&hotplug_db).await {
                            Ok(n) if n > 0 => {
                                // Find the newly-added camera to broadcast event.
                                let cameras =
                                    web::db::list_cameras(&hotplug_db).await.unwrap_or_default();
                                for cam in cameras
                                    .iter()
                                    .filter(|c| matches_device_index(c, device_index))
                                {
                                    let _ = hotplug_tx.send(
                                        web::routes::events::CameraEvent::CameraAdded {
                                            camera_id: cam.id.clone(),
                                            device_index,
                                            name: cam.name.clone(),
                                        },
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "auto-discovery after hot-plug failed")
                            }
                        }
                    }
                    capture::hotplug::HotplugEvent::Removed { device_index } => {
                        tracing::info!(device_index, "camera unplugged, marking offline");
                        match web::db::mark_cameras_offline_by_device_index(
                            &hotplug_db,
                            device_index as i64,
                        )
                        .await
                        {
                            Ok(camera_ids) => {
                                for cam_id in &camera_ids {
                                    // Gracefully stop the active stream (flushes FileOutput).
                                    if hotplug_stream_manager.has_stream(cam_id).await {
                                        tracing::info!(camera_id = %cam_id, "stopping stream for unplugged camera");
                                        if let Err(e) =
                                            hotplug_stream_manager.stop_stream(cam_id).await
                                        {
                                            tracing::warn!(camera_id = %cam_id, error = %e, "failed to stop stream for unplugged camera");
                                        }
                                    }
                                    // Broadcast SSE event.
                                    let _ = hotplug_tx.send(
                                        web::routes::events::CameraEvent::CameraOfflined {
                                            camera_id: cam_id.clone(),
                                            device_index,
                                        },
                                    );
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, device_index, "failed to mark camera offline");
                            }
                        }
                    }
                }
            }
        })
    }
}

/// The ADD-path camera filter: USB cameras whose persisted config points
/// at the device index the kernel just announced.
fn matches_device_index(cam: &web::db::CameraRow, device_index: u32) -> bool {
    cam.camera_type == "usb"
        && cam.config.get("device_index").and_then(|v| v.as_u64()) == Some(device_index as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(camera_type: &str, device_index: Option<u64>) -> web::db::CameraRow {
        web::db::CameraRow {
            id: "c1".into(),
            name: "cam".into(),
            camera_type: camera_type.into(),
            config: serde_json::json!({ "device_index": device_index }),
            status: "stopped".into(),
            created_at: String::new(),
            updated_at: String::new(),
            offline_since: None,
        }
    }

    #[test]
    fn matches_device_index_requires_usb_and_exact_index() {
        assert!(matches_device_index(&cam("usb", Some(2)), 2));
        assert!(
            !matches_device_index(&cam("usb", Some(2)), 3),
            "index mismatch"
        );
        assert!(!matches_device_index(&cam("onvif", Some(2)), 2), "non-usb");
        assert!(!matches_device_index(&cam("usb", None), 2), "missing key");
    }
}
