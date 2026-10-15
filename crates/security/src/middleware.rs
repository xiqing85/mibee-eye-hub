use crate::auth;
use crate::rate_limit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, extract::Request, middleware::Next};
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Extension value inserted into request extensions on successful authentication.
/// Downstream handlers can extract it with `Extension<AuthenticatedUser>`.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser(pub String);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn error_response(status: StatusCode, message: &str) -> Response {
    let code = match status {
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::INTERNAL_SERVER_ERROR => "internal_error",
        StatusCode::TOO_MANY_REQUESTS => "too_many_requests",
        StatusCode::SERVICE_UNAVAILABLE => "service_unavailable",
        _ => "error",
    };
    let body = serde_json::json!({
        "error": code,
        "message": message,
        "status": status.as_u16(),
    });
    (status, Json(body)).into_response()
}

fn extract_session_token(req: &Request) -> Option<String> {
    req.headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|c| c.trim().strip_prefix("session="))
                .map(|s| s.to_owned())
        })
}

/// The client key used for per-IP rate limiting.
///
/// **Forwarding headers are only honoured when the direct socket peer is
/// loopback.** The previous version trusted `X-Forwarded-For` / `X-Real-Ip`
/// unconditionally, which made the rate limiter trivially bypassable — the
/// limiter keys on this string, so a client only had to vary a header to get
/// a fresh bucket per request — and let unauthenticated traffic grow the
/// tracked-key table with attacker-chosen strings.
///
/// Restricting to loopback peers keeps the useful case working (a reverse
/// proxy on the same host sets these headers on our behalf, nginx's
/// `set_real_ip_from` convention) while making the headers unforgeable from
/// anywhere else. Deployments that terminate TLS elsewhere and forward from
/// a distinct address should move the proxy in front of the loopback
/// interface rather than re-enabling blind trust here.
fn client_ip(req: &Request) -> String {
    let socket_ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|addr| addr.0.ip());

    if socket_ip.is_some_and(|ip| ip.is_loopback()) {
        if let Some(val) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            && let Some(ip) = val.split(',').next().map(str::trim)
            && !ip.is_empty()
        {
            return ip.to_owned();
        }
        if let Some(val) = req.headers().get("x-real-ip").and_then(|v| v.to_str().ok())
            && !val.is_empty()
        {
            return val.to_owned();
        }
    }

    socket_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Auth middleware
// ---------------------------------------------------------------------------

