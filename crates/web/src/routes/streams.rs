//! Stream control endpoints.
//!
//! All handlers require authentication (enforced by middleware).

use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use protocols::rtsp_server::RtspServer;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio_stream::StreamExt as TokioStreamExt;
use uuid::Uuid;

use crate::db;
use crate::errors::ApiError;
use crate::stream_manager::StreamManager;
use security::middleware::AuthenticatedUser;

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// POST /api/cameras/{id}/start — begin capturing frames from a camera.
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn start_stream(
    Extension(db): Extension<SqlitePool>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(rtsp_srv): Extension<Arc<RtspServer>>,
    Extension(_user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Verify the camera exists.
    let camera = match db::get_camera(&db, &id).await {
        Ok(Some(cam)) => cam,
        Ok(None) => return ApiError::not_found("camera not found").into_response(),
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to get camera for start");
            return ApiError::internal("failed to start stream").into_response();
        }
    };
    let camera_type = camera.camera_type.clone();
    let config = camera.config.clone();

    match stream_manager
        .create_stream(id.clone(), &camera_type, &config, Some(&rtsp_srv))
        .await
    {
        Ok(info) => {
            // Log stream session start (best-effort).
            let session_id = Uuid::new_v4().to_string();
            {
                let _ = db::insert_stream_session(&db, &session_id, &id).await;
            }

            // Update camera status to "running".
            let now = crate::routes::chrono_now();
            let updated = db::CameraRow {
                status: "running".to_string(),
                updated_at: now,
                ..camera
            };
            let _ = db::update_camera(&db, &updated).await;

            tracing::info!(camera_id = %id, rtsp_url = ?info.rtsp_url, "stream started");
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "running",
                    "rtsp_url": info.rtsp_url,
                    "camera_id": id,
                })),
            )
                .into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("already exists") {
                return ApiError::conflict("stream already running").into_response();
            }
            if msg.contains("exhausted") {
                return ApiError::internal("resource limit reached").into_response();
            }
            tracing::error!(error = %e, camera_id = %id, "failed to start stream");
            ApiError::internal("failed to start stream").into_response()
        }
    }
}

/// POST /api/cameras/{id}/stop — stop capturing from a camera.
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn stop_stream(
    Extension(db): Extension<SqlitePool>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Verify the camera exists.
    let camera = match db::get_camera(&db, &id).await {
        Ok(Some(cam)) => cam,
        Ok(None) => return ApiError::not_found("camera not found").into_response(),
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to get camera for stop");
            return ApiError::internal("failed to stop stream").into_response();
        }
    };

    match stream_manager.stop_stream(&id).await {
        Ok(_info) => {
            // Log stream session end (best-effort).
            // TODO: wire actual stats (bytes/frames/errors) from StreamManager when available.
            {
                let _ = db::finalize_stream_session(&db, &id, 0, 0, 0).await;
            }

            // Update DB status.
            let now = crate::routes::chrono_now();
            let updated = db::CameraRow {
                status: "stopped".to_string(),
                updated_at: now,
                ..camera
            };
            let _ = db::update_camera(&db, &updated).await;

            tracing::info!(camera_id = %id, "stream stopped");
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ok", "camera_id": id})),
            )
                .into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            // Idempotent stop: if no active stream, still mark DB as stopped.
            if msg.contains("no active stream") {
                // Still try to finalize any open session (may be a no-op).
                let _ = db::finalize_stream_session(&db, &id, 0, 0, 0).await;
                let now = crate::routes::chrono_now();
                let updated = db::CameraRow {
                    status: "stopped".to_string(),
                    updated_at: now,
                    ..camera
                };
                let _ = db::update_camera(&db, &updated).await;

                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "stopped", "camera_id": id})),
                )
                    .into_response();
            }
            tracing::error!(error = %e, camera_id = %id, "failed to stop stream");
            ApiError::internal("failed to stop stream").into_response()
        }
    }
}

