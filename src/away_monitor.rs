//! Away-mode watch orchestrator (SPEC v1 §3.6, notebook dialect
//! appendix A #44).
//!
//! One task consumes the AI engine's detection broadcast (the same
//! stream the alarm bridge and the SSE `ai_detection` bridge tap) plus
//! the voice engine's events, and runs the legs the pure decision
//! engine in `web::away` asks for:
//!
//! * **Visitor** (person arrival rising edge) — save the triggering
//!   frame, face-match it, persist + broadcast the record, notify the
//!   desktop, greet over the speaker (known faces by name), ask an
//!   unknown visitor who they are, open the one-shot listening window
//!   and attribute their answer to the record. The VLM describes the
//!   frame asynchronously and patches the same row.
//! * **Activity** (configured labels rising edge) — snapshot + record.
//!
//! Everything here is fail-open per the house contract: a failing
//! record, TTS, VLM or desktop leg logs and moves on; the watch itself
//! never stops on a leg error.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sqlx::SqlitePool;
use streaming::ai::AiEngine;
use streaming::face::FaceEngine;
use streaming::face::FaceHit;
use streaming::tts::TtsEngine;
use streaming::vlm::VlmEngine;
use streaming::voice::VoiceEngine;
use web::away::AwayAction;
use web::away::AwayConfig;
use web::away::AwayEngine;
use web::away::AwayEventRecord;
use web::routes::events::CameraEvent;
use web::routes::events::EventBus;

use crate::desktop::Desktop;

/// Away-side VLM single-flight: one description at a time, new visitor
/// events during a run are dropped (the engine mutex would serialize
/// them anyway — this just keeps contexts from stacking).
static AWAY_VLM_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Grace added to the nominal listen window before the slot is reaped:
/// the voice worker extends its own deadline +2 s per completed segment,
/// and a real answer landing a beat late still belongs to the visitor.
const LISTEN_GRACE_MS: u64 = 5_000;

