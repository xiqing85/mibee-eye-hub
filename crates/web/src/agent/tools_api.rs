//! `GET /api/tools` — the tool/skill registry listing (SPEC §3.5,
//! capability `tools`). The transparency surface: everything exposed to
//! the model, built-ins and MCP plugin servers alike.

use crate::agent::{ToolRegistry, mcp_fingerprint};
use axum::Json;
use axum::extract::{Extension, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::sync::Arc;

#[tracing::instrument(skip_all)]
pub async fn list_tools(
    Extension(registry): Extension<Arc<ToolRegistry>>,
    Extension(_user): Extension<security::middleware::AuthenticatedUser>,
) -> Json<serde_json::Value> {
    let tools = registry.cached_specs();
    Json(json!({ "tools": tools }))
}

/// `POST /api/tools/mcp/{name}/trust` — confirm the CURRENT launch
/// line of one MCP server (SPEC appendix A #43 amendment): the trust
/// gate fingerprints command+args+env; a new or edited entry stays
/// refused until this endpoint confirms the fingerprint as it stands
/// NOW. Persists to the settings bag (`agent.mcp_trust`) and
/// refreshes the registry so confirmation is immediate.
#[tracing::instrument(skip_all)]
pub async fn trust_mcp_server(
    Path(name): Path<String>,
    Extension(registry): Extension<Arc<ToolRegistry>>,
    Extension(pool): Extension<sqlx::SqlitePool>,
    Extension(_user): Extension<security::middleware::AuthenticatedUser>,
) -> Response {
    let Some(cfg) = registry.mcp_configs().into_iter().find(|c| c.name == name) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown MCP server", "name": name })),
        )
            .into_response();
    };
    let fp = mcp_fingerprint(&cfg);
    // Merge into the persisted trust map, then the live one.
    let mut map = registry.trust_map();
    map.insert(cfg.name.clone(), fp.clone());
    let serialized = serde_json::to_string(&map).unwrap_or_else(|_| "{}".into());
    if let Err(e) = crate::db::set_setting(&pool, "agent.mcp_trust", &serialized).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("persist: {e}") })),
        )
            .into_response();
    }
    registry.set_trust(map);
    registry.refresh_mcp().await;
    let tools = registry.cached_specs();
    tracing::info!(server = %name, fingerprint = %fp, "agent: MCP server trust confirmed");
    (
        StatusCode::OK,
        Json(json!({
            "trusted": name,
            "fingerprint": fp,
            "command": cfg.command,
            "tools_total": tools.len(),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentConfig;
    use crate::stream_manager::StreamManager;
    use axum::body::Body;

    #[tokio::test]
    async fn trust_endpoint_confirms_and_persists() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.unwrap();
        let cfg = AgentConfig {
            mcp_servers: vec![crate::agent::McpServerConfig {
                name: "home".into(),
                command: "/nonexistent/home_tools.py".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
            }],
            ..AgentConfig::default()
        };
        let shared = Arc::new(std::sync::RwLock::new(
            streaming::tools::ToolsConfig::default(),
        ));
        let registry = Arc::new(ToolRegistry::new(shared, &cfg));
        let app = axum::Router::new()
            .route(
                "/api/tools/mcp/{name}/trust",
                axum::routing::post(trust_mcp_server),
            )
            .layer(Extension(registry.clone()))
            .layer(Extension(pool.clone()))
            .layer(Extension(AuthenticatedUser("tester".to_string())));
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/tools/mcp/home/trust")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 65536).await.unwrap())
                .unwrap();
        assert_eq!(body["trusted"], "home");
        assert!(body["fingerprint"].as_str().is_some_and(|f| f.len() == 64));
        // Persisted as the settings-bag trust map.
        let saved = crate::db::get_setting(&pool, "agent.mcp_trust")
            .await
            .unwrap()
            .expect("row");
        assert!(saved.contains("home"), "{saved}");
        // The live registry now trusts it.
        assert!(registry.trust_map().contains_key("home"));
        // Unknown server → 404.
        let res = app
            .oneshot(
                Request::post("/api/tools/mcp/nope/trust")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    use axum::http::Request;
    use axum::http::StatusCode;
    use security::middleware::AuthenticatedUser;
    use tower::ServiceExt;

    #[tokio::test]
    async fn tools_endpoint_lists_builtins_with_sources() {
        let shared = Arc::new(std::sync::RwLock::new(streaming::tools::ToolsConfig {
            weather_enabled: true,
            weather_city: "Guangzhou".into(),
            timeout_secs: 2,
        }));
        let registry = Arc::new(ToolRegistry::new(shared, &AgentConfig::default()));
        registry.attach_streams(Arc::new(StreamManager::new()));
        let app = axum::Router::new()
            .route("/api/tools", axum::routing::get(list_tools))
            .layer(Extension(registry))
            .layer(Extension(AuthenticatedUser("tester".to_string())));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/tools")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tools = json["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"time.now"));
        assert!(names.contains(&"weather.current"));
        assert!(names.contains(&"camera.snapshot"));
        assert!(
            tools
                .iter()
                .all(|t| t["source"].as_str() == Some("builtin"))
        );
        assert!(
            tools
                .iter()
                .all(|t| t["input_schema"]["type"] == serde_json::json!("object"))
        );
    }
}