/// GET /api/cameras/{id}/snapshot — capture a single JPEG frame.
///
/// Returns the most recent JPEG frame cached by the capture pipeline. For MJPG
/// cameras this is a zero-cost passthrough of the camera's own JPEG bytes; for
/// YUYV-only cameras a JPEG is re-encoded periodically in the capture loop.
///
/// This replaces the previous ffmpeg-from-RTSP snapshot subprocess — it is
/// faster (no process spawn, no RTSP round-trip) and requires no ffmpeg.
///
/// # Returns
///
/// - `200 OK` with JPEG body on success
/// - `404 Not Found` if camera does not exist
/// - `409 Conflict` if stream is not active or no frame has been captured yet
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn snapshot(
    Extension(db): Extension<SqlitePool>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
    Extension(_advertised_host): Extension<Arc<String>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Verify camera exists.
    match db::get_camera(&db, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return ApiError::not_found("camera not found").into_response(),
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to get camera for snapshot");
            return ApiError::internal("database error").into_response();
        }
    }

    // Fetch the cached JPEG directly from the capture pipeline.
    let jpeg = match stream_manager.latest_jpeg(&id).await {
        Some(j) => j,
        None => {
            // Either no active stream, or stream active but no frame captured yet.
            let msg = if stream_manager.has_stream(&id).await {
                "no frame captured yet — retry in a moment"
            } else {
                "stream not active - start the stream first"
            };
            return ApiError::conflict(msg).into_response();
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("image/jpeg"));
    headers.insert(
        "content-length",
        jpeg.len()
            .to_string()
            .parse::<HeaderValue>()
            .unwrap_or(HeaderValue::from_static("0")),
    );
    // Copy into an owned Vec for the response body (axum body wants 'static).
    (StatusCode::OK, headers, jpeg.to_vec()).into_response()
}

// ---------------------------------------------------------------------------
// Live preview (MJPEG stream)
// ---------------------------------------------------------------------------