/// Axum middleware that validates the session cookie.
///
/// This middleware expects an `Arc<Mutex<rusqlite::Connection>>` to be present
/// in the request extensions. The application is responsible for adding it, e.g.:
///
/// ```ignore
/// let app = Router::new()
///     .route("/api/protected", get(handler))
///     .route_layer(middleware::from_fn(require_auth))
///     .layer(Extension(db_arc));
/// ```
///
/// On success, an `AuthenticatedUser` extension is inserted into the request.
pub async fn require_auth(mut req: Request, next: Next) -> Response {
    let db = match req.extensions().get::<Arc<Mutex<Connection>>>().cloned() {
        Some(db) => db,
        None => {
            tracing::error!("require_auth: no Arc<Mutex<Connection>> in request extensions");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal: database not configured",
            );
        }
    };

    let session_token = match extract_session_token(&req) {
        Some(t) => t,
        None => return error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
    };

    let conn = db.lock().await;
    let user_id = match auth::validate_session(&conn, &session_token) {
        Ok(Some(uid)) => uid,
        Ok(None) => return error_response(StatusCode::UNAUTHORIZED, "unauthorized"),
        Err(e) => {
            tracing::error!(error = %e, "session validation error");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };
    drop(conn);

    req.extensions_mut().insert(AuthenticatedUser(user_id));
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Setup-required middleware
// ---------------------------------------------------------------------------

/// Axum middleware that checks if the application has been configured (first-run check).
///
/// During first run (no admin user), only the following routes are accessible:
/// - `/health`
/// - `/metrics`
/// - `/api/auth/setup`
/// - `/` (static SPA)
/// - `/assets/*` (static assets)
///
/// All other routes return `503 SERVICE_UNAVAILABLE` with body
/// `{"error": "SETUP_REQUIRED", "code": 503}`.
///
/// Once an admin user is created, this middleware passes through unconditionally,
/// allowing subsequent middleware (e.g. `require_auth`) and handlers to run.
pub async fn require_setup(req: Request, next: Next) -> Response {
    let path = req.uri().path();

    // Always allow health check, metrics, setup endpoint, root (SPA), and static assets
    if path == "/health"
        || path == "/api/health"
        || path == "/metrics"
        || path == "/api/auth/setup"
        || path == "/"
        || path.starts_with("/assets/")
        // Shared web UI static assets (SPEC v1 layout: /style.css + /js/*)
        || path == "/style.css"
        || path.starts_with("/js/")
    {
        return next.run(req).await;
    }

    let db = match req.extensions().get::<Arc<Mutex<Connection>>>().cloned() {
        Some(db) => db,
        None => {
            tracing::error!("require_setup: no Arc<Mutex<Connection>> in request extensions");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal: database not configured",
            );
        }
    };

    let conn = db.lock().await;
    match auth::is_first_run(&conn) {
        Ok(true) => {
            drop(conn);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "setup_required", "message": "Initial setup required", "status": 503})),
            )
                .into_response()
        }
        Ok(false) => {
            drop(conn);
            next.run(req).await
        }
        Err(e) => {
            tracing::error!(error = %e, "require_setup: failed to check first-run status");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

// ---------------------------------------------------------------------------
// Rate-limit middleware
// ---------------------------------------------------------------------------

/// Axum middleware that rate-limits requests per client IP.
///
/// Uses the in-memory rate limiter from `rate_limit::check_rate_limit`
/// with configurable limits from `SecurityConfig` (default: 20 requests per 60-second window).
///
/// When a request is rate-limited, a 429 Too Many Requests response is returned.
pub async fn rate_limit(req: Request, next: Next) -> Response {
    let ip = client_ip(&req);

    let cfg = rate_limit::get_rate_limit_config();
    if !rate_limit::check_rate_limit(&ip, cfg.max_requests, cfg.window_secs) {
        tracing::warn!(ip = %ip, "rate limit exceeded");
        observability::increment_auth_failures("rate_limited");
        return error_response(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
    }

    next.run(req).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Extension;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, header};
    use axum::routing::get;
    use tower::ServiceExt;

    fn test_db_arc() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        auth::init_sessions_table(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    // ---------------------------------------------------------------------
    // `client_ip` is the rate-limiter key, so its behaviour *is* the
    // brute-force defence. The pre-2026-10-15 implementation had no test at
    // all (the rate-limit tests call `check_rate_limit` with literal IPs and
    // bypass this function), which is how "trust X-Forwarded-For from anyone"
    // shipped unnoticed.
    // ---------------------------------------------------------------------

    /// Build a request whose socket peer is `peer`, with optional spoofed
    /// forwarding headers.
    fn request_from(peer: &str, xff: Option<&str>, xreal: Option<&str>) -> axum::extract::Request {
        let literal = if peer.contains(':') {
            format!("[{peer}]:44321")
        } else {
            format!("{peer}:44321")
        };
        let socket: std::net::SocketAddr = literal.parse().unwrap();
        let mut builder = axum::http::Request::builder().uri("/api/auth/login");
        if let Some(v) = xff {
            builder = builder.header("x-forwarded-for", v);
        }
        if let Some(v) = xreal {
            builder = builder.header("x-real-ip", v);
        }
        let mut req = builder.body(axum::body::Body::empty()).unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(socket));
        req
    }

    #[test]
    fn client_ip_uses_socket_peer_by_default() {
        let req = request_from("10.0.0.7", None, None);
        assert_eq!(client_ip(&req), "10.0.0.7");
    }

    #[test]
    fn client_ip_ignores_forwarded_headers_from_a_remote_peer() {
        // The regression: a remote client used to be able to pick its own
        // rate-limit bucket by varying one header.
        let req = request_from("10.0.0.7", Some("1.2.3.4"), Some("5.6.7.8"));
        assert_eq!(
            client_ip(&req),
            "10.0.0.7",
            "X-Forwarded-For / X-Real-Ip must be ignored unless the peer is loopback"
        );
    }

    #[test]
    fn client_ip_honours_forwarded_headers_from_a_local_proxy() {
        let req = request_from("127.0.0.1", Some("203.0.113.9, 10.0.0.1"), None);
        assert_eq!(client_ip(&req), "203.0.113.9", "first hop wins");
        let req = request_from("::1", None, Some("203.0.113.10"));
        assert_eq!(client_ip(&req), "203.0.113.10");
    }

    #[test]
    fn client_ip_falls_back_when_forwarded_headers_are_empty() {
        let req = request_from("127.0.0.1", Some("  "), None);
        assert_eq!(client_ip(&req), "127.0.0.1");
        let req = request_from("127.0.0.1", Some(""), None);
        assert_eq!(client_ip(&req), "127.0.0.1");
    }

    #[test]
    fn client_ip_without_connect_info_is_unknown() {
        let req = axum::http::Request::builder()
            .uri("/api/auth/login")
            .header("x-forwarded-for", "1.2.3.4")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            client_ip(&req),
            "unknown",
            "no socket peer means no trust — headers must not fill the gap"
        );
    }

    #[tokio::test]
    async fn test_require_auth_rejects_missing_cookie() {
        let db = test_db_arc();

        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(require_auth))
            .layer(Extension(db));

        let req = HttpRequest::builder()
            .uri("/test")
            .body(Body::empty())
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_require_auth_rejects_invalid_token() {
        let db = test_db_arc();

        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(require_auth))
            .layer(Extension(db));

        let req = HttpRequest::builder()
            .uri("/test")
            .header(header::COOKIE, "session=invalidtoken123")
            .body(Body::empty())
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_require_auth_accepts_valid_token() {
        let db = test_db_arc();

        // Create a valid session
        let token = {
            let conn = db.lock().await;
            auth::create_session(&conn, "admin").unwrap()
        };

        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(require_auth))
            .layer(Extension(db));

        let req = HttpRequest::builder()
            .uri("/test")
            .header(header::COOKIE, format!("session={token}"))
            .body(Body::empty())
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_require_auth_attaches_user_id() {
        let db = test_db_arc();

        let token = {
            let conn = db.lock().await;
            auth::create_session(&conn, "testuser").unwrap()
        };

        async fn handler(
            Extension(user): Extension<AuthenticatedUser>,
        ) -> impl axum::response::IntoResponse {
            assert_eq!(user.0, "testuser");
            "ok"
        }

        let app = Router::new()
            .route("/test", get(handler))
            .route_layer(axum::middleware::from_fn(require_auth))
            .layer(Extension(db));

        let req = HttpRequest::builder()
            .uri("/test")
            .header(header::COOKIE, format!("session={token}"))
            .body(Body::empty())
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A current-thread runtime driven to completion inside a sync test.
    ///
    /// These rate-limit tests must hold the crate-wide serialisation guard for
    /// their whole body, and a `parking_lot::MutexGuard` is not `Send` — in an
    /// `async fn` that means holding it across `.await`, which blocks the
    /// executor thread. Blocking the only runtime thread is precisely the
    /// anti-pattern the audit flagged, so the awaits are driven from a sync
    /// test instead.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(f)
    }

    #[test]
    fn test_rate_limit_allows_first_request() {
        let _serialised = rate_limit::serialise();

        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(rate_limit));

        let socket: std::net::SocketAddr = "192.168.1.1:44321".parse().unwrap();
        let mut req = HttpRequest::builder()
            .uri("/test")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(socket));

        let res = block_on(app.oneshot(req)).unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// A build request carrying a real socket peer, so `client_ip` resolves
    /// to that address. Without `ConnectInfo` every such request collapses to
    /// the `"unknown"` key and the test stops exercising per-client limiting
    /// at all.
    fn build_limited_request() -> axum::extract::Request {
        let socket: std::net::SocketAddr = "10.0.0.99:44321".parse().unwrap();
        let mut req = HttpRequest::builder()
            .uri("/test")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(socket));
        req
    }

    #[test]
    fn test_rate_limit_rejects_over_limit() {
        // Serialise against every other test in the crate that touches the
        // shared rate-limit table.
        let _serialised = rate_limit::serialise();

        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(rate_limit));

        // Default limit is 20 per 60 seconds.
        for _ in 0..20 {
            let res = block_on(app.clone().oneshot(build_limited_request())).unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // 21st request from the same peer should be rate limited.
        let res = block_on(app.oneshot(build_limited_request())).unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// A different peer must get its own bucket — the whole point of keying
    /// the limiter on the resolved client address.
    #[test]
    fn test_rate_limit_is_per_client_address() {
        let _serialised = rate_limit::serialise();

        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(rate_limit));

        for _ in 0..20 {
            let socket: std::net::SocketAddr = "10.0.0.99:44321".parse().unwrap();
            let mut req = HttpRequest::builder()
                .uri("/test")
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(socket));
            let res = block_on(app.clone().oneshot(req)).unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // A different peer, and a spoofed X-Forwarded-For naming the first
        // one, must both still be served: forwarding headers are only
        // honoured from a loopback peer.
        let socket: std::net::SocketAddr = "10.0.0.98:44321".parse().unwrap();
        let mut req = HttpRequest::builder()
            .uri("/test")
            .header("x-forwarded-for", "10.0.0.99")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(socket));
        let res = block_on(app.oneshot(req)).unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "a different client, or one spoofing X-Forwarded-For from a \
             non-loopback peer, must not land in the first client's bucket"
        );
    }
}

