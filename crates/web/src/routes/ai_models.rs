//! Model registry endpoints (SPEC v1 §4.6, capabilities `ai_models` /
//! `ai_upload`): list, hot-switch, runtime upload and delete — mirroring
//! the Pi implementations so all MiBee cameras behave identically.
//!
//! The active choice persists as the `ai.model` db setting (the `[ai]`
//! TOML section stays boot defaults; main.rs overlays the setting at
//! startup).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sqlx::SqlitePool;
use streaming::ai::AiEngine;
use streaming::ai::registry::{ActivateError, UPLOAD_MAX_BYTES, valid_model_id};

use security::middleware::AuthenticatedUser;

use super::events::{CameraEvent, EventBus};

/// `GET /api/ai/models` — the registry + active model (+ upload metadata
/// when `[ai] allow_upload` is on).
#[tracing::instrument(skip_all)]
pub async fn get_models(
    Extension(ai): Extension<Arc<AiEngine>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Json<serde_json::Value> {
    let registry = ai.registry();
    let reg = registry.read();
    let models: Vec<serde_json::Value> = reg
        .list()
        .iter()
        .map(|spec| {
            json!({
                "id": spec.id,
                "family": spec.family,
                "input": spec.input,
                "source": spec.source,
                "available": streaming::ai::registry::is_available(&spec.path),
            })
        })
        .collect();
    let mut payload = json!({
        "active": ai.active_model(),
        "models": models,
    });
    if ai.config().allow_upload {
        payload["upload"] = json!({
            "allowed": true,
            "max_bytes": UPLOAD_MAX_BYTES,
        });
    }
    drop(reg);
    Json(payload)
}

/// `POST /api/ai/models/{id}/activate` — hot-switch (SPEC §4.6). The new
/// detector is fully constructed before the slot is touched (rollback by
/// construction); the choice persists to the `ai.model` db setting and an
/// `ai_model_changed` SSE event is broadcast.
#[tracing::instrument(skip_all)]
pub async fn activate_model(
    Path(id): Path<String>,
    Extension(ai): Extension<Arc<AiEngine>>,
    Extension(db): Extension<SqlitePool>,
    Extension(event_tx): Extension<Arc<EventBus>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Response {
    if !ai.is_active() {
        return api_error(StatusCode::NOT_IMPLEMENTED, "AI engine not running");
    }
    let spec = match ai.registry().read().find(&id) {
        Some(spec) => spec,
        None => return api_error(StatusCode::NOT_FOUND, &format!("unknown model id: {id}")),
    };

    let built = match ai.load_for_blocking(&spec.path).await {
        Ok(built) => built,
        Err(ActivateError::Unavailable(msg)) => return api_error(StatusCode::CONFLICT, &msg),
        Err(ActivateError::LoadFailed(msg)) => {
            return api_error(StatusCode::INTERNAL_SERVER_ERROR, &msg);
        }
    };

    ai.set_detector(&id, built.0);
    if let Err(e) = crate::db::set_setting(&db, "ai.model", &id).await {
        tracing::error!(error = %e, "failed to persist ai.model");
    }
    let _ = event_tx.send(CameraEvent::AiModelChanged { model: id.clone() });
    (
        StatusCode::OK,
        Json(json!({ "active": id, "applied": "immediate" })),
    )
        .into_response()
}

/// `POST /api/ai/models/{id}` — upload a model (capability `ai_upload`):
/// multipart `family` + `file`. The file is fully session-loaded and
/// shape-validated before entering the registry; failures leave no trace.
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn upload_model(
    Path(id): Path<String>,
    Extension(ai): Extension<Arc<AiEngine>>,
    Extension(_user): Extension<AuthenticatedUser>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    if !ai.config().allow_upload {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "model upload disabled ([ai] allow_upload)",
        );
    }
    if !valid_model_id(&id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid model id (want ^[a-z0-9][a-z0-9-]{0,63}$)",
        );
    }
    if ai.registry().read().find(&id).is_some() {
        return api_error(
            StatusCode::CONFLICT,
            &format!("model id already exists: {id}"),
        );
    }
    let dir = match ai.registry().read().models_dir() {
        Some(dir) => dir.to_path_buf(),
        None => {
            return api_error(
                StatusCode::NOT_IMPLEMENTED,
                "no models directory configured",
            );
        }
    };

    let mut family: Option<String> = None;
    let mut file: Option<Vec<u8>> = None;
    while let Some(field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("invalid multipart: {e}")),
    } {
        match field.name().unwrap_or_default() {
            "family" => match field.text().await {
                Ok(text) => family = Some(text),
                Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("family field: {e}")),
            },
            "file" => {
                let mut buf: Vec<u8> = Vec::new();
                let mut field = field;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if buf.len() + chunk.len() > UPLOAD_MAX_BYTES {
                                return api_error(
                                    StatusCode::PAYLOAD_TOO_LARGE,
                                    &format!("model exceeds max_bytes ({UPLOAD_MAX_BYTES})"),
                                );
                            }
                            buf.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            return api_error(StatusCode::BAD_REQUEST, &format!("file field: {e}"));
                        }
                    }
                }
                file = Some(buf);
            }
            _ => {}
        }
    }
    // This build's decoder is NanoDet-only; the family field must say so.
    if family.as_deref() != Some("nanodet") {
        return api_error(
            StatusCode::BAD_REQUEST,
            "family must be nanodet on this device",
        );
    }
    let Some(file) = file else {
        return api_error(StatusCode::BAD_REQUEST, "missing file field");
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("models dir: {e}"),
        );
    }
    let tmp = dir.join(format!(".upload-{id}.tmp"));
    if let Err(e) = std::fs::write(&tmp, &file) {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("write: {e}"));
    }
    let tmp_path = tmp.to_string_lossy().into_owned();
    let validated = match ai.load_for_blocking(&tmp_path).await {
        Ok(v) => v,
        Err(ActivateError::Unavailable(msg)) | Err(ActivateError::LoadFailed(msg)) => {
            let _ = std::fs::remove_file(&tmp);
            return api_error(
                StatusCode::BAD_REQUEST,
                &format!("model failed validation: {msg}"),
            );
        }
    };
    let input = validated.1;
    drop(validated);

    let final_path = dir.join(format!("{id}.onnx"));
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp);
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("rename: {e}"));
    }
    let spec = streaming::ai::registry::ModelSpec {
        id: id.clone(),
        family: "nanodet".into(),
        input,
        path: final_path.to_string_lossy().into_owned(),
        source: "uploaded".into(),
    };
    if let Err(e) = ai.registry().write().insert_uploaded(spec.clone()) {
        let _ = std::fs::remove_file(&final_path);
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("manifest: {e}"));
    }
    tracing::info!(model = %id, input, "ai: model uploaded");
    (
        StatusCode::CREATED,
        Json(json!({
            "id": spec.id,
            "family": spec.family,
            "input": spec.input,
            "source": spec.source,
            "available": true,
        })),
    )
        .into_response()
}

