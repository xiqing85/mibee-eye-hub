//! Away mode endpoints (SPEC v1 §3.6, capability `away`): armed-state
//! status/switch + the event record list/clear + evidence snapshots.
//! The watch loop itself lives in the binary's orchestrator task; these
//! handlers only read the engine flags and the SQLite store.

use std::sync::Arc;

use axum::Json;
use axum::extract::Extension;
use axum::extract::Path;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;

use crate::away::AwayEngine;
use crate::db;
use crate::errors::ApiError;
use crate::routes::events::EventBus;
use security::middleware::AuthenticatedUser;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    /// Max events to return (SPEC §3.6: default 50, cap 200).
    pub limit: Option<i64>,
}

/// `GET /api/away` — armed state + stats (SPEC §3.6).
#[tracing::instrument(skip_all)]
pub async fn get_away(
    Extension(away): Extension<Arc<AwayEngine>>,
    Extension(pool): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Json<serde_json::Value> {
    let stats = db::away_stats(&pool).await.unwrap_or((0, 0));
    Json(json!({
        "active": away.is_armed(),
        "since_ms": away.armed_since_ms(),
        "voice": away.voice_capable(),
        "stats": {"events": stats.0, "visitors": stats.1},
    }))
}

/// `POST /api/away` — arm/disarm (SPEC §3.6). Armed state persists as
/// the `away.active` setting so a service restart keeps the watch on.
/// Disarm carries the additive `summary` field when something happened
/// while armed and `scene.away.summary_on_disarm` is on.
#[tracing::instrument(skip_all)]
pub async fn post_away(
    Extension(away): Extension<Arc<AwayEngine>>,
    Extension(pool): Extension<SqlitePool>,
    Extension(event_tx): Extension<Arc<EventBus>>,
    Extension(chat): Extension<Arc<streaming::llm::ChatEngine>>,
    Extension(_user): Extension<AuthenticatedUser>,
    Json(payload): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(active) = payload.get("active").and_then(|v| v.as_bool()) else {
        return Err(ApiError::bad_request("body must be {\"active\": bool}"));
    };
    let summary = crate::away::set_active(&away, &pool, &event_tx, active, Some(&chat))
        .await
        .map_err(ApiError::bad_request)?;
    let since_ms = away.armed_since_ms();
    let mut body = json!({"applied": "immediate", "active": active, "since_ms": since_ms});
    if let Some(s) = summary {
        body["summary"] = json!(s);
    }
    Ok((StatusCode::OK, Json(body)))
}

/// `GET /api/away/events` — records, newest first (SPEC §3.6).
#[tracing::instrument(skip_all)]
pub async fn list_away_events(
    Extension(pool): Extension<SqlitePool>,
    Extension(_away): Extension<Arc<AwayEngine>>,
    Query(params): Query<ListParams>,
) -> Json<serde_json::Value> {
    let limit = params.limit.unwrap_or(50);
    let events = db::list_away_events(&pool, limit).await.unwrap_or_default();
    Json(json!({ "events": events }))
}

/// `DELETE /api/away/events` — clear every record + snapshot file
/// (SPEC §3.6; `hearing_records` clear-all precedent).
#[tracing::instrument(skip_all)]
pub async fn clear_away_events(
    Extension(away): Extension<Arc<AwayEngine>>,
    Extension(pool): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
) -> Result<impl IntoResponse, ApiError> {
    let (removed, names) = db::clear_away_events(&pool)
        .await
        .map_err(|e| ApiError::internal(format!("away clear: {e}")))?;
    unlink_snapshots(&away.config().snapshot_dir, &names);
    Ok((
        StatusCode::OK,
        Json(json!({ "applied": "immediate", "removed": removed })),
    ))
}

/// `GET /api/away/events/{id}/snapshot` — the evidence JPEG (SPEC §3.6).
/// The file name comes from the row, never the client; anything that
/// would escape the snapshot dir is rejected defensively.
#[tracing::instrument(skip_all)]
pub async fn get_away_snapshot(
    Extension(away): Extension<Arc<AwayEngine>>,
    Extension(pool): Extension<SqlitePool>,
    Extension(_user): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let event = db::get_away_event(&pool, id)
        .await
        .map_err(|e| ApiError::internal(format!("away snapshot: {e}")))?
        .ok_or_else(|| ApiError::not_found("no such away event"))?;
    let Some(name) = event.snapshot else {
        return Err(ApiError::not_found("event has no snapshot"));
    };
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ApiError::not_found("bad snapshot name"));
    }
    let path = std::path::Path::new(&away.config().snapshot_dir).join(&name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("snapshot file missing"))?;
    Ok((StatusCode::OK, [("content-type", "image/jpeg")], bytes))
}

