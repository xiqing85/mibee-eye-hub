//! Boot-time model-manager assembly — the former inline `main()` region,
//! moved verbatim (roadmap #6, M1 final slice).
//!
//! The ModelManager (SPEC §4.9) owns the download engine and the
//! boot-time default selection per capability; download progress rides
//! the SSE bus as `model_task` events.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use web::routes::events::EventBus;

use crate::config::AppConfig;

/// The defaults map the ModelManager advertises: the boot-time selection
/// derived from the config paths, with the AI engine's active registry
/// model winning over the configured `ai.model` when inference is up —
/// the running model is the truth, not the file path.
pub fn resolve_model_defaults(
    config: &AppConfig,
    ai_active_model: Option<&str>,
) -> HashMap<String, String> {
    let mut defaults = crate::config::default_model_selection(config);
    if let Some(active) = ai_active_model {
        defaults.insert("ai".to_string(), active.to_string());
    }
    defaults
}

/// Build the ModelManager and spawn the download-progress forwarder onto
/// the SSE bus.
pub fn spawn_model_manager(
    config: &AppConfig,
    ai_active_model: Option<&str>,
    event_tx: Arc<EventBus>,
) -> Arc<web::routes::models_api::ModelManager> {
    let model_defaults = resolve_model_defaults(config, ai_active_model);
    let model_manager = Arc::new(web::routes::models_api::ModelManager::new(
        PathBuf::from(&config.models.dir),
        model_defaults,
    ));
    let mut task_rx = model_manager.downloads.subscribe();
    tokio::spawn(async move {
        while let Ok(snapshot) = task_rx.recv().await {
            let _ = event_tx.send(web::routes::events::CameraEvent::ModelTask { task: snapshot });
        }
    });
    model_manager
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_overrides_ai_with_active_model() {
        let config = AppConfig::default();
        let without = resolve_model_defaults(&config, None);
        let with = resolve_model_defaults(&config, Some("nanodet-custom"));
        assert_eq!(
            with.get("ai").map(String::as_str),
            Some("nanodet-custom"),
            "active registry model wins over the configured path"
        );
        assert_ne!(without.get("ai"), with.get("ai"));
        // The other capabilities are untouched by the AI override.
        for key in without.keys() {
            if key != "ai" {
                assert_eq!(without.get(key), with.get(key), "{key} drifted");
            }
        }
    }

    #[test]
    fn ai_key_exists_only_through_the_active_model_override() {
        // `default_model_selection` skips apply-immediate capabilities
        // (detection rides the registry), so without an active engine the
        // map has no "ai" key — exactly the former inline `if is_active()`
        // gate in main().
        let config = AppConfig::default();
        let inactive = resolve_model_defaults(&config, None);
        assert!(
            !inactive.contains_key("ai"),
            "selection must not fabricate an ai entry on its own"
        );
        let active = resolve_model_defaults(&config, Some("nanodet-plus-m-320"));
        assert_eq!(
            active.get("ai").map(String::as_str),
            Some("nanodet-plus-m-320")
        );
    }
}
