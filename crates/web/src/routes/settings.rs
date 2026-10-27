//! Settings endpoints.
//!
//! All handlers require authentication (enforced by middleware).

use axum::Json;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::db;
use crate::errors::ApiError;
use security::middleware::AuthenticatedUser;
// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Request body for PUT /api/settings.
#[derive(Debug, Deserialize)]
pub struct UpdateSettingsRequest {
    /// Map of setting key → new value.
    pub settings: std::collections::HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/settings — return all settings as a key-value object.
#[tracing::instrument(skip_all)]
pub async fn get_settings(
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> impl IntoResponse {
    match db::list_settings(&db).await {
        Ok(rows) => {
            let map: std::collections::BTreeMap<String, String> = rows.into_iter().collect();
            match serde_json::to_value(map) {
                Ok(v) => (StatusCode::OK, Json(v)).into_response(),
                Err(e) => {
                    tracing::error!(error = %e, "failed to serialize settings");
                    ApiError::internal("failed to serialize settings").into_response()
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to list settings");
            ApiError::internal("failed to list settings").into_response()
        }
    }
}

/// PUT /api/settings — update one or more settings.
#[tracing::instrument(skip_all)]
pub async fn update_settings(
    Extension(db): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
    Json(body): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    for (key, value) in &body.settings {
        if let Err(e) = db::set_setting(&db, key, value).await {
            tracing::error!(error = %e, setting_key = %key, "failed to set setting");
            return ApiError::internal("failed to update settings").into_response();
        }
    }
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))).into_response()
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
            video_capable: true,
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
    async fn test_get_config_settings_empty() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/config")
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
        // SPEC envelope: {"ok":true,"data":{"settings":{},"protocols":{...}}}
        assert_eq!(body["ok"], serde_json::json!(true));
        assert!(body["data"]["settings"].is_object());
    }

    #[tokio::test]
    async fn test_update_settings() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        // Set two settings
        let req = Request::builder()
            .uri("/api/config")
            .method("PUT")
            .header("content-type", "application/json")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "settings": {
                        "theme": "dark",
                        "language": "zh-CN"
                    }
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Read them back
        let req = Request::builder()
            .uri("/api/config")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 1024 * 16)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"]["settings"]["theme"], "dark");
        assert_eq!(body["data"]["settings"]["language"], "zh-CN");

        // Override one, keep the other
        let req = Request::builder()
            .uri("/api/config")
            .method("PUT")
            .header("content-type", "application/json")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "settings": { "theme": "light" }
                }))
                .unwrap(),
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let req = Request::builder()
            .uri("/api/config")
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
        assert_eq!(body["data"]["settings"]["theme"], "light");
        assert_eq!(body["data"]["settings"]["language"], "zh-CN");
    }

    #[tokio::test]
    async fn test_config_requires_auth() {
        let (pool, auth_db) = test_db().await;
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// SPEC v1 §5.2: `protocols.watermark` round-trips through
    /// `PUT/GET /api/config` with partial-merge semantics, and invalid
    /// updates are rejected with 400 without touching stored state.
    #[tokio::test]
    async fn test_config_watermark_section_roundtrip() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let state = build_state(pool, auth_db);
        let app = crate::server::build_app_with_state(state);

        let put = |body: serde_json::Value| {
            Request::builder()
                .uri("/api/config")
                .method("PUT")
                .header("content-type", "application/json")
                .header("cookie", format!("session={token}; csrf-token=test-csrf"))
                .header("x-csrf-token", "test-csrf")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };
        let get = || {
            Request::builder()
                .uri("/api/config")
                .header("cookie", format!("session={token}; csrf-token=test-csrf"))
                .header("x-csrf-token", "test-csrf")
                .body(Body::empty())
                .unwrap()
        };
        let json_body = |res: axum::response::Response| async move {
            serde_json::from_slice::<serde_json::Value>(
                &axum::body::to_bytes(res.into_body(), 1024 * 64)
                    .await
                    .unwrap(),
            )
            .unwrap()
        };

        // Full-section write.
        let res = app
            .clone()
            .oneshot(put(serde_json::json!({
                "protocols": { "watermark": {
                    "enabled": true, "text": "前门", "position": "bottom-right", "font_size": 32
                }}
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Partial write touches only the submitted key.
        let res = app
            .clone()
            .oneshot(put(serde_json::json!({
                "protocols": {"watermark": {"text": "后院"}}
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = json_body(app.clone().oneshot(get()).await.unwrap()).await;
        let wm = &body["data"]["protocols"]["watermark"];
        assert_eq!(wm["enabled"], serde_json::json!(true));
        assert_eq!(wm["text"], "后院");
        assert_eq!(wm["position"], "bottom-right");
        assert_eq!(wm["font_size"], serde_json::json!(32));

        // Enum violation → 400, stored state keeps the last valid values.
        let res = app
            .clone()
            .oneshot(put(serde_json::json!({
                "protocols": {"watermark": {"position": "middle"}}
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Semantic violation (SPEC §5.2: enabled requires content) → 400.
        let res = app
            .clone()
            .oneshot(put(serde_json::json!({
                "protocols": {"watermark": {"text": "", "show_timestamp": false}}
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let err = json_body(res).await;
        assert!(
            err["message"].as_str().unwrap().contains("show_timestamp"),
            "got: {err}"
        );

        // Semantic violation (format whitelist) → 400.
        let res = app
            .clone()
            .oneshot(put(serde_json::json!({
                "protocols": {"watermark": {"timestamp_format": "%y"}}
            })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Stored values survive the rejected updates.
        let body = json_body(app.oneshot(get()).await.unwrap()).await;
        let wm = &body["data"]["protocols"]["watermark"];
        assert_eq!(wm["text"], "后院");
        assert_eq!(wm["position"], "bottom-right");
    }
}