/// GET /api/cameras/{id}/live — MJPEG live preview stream.
///
/// Returns a `multipart/x-mixed-replace` response with JPEG frames as they
/// are captured. Suitable for direct use in `<img src=...>` — the browser
/// handles the multipart MJPEG decoding natively, no JavaScript or MSE
/// required.
///
/// This replaces the previous ffmpeg-from-RTSP transcoding subprocess. The
/// capture pipeline already produces JPEG frames (zero-cost for MJPG cameras,
/// periodically re-encoded for YUYV cameras); this endpoint simply forwards
/// them to subscribers.
///
/// # Requirements
/// - Camera must exist and have an active stream (returns 409 otherwise).
///
/// # Notes
/// - Authentication is via the session cookie (sent automatically by `<img>`
///   on same-origin requests).
/// - The stream runs until the client disconnects (dropping the response body
///   drops the broadcast subscription naturally — no reaper task needed).
#[tracing::instrument(skip_all, fields(camera_id = %id))]
pub async fn live_preview(
    Extension(db): Extension<SqlitePool>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(_user): Extension<AuthenticatedUser>,
    Extension(_advertised_host): Extension<Arc<String>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    // Verify camera exists.
    match db::get_camera(&db, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return ApiError::not_found("camera not found").into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, camera_id = %id, "failed to get camera");
            return ApiError::internal("database error").into_response();
        }
    }

    // Subscribe to the JPEG preview broadcast.
    let rx = match stream_manager.subscribe_jpeg(&id).await {
        Some(rx) => rx,
        None => {
            return ApiError::conflict("stream not active - start the stream first")
                .into_response();
        }
    };

    tracing::info!(camera_id = %id, "starting MJPEG live preview");

    // The stream closures below must be 'static (axum body requirement), so
    // we clone `id` here for the tracing field inside the closure.
    let cam_id = id.clone();

    // Wrap the broadcast receiver in a BroadcastStream, then map each JPEG
    // frame into a multipart MIME part. When the HTTP client disconnects,
    // axum drops the response body, dropping this stream and the underlying
    // receiver — no reaper task or kill channel is needed (unlike the old
    // ffmpeg subprocess design).
    let boundary = "mibeejpeg";
    let frame_stream = tokio_stream::wrappers::BroadcastStream::new(rx)
        .filter_map(move |res| match res {
            Ok(jpeg) => Some(jpeg),
            // Lagged = subscriber fell behind; skip ahead to the latest frame.
            Err(e) => {
                tracing::debug!(camera_id = %cam_id, error = ?e, "preview subscriber lagged");
                None
            }
        })
        .map(move |jpeg| {
            // Build the multipart part: boundary header + JPEG + trailing CRLF.
            let mut part = Vec::with_capacity(jpeg.len() + 64);
            use std::io::Write;
            let _ = write!(
                part,
                "--{boundary}\r\n\
                 Content-Type: image/jpeg\r\n\
                 Content-Length: {}\r\n\r\n",
                jpeg.len()
            );
            part.extend_from_slice(&jpeg);
            part.extend_from_slice(b"\r\n");
            Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::from(part))
        });

    axum::response::Response::builder()
        .header(
            "content-type",
            format!("multipart/x-mixed-replace; boundary={boundary}"),
        )
        .header("cache-control", "no-store, no-cache, must-revalidate")
        .header("pragma", "no-cache")
        .body(axum::body::Body::from_stream(frame_stream))
        .unwrap_or_else(|e| {
            tracing::error!(error = ?e, "failed to build MJPEG response");
            ApiError::internal("response build failed").into_response()
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use protocols::rtsp_server::RtspServerConfig;
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
            stream_manager: Arc::new(StreamManager::new()),
            rtsp_server: Arc::new(RtspServer::new(RtspServerConfig::default())),
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

    /// Helper: create a test app with a seeded camera and session token.
    async fn setup_with_camera() -> (crate::server::AppRouterState, String, String) {
        let (pool, auth_db) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.unwrap();
        let token = {
            let conn = auth_db.lock().await;
            conn.execute_batch(include_str!("../../../../migrations/001_initial.sql"))
                .unwrap();
            seed_user(&conn);
            security::auth::create_session(&conn, "admin").unwrap()
        };
        let id = uuid::Uuid::new_v4().to_string();
        let now = format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );

        sqlx::query(
            "INSERT INTO cameras (id, name, camera_type, config, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&id)
        .bind("Test Cam")
        // "usb" is the only camera type StreamManager can create; the device
        // itself is opened lazily by the hub's run task, so this stays
        // deterministic on hosts without a camera.
        .bind("usb")
        .bind("{\"device_index\":0}")
        .bind("stopped")
        .bind(&now)
        .bind(&now)
        .execute(&pool)
        .await
        .unwrap();

        let state = build_state(pool, auth_db);
        (state, token, id)
    }

    #[tokio::test]
    async fn test_start_stream_success() {
        let (state, token, cam_id) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri(format!("/api/cameras/{}/start", cam_id))
            .method("POST")
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
        assert_eq!(body["data"]["status"], "running");
        assert!(
            body["data"]["rtsp_url"]
                .as_str()
                .unwrap()
                .contains("rtsp://")
        );
        assert_eq!(body["data"]["camera_id"], cam_id);
    }

    #[tokio::test]
    async fn test_start_stream_nonexistent_camera() {
        let (state, token, _) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras/does-not-exist/start")
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_start_stream_twice_returns_conflict() {
        let (state, token, cam_id) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        // First start succeeds.
        let req = Request::builder()
            .uri(format!("/api/cameras/{}/start", cam_id))
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Second start should conflict.
        let req = Request::builder()
            .uri(format!("/api/cameras/{}/start", cam_id))
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_stop_stream_success() {
        let (state, token, cam_id) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        // Start first.
        let req = Request::builder()
            .uri(format!("/api/cameras/{}/start", cam_id))
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let _ = app.clone().oneshot(req).await.unwrap();

        // Stop.
        let req = Request::builder()
            .uri(format!("/api/cameras/{}/stop", cam_id))
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_stop_stream_nonexistent_camera() {
        let (state, token, _) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras/ghost/stop")
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_snapshot_nonexistent_camera() {
        let (state, token, _) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/cameras/ghost/snapshot")
            .method("GET")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_snapshot_inactive_stream() {
        let (state, token, cam_id) = setup_with_camera().await;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri(format!("/api/cameras/{}/snapshot", cam_id))
            .method("GET")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_stream_routes_require_auth() {
        let (pool, auth_db) = test_db().await;
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        let cam_id = "some-id";
        for path in &[
            format!("/api/cameras/{cam_id}/start"),
            format!("/api/cameras/{cam_id}/stop"),
            format!("/api/cameras/{cam_id}/snapshot"),
        ] {
            let req = Request::builder()
                .uri(path.as_str())
                .method("POST")
                .header("cookie", "csrf-token=test-csrf")
                .header("x-csrf-token", "test-csrf")
                .body(Body::empty())
                .unwrap();
            let res = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "stream route {path} should require auth"
            );
        }
    }
}
