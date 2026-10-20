//! Unified config + status endpoints (SPEC v1 §3, §5).
//!
//! `GET/PUT /api/config` folds the former `/api/settings` dotted-key bag and
//! the `/api/protocols/*` section endpoints into one document:
//!
//! ```json
//! {
//!   "settings":  { "ui": {"theme": "dark"}, "web": {"port": "8443"}, … },
//!   "protocols": { "onvif": {…}, "gb28181": {…}, "rtmp": {…},
//!                  "recording": {…}, "webrtc": {…} }
//! }
//! ```
//!
//! PUT accepts a partial document (deep merge). Protocol sections go through
//! the same validate/coerce/persist path as before and hot-toggle the
//! corresponding runtime (ONVIF/GB28181 start/stop) — semantics unchanged,
//! `config_apply.default = "immediate"`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::db::{self};
use crate::errors::ApiError;
use crate::protocol_runtime::{
    ProtocolRuntime, build_onvif_config_from_json, extract_gb28181_config,
};
use crate::routes::protocols::Db;
use crate::server::SharedTools;
use crate::stream_manager::StreamManager;

use super::protocols::validate_and_coerce;

static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Runtime handles behind the `scene` config section (SPEC appendix A
/// #31): the hot-adjustable capability knobs. GET reads live values from
/// these; PUT validates, persists `scene.*` dotted keys into the settings
/// store and stores the new values here.
#[derive(Clone)]
pub struct SceneHandles {
    /// Follow-up window length in ms (0 = off) — shared with the voice
    /// engine's `arm_follow_up`.
    pub follow_up_window_ms: Arc<AtomicU64>,
    /// Weather tool config — read per dialogue turn.
    pub tools: SharedTools,
    /// Away-mode feature knobs (SPEC appendix A #44) — shared with the
    /// away engine; `scene.away.*` writes land here hot.
    pub away: crate::away::SharedAwaySettings,
}

/// Validate a `scene` section and return the dotted key/value rows to
/// persist (settings bag, `scene.` prefix). Strict: unknown keys, wrong
/// types and out-of-range values reject the whole update.
fn validate_scene(scene: &Value) -> Result<Vec<(String, String)>, String> {
    let mut rows = Vec::new();
    let voice = scene.get("voice");
    if let Some(v) = voice {
        let obj = v.as_object().ok_or("scene.voice must be an object")?;
        for (k, val) in obj {
            match k.as_str() {
                "wake_word" => {
                    let w = val
                        .as_str()
                        .ok_or("scene.voice.wake_word must be a string")?;
                    // Same validator that generates the keywords line —
                    // an unusable word must never reach the store (it
                    // would deafen the wake word after restart).
                    streaming::voice::wake_word_to_keyword_line(w)?;
                    rows.push(("scene.voice.wake_word".into(), w.to_string()));
                }
                "follow_up_window_secs" => {
                    let n = val
                        .as_f64()
                        .ok_or("scene.voice.follow_up_window_secs must be a number")?;
                    if !(0.0..=120.0).contains(&n) {
                        return Err(
                            "scene.voice.follow_up_window_secs must be within 0..=120".into()
                        );
                    }
                    rows.push(("scene.voice.follow_up_window_secs".into(), format_number(n)));
                }
                other => return Err(format!("unknown scene.voice key: {other}")),
            }
        }
    }
    let away = scene.get("away");
    if let Some(a) = away {
        let obj = a.as_object().ok_or("scene.away must be an object")?;
        for (k, val) in obj {
            match k.as_str() {
                "voice_control"
                | "summary_on_disarm"
                | "deterrence"
                | "schedule_weekdays_only"
                | "audio_link" => {
                    let b = val
                        .as_bool()
                        .ok_or_else(|| format!("scene.away.{k} must be a boolean"))?;
                    rows.push((format!("scene.away.{k}"), b.to_string()));
                }
                "deterrence_text" | "reask_text" => {
                    let s = val
                        .as_str()
                        .ok_or_else(|| format!("scene.away.{k} must be a string"))?;
                    if s.chars().count() > 80 {
                        return Err(format!("scene.away.{k} too long (max 80 chars)"));
                    }
                    rows.push((format!("scene.away.{k}"), s.to_string()));
                }
                "schedule_arm" | "schedule_disarm" => {
                    let s = val
                        .as_str()
                        .ok_or_else(|| format!("scene.away.{k} must be a string"))?;
                    if !crate::away::valid_schedule_time(s) {
                        return Err(format!("scene.away.{k} must be HH:MM or empty"));
                    }
                    rows.push((format!("scene.away.{k}"), s.to_string()));
                }
                "push_url" => {
                    let s = val.as_str().ok_or("scene.away.push_url must be a string")?;
                    if !s.is_empty() && !(s.starts_with("http://") || s.starts_with("https://")) {
                        return Err("scene.away.push_url must be an http(s) URL or empty".into());
                    }
                    if s.len() > 256 {
                        return Err("scene.away.push_url too long (max 256 chars)".into());
                    }
                    rows.push(("scene.away.push_url".into(), s.to_string()));
                }
                other => return Err(format!("unknown scene.away key: {other}")),
            }
        }
    }
    let tools = scene.get("tools");
    if let Some(t) = tools {
        let obj = t.as_object().ok_or("scene.tools must be an object")?;
        for (k, val) in obj {
            match k.as_str() {
                "weather_enabled" => {
                    let b = val
                        .as_bool()
                        .ok_or("scene.tools.weather_enabled must be a boolean")?;
                    rows.push(("scene.tools.weather_enabled".into(), b.to_string()));
                }
                "weather_city" => {
                    let c = val
                        .as_str()
                        .ok_or("scene.tools.weather_city must be a string")?;
                    if c.chars().count() > 64 {
                        return Err("scene.tools.weather_city too long (max 64 chars)".into());
                    }
                    rows.push(("scene.tools.weather_city".into(), c.to_string()));
                }
                "weather_timeout_secs" => {
                    let n = val
                        .as_f64()
                        .ok_or("scene.tools.weather_timeout_secs must be a number")?;
                    if !(1.0..=30.0).contains(&n) || n.fract() != 0.0 {
                        return Err(
                            "scene.tools.weather_timeout_secs must be an integer within 1..=30"
                                .into(),
                        );
                    }
                    rows.push(("scene.tools.weather_timeout_secs".into(), format_number(n)));
                }
                other => return Err(format!("unknown scene.tools key: {other}")),
            }
        }
    }
    Ok(rows)
}

/// Canonical numeric serialization for the settings bag: integers without
/// a decimal tail, fractional values as-is (round-trips through the boot
/// overlay parser).
fn format_number(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Snapshot of the live scene values (GET shape, SPEC #31/#32). The
/// wake word is restart-class: reported from the persisted store value.
fn scene_document(scene: &SceneHandles, wake_word: Option<String>) -> Value {
    let window_ms = scene.follow_up_window_ms.load(Ordering::SeqCst);
    let tools = tools_snapshot(&scene.tools);
    let away = serde_json::to_value(&*scene.away.read()).unwrap_or_default();
    json!({
        "voice": {
            "follow_up_window_secs": window_ms as f64 / 1000.0,
            "wake_word": wake_word.unwrap_or_else(|| streaming::voice::DEFAULT_WAKE_WORD.into()),
        },
        "tools": tools,
        "away": away,
    })
}

fn tools_snapshot(tools: &SharedTools) -> Value {
    let t = tools.read().expect("tools config lock poisoned");
    json!({
        "weather_enabled": t.weather_enabled,
        "weather_city": t.weather_city,
        "weather_timeout_secs": t.timeout_secs,
    })
}

// ---------------------------------------------------------------------------
// GET /api/status (SPEC §3)
// ---------------------------------------------------------------------------

#[tracing::instrument(skip_all)]
pub async fn status_handler(
    Extension(db): Extension<Db>,
    Extension(advertised_host): Extension<Arc<String>>,
) -> Response {
    let start = PROCESS_START.get_or_init(std::time::Instant::now);
    let cameras = db::list_cameras(&db)
        .await
        .map(|rows| rows.len())
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({
            "device_name": "mibee-eye",
            "model": format!("notebook ({advertised_host})"),
            "vendor": "MiBee Studio",
            "firmware": env!("CARGO_PKG_VERSION"),
            "uptime": start.elapsed().as_secs(),
            "cameras": cameras,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

fn nest_dotted(rows: Vec<(String, String)>) -> Value {
    let mut root = serde_json::Map::new();
    for (key, value) in rows {
        let parts: Vec<&str> = key.split('.').collect();
        let mut node = &mut root;
        for part in &parts[..parts.len().saturating_sub(1)] {
            let entry = node
                .entry(part.to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(serde_json::Map::new());
            }
            node = entry.as_object_mut().expect("just made an object");
        }
        if let Some(last) = parts.last() {
            node.insert((*last).to_string(), Value::String(value));
        }
    }
    Value::Object(root)
}

fn flatten_to_dotted(prefix: &str, value: &Value, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_to_dotted(&key, v, out);
            }
        }
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Bool(b) => out.push((prefix.to_string(), b.to_string())),
        Value::Number(n) => out.push((prefix.to_string(), n.to_string())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

#[tracing::instrument(skip_all)]
pub async fn get_config(
    Extension(db): Extension<Db>,
    Extension(scene): Extension<SceneHandles>,
) -> Response {
    let mut persisted_wake_word: Option<String> = None;
    let settings = match db::list_settings(&db).await {
        Ok(rows) => nest_dotted(
            rows.into_iter()
                .filter(|(k, v)| {
                    if k == "scene.voice.wake_word" {
                        persisted_wake_word = Some(v.clone());
                    }
                    !k.starts_with("scene.")
                })
                .collect(),
        ),
        Err(e) => {
            tracing::error!(error = %e, "failed to list settings");
            return ApiError::internal("failed to list settings").into_response();
        }
    };
    let mut protocols = serde_json::Map::new();
    for key in [
        "onvif",
        "gb28181",
        "rtmp_push",
        "recording",
        "webrtc",
        "watermark",
    ] {
        match db::get_protocol_config(&db, key).await {
            Ok(Some(v)) => {
                protocols.insert(key.to_string(), redact_secrets(v));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, protocol = key, "failed to read protocol config");
                return ApiError::internal("database error").into_response();
            }
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "settings": settings,
            "protocols": protocols,
            "scene": scene_document(&scene, persisted_wake_word),
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// PUT /api/config
// ---------------------------------------------------------------------------

#[tracing::instrument(skip_all)]
pub async fn put_config(
    Extension(db): Extension<Db>,
    Extension(protocol_runtime): Extension<Arc<Mutex<ProtocolRuntime>>>,
    Extension(stream_manager): Extension<Arc<StreamManager>>,
    Extension(advertised_host): Extension<Arc<String>>,
    Extension(scene): Extension<SceneHandles>,
    Json(body): Json<Value>,
) -> Response {
    if !body.is_object()
        || (body.get("settings").is_none()
            && body.get("protocols").is_none()
            && body.get("scene").is_none())
    {
        return ApiError::bad_request("body must contain settings, protocols and/or scene")
            .into_response();
    }

    // -- settings section: flatten + upsert --
    if let Some(settings) = body.get("settings") {
        if !settings.is_object() {
            return ApiError::bad_request("settings must be an object").into_response();
        }
        let mut pairs = Vec::new();
        flatten_to_dotted("", settings, &mut pairs);
        for (key, value) in pairs {
            if let Err(e) = db::set_setting(&db, &key, &value).await {
                tracing::error!(error = %e, setting_key = %key, "failed to set setting");
                return ApiError::internal("failed to update settings").into_response();
            }
        }
    }

    // -- protocol sections: validate + merge + persist + hot-toggle --
    if let Some(proto_map) = body.get("protocols").and_then(|v| v.as_object()) {
        // Key alias: the canonical spec name "rtmp" maps to the DB key "rtmp_push".
        let mut rt = protocol_runtime.lock().await;
        for (name, section) in proto_map {
            let db_key = match name.as_str() {
                "rtmp" => "rtmp_push",
                other => other,
            };
            let section = section.clone();
            let mut validated = match validate_and_coerce(db_key, &section) {
                Ok(v) => v,
                Err(msg) => return ApiError::bad_request(&msg).into_response(),
            };
            let mut current = match db::get_protocol_config(&db, db_key).await {
                Ok(Some(v)) => v,
                Ok(None) => json!({}),
                Err(e) => {
                    tracing::error!(error = %e, protocol = db_key, "failed to read protocol config");
                    return ApiError::internal("database error").into_response();
                }
            };
            // SPEC §5: `"****"` written back verbatim means "keep the stored
            // secret". GET hands the settings UI redacted values, and that UI
            // does a read-modify-write of the whole document — so without this
            // restore, simply saving an unrelated setting would overwrite the
            // GB28181 SIP password with four asterisks and silently break SIP
            // Digest auth on the next re-register.
            restore_secrets(&mut validated, &current);
            if let Some(obj) = validated.as_object() {
                for (k, v) in obj {
                    current[k.as_str()] = v.clone();
                }
            }
            // SPEC §5.2 semantic checks over the MERGED blob (types, ranges
            // and enums are already schema-validated above) — before persist,
            // so a rejected update leaves the stored config untouched:
            // enabled requires content; timestamp format whitelist.
            if db_key == "watermark"
                && let Err(msg) = crate::config::validate_watermark_blob(&current)
            {
                return ApiError::bad_request(&msg).into_response();
            }
            if let Err(e) = db::set_protocol_config(&db, db_key, &current).await {
                tracing::error!(error = %e, protocol = db_key, "failed to persist protocol config");
                return ApiError::internal("database error").into_response();
            }
            tracing::info!(protocol = db_key, "protocol config updated via /api/config");

            // Hot-toggle exactly like the former per-protocol endpoints.
            if db_key == "onvif" {
                let enabled = current
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if enabled {
                    let cfg = build_onvif_config_from_json(&current, &advertised_host);
                    if let Err(e) = rt.start_onvif(cfg, stream_manager.clone()).await {
                        tracing::warn!(error = %e, "failed to start ONVIF after config update");
                    }
                } else {
                    rt.stop_onvif().await;
                }
            } else if db_key == "gb28181" {
                let enabled = current
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if enabled {
                    let cfg = extract_gb28181_config(&current);
                    if let Err(e) = rt.start_gb28181(&cfg, stream_manager.clone()).await {
                        tracing::warn!(error = %e, "failed to start GB28181 after config update");
                    }
                } else {
                    rt.stop_gb28181().await;
                }
            }
            // rtmp_push / recording / webrtc are read at use time — no action.
        }
    }

    // -- scene section (#31): validate + persist + hot-apply --
    if let Some(scene_body) = body.get("scene") {
        if !scene_body.is_object() {
            return ApiError::bad_request("scene must be an object").into_response();
        }
        let rows = match validate_scene(scene_body) {
            Ok(rows) => rows,
            Err(msg) => return ApiError::bad_request(&msg).into_response(),
        };
        for (key, value) in &rows {
            if let Err(e) = db::set_setting(&db, key, value).await {
                tracing::error!(error = %e, setting_key = %key, "failed to persist scene key");
                return ApiError::internal("failed to update scene config").into_response();
            }
        }
        // Hot-apply AFTER every row persisted — a late failure can't leave
        // the runtime diverged from the store on the persisted keys.
        for (key, value) in &rows {
            match key.as_str() {
                "scene.voice.wake_word" => {
                    // Restart-class: the KWS engine is built at boot. The
                    // auto-restart flow (config_apply.auto) applies it.
                }
                "scene.voice.follow_up_window_secs" => {
                    let secs: f64 = value.parse().expect("validated number");
                    scene
                        .follow_up_window_ms
                        .store((secs * 1000.0) as u64, Ordering::SeqCst);
                }
                "scene.tools.weather_enabled" => {
                    scene
                        .tools
                        .write()
                        .expect("tools config lock poisoned")
                        .weather_enabled = value == "true";
                }
                "scene.tools.weather_city" => {
                    scene
                        .tools
                        .write()
                        .expect("tools config lock poisoned")
                        .weather_city = value.clone();
                }
                "scene.tools.weather_timeout_secs" => {
                    let secs: f64 = value.parse().expect("validated number");
                    scene
                        .tools
                        .write()
                        .expect("tools config lock poisoned")
                        .timeout_secs = secs as u64;
                }
                k if k.starts_with("scene.away.") => {
                    // Every away key maps 1:1 onto an AwaySettings field —
                    // validated above, hot-applied under the write lock.
                    let mut s = scene.away.write();
                    let field = &k["scene.away.".len()..];
                    match field {
                        "voice_control" => s.voice_control = value == "true",
                        "summary_on_disarm" => s.summary_on_disarm = value == "true",
                        "deterrence" => s.deterrence = value == "true",
                        "schedule_weekdays_only" => s.schedule_weekdays_only = value == "true",
                        "audio_link" => s.audio_link = value == "true",
                        "deterrence_text" => s.deterrence_text = value.clone(),
                        "reask_text" => s.reask_text = value.clone(),
                        "schedule_arm" => s.schedule_arm = value.clone(),
                        "schedule_disarm" => s.schedule_disarm = value.clone(),
                        "push_url" => s.push_url = value.clone(),
                        // This arm used to be `unreachable!("validated scene.away keys")`,
                        // which asserted that the validator above and this
                        // applier enumerate the same key set. Two hand-kept
                        // lists that must agree is a panic waiting to happen —
                        // and a bad one: the settings row is already
                        // persisted by the time we get here, so drifting the
                        // lists turns an authenticated PUT into a remote 500
                        // plus a stored-but-unapplied setting. Warn like the
                        // sibling `other` arm instead; the next boot overlay
                        // still picks it up.
                        unknown => tracing::warn!(
                            key = unknown,
                            "scene.away key validated but not hot-applied; \
                             it will apply on next start"
                        ),
                    }
                }
                other => tracing::warn!(key = other, "scene key persisted but not hot-applied"),
            }
        }
        tracing::info!(keys = ?rows.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            "scene config updated via /api/config (hot)");
    }

    let applied = if body
        .get("scene")
        .and_then(|s| s.get("voice"))
        .and_then(|v| v.get("wake_word"))
        .is_some()
    {
        "restart"
    } else {
        "immediate"
    };
    (StatusCode::OK, Json(json!({"applied": applied}))).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/system/restart (SPEC §5.1)
// ---------------------------------------------------------------------------

/// Graceful restart: fires the same shutdown watch as SIGTERM (the
/// service unit restarts us — deregister + cleanup all run first).
/// Idempotent — the watch send is a no-op once true.
#[tracing::instrument(skip_all)]
pub async fn restart_handler(
    Extension(restart_tx): Extension<tokio::sync::watch::Sender<bool>>,
) -> Response {
    tracing::info!("restart requested via POST /api/system/restart");
    tokio::spawn(async move {
        // Let the 200 flush before the listener tears down.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = restart_tx.send(true);
    });
    (StatusCode::OK, Json(json!({"status": "restarting"}))).into_response()
}

// ---------------------------------------------------------------------------
// Router helper
pub fn routes() -> Router {
    Router::new()
        .route("/api/status", get(status_handler))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/system/restart", axum::routing::post(restart_handler))
}

// ---------------------------------------------------------------------------
// Secret redaction (SPEC §5)
// ---------------------------------------------------------------------------

/// The placeholder a redacted secret reads as on the wire, and the value a
/// client sends back unchanged to mean "keep what is stored".
pub const REDACTED: &str = "****";

/// Config keys whose values are credentials and must never leave the device
/// in cleartext.
///
/// This is a name match on the leaf key, applied at any depth, so a future
/// subsection cannot accidentally opt out of it by nesting.
///
/// - `password` — the GB28181 SIP Digest secret (`Gb28181Config::password`).
/// - `push_url` — RTMP ingest URLs conventionally carry the stream key, and
///   usually the userinfo too (`rtmp://user:pass@host/app/key`).
/// - `api_key` — the OpenRouter-compatible cloud key (SPEC §4.10), if it
///   ever appears in the config surface rather than only the dedicated table.
///
/// ONVIF `username` is deliberately NOT redacted: SPEC treats it as
/// configuration the settings UI renders, and the device's ONVIF credentials
/// are already discoverable from the device itself by anyone on the LAN.
const SECRET_KEYS: &[&str] = &["password", "push_url", "api_key"];

/// Replace every secret leaf value with [`REDACTED`], recursing through
/// objects and arrays. Non-string values under a secret key are redacted too
/// (a number is still a value we should not echo).
///
/// Empty secrets stay empty rather than becoming `"****"` — `"****"` means
/// "a secret exists", so showing it for an unset field would misreport the
/// device's configuration state to the settings UI.
#[must_use]
pub fn redact_secrets(mut value: serde_json::Value) -> serde_json::Value {
    fn walk(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if SECRET_KEYS.contains(&key.to_ascii_lowercase().as_str())
                        && !matches!(child, serde_json::Value::Null)
                        && child.as_str() != Some("")
                    {
                        *child = serde_json::Value::String(REDACTED.to_string());
                    } else {
                        walk(child);
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut value);
    value
}

/// Restore stored secrets under a client-submitted document.
///
/// SPEC §5: a `"****"` value written back verbatim means "keep the stored
/// secret" — that is what makes the settings UI's read-modify-write cycle
/// safe. Without it, every save would overwrite the real password with four
/// asterisks, silently breaking SIP Digest auth and RTMP push on the next
/// reconnect. `stored` is the current document straight from storage.
pub fn restore_secrets(submitted: &mut serde_json::Value, stored: &serde_json::Value) {
    fn walk(submitted: &mut serde_json::Value, stored: &serde_json::Value) {
        match (submitted, stored) {
            (serde_json::Value::Object(sub_map), serde_json::Value::Object(stored_map)) => {
                for (key, sub_child) in sub_map.iter_mut() {
                    let Some(stored_child) = stored_map.get(key) else {
                        continue;
                    };
                    if sub_child.as_str() == Some(REDACTED) {
                        *sub_child = stored_child.clone();
                    } else {
                        walk(sub_child, stored_child);
                    }
                }
            }
            (serde_json::Value::Array(sub_items), serde_json::Value::Array(stored_items)) => {
                for (sub_child, stored_child) in sub_items.iter_mut().zip(stored_items) {
                    walk(sub_child, stored_child);
                }
            }
            _ => {}
        }
    }
    walk(submitted, stored);
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rusqlite::Connection;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    // -- Secret redaction (SPEC §5) ----------------------------------------
    //
    // Before this, `GET /api/config` and both `GET /api/protocols/*` returned
    // the stored document verbatim: the GB28181 SIP Digest password and the
    // RTMP push URL (which conventionally embeds the stream key and userinfo)
    // travelled to every browser session in cleartext, and every future
    // credential field added to any subsection would too.

    #[test]
    fn redaction_masks_credentials_at_any_depth() {
        let doc = serde_json::json!({
            "enabled": true,
            "password": "sip-secret",
            "device_id": "34020000001320000001",
            "nested": { "password": "deep-secret", "port": 5060 },
            "list": [{ "password": "in-array" }],
            "push_url": "rtmp://user:pw@ingest.example/live/key",
        });
        let redacted = redact_secrets(doc);
        assert_eq!(redacted["password"], serde_json::json!(REDACTED));
        assert_eq!(redacted["nested"]["password"], serde_json::json!(REDACTED));
        assert_eq!(redacted["list"][0]["password"], serde_json::json!(REDACTED));
        assert_eq!(redacted["push_url"], serde_json::json!(REDACTED));
        // Non-secret siblings survive intact — the settings UI renders them.
        assert_eq!(
            redacted["device_id"],
            serde_json::json!("34020000001320000001")
        );
        assert_eq!(redacted["nested"]["port"], serde_json::json!(5060));
        assert_eq!(redacted["enabled"], serde_json::json!(true));
    }

    #[test]
    fn redaction_leaves_unset_secrets_empty_so_state_is_not_misreported() {
        // `"****"` means "a secret exists". Showing it for an unset field
        // would make the settings UI claim a password is configured when it
        // is not.
        let redacted = redact_secrets(serde_json::json!({
            "password": "",
            "push_url": null,
        }));
        assert_eq!(redacted["password"], serde_json::json!(""));
        assert!(redacted["push_url"].is_null());
    }

    #[test]
    fn settings_round_trip_does_not_overwrite_stored_secrets() {
        // The exact bug: the UI GETs a redacted document, edits an unrelated
        // field and PUTs the whole thing back. Without the restore, that saves
        // `"****"` as the live SIP password and silently breaks Digest auth.
        let stored = serde_json::json!({
            "enabled": true,
            "password": "real-sip-secret",
            "device_id": "old-id",
            "push_url": "rtmp://u:p@host/live/key",
        });
        let mut submitted = redact_secrets(stored.clone());
        // The user flips one unrelated setting.
        submitted["device_id"] = serde_json::json!("new-id");
        restore_secrets(&mut submitted, &stored);

        assert_eq!(submitted["password"], serde_json::json!("real-sip-secret"));
        assert_eq!(
            submitted["push_url"],
            serde_json::json!("rtmp://u:p@host/live/key")
        );
        assert_eq!(submitted["device_id"], serde_json::json!("new-id"));
    }

    #[test]
    fn an_explicitly_typed_secret_still_replaces_the_stored_one() {
        // A user who actually wants to change the password types a new value,
        // and that must win — the restore only fires on the exact placeholder.
        let stored = serde_json::json!({ "password": "old" });
        let mut submitted = serde_json::json!({ "password": "new" });
        restore_secrets(&mut submitted, &stored);
        assert_eq!(submitted["password"], serde_json::json!("new"));

        // Clearing it is also intentional and must not be read as a placeholder.
        let mut cleared = serde_json::json!({ "password": "" });
        restore_secrets(&mut cleared, &stored);
        assert_eq!(cleared["password"], serde_json::json!(""));
    }

    #[test]
    fn restoring_is_safe_when_nothing_is_stored_yet() {
        let mut submitted = serde_json::json!({ "password": REDACTED });
        restore_secrets(&mut submitted, &serde_json::json!({}));
        // No stored value to restore from: the placeholder stays rather than
        // becoming a literal four-asterisk password.
        assert_eq!(submitted["password"], serde_json::json!(REDACTED));
    }

    async fn test_db() -> (sqlx::SqlitePool, Arc<Mutex<Connection>>) {
        let (pool, auth_db) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool).await.unwrap();
        {
            let conn = auth_db.lock().await;
            conn.execute_batch(include_str!("../../../../migrations/001_initial.sql"))
                .unwrap();
            security::auth::init_users_table(&conn).unwrap();
            let hash = security::password::hash_password("test_pass").unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO users (username, password_hash) VALUES (?1, ?2)",
                rusqlite::params!["admin", hash],
            )
            .unwrap();
        }
        (pool, auth_db)
    }

    fn build_state(
        db: sqlx::SqlitePool,
        auth_db: Arc<Mutex<Connection>>,
    ) -> crate::server::AppRouterState {
        // Reuse the settings tests' default-state shape via a fresh build:
        // every engine is a fail-open default instance.
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
            protocol_runtime: Arc::new(Mutex::new(crate::protocol_runtime::ProtocolRuntime::new())),
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
            agent: crate::server::AppRouterState::agent_for_tests().0,
            agent_config: crate::server::AppRouterState::agent_for_tests().1,
            models_routing: streaming::model_route::RoutingConfig::default(),
            plugins_loaded: 0,
            away: crate::server::AppRouterState::away_for_tests(),
        }
    }

    fn config_req(method: &str, token: &str, body: Option<Value>) -> Request<Body> {
        let mut b = Request::builder()
            .uri("/api/config")
            .method(method)
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf");
        if let Some(v) = body {
            b = b.header("content-type", "application/json");
            b.body(Body::from(serde_json::to_vec(&v).unwrap())).unwrap()
        } else {
            b.body(Body::empty()).unwrap()
        }
    }

    /// End-to-end through the real router: a stored GB28181 SIP password and RTMP
    /// push URL must not appear anywhere in `GET /api/config`, and saving an
    /// unrelated setting must leave them intact in storage.
    #[tokio::test]
    async fn config_endpoint_never_emits_stored_credentials() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };

        let stored = serde_json::json!({
            "enabled": true,
            "device_id": "34020000001320000001",
            "platform_sip_address": "192.0.2.10",
            "platform_sip_port": 5060,
            "username": "3402000000",
            "password": "super-secret-sip-password",
            "sip_domain": "3402000000",
            "register_interval_secs": 60,
            "heartbeat_interval_secs": 30,
            "heartbeat_timeout_count": 3,
            "channel_id": "34020000001320000001",
            "local_sip_port": 5060,
            "talkback_playback": false,
        });
        db::set_protocol_config(&pool, "gb28181", &stored)
            .await
            .unwrap();
        db::set_protocol_config(
            &pool,
            "rtmp_push",
            &serde_json::json!({
                "enabled": true,
                "push_url": "rtmp://user:hunter2@ingest.example/live/streamkey",
            }),
        )
        .await
        .unwrap();

        let app = crate::server::build_app_with_state(build_state(pool.clone(), auth_db));
        let res = app
            .clone()
            .oneshot(config_req("GET", &token, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let raw = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&raw);

        assert!(
            !body.contains("super-secret-sip-password"),
            "GET /api/config leaked the GB28181 SIP password in cleartext"
        );
        assert!(
            !body.contains("hunter2") && !body.contains("streamkey"),
            "GET /api/config leaked RTMP push credentials"
        );
        // Redacted placeholders present, non-secret config intact.
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            json["data"]["protocols"]["gb28181"]["password"],
            Value::String(REDACTED.into())
        );
        assert_eq!(
            json["data"]["protocols"]["gb28181"]["device_id"],
            Value::String("34020000001320000001".into())
        );

        // Now do what the settings UI does: read the redacted document, change an
        // unrelated field, write the whole `protocols` subtree back.
        let mut protocols = json["data"]["protocols"].clone();
        protocols["gb28181"]["device_id"] = Value::String("34020000001320000009".into());
        let res = app
            .clone()
            .oneshot(config_req(
                "PUT",
                &token,
                Some(serde_json::json!({ "protocols": protocols })),
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "the save must be accepted; body was {}",
            serde_json::to_string_pretty(&protocols).unwrap()
        );

        // The secret in storage must be untouched.
        let after = db::get_protocol_config(&pool, "gb28181")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after["password"],
            Value::String("super-secret-sip-password".into()),
            "saving an unrelated setting overwrote the SIP password with \"****\" — \
         SIP Digest auth would fail on the next re-register"
        );
        assert_eq!(
            after["device_id"],
            Value::String("34020000001320000009".into())
        );

        let rtmp = db::get_protocol_config(&pool, "rtmp_push")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rtmp["push_url"],
            Value::String("rtmp://user:hunter2@ingest.example/live/streamkey".into())
        );
    }

    #[tokio::test]
    async fn scene_get_put_roundtrip_hot_and_persisted() {
        let (pool, auth_db) = test_db().await;
        // create a session directly in the auth db
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let app = crate::server::build_app_with_state(build_state(pool.clone(), auth_db));

        // Defaults visible in GET (voice window 0 = boot default)
        let res = app
            .clone()
            .oneshot(config_req("GET", &token, None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"]["scene"]["voice"]["follow_up_window_secs"], 0.0);
        assert_eq!(body["data"]["scene"]["tools"]["weather_city"], "");

        // PUT partial scene
        let res = app
            .clone()
            .oneshot(config_req(
                "PUT",
                &token,
                Some(json!({
                    "scene": {
                        "voice": {"follow_up_window_secs": 8},
                        "tools": {"weather_city": "Shanghai", "weather_enabled": true}
                    }
                })),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["data"]["applied"], "immediate");

        // GET reflects the hot values and does NOT leak scene.* into settings
        let res = app
            .clone()
            .oneshot(config_req("GET", &token, None))
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"]["scene"]["voice"]["follow_up_window_secs"], 8.0);
        assert_eq!(body["data"]["scene"]["tools"]["weather_city"], "Shanghai");
        assert!(
            body["data"]["scene"]["tools"]["weather_enabled"]
                .as_bool()
                .unwrap()
        );
        assert!(
            body["data"]["settings"].get("scene").is_none(),
            "scene.* rows must not surface under settings: {}",
            body["data"]["settings"]
        );

        // Away feature knobs (SPEC appendix A #44 amendment): defaults →
        // PUT → hot GET.
        assert_eq!(body["data"]["scene"]["away"]["voice_control"], true);
        assert_eq!(body["data"]["scene"]["away"]["deterrence"], false);
        assert_eq!(body["data"]["scene"]["away"]["schedule_arm"], "");
        let res = app
            .clone()
            .oneshot(config_req(
                "PUT",
                &token,
                Some(json!({
                    "scene": {
                        "away": {
                            "deterrence": true,
                            "schedule_arm": "09:00",
                            "schedule_disarm": "18:00",
                            "schedule_weekdays_only": true,
                            "push_url": "https://example.local/hook",
                            "deterrence_text": "影像已记录。"
                        }
                    }
                })),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(config_req("GET", &token, None))
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let away = &body["data"]["scene"]["away"];
        assert_eq!(away["deterrence"], true);
        assert_eq!(away["schedule_arm"], "09:00");
        assert_eq!(away["schedule_disarm"], "18:00");
        assert_eq!(away["schedule_weekdays_only"], true);
        assert_eq!(away["push_url"], "https://example.local/hook");
        assert_eq!(away["deterrence_text"], "影像已记录。");
        assert_eq!(away["voice_control"], true, "untouched key keeps value");

        // Bad values reject the whole update (strict scene contract).
        for bad in [
            json!({"away": {"schedule_arm": "9:00"}}),
            json!({"away": {"schedule_arm": "24:00"}}),
            json!({"away": {"push_url": "ftp://x"}}),
            json!({"away": {"voice_control": "yes"}}),
            json!({"away": {"nonsense": true}}),
        ] {
            let res = app
                .clone()
                .oneshot(config_req("PUT", &token, Some(json!({"scene": bad}))))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "reject {bad}");
        }

        // Persisted as dotted scene.* rows (boot overlay source)
        let rows = db::list_settings(&pool).await.unwrap();
        let row = rows
            .iter()
            .find(|(k, _)| k == "scene.voice.follow_up_window_secs")
            .expect("persisted");
        assert_eq!(row.1, "8");
        assert!(
            rows.iter()
                .any(|(k, v)| k == "scene.tools.weather_city" && v == "Shanghai")
        );
    }

    #[tokio::test]
    async fn scene_wake_word_is_restart_class_and_persisted() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let app = crate::server::build_app_with_state(build_state(pool.clone(), auth_db));

        // GET default
        let res = app
            .clone()
            .oneshot(config_req("GET", &token, None))
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["data"]["scene"]["voice"]["wake_word"],
            streaming::voice::DEFAULT_WAKE_WORD
        );

        // PUT a custom word → applied restart + persisted
        let res = app
            .clone()
            .oneshot(config_req(
                "PUT",
                &token,
                Some(json!({"scene": {"voice": {"wake_word": "你好小蜂"}}})),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["data"]["applied"], "restart");
        let rows = db::list_settings(&pool).await.unwrap();
        assert!(
            rows.iter()
                .any(|(k, v)| k == "scene.voice.wake_word" && v == "你好小蜂")
        );

        // GET reflects it
        let res = app
            .clone()
            .oneshot(config_req("GET", &token, None))
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(res.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["data"]["scene"]["voice"]["wake_word"], "你好小蜂");

        // Non-wake-word scene saves stay immediate
        let res = app
            .clone()
            .oneshot(config_req(
                "PUT",
                &token,
                Some(json!({"scene": {"tools": {"weather_city": "Shenzhen"}}})),
            ))
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["data"]["applied"], "immediate");
    }

    #[tokio::test]
    async fn restart_endpoint_returns_restarting_and_signals() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let mut state = build_state(pool, auth_db);
        state.restart_tx = tx;
        let app = crate::server::build_app_with_state(state);

        let req = Request::builder()
            .uri("/api/system/restart")
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["data"]["status"], "restarting");
        let req2 = Request::builder()
            .uri("/api/system/restart")
            .method("POST")
            .header("cookie", format!("session={token}; csrf-token=test-csrf"))
            .header("x-csrf-token", "test-csrf")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(req2).await.unwrap().status(), StatusCode::OK);
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.changed())
            .await
            .expect("restart signal within 2s")
            .unwrap();
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn scene_put_rejects_invalid_payloads() {
        let (pool, auth_db) = test_db().await;
        let token = {
            let c = auth_db.lock().await;
            security::auth::create_session(&c, "admin").unwrap()
        };
        let app = crate::server::build_app_with_state(build_state(pool.clone(), auth_db));

        for bad in [
            json!({"scene": {"voice": {"follow_up_window_secs": -1}}}),
            json!({"scene": {"voice": {"follow_up_window_secs": 121}}}),
            json!({"scene": {"voice": {"follow_up_window_secs": "eight"}}}),
            json!({"scene": {"voice": {"no_such_key": 1}}}),
            json!({"scene": {"tools": {"weather_timeout_secs": 0}}}),
            json!({"scene": {"tools": {"weather_timeout_secs": 2.5}}}),
            json!({"scene": {"tools": {"weather_enabled": "yes"}}}),
            json!({"scene": {"voice": {"wake_word": "bee"}}}),
            json!({"scene": {"voice": {"wake_word": "一二三四五六七"}}}),
            json!({"scene": "not-an-object"}),
        ] {
            let res = app
                .clone()
                .oneshot(config_req("PUT", &token, Some(bad.clone())))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "payload: {bad}");
        }
        // Nothing persisted by the rejected writes
        let rows = db::list_settings(&pool).await.unwrap();
        assert!(rows.iter().all(|(k, _)| !k.starts_with("scene.")));
    }
}
