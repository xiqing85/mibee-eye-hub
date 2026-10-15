//! Model manager endpoints (SPEC §4.9, capability `model_manager`): the
//! catalog of every AI capability's runnable models, async downloads with
//! SSE progress, activation (restart-class persists `model.<cap>` into
//! the settings bag — the boot overlay applies it to engine config), and
//! deletion of installed files.
//!
//! The `ai` capability is the one `apply:"immediate"` entry: activation
//! delegates to the §4.6 hot switch against the AI registry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use serde_json::json;
use sqlx::SqlitePool;

use streaming::ai::AiEngine;
use streaming::models::{self, CapabilitySpec, CatalogModel, DownloadManager, StartError};

use crate::db;
use crate::errors::ApiError;
use crate::routes::events::{CameraEvent, EventBus};
use security::middleware::AuthenticatedUser;

/// Shared model-manager state: the download engine plus where models
/// live and what the TOML-config defaults selected at boot.
pub struct ModelManager {
    pub downloads: Arc<DownloadManager>,
    /// Models root (`[models] dir`, cwd-relative like the engine paths).
    pub root: PathBuf,
    /// Boot-time selection per capability (engine-config path match) —
    /// the answer until a web activation persists `model.<cap>`.
    pub defaults: HashMap<String, String>,
}

impl ModelManager {
    pub fn new(root: PathBuf, defaults: HashMap<String, String>) -> Self {
        Self {
            downloads: Arc::new(DownloadManager::new()),
            root,
            defaults,
        }
    }
}

fn model_json(root: &std::path::Path, m: &CatalogModel, active: bool) -> serde_json::Value {
    json!({
        "id": m.id,
        "name": m.name,
        "size_bytes": models::model_size(m),
        "languages": m.languages,
        "license": m.license,
        "notes": m.notes,
        "installed": models::is_installed(root, m),
        "active": active,
        "downloadable": models::downloadable(m),
    })
}

fn capability_json(
    mgr: &ModelManager,
    selected: &HashMap<String, String>,
    cap: &CapabilitySpec,
) -> serde_json::Value {
    let active = selected
        .get(cap.id)
        .cloned()
        .or_else(|| mgr.defaults.get(cap.id).cloned());
    let models: Vec<serde_json::Value> = cap
        .models
        .iter()
        .map(|m| model_json(&mgr.root, m, active.as_deref() == Some(m.id)))
        .collect();
    json!({
        "id": cap.id,
        "label": cap.label,
        "apply": cap.apply,
        "active": active,
        "models": models,
    })
}

/// `GET /api/models` — the full catalog with live state.
#[tracing::instrument(skip_all)]
pub async fn get_models(
    Extension(mgr): Extension<Arc<ModelManager>>,
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let selected = selection_rows(&db).await;
    let capabilities: Vec<serde_json::Value> = models::catalog()
        .iter()
        .map(|cap| capability_json(&mgr, &selected, cap))
        .collect();
    Ok(Json(json!({
        "dir": mgr.root.display().to_string(),
        "capabilities": capabilities,
        "tasks": mgr.downloads.tasks(),
    })))
}

/// `GET /api/models/tasks` — task list (SPEC §4.9).
#[tracing::instrument(skip_all)]
pub async fn list_tasks(
    Extension(mgr): Extension<Arc<ModelManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Json<serde_json::Value> {
    Json(json!({ "tasks": mgr.downloads.tasks() }))
}

/// `POST /api/models/{capability}/{model_id}/download` — 202 + task, or
/// 409 with `force` semantics per SPEC.
#[tracing::instrument(skip_all)]
pub async fn download_model(
    Path((capability, model_id)): Path<(String, String)>,
    Extension(mgr): Extension<Arc<ModelManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let force = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("force").and_then(|f| f.as_bool()).to_owned())
        .unwrap_or(false);
    match mgr
        .downloads
        .start(mgr.root.clone(), &capability, &model_id, force)
    {
        Ok(task) => Ok((StatusCode::ACCEPTED, Json(json!({ "task": task })))),
        Err(StartError::NotFound) => Err(ApiError::not_found(format!(
            "unknown capability or model: {capability}/{model_id}"
        ))),
        Err(StartError::AlreadyInstalled) => Err(ApiError::conflict(
            "already installed (force=true to re-download)",
        )),
        Err(StartError::TaskRunning) => Err(ApiError::conflict(
            "a task is already running for this model",
        )),
        Err(StartError::InsufficientDisk { need, avail }) => Err(ApiError::new(
            crate::errors::ApiErrorKind::InsufficientStorage,
            format!("insufficient disk: need {need} bytes, {avail} available"),
        )),
    }
}

/// `POST /api/models/tasks/{task_id}/cancel`.
#[tracing::instrument(skip_all)]
pub async fn cancel_task(
    Path(task_id): Path<String>,
    Extension(mgr): Extension<Arc<ModelManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match mgr.downloads.cancel(&task_id) {
        Ok(_) => Ok(Json(json!({ "status": "canceled" }))),
        Err(models::CancelError::NotFound) => Err(ApiError::not_found("no such task")),
        Err(models::CancelError::Finished) => Err(ApiError::conflict("task already finished")),
    }
}

