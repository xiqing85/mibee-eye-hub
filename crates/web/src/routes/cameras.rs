//! Camera CRUD endpoints.
//!
//! All handlers require authentication (enforced by middleware).

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use std::sync::Arc;

use crate::db::{self, CameraRow};
use crate::errors::ApiError;
use crate::stream_manager::StreamManager;
use security::middleware::AuthenticatedUser;

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

/// Request body for POST /api/cameras.
#[derive(Debug, Deserialize)]
pub struct CreateCameraRequest {
    pub name: String,
    pub camera_type: String,
    #[serde(default = "default_config")]
    pub config: serde_json::Value,
}

fn default_config() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

/// Request body for PUT /api/cameras/{id}.
#[derive(Debug, Deserialize)]
pub struct UpdateCameraRequest {
    pub name: Option<String>,
    pub camera_type: Option<String>,
    pub config: Option<serde_json::Value>,
    pub status: Option<String>,
}

/// Response body for a single camera.
#[derive(Debug, Serialize)]
pub struct CameraResponse {
    pub id: String,
    pub name: String,
    pub camera_type: String,
    pub config: serde_json::Value,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtsp_url: Option<String>,
}

impl CameraResponse {
    /// Build a CameraResponse from a DB row, enriching with rtsp_url from active streams.
    fn from_row(row: CameraRow, rtsp_url: Option<String>) -> Self {
        Self {
            id: row.id,
            name: row.name,
            camera_type: row.camera_type,
            config: row.config,
            status: row.status,
            created_at: row.created_at,
            updated_at: row.updated_at,
            offline_since: row.offline_since,
            rtsp_url,
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/cameras — list all cameras.
#[tracing::instrument(skip_all)]
pub async fn list_cameras(
    Extension(db): Extension<SqlitePool>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> std::result::Result<impl IntoResponse, ApiError> {
    match db::list_cameras(&db).await {
        Ok(rows) => {
            let active_streams = stream_manager.list_active_streams().await;
            let url_map: std::collections::HashMap<String, String> = active_streams
                .into_iter()
                .filter_map(|s| s.rtsp_url.map(|u| (s.camera_id, u)))
                .collect();
            let cameras: Vec<CameraResponse> = rows
                .into_iter()
                .map(|row| {
                    let url = url_map.get(&row.id).cloned();
                    CameraResponse::from_row(row, url)
                })
                .collect();
            let value = serde_json::to_value(cameras)?;
            Ok((StatusCode::OK, Json(value)))
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to list cameras");
            Err(ApiError::internal("failed to list cameras"))
        }
    }
}

/// GET /api/cameras/{id} — get a single camera.
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn get_camera(
    Extension(db): Extension<SqlitePool>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> std::result::Result<impl IntoResponse, ApiError> {
    match db::get_camera(&db, &id).await {
        Ok(Some(row)) => {
            let url = stream_manager
                .list_active_streams()
                .await
                .into_iter()
                .find(|s| s.camera_id == id)
                .and_then(|s| s.rtsp_url);
            let value = serde_json::to_value(CameraResponse::from_row(row, url))?;
            Ok((StatusCode::OK, Json(value)))
        }
        Ok(None) => Err(ApiError::not_found("camera not found")),
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to get camera");
            Err(ApiError::internal("failed to get camera"))
        }
    }
}

/// POST /api/cameras — create a new camera.
#[tracing::instrument(skip_all, fields(name = %body.name))]
pub async fn create_camera(
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
    Json(body): Json<CreateCameraRequest>,
) -> std::result::Result<impl IntoResponse, ApiError> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::routes::chrono_now();

    let row = CameraRow {
        id: id.clone(),
        name: body.name,
        camera_type: body.camera_type,
        config: body.config,
        status: "stopped".to_string(),
        created_at: now.clone(),
        updated_at: now,
        offline_since: None,
    };

    match db::create_camera(&db, &row).await {
        Ok(()) => {
            let value = serde_json::to_value(CameraResponse::from_row(row, None))?;
            Ok((StatusCode::CREATED, Json(value)))
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to create camera");
            Err(ApiError::internal("failed to create camera"))
        }
    }
}

/// PUT /api/cameras/{id} — update an existing camera.
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn update_camera(
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCameraRequest>,
) -> std::result::Result<impl IntoResponse, ApiError> {
    // Fetch existing camera, or return 404.
    let existing = match db::get_camera(&db, &id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(ApiError::not_found("camera not found")),
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to fetch camera for update");
            return Err(ApiError::internal("failed to update camera"));
        }
    };

    // SPEC v1 appendix A #19: reject an invalid rotation value at the API
    // boundary — the stream would refuse to start otherwise (the error
    // surfaces at create_stream, far from the PUT that caused it).
    if let Some(cfg) = &body.config
        && let Some(rotation) = cfg.get("rotation")
        && !rotation
            .as_u64()
            .is_some_and(|v| matches!(v, 0 | 90 | 180 | 270))
    {
        return Err(ApiError::bad_request(
            "config.rotation must be 0, 90, 180 or 270 (degrees clockwise)",
        ));
    }

    // SPEC v1 appendix A #20: reject structurally invalid substream
    // settings at the API boundary (same surface principle as rotation).
    if let Some(cfg) = &body.config
        && let Err(e) = crate::stream_manager::validate_substream_config(cfg)
    {
        return Err(ApiError::bad_request(format!("config.substream: {e}")));
    }

    let now = crate::routes::chrono_now();
    let updated = CameraRow {
        id: existing.id,
        name: body.name.unwrap_or(existing.name),
        camera_type: body.camera_type.unwrap_or(existing.camera_type),
        config: body.config.unwrap_or(existing.config),
        status: body.status.unwrap_or(existing.status),
        created_at: existing.created_at,
        updated_at: now,
        offline_since: existing.offline_since,
    };

    match db::update_camera(&db, &updated).await {
        Ok(()) => {
            let value = serde_json::to_value(CameraResponse::from_row(updated, None))?;
            Ok((StatusCode::OK, Json(value)))
        }
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to update camera");
            Err(ApiError::internal("failed to update camera"))
        }
    }
}

/// DELETE /api/cameras/{id} — delete a camera.
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn delete_camera(
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match db::delete_camera(&db, &id).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response(),
        Err(e) => {
            if e.downcast_ref::<rusqlite::Error>().is_some() {
                tracing::error!(error = %e, camera_id = %id, "failed to delete camera");
                ApiError::internal("failed to delete camera").into_response()
            } else {
                ApiError::not_found("camera not found").into_response()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rusqlite::Connection;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    /// Helper: create in-memory test DBs (pool + auth_db) with migrations and seeded user.
    async fn test_db() -> (SqlitePool, Arc<Mutex<Connection>>) {
        let (pool, auth_db) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.unwrap();
        {
            let conn = auth_db.lock().await;
            conn.execute_batch(include_str!("../../../../migrations/001_initial.sql"))
                .unwrap();
            seed_user(&conn);
        }
        (pool, auth_db)
    }

    /// Helper: construct AppRouterState with default stream infrastructure.
    fn build_state(
        db: SqlitePool,
        auth_db: Arc<Mutex<Connection>>,
    ) -> crate::server::AppRouterState {
        let event_tx = Arc::new(crate::routes::events::new_event_bus());
        let conversations = Arc::new(crate::conversations::ConversationLog::new(
            db.clone(),
            event_tx.clone(),
            true,
        ));
        crate::server::AppRouterState {
            db,
            auth_db,
            active: crate::server::ActiveStreams::default(),
            stream_manager: Arc::new(crate::stream_manager::StreamManager::new()),
            rtsp_server: Arc::new(protocols::rtsp_server::RtspServer::new(
                protocols::rtsp_server::RtspServerConfig::default(),
            )),
            protocol_configs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx,
            advertised_host: Arc::new("localhost".to_string()),
            ai: Arc::new(streaming::ai::AiEngine::from_parts(
                streaming::ai::AiConfig::default(),
                None,
            )),
            audio_ai: Arc::new(streaming::audio_ai::AudioAiEngine::from_config(
                &streaming::audio_ai::AudioAiConfig::default(),
            )),
            zones: crate::zones::new_shared(),
            ocr: Arc::new(streaming::ocr::OcrEngine::from_config(
                &streaming::ocr::OcrConfig::default(),
            )),
            voice: Arc::new(streaming::voice::VoiceEngine::from_config(
                &streaming::voice::VoiceConfig::default(),
            )),
            face: Arc::new(streaming::face::FaceEngine::from_config(
                &streaming::face::FaceConfig::default(),
            )),
            chat: Arc::new(streaming::llm::ChatEngine::from_config(
                &streaming::llm::LlmConfig::default(),
            )),
            vlm: Arc::new(streaming::vlm::VlmEngine::from_config(
                &streaming::vlm::VlmConfig::default(),
            )),
            decision: Arc::new(streaming::decision::DecisionEngine::from_config(
                &streaming::decision::DecisionConfig::default(),
            )),
            meeting: Arc::new(streaming::meeting::MeetingEngine::from_config(
                &streaming::meeting::MeetingConfig::default(),
                &streaming::voice::VoiceConfig::default(),
            )),
            grounding: Arc::new(crate::grounding::GroundingState::new()),
            tools: Arc::new(std::sync::RwLock::new(
                streaming::tools::ToolsConfig::default(),
            )),
            llm_tier: Arc::new(crate::routes::capabilities::LlmTier("manual".into())),
            resource: Arc::new(streaming::feature_gate::ResourceProfile::unrestricted()),
            wake_word: Arc::new(crate::routes::capabilities::WakeWord(
                streaming::voice::DEFAULT_WAKE_WORD.into(),
            )),
            restart_tx: tokio::sync::watch::channel(false).0,
            models: crate::server::AppRouterState::models_cloud_for_tests().0,
            cloud: crate::server::AppRouterState::models_cloud_for_tests().1,
            conversations,
            protocol_runtime: Arc::new(tokio::sync::Mutex::new(
                crate::protocol_runtime::ProtocolRuntime::new(),
            )),
            agent: crate::server::AppRouterState::agent_for_tests().0,
            agent_config: crate::server::AppRouterState::agent_for_tests().1,
            models_routing: streaming::model_route::RoutingConfig::default(),
            plugins_loaded: 0,
            away: crate::server::AppRouterState::away_for_tests(),
        }
    }

    /// Helper: build a test app with a seeded user and session token.
    async fn test_app_with_token() -> (crate::server::AppRouterState, String) {
        let (pool, auth_db) = test_db().await;
        let token = {
            let conn = auth_db.lock().await;
            security::auth::create_session(&conn, "admin").unwrap()
        };
        (build_state(pool, auth_db), token)
    }

    /// SPEC appendix A #20: the substream MSE endpoint 404s when the
    /// camera runs without a substream (capability false / no pipeline).
    #[tokio::test]
    async fn stream_sub_mse_404_without_substream() {
        use tower::ServiceExt;
        let (state, token) = test_app_with_token().await;
        let app = crate::server::build_app_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/api/cameras/cam-1/stream.sub.mse")
            .header("cookie", format!("session={token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), axum::http::StatusCode::NOT_FOUND);
    }

    /// Seed the users table with an admin user for test setup.
    fn seed_user(conn: &rusqlite::Connection) {
        security::auth::init_users_table(conn).unwrap();
        let hash = security::password::hash_password("test_pass").unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO users (username, password_hash) VALUES (?1, ?2)",
            rusqlite::params!["admin", hash],
        )
        .unwrap();
    }

    #[tokio::test]
    async fn test_list_cameras_empty() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_create_camera() {
        let (state, token) = test_app_with_token().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras")
            .method("POST")
            .header("content-type", "application/json")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "name": "Front Door",
                    "camera_type": "rtsp",
                    "config": {"url": "rtsp://192.168.1.100:554/stream1"}
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"]["name"], "Front Door");
        assert_eq!(body["data"]["camera_type"], "rtsp");
        assert_eq!(body["data"]["status"], "stopped");
        assert!(!body["data"]["id"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_update_camera_rejects_invalid_rotation() {
        // SPEC v1 appendix A #19: rotation must be a quarter turn; the PUT
        // boundary rejects anything else with 400 instead of letting the
        // stream fail at start.
        let (state, token) = test_app_with_token().await;
        let app = crate::server::build_app_with_state(state);

        // Create a camera to update.
        let req = Request::builder()
            .uri("/api/cameras")
            .method("POST")
            .header("content-type", "application/json")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "name": "Rot", "camera_type": "usb", "config": {}
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        let id = body["data"]["id"].as_str().unwrap().to_string();

        let bad_values = [
            serde_json::json!(45),
            serde_json::json!(-90),
            serde_json::json!("90"),
            serde_json::json!(true),
        ];
        for bad in bad_values {
            let req = Request::builder()
                .uri(format!("/api/cameras/{id}"))
                .method("PUT")
                .header("content-type", "application/json")
                .header("cookie", format!("session={token}; csrf-token=test-csrf"))
                .header("x-csrf-token", "test-csrf")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "config": { "rotation": bad } }))
                        .unwrap(),
                ))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "rotation {bad:?}");
        }

        // Quarter turns (and absent key) are accepted.
        for good in [0, 90, 180, 270] {
            let req = Request::builder()
                .uri(format!("/api/cameras/{id}"))
                .method("PUT")
                .header("content-type", "application/json")
                .header("cookie", format!("session={token}; csrf-token=test-csrf"))
                .header("x-csrf-token", "test-csrf")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({ "config": { "rotation": good } }))
                        .unwrap(),
                ))
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK, "rotation {good}");
        }
    }

    #[tokio::test]
    async fn test_camera_crud_full_cycle() {
        let (state, token) = test_app_with_token().await;
        let app = crate::server::build_app_with_state(state);

        // Create
        let req = Request::builder()
            .uri("/api/cameras")
            .method("POST")
            .header("content-type", "application/json")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "name": "Driveway",
                    "camera_type": "onvif",
                    "config": {"host": "192.168.1.200"}
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        let created: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        let cam_id = created["data"]["id"].as_str().unwrap().to_string();

        // Get by ID
        let req = Request::builder()
            .uri(format!("/api/cameras/{}", cam_id))
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Update
        let req = Request::builder()
            .uri(format!("/api/cameras/{}", cam_id))
            .method("PUT")
            .header("content-type", "application/json")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "name": "Back Driveway",
                    "status": "running"
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let updated: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(updated["data"]["name"], "Back Driveway");
        assert_eq!(updated["data"]["status"], "running");

        // List should now have one camera
        let req = Request::builder()
            .uri("/api/cameras")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(list["data"].as_array().unwrap().len(), 1);

        // Delete
        let req = Request::builder()
            .uri(format!("/api/cameras/{}", cam_id))
            .method("DELETE")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // List empty
        let req = Request::builder()
            .uri("/api/cameras")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(list["data"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_get_nonexistent_camera_returns_404() {
        let (state, token) = test_app_with_token().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras/nonexistent-id")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_delete_nonexistent_camera_returns_404() {
        let (state, token) = test_app_with_token().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras/ghost")
            .method("DELETE")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_create_camera_requires_auth() {
        let (pool, auth_db) = test_db().await;
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras")
            .method("POST")
            .header("content-type", "application/json")
            .header("cookie", "csrf-token=test-csrf")
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "name": "Unauth",
                    "camera_type": "rtsp"
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