/// Best-effort snapshot unlink (record prune / clear-all). Names are
/// server-generated; the guards here are defense in depth.
pub fn unlink_snapshots(dir: &str, names: &[String]) {
    for name in names {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            tracing::warn!(%name, "away: refusing to unlink suspicious snapshot name");
            continue;
        }
        let path = std::path::Path::new(dir).join(name);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, %name, "away: snapshot unlink failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::away::AwayConfig;
    use crate::away::AwayEventRecord;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use streaming::ai::AiConfig;
    use streaming::ai::AiEngine;
    use tower::ServiceExt;

    async fn test_pool() -> SqlitePool {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("migrations for test pool");
        pool
    }

    fn away_engine() -> Arc<AwayEngine> {
        // No model → AI inactive → the arm gate is testable.
        Arc::new(AwayEngine::new(
            AwayConfig::default(),
            Arc::new(AiEngine::from_parts(AiConfig::default(), None)),
            false,
        ))
    }

    fn rec(id: i64, state: &str, snapshot: Option<&str>) -> AwayEventRecord {
        AwayEventRecord {
            id,
            camera_id: "0".into(),
            kind: "person".into(),
            started_ms: 1000,
            labels: "person×1".into(),
            face_name: None,
            description: None,
            visitor_reply: None,
            snapshot: snapshot.map(String::from),
            state: state.into(),
        }
    }

    #[tokio::test]
    async fn away_endpoints_roundtrip() {
        let pool = test_pool().await;
        let away = away_engine();
        let event_tx = Arc::new(crate::routes::events::new_event_bus());
        let app = axum::Router::new()
            .route("/api/away", get(get_away).post(post_away))
            .route(
                "/api/away/events",
                get(list_away_events).delete(clear_away_events),
            )
            .route("/api/away/events/{id}/snapshot", get(get_away_snapshot))
            .layer(Extension(pool.clone()))
            .layer(Extension(Arc::clone(&away)))
            .layer(Extension(event_tx))
            .layer(Extension(Arc::new(
                streaming::llm::ChatEngine::from_config(&streaming::llm::LlmConfig::default()),
            )))
            .layer(Extension(AuthenticatedUser("tester".to_string())));

        // Status starts disarmed.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/away")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["active"], json!(false));
        assert_eq!(body["voice"], json!(false));
        assert_eq!(body["stats"], json!({"events": 0, "visitors": 0}));

        // Arming with an inactive AI engine is refused with a reason.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/away")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"active":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(body["message"].as_str().unwrap().contains("unavailable"));

        // Disarm with no window → no summary key (disarmed already).
        // Then arm+disarm is impossible via this fixture (AI inactive),
        // so the summary path is covered by the away.rs unit test.

        // Records list + snapshot 404 + clear-all.
        crate::db::insert_away_event(&pool, &rec(1, "answered", Some("s.jpg")))
            .await
            .unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/away/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["events"].as_array().map(Vec::len), Some(1));

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/away/events/1/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "file not on disk");

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/away/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["removed"], json!(1));
    }
}