/// `DELETE /api/ai/models/{id}` — remove an uploaded model (SPEC §4.6).
#[tracing::instrument(skip_all)]
pub async fn delete_model(
    Path(id): Path<String>,
    Extension(ai): Extension<Arc<AiEngine>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Response {
    if !ai.config().allow_upload {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "model upload disabled ([ai] allow_upload)",
        );
    }
    if ai.is_active() && ai.active_model() == id {
        return api_error(
            StatusCode::CONFLICT,
            "cannot delete the active model; activate another first",
        );
    }
    let removed = ai.registry().write().remove_uploaded(&id);
    match removed {
        Some(spec) => {
            let _ = std::fs::remove_file(&spec.path);
            tracing::info!(model = %id, "ai: model removed");
            StatusCode::NO_CONTENT.into_response()
        }
        None => {
            if ai.registry().read().find(&id).is_some() {
                api_error(StatusCode::CONFLICT, "builtin models cannot be deleted")
            } else {
                api_error(StatusCode::NOT_FOUND, &format!("unknown model id: {id}"))
            }
        }
    }
}

/// SPEC §0 envelope for a failure.
fn api_error(status: StatusCode, msg: &str) -> Response {
    let code = match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        StatusCode::PAYLOAD_TOO_LARGE => "bad_request",
        StatusCode::NOT_IMPLEMENTED => "not_implemented",
        _ => "internal_error",
    };
    (
        status,
        Json(json!({ "ok": false, "error": code, "message": msg })),
    )
        .into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    struct FakeDetector;
    impl streaming::ai::AiDetector for FakeDetector {
        fn detect(&self, _jpeg: &[u8]) -> anyhow::Result<Vec<streaming::ai::Detection>> {
            Ok(vec![])
        }
        fn model_name(&self) -> &str {
            "fake.onnx"
        }
    }

    fn engine_with_factory(dir: Option<&std::path::Path>, allow_upload: bool) -> Arc<AiEngine> {
        let config = streaming::ai::AiConfig {
            allow_upload,
            ..streaming::ai::AiConfig::default()
        };
        let engine = AiEngine::from_parts(config, Some(Arc::new(FakeDetector)));
        let factory: streaming::ai::DetectorFactory = Arc::new(|_path: &str| {
            Ok((
                Arc::new(FakeDetector) as Arc<dyn streaming::ai::AiDetector>,
                416,
            ))
        });
        let engine = Arc::new(engine.with_factory(factory));
        if let Some(dir) = dir {
            *engine.registry().write() = streaming::ai::Registry::load(dir);
        }
        engine
    }

    async fn app_with(engine: Arc<AiEngine>) -> (Router, sqlx::SqlitePool, Arc<EventBus>) {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("test migrations");
        let bus = Arc::new(super::super::events::new_event_bus());
        let app = Router::new()
            .route("/api/ai/models", get(get_models))
            .route("/api/ai/models/{id}/activate", post(activate_model))
            .route(
                "/api/ai/models/{id}",
                post(upload_model).delete(delete_model),
            )
            .layer(Extension(engine))
            .layer(Extension(pool.clone()))
            .layer(Extension(bus.clone()))
            .layer(Extension(AuthenticatedUser("tester".to_string())));
        (app, pool, bus)
    }

    fn multipart_body(family: &str, content: &[u8]) -> (String, Body) {
        let boundary = "nb-test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"family\"\r\n\r\n{family}\r\n").as_bytes(),
        );
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"m.onnx\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(content);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        (
            format!("multipart/form-data; boundary={boundary}"),
            Body::from(body),
        )
    }

    /// The model-load path must not run on a runtime worker thread.
    ///
    /// Building an ONNX session reads the model file and initialises the
    /// graph — hundreds of ms to seconds for a real detector. The activate and
    /// upload handlers both call it and both are `async fn`, so a synchronous
    /// call stalls every other request sharing that worker for the duration.
    /// This test pins the property directly: on a single-threaded runtime a
    /// concurrent timer task must keep ticking while a slow load is in flight.
    /// A synchronous load starves it completely.
    #[tokio::test(flavor = "current_thread")]
    async fn model_loading_does_not_block_the_executor_thread() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let config = streaming::ai::AiConfig {
            allow_upload: true,
            ..streaming::ai::AiConfig::default()
        };
        let engine = AiEngine::from_parts(config, Some(StdArc::new(FakeDetector)));
        let factory: streaming::ai::DetectorFactory = StdArc::new(|_path: &str| {
            // ~200ms of CPU spin, so a blocking implementation is unmistakable.
            let start = std::time::Instant::now();
            let mut acc: u64 = 0;
            while start.elapsed() < Duration::from_millis(200) {
                acc = acc
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
            }
            std::hint::black_box(acc);
            Ok((
                StdArc::new(FakeDetector) as StdArc<dyn streaming::ai::AiDetector>,
                416,
            ))
        });
        let engine = StdArc::new(engine.with_factory(factory));

        let ticks = StdArc::new(AtomicUsize::new(0));
        let ticker = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                while ticks.load(Ordering::SeqCst) < 1_000 {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
        };

        let loaded = engine.load_for_blocking("models/ai/fake.onnx").await;
        let ticks_during_load = ticks.load(Ordering::SeqCst);
        ticker.abort();

        assert!(loaded.is_ok(), "the injected factory must succeed");
        assert!(
            ticks_during_load > 0,
            "the executor was starved for the whole model load \
             ({ticks_during_load} timer ticks) — the load ran on the worker thread"
        );
    }

    /// A build without the `ai` feature has no factory at all; the async
    /// wrapper must report that rather than panicking on the missing one.
    #[tokio::test]
    async fn load_for_blocking_reports_a_missing_factory_instead_of_panicking() {
        let engine = AiEngine::from_parts(
            streaming::ai::AiConfig::default(),
            Some(std::sync::Arc::new(FakeDetector)),
        );
        assert!(!engine.can_load_models());
        let Err(err) = engine.load_for_blocking("models/ai/fake.onnx").await else {
            panic!("a factoryless engine must not report success");
        };
        let streaming::ai::registry::ActivateError::LoadFailed(msg) = err else {
            panic!("expected LoadFailed, got a different variant");
        };
        assert!(
            msg.contains("factory unavailable"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn test_models_lists_registry_and_active() {
        let engine = engine_with_factory(None, false);
        let (app, _pool, _bus) = app_with(engine).await;
        let res = app
            .oneshot(Request::get("/api/ai/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active"], "nanodet-plus-m-320");
        let ids: Vec<&str> = json["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert!(ids.contains(&"nanodet-plus-m-320"));
        assert!(ids.contains(&"nanodet-plus-m-416"));
        assert!(json.get("upload").is_none(), "no metadata when disabled");
    }

    #[tokio::test]
    async fn test_upload_activate_delete_roundtrip() {
        let dir = std::env::temp_dir().join(format!("nb-ai-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = engine_with_factory(Some(&dir), true);
        let (app, pool, bus) = app_with(engine.clone()).await;
        let mut sse = bus.subscribe();

        let (ctype, body) = multipart_body("nanodet", b"fake-onnx-bytes");
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/my-uploaded-model")
                    .header("content-type", ctype)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source"], "uploaded");
        assert_eq!(json["input"], 416);
        assert!(dir.join("my-uploaded-model.onnx").exists());
        assert!(dir.join("uploaded.json").exists());

        // Activates; the db setting persists; SSE broadcast fires.
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/my-uploaded-model/activate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(engine.active_model(), "my-uploaded-model");
        let saved = crate::db::get_setting(&pool, "ai.model")
            .await
            .expect("setting read")
            .expect("setting written");
        assert_eq!(saved, "my-uploaded-model");
        match tokio::time::timeout(std::time::Duration::from_secs(2), sse.recv()).await {
            Ok(Ok(CameraEvent::AiModelChanged { model })) => assert_eq!(model, "my-uploaded-model"),
            other => panic!("expected AiModelChanged broadcast, got {other:?}"),
        }

        // Active model cannot be deleted…
        let res = app
            .clone()
            .oneshot(
                Request::delete("/api/ai/models/my-uploaded-model")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // …switch to a second uploaded model (builtin paths don't exist in
        // the test cwd), then delete removes file + entry.
        let (ctype, body) = multipart_body("nanodet", b"fake-onnx-bytes-2");
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/back-model")
                    .header("content-type", ctype)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/back-model/activate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(
                Request::delete("/api/ai/models/my-uploaded-model")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(!dir.join("my-uploaded-model.onnx").exists());
        assert!(engine.registry().read().find("my-uploaded-model").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn test_upload_guards() {
        let dir = std::env::temp_dir().join(format!("nb-ai-gd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Disabled → 501.
        let engine = engine_with_factory(Some(&dir), false);
        let (app, _pool, _bus) = app_with(engine).await;
        let (ctype, body) = multipart_body("nanodet", b"x");
        let res = app
            .oneshot(
                Request::post("/api/ai/models/some-model")
                    .header("content-type", ctype)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);

        // Enabled: duplicate id 409, bad id 400, wrong family 400,
        // builtin delete 409, unknown delete 404.
        let engine = engine_with_factory(Some(&dir), true);
        let (app, _pool, _bus) = app_with(engine).await;
        let (ctype, body) = multipart_body("nanodet", b"x");
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/nanodet-plus-m-320")
                    .header("content-type", ctype)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        let (ctype, body) = multipart_body("nanodet", b"x");
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/Bad_ID")
                    .header("content-type", ctype)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let (ctype, body) = multipart_body("yolox", b"x");
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/ai/models/fine-id")
                    .header("content-type", ctype)
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = app
            .clone()
            .oneshot(
                Request::delete("/api/ai/models/nanodet-plus-m-320")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let res = app
            .oneshot(
                Request::delete("/api/ai/models/no-such")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(&dir).ok();
    }
}