pub struct AwayMonitor {
    pub away: Arc<AwayEngine>,
    pub ai: Arc<AiEngine>,
    pub voice: Arc<VoiceEngine>,
    pub tts: Arc<TtsEngine>,
    pub face: Arc<FaceEngine>,
    pub vlm: Arc<VlmEngine>,
    pub desktop: Arc<Desktop>,
    pub pool: SqlitePool,
    pub event_tx: Arc<EventBus>,
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Spawn the watch task. Runs forever; both source channels live as
/// long as their engines (the engines own the sender halves), so the
/// loop only exits if something structurally shuts down.
pub fn spawn(m: AwayMonitor) {
    let AwayMonitor {
        away,
        ai,
        voice,
        tts,
        face,
        vlm,
        desktop,
        pool,
        event_tx,
    } = m;
    let mut ai_events = ai.subscribe_events();
    let mut voice_events = voice.subscribe_events();
    tracing::info!(
        available = away.is_available(),
        armed = away.is_armed(),
        voice = away.voice_capable(),
        "away: watch task started"
    );
    tokio::spawn(async move {
        loop {
            tokio::select! {
                r = ai_events.recv() => match r {
                    Ok(ev) => {
                        if !away.is_armed() {
                            continue;
                        }
                        let now = unix_now_ms();
                        match away.observe(&ev.camera_id, &ev.detections, now) {
                            AwayAction::Visitor { person_count } => {
                                visitor_flow(
                                    &away, &voice, &tts, &face, &vlm, &desktop,
                                    &pool, &event_tx, &ev.camera_id, person_count,
                                    ev.jpeg.clone(),
                                )
                                .await;
                            }
                            AwayAction::Activity { labels } => {
                                activity_flow(&away, &pool, &event_tx, &ev.camera_id, &labels, ev.jpeg.clone())
                                    .await;
                            }
                            AwayAction::None => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "away: ai stream lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                r = voice_events.recv() => match r {
                    Ok(v) => {
                        // Only no-wake-word segments can be visitor
                        // answers; wake-word hits belong to the normal
                        // voice bridge.
                        if !v.follow_up || v.transcript.trim().is_empty() || !away.is_armed() {
                            continue;
                        }
                        let id = match away.attribute_listen(unix_now_ms()) {
                            Some(id) => id,
                            None => continue,
                        };
                        match web::db::update_away_reply(&pool, id, &v.transcript).await {
                            Ok(Some(rec)) => {
                                tracing::info!(event = id, reply = %v.transcript, "away: visitor answered");
                                broadcast(&event_tx, rec);
                            }
                            _ => tracing::warn!(event = id, "away: visitor reply record failed"),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "away: voice stream lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}

fn broadcast(event_tx: &Arc<EventBus>, rec: AwayEventRecord) {
    let _ = event_tx.send(CameraEvent::AwayEvent { event: rec });
}

/// Speak one utterance with the playback self-mute held for its whole
/// duration (greeting / re-ask / deterrence share this leg).
async fn speak_text(
    tts: &Arc<TtsEngine>,
    voice: &Arc<VoiceEngine>,
    text: &str,
) -> anyhow::Result<()> {
    voice.begin_playback_mute();
    let tts = Arc::clone(tts);
    let text = text.to_string();
    let result = tokio::task::spawn_blocking(move || tts.speak(&text)).await;
    voice.end_playback_mute();
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// The greeting for a visitor: enrolled faces by name (no question),
/// unknown faces get the identity question (SPEC §3.6).
#[must_use]
pub fn greeting_text(config: &AwayConfig, face_name: Option<&str>) -> String {
    match face_name {
        Some(name) => config.greeting_known.replace("{name}", name),
        None => config.greeting_unknown.clone(),
    }
}

/// Highest-scoring enrolled match among the hits (unknown-only → None).
#[must_use]
pub fn best_named_hit(hits: &[FaceHit]) -> Option<String> {
    hits.iter()
        .filter(|h| h.name.is_some())
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .and_then(|h| h.name.clone())
}

/// Sanitize the camera id into a file-name-safe fragment (ids are UUIDs
/// or "0", but a hostile row must not smuggle a path).
#[must_use]
fn safe_fragment(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect()
}

async fn save_snapshot(
    config: &AwayConfig,
    camera_id: &str,
    jpeg: Option<&[u8]>,
    now_ms: u64,
) -> Option<String> {
    let jpeg = jpeg?;
    let name = format!("{}-{}.jpg", now_ms, safe_fragment(camera_id));
    let dir = config.snapshot_dir.clone();
    let path = std::path::Path::new(&dir).join(&name);
    let write = tokio::task::spawn_blocking({
        let path = path.clone();
        let jpeg = jpeg.to_vec();
        move || {
            std::fs::create_dir_all(&dir).ok();
            std::fs::write(&path, jpeg)
        }
    })
    .await;
    match write {
        Ok(Ok(())) => Some(name),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "away: snapshot write failed");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "away: snapshot task panicked");
            None
        }
    }
}

async fn insert_and_broadcast(
    away: &Arc<AwayEngine>,
    pool: &SqlitePool,
    event_tx: &Arc<EventBus>,
    record: AwayEventRecord,
) -> Option<AwayEventRecord> {
    match web::db::insert_away_event(pool, &record).await {
        Ok((stored, pruned)) => {
            web::routes::away::unlink_snapshots(&away.config().snapshot_dir, &pruned);
            broadcast(event_tx, stored.clone());
            Some(stored)
        }
        Err(e) => {
            tracing::warn!(error = %e, "away: event insert failed");
            None
        }
    }
}

/// Visitor pipeline (person rising edge). Runs to completion inside the
/// watch task's `select!` arm — the slow legs (TTS, VLM, listen
/// expiry) run on their own spawned tasks.
#[allow(clippy::too_many_arguments)]
async fn visitor_flow(
    away: &Arc<AwayEngine>,
    voice: &Arc<VoiceEngine>,
    tts: &Arc<TtsEngine>,
    face: &Arc<FaceEngine>,
    vlm: &Arc<VlmEngine>,
    desktop: &Arc<Desktop>,
    pool: &SqlitePool,
    event_tx: &Arc<EventBus>,
    camera_id: &str,
    person_count: usize,
    jpeg: Option<Arc<[u8]>>,
) {
    let now = unix_now_ms();
    let cfg = away.config().clone();

    // Face match on the triggering frame (~100 ms ONNX; blocking pool).
    let face_name = match (&jpeg, face.is_active()) {
        (Some(j), true) => {
            let face = Arc::clone(face);
            let j = Arc::clone(j);
            match tokio::task::spawn_blocking(move || best_named_hit(&face.match_jpeg(&j))).await {
                Ok(name) => name,
                Err(e) => {
                    tracing::warn!(error = %e, "away: face match task failed");
                    None
                }
            }
        }
        _ => None,
    };

    let snapshot = save_snapshot(&cfg, camera_id, jpeg.as_deref(), now).await;
    let initial_state = if !away.voice_capable() || !tts.is_active() {
        "no_voice"
    } else if face_name.is_some() {
        "known"
    } else {
        "greeting"
    };
    let Some(stored) = insert_and_broadcast(
        away,
        pool,
        event_tx,
        AwayEventRecord {
            id: 0,
            camera_id: camera_id.to_string(),
            kind: "person".into(),
            started_ms: now as i64,
            labels: format!("person×{person_count}"),
            face_name: face_name.clone(),
            description: None,
            visitor_reply: None,
            snapshot,
            state: initial_state.to_string(),
        },
    )
    .await
    else {
        return;
    };
    tracing::info!(
        event = stored.id,
        face = face_name.as_deref().unwrap_or("<unknown>"),
        "away: visitor event"
    );
    desktop.notify_away_visitor(face_name.as_deref());

    // Opt-in visitor push (scene.away.push_url): one JSON POST per
    // visitor event, text only (never the snapshot). Empty URL = the
    // device stays fully air-gapped.
    {
        let settings = away.settings_snapshot();
        if !settings.push_url.is_empty() {
            let url = settings.push_url;
            let id = stored.id;
            let title = "MiBee Eye · 离家模式访客".to_string();
            let body = match &face_name {
                Some(name) => format!("识别到已登记成员：{name}"),
                None => "发现陌生访客，已问候并询问身份".to_string(),
            };
            tokio::spawn(async move {
                web::away::push_visitor_event(&url, id, &title, &body).await;
            });
        }
    }

    // VLM description — async, single-flight, patches the same row.
    if vlm.is_active()
        && let Some(j) = jpeg.as_ref()
        && !AWAY_VLM_IN_FLIGHT.swap(true, Ordering::SeqCst)
    {
        let vlm = Arc::clone(vlm);
        let pool = pool.clone();
        let event_tx = Arc::clone(event_tx);
        let id = stored.id;
        let jpeg_bytes = Arc::clone(j);
        tokio::spawn(async move {
            let described =
                tokio::task::spawn_blocking(move || vlm.describe_jpeg(&jpeg_bytes)).await;
            AWAY_VLM_IN_FLIGHT.store(false, Ordering::SeqCst);
            match described {
                Ok(Ok(description)) => {
                    if let Ok(Some(rec)) =
                        web::db::update_away_description(&pool, id, &description).await
                    {
                        broadcast(&event_tx, rec);
                    }
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "away: vlm description failed"),
                Err(e) => tracing::warn!(error = %e, "away: vlm task panicked"),
            }
        });
    }

    // Voice legs: greet (self-muted), then ask + listen if unknown. Runs
    // on its own task — playback takes seconds and the watch loop must
    // keep consuming detections meanwhile. With `deterrence` on
    // (scene.away.deterrence), an unanswered stranger is re-asked once
    // (state `reask`, half window) and a still-silent one gets the
    // deterrence message (SPEC appendix A #44 amendment).
    if away.voice_capable() && tts.is_active() {
        let away = Arc::clone(away);
        let voice = Arc::clone(voice);
        let tts = Arc::clone(tts);
        let pool = pool.clone();
        let event_tx = Arc::clone(event_tx);
        let stored_id = stored.id;
        let stored_state = stored.state.clone();
        let face_name = face_name.clone();
        tokio::spawn(async move {
            let text = greeting_text(&away.config().clone(), face_name.as_deref());
            voice.begin_playback_mute();
            let tts_for_speak = Arc::clone(&tts);
            let spoken = tokio::task::spawn_blocking(move || tts_for_speak.speak(&text)).await;
            voice.end_playback_mute();
            match spoken {
                Ok(Ok(_)) if face_name.is_none() => {
                    let listen_ms = away.config().listen_secs * 1000;
                    let deadline = unix_now_ms() + listen_ms + LISTEN_GRACE_MS;
                    if voice.arm_listen(listen_ms) && away.open_listen(stored_id, deadline) {
                        if let Ok(Some(rec)) =
                            web::db::update_away_state(&pool, stored_id, "listening").await
                        {
                            broadcast(&event_tx, rec);
                        }
                        // Reap the window: an unanswered visitor escalates
                        // (re-ask + deterrence) or flips straight to silent.
                        let away = Arc::clone(&away);
                        let voice = Arc::clone(&voice);
                        let tts = Arc::clone(&tts);
                        let pool = pool.clone();
                        let event_tx = Arc::clone(&event_tx);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(
                                listen_ms + LISTEN_GRACE_MS + 3_000,
                            ))
                            .await;
                            let Some(id) = away.expire_listen(unix_now_ms()) else {
                                return; // answered in time
                            };
                            let settings = away.settings_snapshot();
                            if !settings.deterrence {
                                if let Ok(Some(rec)) =
                                    web::db::update_away_state(&pool, id, "silent").await
                                {
                                    broadcast(&event_tx, rec);
                                }
                                return;
                            }
                            // Escalation 1: re-ask once on a half window.
                            if let Ok(Some(rec)) =
                                web::db::update_away_state(&pool, id, "reask").await
                            {
                                broadcast(&event_tx, rec);
                            }
                            let reask_ms = (listen_ms / 2).max(4_000);
                            if speak_text(&tts, &voice, &settings.reask_text)
                                .await
                                .is_err()
                            {
                                tracing::warn!("away: re-ask playback failed");
                            }
                            let deadline = unix_now_ms() + reask_ms + LISTEN_GRACE_MS;
                            if !voice.arm_listen(reask_ms) || !away.open_listen(id, deadline) {
                                if let Ok(Some(rec)) =
                                    web::db::update_away_state(&pool, id, "silent").await
                                {
                                    broadcast(&event_tx, rec);
                                }
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(
                                reask_ms + LISTEN_GRACE_MS + 3_000,
                            ))
                            .await;
                            if let Some(id) = away.expire_listen(unix_now_ms()) {
                                // Escalation 2: deterrence message.
                                if let Ok(Some(rec)) =
                                    web::db::update_away_state(&pool, id, "silent").await
                                {
                                    broadcast(&event_tx, rec);
                                }
                                if speak_text(&tts, &voice, &settings.deterrence_text)
                                    .await
                                    .is_err()
                                {
                                    tracing::warn!("away: deterrence playback failed");
                                }
                            }
                            // Answered during the re-ask window: the watch
                            // loop's attribution already flipped `answered`.
                        });
                    } else {
                        tracing::warn!("away: listen window unavailable (no VAD or slot busy)");
                        if let Ok(Some(rec)) =
                            web::db::update_away_state(&pool, stored_id, "no_voice").await
                        {
                            broadcast(&event_tx, rec);
                        }
                    }
                }
                Ok(Ok(_)) => { /* known face: greeted, nothing to listen for */ }
                spoke => {
                    tracing::warn!(error = ?spoke, "away: greeting playback failed");
                    if stored_state == "greeting"
                        && let Ok(Some(rec)) =
                            web::db::update_away_state(&pool, stored_id, "no_voice").await
                    {
                        broadcast(&event_tx, rec);
                    }
                }
            }
        });
    }
}

/// Activity pipeline (configured non-person label rising edge): record
/// with snapshot — no voice, no desktop noise, no VLM.
async fn activity_flow(
    away: &Arc<AwayEngine>,
    pool: &SqlitePool,
    event_tx: &Arc<EventBus>,
    camera_id: &str,
    labels: &str,
    jpeg: Option<Arc<[u8]>>,
) {
    let now = unix_now_ms();
    let snapshot = save_snapshot(away.config(), camera_id, jpeg.as_deref(), now).await;
    insert_and_broadcast(
        away,
        pool,
        event_tx,
        AwayEventRecord {
            id: 0,
            camera_id: camera_id.to_string(),
            kind: "activity".into(),
            started_ms: now as i64,
            labels: labels.to_string(),
            face_name: None,
            description: None,
            visitor_reply: None,
            snapshot,
            state: "recorded".into(),
        },
    )
    .await;
}

/// Sound-alarm record (SPEC appendix A #44 audio link): called by the
/// sound bridge on a voted rising edge while armed. `labels` comes from
/// [`AwayEngine::observe_audio`] (it already applied the audio_link
/// gate and the activity cooldown). No snapshot — there is no frame.
pub async fn record_audio_event(
    away: &Arc<AwayEngine>,
    pool: &SqlitePool,
    event_tx: &Arc<EventBus>,
    labels: &str,
) {
    let now = unix_now_ms();
    insert_and_broadcast(
        away,
        pool,
        event_tx,
        AwayEventRecord {
            id: 0,
            camera_id: "0".to_string(),
            kind: "activity".into(),
            started_ms: now as i64,
            labels: labels.to_string(),
            face_name: None,
            description: None,
            visitor_reply: None,
            snapshot: None,
            state: "recorded".into(),
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_text_known_vs_unknown() {
        let cfg = AwayConfig::default();
        assert_eq!(
            greeting_text(&cfg, Some("小明")),
            "欢迎回家，小明。".to_string()
        );
        assert_eq!(greeting_text(&cfg, None), cfg.greeting_unknown);
    }

    #[test]
    fn best_named_hit_takes_top_score_and_ignores_unknowns() {
        let hit = |name: Option<&str>, score: f32| FaceHit {
            name: name.map(String::from),
            score,
            bbox: (0.0, 0.0, 1.0, 1.0),
        };
        let hits = vec![hit(None, 0.9), hit(Some("甲"), 0.4), hit(Some("乙"), 0.7)];
        assert_eq!(best_named_hit(&hits), Some("乙".to_string()));
        assert_eq!(best_named_hit(&[hit(None, 0.99)]), None);
        assert_eq!(best_named_hit(&[]), None);
    }

    #[test]
    fn safe_fragment_strips_path_characters() {
        assert_eq!(safe_fragment("0"), "0");
        assert_eq!(safe_fragment("../../etc/passwd"), "etcpasswd");
        assert_eq!(safe_fragment("a/b\\c"), "abc");
    }
}
