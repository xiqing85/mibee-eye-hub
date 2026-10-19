//! Protocol boot assembly — the former inline `main()` region, moved
//! verbatim (roadmap #6, M1 final slice).
//!
//! [`boot_protocol_runtime`] checks the DB for enabled protocols and
//! starts them through the `ProtocolRuntime` (hot-toggle ONVIF / GB28181
//! / RTMP via the Web UI); [`protocol_config_store`] builds the REST
//! config store the protocol settings API reads.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use web::protocol_runtime::ProtocolRuntime;
use web::stream_manager::StreamManager;

use crate::config::AppConfig;

/// The per-protocol boot config: the DB row (Web UI edits) wins; a fresh
/// install falls back to the TOML section serialized to JSON.
pub fn boot_config(
    db: Option<serde_json::Value>,
    toml_value: serde_json::Value,
) -> serde_json::Value {
    db.unwrap_or(toml_value)
}

/// `enabled` flag of a protocol config row — absent or non-bool reads as
/// off (protocols are opt-in).
pub fn protocol_enabled(cfg: &serde_json::Value) -> bool {
    cfg.get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The REST config store seeded from the TOML sections (the Web UI edits
/// the DB copy live; this map is the fallback/initial view).
pub fn protocol_config_store(config: &AppConfig) -> HashMap<String, serde_json::Value> {
    let mut store = HashMap::new();
    store.insert("onvif".into(), serde_json::to_value(&config.onvif).unwrap());
    store.insert(
        "gb28181".into(),
        serde_json::to_value(&config.gb28181).unwrap(),
    );
    store.insert(
        "rtmp_push".into(),
        serde_json::to_value(&config.rtmp_push).unwrap(),
    );
    store
}

/// Shared control slots the ProtocolRuntime hot-toggles at runtime.
pub struct ProtocolShares {
    pub notifier_slot: Arc<std::sync::Mutex<Option<Arc<gb28181_rs::subscribe::DeviceNotifier>>>>,
    pub alarm_notify_gate: Arc<std::sync::atomic::AtomicBool>,
    pub recording_paused: Arc<std::sync::atomic::AtomicBool>,
    pub force_idr: Arc<std::sync::atomic::AtomicBool>,
    pub gb_flips: Arc<streaming::capture_source::Flips>,
    pub onvif_events_slot:
        Arc<std::sync::Mutex<Option<Arc<onvif_device_rs::events::EventsService>>>>,
}

/// Build the ProtocolRuntime and start every protocol enabled in the DB
/// (TOML fallback). Failures warn and continue — a dead protocol must
/// never take the others down.
pub async fn boot_protocol_runtime(
    shares: ProtocolShares,
    pool: &sqlx::SqlitePool,
    config: &AppConfig,
    advertised_host: &str,
    stream_manager: Arc<StreamManager>,
) -> Arc<Mutex<ProtocolRuntime>> {
    let ProtocolShares {
        notifier_slot,
        alarm_notify_gate,
        recording_paused,
        force_idr,
        gb_flips,
        onvif_events_slot,
    } = shares;
    let mut protocol_runtime = ProtocolRuntime::new_with_shares(
        notifier_slot,
        alarm_notify_gate,
        recording_paused,
        force_idr,
        gb_flips,
        onvif_events_slot,
    );

    // ONVIF
    {
        let cfg = boot_config(
            web::db::get_protocol_config(pool, "onvif")
                .await
                .ok()
                .flatten(),
            serde_json::to_value(&config.onvif).unwrap_or_default(),
        );
        if protocol_enabled(&cfg) {
            let onvif_config =
                web::protocol_runtime::build_onvif_config_from_json(&cfg, advertised_host);
            if let Err(e) = protocol_runtime
                .start_onvif(onvif_config, stream_manager.clone())
                .await
            {
                tracing::warn!(error = %e, "failed to start ONVIF at startup");
            }
        }
    }

    // GB28181
    {
        let cfg = boot_config(
            web::db::get_protocol_config(pool, "gb28181")
                .await
                .ok()
                .flatten(),
            serde_json::to_value(&config.gb28181).unwrap_or_default(),
        );
        if protocol_enabled(&cfg) {
            let gb_config = web::protocol_runtime::extract_gb28181_config(&cfg);
            if let Err(e) = protocol_runtime
                .start_gb28181(&gb_config, stream_manager.clone())
                .await
            {
                tracing::warn!(error = %e, "failed to start GB28181 at startup");
            }
        }
    }

    // RTMP (per-stream; just track enabled state)
    {
        let cfg = boot_config(
            web::db::get_protocol_config(pool, "rtmp_push")
                .await
                .ok()
                .flatten(),
            serde_json::to_value(&config.rtmp_push).unwrap_or_default(),
        );
        if protocol_enabled(&cfg)
            && let Err(e) = protocol_runtime.start_rtmp().await
        {
            tracing::warn!(error = %e, "failed to enable RTMP at startup");
        }
    }

    Arc::new(Mutex::new(protocol_runtime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_config_db_row_wins_over_toml() {
        let db = serde_json::json!({ "enabled": true, "source": "db" });
        let toml = serde_json::json!({ "enabled": false, "source": "toml" });
        let merged = boot_config(Some(db), toml);
        assert_eq!(merged["source"], "db");
    }

    #[test]
    fn boot_config_falls_back_to_toml_when_no_db_row() {
        let toml = serde_json::json!({ "enabled": false, "source": "toml" });
        let merged = boot_config(None, toml.clone());
        assert_eq!(merged, toml);
    }

    #[test]
    fn protocol_enabled_reads_flag_and_defaults_off() {
        assert!(protocol_enabled(&serde_json::json!({ "enabled": true })));
        assert!(!protocol_enabled(&serde_json::json!({ "enabled": false })));
        assert!(!protocol_enabled(&serde_json::json!({})), "absent = off");
        assert!(
            !protocol_enabled(&serde_json::json!({ "enabled": "yes" })),
            "non-bool = off"
        );
    }

    #[test]
    fn protocol_config_store_covers_the_three_protocols() {
        let store = protocol_config_store(&AppConfig::default());
        assert_eq!(store.len(), 3);
        for key in ["onvif", "gb28181", "rtmp_push"] {
            assert!(store.contains_key(key), "missing {key}");
            assert!(
                store[key].get("enabled").is_some(),
                "{key} section must serialize an enabled flag"
            );
        }
    }
}