/// `POST /api/models/{capability}/{model_id}/activate`.
#[tracing::instrument(skip_all)]
pub async fn activate_model(
    Path((capability, model_id)): Path<(String, String)>,
    Extension(mgr): Extension<Arc<ModelManager>>,
    Extension(ai): Extension<Arc<AiEngine>>,
    Extension(db): Extension<SqlitePool>,
    Extension(event_tx): Extension<Arc<EventBus>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cap = models::capability(&capability)
        .ok_or_else(|| ApiError::not_found(format!("unknown capability: {capability}")))?;
    let model = models::find(&capability, &model_id)
        .ok_or_else(|| ApiError::not_found(format!("unknown model: {model_id}")))?;
    if !models::is_installed(&mgr.root, model) {
        return Err(ApiError::conflict("model not installed"));
    }
    if cap.apply == "immediate" {
        // The detection capability: §4.6 hot switch via the registry.
        return hot_switch(ai, event_tx, &model_id).await;
    }
    let selected = selection_rows(&db).await;
    if selected.get(capability.as_str()).map(String::as_str) == Some(model_id.as_str()) {
        return Ok(Json(json!({ "applied": "restart", "active": model_id })));
    }
    db::set_setting(&db, &format!("model.{capability}"), &model_id)
        .await
        .map_err(|e| ApiError::internal(format!("persist selection: {e}")))?;
    tracing::info!(capability = %capability, model = %model_id, "models: selection activated (restart pending)");
    Ok(Json(json!({ "applied": "restart", "active": model_id })))
}

/// The §4.6 hot-switch core (same contract as `/api/ai/models/{id}/activate`).
async fn hot_switch(
    ai: Arc<AiEngine>,
    event_tx: Arc<EventBus>,
    id: &str,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !ai.is_active() {
        return Err(ApiError::not_implemented("AI engine not running"));
    }
    let Some(spec) = ai.registry().read().find(id).map(|s| s.path) else {
        return Err(ApiError::not_found(format!("unknown model id: {id}")));
    };
    let built = match ai.load_for_blocking(&spec).await {
        Ok(b) => b,
        Err(streaming::ai::registry::ActivateError::Unavailable(msg)) => {
            return Err(ApiError::conflict(msg));
        }
        Err(streaming::ai::registry::ActivateError::LoadFailed(msg)) => {
            return Err(ApiError::internal(format!("load model {id}: {msg}")));
        }
    };
    ai.set_detector(id, built.0);
    let _ = event_tx.send(CameraEvent::AiModelChanged {
        model: id.to_string(),
    });
    Ok(Json(json!({ "applied": "immediate", "active": id })))
}