#[cfg(test)]
mod setup_tests {
    use super::*;
    use axum::Extension;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    fn test_db_empty() -> Arc<Mutex<Connection>> {
        Arc::new(Mutex::new(Connection::open_in_memory().unwrap()))
    }

    fn test_db_with_user() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().unwrap();
        crate::auth::init_users_table(&conn).unwrap();
        let hash = crate::password::hash_password("test_pass").unwrap();
        conn.execute(
            "INSERT INTO users (username, password_hash) VALUES (?1, ?2)",
            rusqlite::params!["admin", hash],
        )
        .unwrap();
        Arc::new(Mutex::new(conn))
    }

    fn make_app(db: Arc<Mutex<Connection>>) -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/metrics", get(|| async { "metrics" }))
            .route("/api/auth/setup", get(|| async { "setup" }))
            .route("/", get(|| async { "index" }))
            .route("/api/cameras", get(|| async { "cameras" }))
            .route("/api/auth/login", get(|| async { "login" }))
            .route_layer(axum::middleware::from_fn(require_setup))
            .layer(Extension(db))
    }

    #[tokio::test]
    async fn test_setup_allows_health_during_first_run() {
        let app = make_app(test_db_empty());
        let req = HttpRequest::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_setup_allows_metrics_during_first_run() {
        let app = make_app(test_db_empty());
        let req = HttpRequest::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_setup_allows_setup_endpoint_during_first_run() {
        let app = make_app(test_db_empty());
        let req = HttpRequest::builder()
            .uri("/api/auth/setup")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_setup_allows_root_during_first_run() {
        let app = make_app(test_db_empty());
        let req = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_setup_blocks_protected_routes_during_first_run() {
        let app = make_app(test_db_empty());
        let req = HttpRequest::builder()
            .uri("/api/cameras")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Verify the body matches canonical ApiError JSON format
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "setup_required", "error code mismatch");
        assert_eq!(
            json["message"], "Initial setup required",
            "message mismatch"
        );
        assert_eq!(json["status"], 503, "status mismatch");
    }

    #[tokio::test]
    async fn test_setup_blocks_login_during_first_run() {
        let app = make_app(test_db_empty());
        let req = HttpRequest::builder()
            .uri("/api/auth/login")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_setup_passes_through_after_configuration() {
        let app = make_app(test_db_with_user());
        let req = HttpRequest::builder()
            .uri("/api/cameras")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        // After setup, the route becomes accessible
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_setup_does_not_interfere_with_health_after_setup() {
        let app = make_app(test_db_with_user());
        let req = HttpRequest::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