/// `DELETE /api/models/{capability}/{model_id}` — remove the installed
/// files. Active models (settings selection OR boot default) are guarded.
#[tracing::instrument(skip_all)]
pub async fn delete_model(
    Path((capability, model_id)): Path<(String, String)>,
    Extension(mgr): Extension<Arc<ModelManager>>,
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<StatusCode, ApiError> {
    let model = models::find(&capability, &model_id)
        .ok_or_else(|| ApiError::not_found(format!("unknown model: {model_id}")))?;
    if !models::is_installed(&mgr.root, model) {
        return Err(ApiError::not_found("model not installed"));
    }
    let selected = selection_rows(&db).await;
    let active = selected
        .get(capability.as_str())
        .cloned()
        .or_else(|| mgr.defaults.get(capability.as_str()).cloned());
    if active.as_deref() == Some(model_id.as_str()) {
        return Err(ApiError::conflict(
            "cannot delete the active model; activate another first",
        ));
    }
    // Shared FILE sets (melo serves tts.zh AND tts.en/melo-en — the same
    // files under one dir): refuse when another capability's active
    // selection uses any of these files. Same-dir-but-distinct-files
    // (face detect/recog) is fine.
    for other_cap in models::catalog() {
        if other_cap.id == capability {
            continue;
        }
        let other_active = selected
            .get(other_cap.id)
            .cloned()
            .or_else(|| mgr.defaults.get(other_cap.id).cloned());
        let Some(other_id) = other_active else {
            continue;
        };
        let Some(other_model) = models::find(other_cap.id, &other_id) else {
            continue;
        };
        let shares_files = other_model.files.iter().any(|of| {
            model.files.iter().any(|fl| fl.path == of.path) && other_model.dir == model.dir
        });
        if shares_files && other_cap.apply == "restart" {
            return Err(ApiError::conflict(format!(
                "files are shared with the active {} model",
                other_cap.id
            )));
        }
    }
    for fl in model.files {
        let target = mgr.root.join(model.dir).join(fl.path);
        let _ = std::fs::remove_file(&target);
    }
    // Drop now-empty subdirectories (leave the models root alone).
    if model.dir != "." {
        let dir = mgr.root.join(model.dir);
        let empty = std::fs::read_dir(&dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if empty {
            let _ = std::fs::remove_dir(&dir);
        }
    }
    tracing::info!(capability = %capability, model = %model_id, "models: files deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// The persisted `model.<capability>` settings rows (validated on write;
/// boot overlay skips garbage, so stale rows are cosmetic only).
async fn selection_rows(db: &SqlitePool) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(rows) = db::list_settings(db).await else {
        return out;
    };
    for (key, value) in rows {
        if let Some(cap) = key.strip_prefix("model.")
            && models::find(cap, &value).is_some()
        {
            out.insert(cap.to_string(), value);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{delete, get, post};
    use tower::ServiceExt;

    async fn app_with(root: &std::path::Path) -> (Router, sqlx::SqlitePool, Arc<ModelManager>) {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.expect("migrations");
        let ai = Arc::new(AiEngine::from_parts(
            streaming::ai::AiConfig::default(),
            None,
        ));
        let mgr = Arc::new(ModelManager::new(root.to_path_buf(), HashMap::new()));
        let bus = Arc::new(crate::routes::events::new_event_bus());
        let app = Router::new()
            .route("/api/models", get(get_models))
            .route("/api/models/tasks", get(list_tasks))
            .route("/api/models/{cap}/{id}/download", post(download_model))
            .route("/api/models/{cap}/{id}", delete(delete_model))
            .route("/api/models/{cap}/{id}/activate", post(activate_model))
            .route("/api/models/tasks/{id}/cancel", post(cancel_task))
            .layer(Extension(Arc::clone(&mgr)))
            .layer(Extension(pool.clone()))
            .layer(Extension(ai))
            .layer(Extension(bus))
            .layer(Extension(AuthenticatedUser("tester".to_string())));
        (app, pool, mgr)
    }

    fn fake_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nb-models-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("face")).unwrap();
        dir
    }

    fn install_yunet(dir: &std::path::Path) {
        std::fs::write(
            dir.join("face/face_detection_yunet_2023mar.onnx"),
            vec![0u8; 232589],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn catalog_lists_installed_state_and_tasks() {
        let dir = fake_root(&format!("{}-{}", std::module_path!(), line!()));
        install_yunet(&dir);
        let (app, _pool, _mgr) = app_with(&dir).await;
        let res = app
            .oneshot(Request::get("/api/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap();
        let caps = body["capabilities"].as_array().unwrap();
        let llm = caps.iter().find(|c| c["id"] == "llm").unwrap();
        let m0 = llm["models"][0].clone();
        assert_eq!(m0["id"], "qwen3-0.6b-q8_0");
        assert_eq!(
            m0["installed"],
            serde_json::json!(false),
            "wrong size = not installed"
        );
        assert_eq!(m0["downloadable"], serde_json::json!(true));
        assert_eq!(m0["active"], serde_json::json!(false));
        let face = caps.iter().find(|c| c["id"] == "face.detect").unwrap();
        assert_eq!(face["models"][0]["downloadable"], serde_json::json!(true));
        assert_eq!(face["models"][0]["installed"], serde_json::json!(true));
        assert_eq!(face["apply"], "restart");
        assert!(body["tasks"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn download_unknown_404_and_nosource_404() {
        let dir = fake_root(&format!("{}-{}", std::module_path!(), line!()));
        let (app, _pool, _mgr) = app_with(&dir).await;
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/models/llm/nope/download")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app
            .oneshot(
                Request::post("/api/models/ocr/ppocr-ch-v4det-v5rec/download")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "no-source model");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn activate_restart_class_persists_and_is_idempotent() {
        let dir = fake_root(&format!("{}-{}", std::module_path!(), line!()));
        install_yunet(&dir);
        let (app, pool, _mgr) = app_with(&dir).await;
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/models/face.detect/yunet-2023mar/activate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 65536).await.unwrap())
                .unwrap();
        assert_eq!(body["applied"], "restart");
        // Persisted as a model.<cap> settings row.
        let saved = db::get_setting(&pool, "model.face.detect")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(saved, "yunet-2023mar");
        // Idempotent second call.
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/models/face.detect/yunet-2023mar/activate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // Not installed → 409.
        let res = app
            .oneshot(
                Request::post("/api/models/face.recog/sface-2021dec/activate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_guards_active_and_missing() {
        let dir = fake_root(&format!("{}-{}", std::module_path!(), line!()));
        install_yunet(&dir);
        let (app, pool, _mgr) = app_with(&dir).await;
        db::set_setting(&pool, "model.face.detect", "yunet-2023mar")
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::delete("/api/models/face.detect/yunet-2023mar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT, "active model guarded");
        let res = app
            .clone()
            .oneshot(
                Request::delete("/api/models/face.recog/sface-2021dec")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "not installed");
        // Unselected but installed → deleted with the file.
        std::fs::write(
            dir.join("face/face_recognition_sface_2021dec.onnx"),
            vec![0u8; 38696353],
        )
        .unwrap();
        let res = app
            .oneshot(
                Request::delete("/api/models/face.recog/sface-2021dec")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(
            !dir.join("face/face_recognition_sface_2021dec.onnx")
                .exists()
        );
        // yunet survives (still active) and the shared face/ dir stays.
        assert!(dir.join("face/face_detection_yunet_2023mar.onnx").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
