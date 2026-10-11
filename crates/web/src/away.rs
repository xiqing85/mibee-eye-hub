//! Away mode (SPEC v1 §3.6, notebook dialect appendix A #44): the
//! armed watch loop over the existing AI detection stream.
//!
//! The engine owns the pure decision state — arm/disarm flag, per-camera
//! person presence latches (arrival gap + greeting cooldown), activity
//! cooldowns, the away analysis interval throttle, and the single
//! device-wide listening slot (one microphone). The orchestration task
//! in the binary (`mibee_eye::away_monitor`) feeds it detection events,
//! runs the voice/VLM/desktop legs its decisions trigger, and persists
//! records through [`crate::db`].
//!
//! Semantics mirror the family alarm bridge (`crate::alarm`): rising
//! edges only, edges inside a cooldown are dropped (not deferred), the
//! falling edge re-arms.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use chrono::Datelike;
use serde::Deserialize;
use serde::Serialize;
use sqlx::SqlitePool;
use streaming::ai::AiEngine;
use streaming::ai::Detection;
use streaming::llm::ChatEngine;
use streaming::llm::ChatTurn;

use crate::db;
use crate::routes::events::CameraEvent;
use crate::routes::events::EventBus;

/// `[away]` config (SPEC appendix A #44): boot defaults. The
/// cadence/cooldown/greeting keys are boot-only TOML; the runtime-tunable
/// feature knobs (`voice_control` … `audio_link`) seed the
/// [`AwaySettings`] hot-config surface and can be changed live via
/// `PUT /api/config` (`scene.away.*`) — see `config_api`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AwayConfig {
    /// Minimum spacing between away-mode analyses per camera, stacked on
    /// top of the `[ai] interval_ms` detection cadence (the away loop
    /// never samples faster than the AI worker).
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// Person absence grace — a re-detection inside this window is the
    /// same visit, not a new arrival (detection flicker).
    #[serde(default = "default_person_gap_secs")]
    pub person_gap_secs: u64,
    /// Minimum spacing between visitor interactions per camera — a
    /// lingering person is greeted at most this often.
    #[serde(default = "default_greeting_cooldown_secs")]
    pub greeting_cooldown_secs: u64,
    /// Minimum spacing between activity records per camera.
    #[serde(default = "default_activity_cooldown_secs")]
    pub activity_cooldown_secs: u64,
    /// Non-person labels that count as activity while armed. Defaults to
    /// pets — empty the list if the animal lives here.
    #[serde(default = "default_activity_labels")]
    pub activity_labels: Vec<String>,
    /// One-shot no-wake-word listening window after the greeting
    /// (seconds). Does not touch `follow_up_window_secs`.
    #[serde(default = "default_listen_secs")]
    pub listen_secs: u64,
    /// Directory for evidence snapshots (cwd-relative, like `models/`).
    #[serde(default = "default_snapshot_dir")]
    pub snapshot_dir: String,
    /// Greeting for an enrolled face; `{name}` is replaced.
    #[serde(default = "default_greeting_known")]
    pub greeting_known: String,
    /// Greeting + identity question for an unknown visitor.
    #[serde(default = "default_greeting_unknown")]
    pub greeting_unknown: String,
    // ── Runtime-tunable feature knobs (seed AwaySettings; hot via
    //    scene.away.*) ────────────────────────────────────────────────
    /// Voice arm/disarm phrases ("我要出门了" / "我回来了").
    #[serde(default = "default_true")]
    pub voice_control: bool,
    /// LLM-generated summary spoken/returned on disarm.
    #[serde(default = "default_true")]
    pub summary_on_disarm: bool,
    /// Stranger escalation: re-ask once, then play the deterrence text.
    #[serde(default)]
    pub deterrence: bool,
    #[serde(default = "default_deterrence_text")]
    pub deterrence_text: String,
    #[serde(default = "default_reask_text")]
    pub reask_text: String,
    /// Schedule times, `"HH:MM"` local; empty = that edge disabled.
    #[serde(default)]
    pub schedule_arm: String,
    #[serde(default)]
    pub schedule_disarm: String,
    /// Schedule only fires Monday–Friday.
    #[serde(default)]
    pub schedule_weekdays_only: bool,
    /// Opt-in visitor push webhook (empty = zero egress).
    #[serde(default)]
    pub push_url: String,
    /// Sound alarms (YAMNet rising edges) become away activity records.
    #[serde(default = "default_true")]
    pub audio_link: bool,
}

fn default_true() -> bool {
    true
}
fn default_deterrence_text() -> String {
    "请注意：您的影像和声音已被记录，主人将收到通知。".to_string()
}
fn default_reask_text() -> String {
    "请问你是谁？请说明来意。".to_string()
}

fn default_interval_ms() -> u64 {
    1000
}
fn default_person_gap_secs() -> u64 {
    10
}
fn default_greeting_cooldown_secs() -> u64 {
    120
}
fn default_activity_cooldown_secs() -> u64 {
    60
}
fn default_activity_labels() -> Vec<String> {
    ["cat", "dog", "bird"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_listen_secs() -> u64 {
    10
}
fn default_snapshot_dir() -> String {
    "away-snapshots".to_string()
}
fn default_greeting_known() -> String {
    "欢迎回家，{name}。".to_string()
}
fn default_greeting_unknown() -> String {
    "你好，这里是主人的智能看家助手。主人现在不在家，请问你是谁？".to_string()
}

impl Default for AwayConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_interval_ms(),
            person_gap_secs: default_person_gap_secs(),
            greeting_cooldown_secs: default_greeting_cooldown_secs(),
            activity_cooldown_secs: default_activity_cooldown_secs(),
            activity_labels: default_activity_labels(),
            listen_secs: default_listen_secs(),
            snapshot_dir: default_snapshot_dir(),
            greeting_known: default_greeting_known(),
            greeting_unknown: default_greeting_unknown(),
            voice_control: true,
            summary_on_disarm: true,
            deterrence: false,
            deterrence_text: default_deterrence_text(),
            reask_text: default_reask_text(),
            schedule_arm: String::new(),
            schedule_disarm: String::new(),
            schedule_weekdays_only: false,
            push_url: String::new(),
            audio_link: true,
        }
    }
}

/// Runtime away settings (SPEC appendix A #44 same-day amendment): the
/// six feature knobs that must be web-configurable without a restart.
/// Seeded from `[away]` TOML at boot, then hot-adjusted through
/// `PUT /api/config` (`scene.away.*`) via the shared lock — the engine,
/// the orchestrator, the scheduler and the config API all see the same
/// values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AwaySettings {
    pub voice_control: bool,
    pub summary_on_disarm: bool,
    pub deterrence: bool,
    pub deterrence_text: String,
    pub reask_text: String,
    /// `"HH:MM"` local time or empty (that schedule edge disabled).
    pub schedule_arm: String,
    pub schedule_disarm: String,
    pub schedule_weekdays_only: bool,
    /// Visitor push webhook; empty = zero outbound traffic.
    pub push_url: String,
    pub audio_link: bool,
}

impl From<&AwayConfig> for AwaySettings {
    fn from(c: &AwayConfig) -> Self {
        Self {
            voice_control: c.voice_control,
            summary_on_disarm: c.summary_on_disarm,
            deterrence: c.deterrence,
            deterrence_text: c.deterrence_text.clone(),
            reask_text: c.reask_text.clone(),
            schedule_arm: c.schedule_arm.clone(),
            schedule_disarm: c.schedule_disarm.clone(),
            schedule_weekdays_only: c.schedule_weekdays_only,
            push_url: c.push_url.clone(),
            audio_link: c.audio_link,
        }
    }
}

/// Shared hot-config handle (`scene.away.*` writes, engine reads).
pub type SharedAwaySettings = Arc<parking_lot::RwLock<AwaySettings>>;

/// Validate a `"HH:MM"` schedule value (`""` allowed = disabled).
#[must_use]
pub fn valid_schedule_time(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    let (h, m) = (&s[..2], &s[3..]);
    h.bytes().all(|b| b.is_ascii_digit())
        && m.bytes().all(|b| b.is_ascii_digit())
        && h.parse::<u32>().is_ok_and(|v| v < 24)
        && m.parse::<u32>().is_ok_and(|v| v < 60)
}

/// One persisted away event (SPEC §3.6 event object — the API list item
/// and the SSE `away_event` payload are the same shape; the SSE update
/// carries the full record so the browser upserts by `id`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AwayEventRecord {
    pub id: i64,
    pub camera_id: String,
    /// `"person"` (visitor pipeline) | `"activity"`.
    pub kind: String,
    pub started_ms: i64,
    /// Detection summary at fire time, e.g. `person×1`.
    pub labels: String,
    /// Enrolled-face match (person events; `None` = unknown/absent).
    pub face_name: Option<String>,
    /// VLM description — patched in asynchronously when it lands.
    pub description: Option<String>,
    /// The visitor's spoken answer inside the listen window.
    pub visitor_reply: Option<String>,
    /// Server-generated snapshot file name (never a client path).
    pub snapshot: Option<String>,
    /// `greeting` → `listening` → `answered` | `silent`; terminal:
    /// `known` / `no_voice` / `recorded`.
    pub state: String,
}

/// What the engine wants the orchestrator to do for one detection
/// sample — the decision is pure, the legs (snapshot, face, TTS, VLM,
/// records) belong to the async side.
#[derive(Debug, Clone, PartialEq)]
pub enum AwayAction {
    /// New person arrival outside both gap and cooldown — run the full
    /// visitor pipeline.
    Visitor {
        person_count: usize,
    },
    /// Configured activity label rising edge outside its cooldown —
    /// record (with snapshot), no voice.
    Activity {
        labels: String,
    },
    None,
}

#[derive(Debug, Default)]
struct CameraLatch {
    /// Last unix-ms a person was detected on this camera. `None` until
    /// the first sight — a 0-stamp would read as "seen a moment ago"
    /// for timestamps inside the gap window and swallow the first
    /// arrival.
    person_seen_ms: Option<u64>,
    /// Last unix-ms a visitor event fired on this camera.
    person_greeted_ms: Option<u64>,
    /// Last unix-ms an away analysis ran (interval throttle).
    analysis_ms: Option<u64>,
    /// Last unix-ms an activity record landed.
    activity_ms: Option<u64>,
}

#[derive(Debug)]
struct OpenListen {
    event_id: i64,
    deadline_ms: u64,
}

#[derive(Debug, Default)]
struct AwayInner {
    cameras: HashMap<String, CameraLatch>,
    /// The single no-wake-word listening window (one microphone — a
    /// second visitor while the slot is open records without voice).
    listen: Option<OpenListen>,
}

/// Process-wide away-mode engine: armed flag + decision state machine.
/// Clone-free shared state (`Arc<AwayEngine>` everywhere); the inner
/// map sits behind a `std::sync::Mutex` — decisions are synchronous and
/// short (no await while held).
pub struct AwayEngine {
    config: AwayConfig,
    /// Live AI-detection availability — the away surface cannot arm
    /// without it (capability `away.available`).
    ai: Arc<AiEngine>,
    /// Whether the voice legs can run at all (TTS + voice engines
    /// active at boot; capability `away.voice`).
    voice_capable: bool,
    /// Hot feature knobs (`scene.away.*`) — shared with the config API.
    settings: SharedAwaySettings,
    armed: AtomicBool,
    armed_since_ms: AtomicU64,
    inner: Mutex<AwayInner>,
}

impl AwayEngine {
    #[must_use]
    pub fn new(config: AwayConfig, ai: Arc<AiEngine>, voice_capable: bool) -> Self {
        let settings = Arc::new(parking_lot::RwLock::new(AwaySettings::from(&config)));
        Self {
            config,
            ai,
            voice_capable,
            settings,
            armed: AtomicBool::new(false),
            armed_since_ms: AtomicU64::new(0),
            inner: Mutex::new(AwayInner::default()),
        }
    }

    /// The shared hot-config handle — hand to the scene config API so
    /// `PUT /api/config` writes land in the engine live.
    #[must_use]
    pub fn settings_share(&self) -> SharedAwaySettings {
        Arc::clone(&self.settings)
    }

    /// Point-in-time copy of the runtime settings.
    #[must_use]
    pub fn settings_snapshot(&self) -> AwaySettings {
        self.settings.read().clone()
    }

    #[must_use]
    pub fn config(&self) -> &AwayConfig {
        &self.config
    }

    /// Whether the device can arm at all (AI detection active).
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.ai.is_active()
    }

    /// Why arming is refused (empty when available) — surfaced by the
    /// arm endpoint so the UI can show the honest reason.
    #[must_use]
    pub fn unavailable_reason(&self) -> String {
        self.ai.inactive_reason().to_string()
    }

    #[must_use]
    pub fn voice_capable(&self) -> bool {
        self.voice_capable
    }

    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn armed_since_ms(&self) -> Option<u64> {
        if self.is_armed() {
            Some(self.armed_since_ms.load(Ordering::SeqCst))
        } else {
            None
        }
    }

    /// Arm the watch (no-op when already armed).
    pub fn arm(&self, now_ms: u64) {
        if !self.armed.swap(true, Ordering::SeqCst) {
            self.armed_since_ms.store(now_ms, Ordering::SeqCst);
        }
    }

    /// Disarm (no-op when already disarmed; the since stamp resets so a
    /// later arm starts fresh). Any open listening window drops too — a
    /// disarmed device stops asking questions.
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
        self.armed_since_ms.store(0, Ordering::SeqCst);
        self.inner.lock().expect("away inner lock").listen = None;
    }

    /// Feed one detection sample while armed. Presence latches always
    /// update; record decisions are throttled to `interval_ms` and gated
    /// by the arrival gap / cooldowns. Detection labels come from the
    /// engine's own COCO vocabulary — `person` is the visitor trigger.
    pub fn observe(&self, camera_id: &str, detections: &[Detection], now_ms: u64) -> AwayAction {
        let gap_ms = self.config.person_gap_secs * 1000;
        let cooldown_ms = self.config.greeting_cooldown_secs * 1000;
        let activity_ms = self.config.activity_cooldown_secs * 1000;
        let interval_ms = self.config.interval_ms.max(1);
        let mut inner = self.inner.lock().expect("away inner lock");
        let latch = inner.cameras.entry(camera_id.to_string()).or_default();
        // Interval throttle — a camera's first sample is never throttled.
        let throttled = latch
            .analysis_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < interval_ms);

        let person_count = detections.iter().filter(|d| d.label == "person").count();
        if person_count > 0 {
            let last_seen = latch.person_seen_ms;
            latch.person_seen_ms = Some(now_ms);
            // Arrival = no person seen within the gap window. The very
            // first sight (None) is an arrival: someone IS here.
            let is_arrival = last_seen.is_none_or(|t| now_ms.saturating_sub(t) > gap_ms);
            let past_cooldown = latch
                .person_greeted_ms
                .is_none_or(|t| now_ms.saturating_sub(t) > cooldown_ms);
            if is_arrival && past_cooldown && !throttled {
                latch.person_greeted_ms = Some(now_ms);
                latch.analysis_ms = Some(now_ms);
                return AwayAction::Visitor { person_count };
            }
            return AwayAction::None;
        }

        // No person: configured activity labels (person presence
        // suppresses activity records — the visitor pipeline owns the
        // moment).
        let summary = summarize_labels(detections, &self.config.activity_labels);
        if !summary.is_empty()
            && latch
                .activity_ms
                .is_none_or(|t| now_ms.saturating_sub(t) > activity_ms)
            && !throttled
        {
            latch.activity_ms = Some(now_ms);
            latch.analysis_ms = Some(now_ms);
            return AwayAction::Activity { labels: summary };
        }
        AwayAction::None
    }

    /// Sound-alarm hook (SPEC appendix A #44 audio link): while armed
    /// and `audio_link` on, a voted sound rising edge becomes one away
    /// activity record per activity cooldown (device-level — one
    /// microphone, latch key `"audio"`). Returns the label summary when
    /// a record should land.
    #[must_use]
    pub fn observe_audio(&self, class: &str, score: f32, now_ms: u64) -> Option<String> {
        if !self.settings_snapshot().audio_link {
            return None;
        }
        let cooldown = self.config.activity_cooldown_secs * 1000;
        let mut inner = self.inner.lock().expect("away inner lock");
        let latch = inner.cameras.entry("audio".to_string()).or_default();
        if latch
            .activity_ms
            .is_none_or(|t| now_ms.saturating_sub(t) > cooldown)
        {
            latch.activity_ms = Some(now_ms);
            Some(format!("\u{1f50a} {class} {:.2}", score))
        } else {
            None
        }
    }

    /// Open the single listening slot for a visitor event. `false` when
    /// the slot is busy (single microphone — the caller records the
    /// event without voice).
    pub fn open_listen(&self, event_id: i64, deadline_ms: u64) -> bool {
        let mut inner = self.inner.lock().expect("away inner lock");
        if inner.listen.is_some() {
            return false;
        }
        inner.listen = Some(OpenListen {
            event_id,
            deadline_ms,
        });
        true
    }

    /// Attribute a follow-up transcript to the open listening window.
    /// Returns the event id exactly once (the slot closes on first
    /// attribution); `None` when no window is open or it already
    /// answered.
    pub fn attribute_listen(&self, now_ms: u64) -> Option<i64> {
        let mut inner = self.inner.lock().expect("away inner lock");
        match inner.listen.take() {
            Some(l) if now_ms <= l.deadline_ms => Some(l.event_id),
            // Expired but not yet reaped — treat as closed.
            Some(_) => None,
            None => None,
        }
    }

    /// Reap an expired unanswered window (the orchestrator's timer).
    /// Returns the event id when the window was still open and past its
    /// deadline — the caller flips it to `silent`.
    pub fn expire_listen(&self, now_ms: u64) -> Option<i64> {
        let mut inner = self.inner.lock().expect("away inner lock");
        // Only an expired window is reaped — a live one stays armed.
        if inner
            .listen
            .as_ref()
            .is_some_and(|l| now_ms > l.deadline_ms)
        {
            inner.listen.take().map(|l| l.event_id)
        } else {
            None
        }
    }

    /// Test seam: force a listening slot (integration tests).
    #[cfg(test)]
    pub fn force_listen(&self, event_id: i64, deadline_ms: u64) {
        self.inner.lock().expect("away inner lock").listen = Some(OpenListen {
            event_id,
            deadline_ms,
        });
    }
}

/// `label×count` summary over the configured activity set, in
/// detection order, deduplicated — `cat×2、dog×1`. Empty when nothing
/// matches (person is deliberately not an activity label).
#[must_use]
pub fn summarize_labels(detections: &[Detection], activity_labels: &[String]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for d in detections {
        if activity_labels.iter().any(|l| l == &d.label) {
            let entry = counts.entry(d.label.as_str()).or_insert(0);
            if *entry == 0 {
                order.push(d.label.as_str());
            }
            *entry += 1;
        }
    }
    order
        .into_iter()
        .map(|label| format!("{label}×{}", counts[label]))
        .collect::<Vec<_>>()
        .join("、")
}

// ---------------------------------------------------------------------------
// Runtime features (SPEC appendix A #44 same-day amendment) — all knobs
// live in AwaySettings (scene.away.* hot config).
// ---------------------------------------------------------------------------

/// Short zh utterances that arm/disarm the watch ahead of the LLM — the
/// same keyword-heuristic class as `voice_tool_gate` (#43): a built-in
/// intent, not a model decision. `Some(true)` = arm, `Some(false)` =
/// disarm, `None` = not an away command. Question-shaped utterances are
/// excluded so "明天出门吗" never arms.
#[must_use]
pub fn match_voice_command(transcript: &str) -> Option<bool> {
    let t = transcript.trim();
    // Longer utterances are conversation, not commands.
    if t.chars().count() > 8 {
        return None;
    }
    // Any question shape disqualifies the utterance.
    if ["吗", "？", "?", "几", "什么", "怎么", "为啥", "为什么"]
        .iter()
        .any(|q| t.contains(q))
    {
        return None;
    }
    let disarm = ["回来了", "到家了", "回家了", "回家啦"];
    let arm = ["出门了", "出门啦", "上班去", "上班了", "出差了", "离家了"];
    if disarm.iter().any(|k| t.contains(k)) {
        return Some(false);
    }
    if arm.iter().any(|k| t.contains(k)) {
        return Some(true);
    }
    None
}

/// `HH:MM` equality + weekday gate for the scheduler.
#[must_use]
fn schedule_fires(setting: &str, now_hhmm: &str, weekday: u32, weekdays_only: bool) -> bool {
    if setting.is_empty() || setting != now_hhmm {
        return false;
    }
    // chrono: Monday = 0 … Sunday = 6.
    !weekdays_only || weekday < 5
}

/// Central arm/disarm (SPEC §3.6): used by the POST route, the voice
/// command branch and the scheduler so every path persists the state,
/// broadcasts `away_state` and — on disarm with events — produces the
/// summary. Returns the disarm summary when one was generated.
pub async fn set_active(
    away: &Arc<AwayEngine>,
    pool: &SqlitePool,
    event_tx: &Arc<EventBus>,
    active: bool,
    chat: Option<&Arc<ChatEngine>>,
) -> Result<Option<String>, String> {
    if active {
        if !away.is_available() {
            return Err(format!(
                "away mode unavailable: {}",
                away.unavailable_reason()
            ));
        }
        away.arm(unix_now_ms());
        if let Err(e) = db::set_setting(pool, "away.active", "true").await {
            tracing::warn!(error = %e, "away: persist armed state failed");
        }
        let since_ms = away.armed_since_ms();
        let _ = event_tx.send(CameraEvent::AwayState {
            active: true,
            since_ms,
        });
        tracing::info!("away: armed");
        return Ok(None);
    }
    // Summary BEFORE disarm (it reads armed_since_ms and the window's
    // events).
    let summary = if away.settings_snapshot().summary_on_disarm {
        disarm_summary(away, pool, chat).await
    } else {
        None
    };
    away.disarm();
    if let Err(e) = db::set_setting(pool, "away.active", "false").await {
        tracing::warn!(error = %e, "away: persist armed state failed");
    }
    let _ = event_tx.send(CameraEvent::AwayState {
        active: false,
        since_ms: None,
    });
    tracing::info!(has_summary = summary.is_some(), "away: disarmed");
    Ok(summary)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One human line per event for the summary prompt / fallback text.
fn summary_event_line(e: &AwayEventRecord) -> String {
    let time = {
        let secs = e.started_ms.max(0) as u64 / 1000;
        let (h, m) = ((secs / 3600 + 8) % 24, (secs / 60) % 60);
        format!("{h:02}:{m:02}")
    };
    let kind = if e.kind == "person" {
        "访客"
    } else {
        "活动"
    };
    let mut detail = String::new();
    if let Some(name) = &e.face_name {
        detail.push_str(&format!("（已登记：{name}）"));
    } else if let Some(d) = &e.description {
        let d: String = d.chars().take(40).collect();
        detail.push_str(&format!("（{d}）"));
    } else {
        detail.push_str(&format!("（{}）", e.labels));
    }
    if let Some(reply) = &e.visitor_reply {
        let reply: String = reply.chars().take(20).collect();
        detail.push_str(&format!(" 回答：{reply}"));
    }
    format!("{time} {kind}{detail}")
}

/// Disarm summary (SPEC §3.6 `summary` additive field): plain counts,
/// upgraded to a one-liner by the local LLM when it is active. `None`
/// when nothing happened while armed.
async fn disarm_summary(
    away: &AwayEngine,
    pool: &SqlitePool,
    chat: Option<&Arc<ChatEngine>>,
) -> Option<String> {
    let since_ms = away.armed_since_ms()? as i64;
    let (total, visitors) = db::away_stats_since(pool, since_ms).await.ok()?;
    if total == 0 {
        return None;
    }
    let plain = format!("离家期间共记录 {total} 条事件，其中访客 {visitors} 次。");
    let Some(chat) = chat.filter(|c| c.is_active()) else {
        return Some(plain);
    };
    let events = db::list_away_events_since(pool, since_ms, 20)
        .await
        .unwrap_or_default();
    // Newest-first from SQL; oldest-first reads like a diary.
    let mut lines: Vec<String> = events.iter().map(summary_event_line).collect();
    lines.reverse();
    let prompt = format!(
        "主人家里的看家摄像头刚被撤防。根据下面的离家期间监控记录，用一两句话向主人总结发生了什么（以记录为准，不编造，不超过60字）：\n{}",
        lines.join("\n")
    );
    let turns = vec![ChatTurn {
        role: "user".into(),
        content: prompt,
    }];
    let chat = Arc::clone(chat);
    match tokio::task::spawn_blocking(move || chat.complete(&turns)).await {
        Ok(Ok(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Ok(Ok(_)) => Some(plain),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "away: summary llm failed (plain fallback)");
            Some(plain)
        }
        Err(e) => {
            tracing::warn!(error = %e, "away: summary task failed (plain fallback)");
            Some(plain)
        }
    }
}

/// Opt-in visitor push (SPEC appendix A #44): POSTs a small JSON
/// document to the configured webhook. Default-empty keeps the device
/// fully air-gapped; configuring a URL is the informed opt-in. Fail-open
/// with a WARN — a dead webhook never touches the watch.
pub async fn push_visitor_event(url: &str, event_id: i64, title: &str, body: &str) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "away: push client build failed");
            return;
        }
    };
    let payload = serde_json::json!({
        "device": "mibee-eye",
        "event_id": event_id,
        "title": title,
        "body": body,
        "timestamp_ms": unix_now_ms(),
    });
    match client.post(url).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() || resp.status().is_redirection() => {}
        Ok(resp) => tracing::warn!(status = %resp.status(), "away: push webhook non-2xx"),
        Err(e) => tracing::warn!(error = %e, "away: push webhook failed"),
    }
}

/// Schedule watcher (SPEC appendix A #44): fires the arm/disarm edges at
/// their local-time boundary minutes. It only acts at the exact minute —
/// manual arm/disarm between boundaries is never fought (an already
/// armed device stays armed at schedule_arm, and a manual disarm inside
/// the window stays disarmed until the next boundary).
pub fn spawn_scheduler(
    away: Arc<AwayEngine>,
    pool: SqlitePool,
    event_tx: Arc<EventBus>,
    chat: Option<Arc<ChatEngine>>,
) {
    tokio::spawn(async move {
        let mut last_minute = String::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            let settings = away.settings_snapshot();
            if settings.schedule_arm.is_empty() && settings.schedule_disarm.is_empty() {
                continue;
            }
            let now = chrono::Local::now();
            let hhmm = now.format("%H:%M").to_string();
            if hhmm == last_minute {
                continue; // already acted (or declined) inside this minute
            }
            let weekday = now.weekday().num_days_from_monday();
            if schedule_fires(
                &settings.schedule_arm,
                &hhmm,
                weekday,
                settings.schedule_weekdays_only,
            ) && !away.is_armed()
            {
                last_minute = hhmm.clone();
                match set_active(&away, &pool, &event_tx, true, chat.as_ref()).await {
                    Ok(_) => tracing::info!(at = %hhmm, "away: scheduled arm"),
                    Err(e) => tracing::warn!(error = %e, "away: scheduled arm failed"),
                }
            } else if schedule_fires(
                &settings.schedule_disarm,
                &hhmm,
                weekday,
                settings.schedule_weekdays_only,
            ) && away.is_armed()
            {
                last_minute = hhmm.clone();
                match set_active(&away, &pool, &event_tx, false, chat.as_ref()).await {
                    Ok(_) => tracing::info!(at = %hhmm, "away: scheduled disarm"),
                    Err(e) => tracing::warn!(error = %e, "away: scheduled disarm failed"),
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use streaming::ai::AiConfig;

    fn engine() -> AwayEngine {
        AwayEngine::new(
            AwayConfig::default(),
            Arc::new(AiEngine::from_parts(AiConfig::default(), None)),
            true,
        )
    }

    fn det(label: &str) -> Detection {
        Detection {
            label: label.to_string(),
            confidence: 0.9,
            bbox: [1, 2, 3, 4],
        }
    }

    #[test]
    fn config_defaults_roundtrip_toml() {
        let cfg = AwayConfig::default();
        assert_eq!(cfg.interval_ms, 1000);
        assert_eq!(cfg.person_gap_secs, 10);
        assert_eq!(cfg.greeting_cooldown_secs, 120);
        assert_eq!(cfg.activity_labels, vec!["cat", "dog", "bird"]);
        assert_eq!(cfg.listen_secs, 10);
        assert_eq!(cfg.snapshot_dir, "away-snapshots");
        assert!(cfg.greeting_known.contains("{name}"));
        // Empty document → all defaults (the boot TOML parse path; the
        // root crate owns the actual toml dep, serde defaults are the
        // shared contract).
        let again: AwayConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(again, cfg);
    }

    #[test]
    fn arm_disarm_tracks_since() {
        let e = engine();
        assert!(!e.is_armed());
        e.arm(1_000);
        assert!(e.is_armed());
        assert_eq!(e.armed_since_ms(), Some(1_000));
        e.arm(2_000); // re-arm keeps the original stamp
        assert_eq!(e.armed_since_ms(), Some(1_000));
        e.disarm();
        assert_eq!(e.armed_since_ms(), None);
    }

    #[test]
    fn person_arrival_fires_once_per_visit() {
        let e = engine();
        let d = vec![det("person")];
        // First sight = arrival.
        assert_eq!(
            e.observe("0", &d, 1_000),
            AwayAction::Visitor { person_count: 1 }
        );
        // Flicker within the gap = same visit.
        assert_eq!(e.observe("0", &d, 5_000), AwayAction::None);
        assert_eq!(e.observe("0", &d, 9_000), AwayAction::None);
        // Away past the gap but inside the greeting cooldown → dropped.
        assert_eq!(e.observe("0", &d, 30_000), AwayAction::None);
        // Left and came back past the cooldown → new visitor event.
        assert_eq!(
            e.observe("0", &d, 300_000),
            AwayAction::Visitor { person_count: 1 }
        );
    }

    #[test]
    fn steady_presence_then_departure_rearms() {
        let e = engine();
        let d = vec![det("person")];
        assert!(matches!(e.observe("0", &d, 0), AwayAction::Visitor { .. }));
        // Present continuously for minutes — one event only.
        for t in (1..200).map(|i| i * 1000) {
            assert_eq!(e.observe("0", &d, t), AwayAction::None);
        }
        // Gone well past the gap + cooldown, then returns → fires again.
        assert!(matches!(
            e.observe("0", &d, 600_000),
            AwayAction::Visitor { .. }
        ));
    }

    #[test]
    fn activity_labels_and_cooldown() {
        let e = engine();
        let cats = vec![det("cat"), det("cat"), det("dog")];
        assert_eq!(
            e.observe("0", &cats, 1_000),
            AwayAction::Activity {
                labels: "cat×2、dog×1".to_string()
            }
        );
        // Inside the activity cooldown → nothing.
        assert_eq!(e.observe("0", &cats, 30_000), AwayAction::None);
        assert_eq!(e.observe("0", &cats, 50_000), AwayAction::None);
        assert!(matches!(
            e.observe("0", &cats, 120_000),
            AwayAction::Activity { .. }
        ));
        // Unconfigured labels never count as activity.
        assert_eq!(e.observe("1", &[det("chair")], 1_000), AwayAction::None);
    }

    #[test]
    fn person_suppresses_activity() {
        let e = engine();
        let mixed = vec![det("person"), det("cat")];
        assert!(matches!(
            e.observe("0", &mixed, 1_000),
            AwayAction::Visitor { .. }
        ));
        assert_eq!(e.observe("0", &mixed, 2_000), AwayAction::None);
    }

    #[test]
    fn interval_throttle_skips_analysis() {
        // interval longer than the activity cooldown: the throttle —
        // not the cooldown — is what drops the middle sample.
        let cfg = AwayConfig {
            interval_ms: 100_000,
            ..AwayConfig::default()
        };
        let e = AwayEngine::new(
            cfg,
            Arc::new(AiEngine::from_parts(AiConfig::default(), None)),
            true,
        );
        let cats = vec![det("cat")];
        assert!(matches!(
            e.observe("0", &cats, 0),
            AwayAction::Activity { .. }
        ));
        assert_eq!(
            e.observe("0", &cats, 70_000),
            AwayAction::None,
            "activity cooldown passed but interval throttle drops it"
        );
        assert!(matches!(
            e.observe("0", &cats, 150_000),
            AwayAction::Activity { .. }
        ));
    }

    #[test]
    fn cameras_are_independent() {
        let e = engine();
        let d = vec![det("person")];
        assert!(matches!(e.observe("0", &d, 0), AwayAction::Visitor { .. }));
        assert!(matches!(
            e.observe("1", &d, 1_000),
            AwayAction::Visitor { .. }
        ));
    }

    #[test]
    fn listen_slot_is_single_shot_and_expires() {
        let e = engine();
        assert!(e.open_listen(7, 10_000));
        assert!(!e.open_listen(8, 20_000), "one microphone — slot busy");
        assert_eq!(e.attribute_listen(9_999), Some(7), "inside deadline");
        assert_eq!(e.attribute_listen(9_999), None, "answered exactly once");
        // Slot free again after attribution.
        assert!(e.open_listen(9, 30_000));
        assert_eq!(e.expire_listen(29_999), None, "still inside deadline");
        assert_eq!(e.expire_listen(30_001), Some(9));
        assert!(e.open_listen(10, 40_000));
        // Attribution after the deadline is closed, not late-answered.
        assert_eq!(e.attribute_listen(45_000), None);
    }

    #[test]
    fn disarm_closes_open_listen() {
        let e = engine();
        e.open_listen(3, 100_000);
        e.disarm();
        assert_eq!(e.attribute_listen(1_000), None);
    }

    #[test]
    fn availability_follows_ai_engine() {
        // The fixture engine has no model → inactive → not armable.
        assert!(!engine().is_available());
    }

    #[test]
    fn voice_command_matches_short_phrrases_and_rejects_questions() {
        use super::match_voice_command as m;
        assert_eq!(m("我要出门了"), Some(true));
        assert_eq!(m("上班去了"), Some(true));
        assert_eq!(m("我回来了"), Some(false));
        assert_eq!(m("到家了"), Some(false));
        // Conversation, questions and long utterances never trigger.
        assert_eq!(m("现在几点了"), None);
        assert_eq!(m("明天出门吗"), None);
        assert_eq!(m("今天外面下雨了吗"), None);
        assert_eq!(m("帮我看看明天出门要带什么行李好不好"), None);
    }

    #[test]
    fn schedule_time_validation_and_firing() {
        use super::schedule_fires as f;
        use super::valid_schedule_time as v;
        assert!(v("") && v("09:00") && v("23:59") && v("00:00"));
        assert!(!v("9:00") && !v("09:0") && !v("24:00") && !v("09:60") && !v("ab:cd"));
        // Fire only at the exact minute on allowed days (Mon=0…Fri=4).
        assert!(f("09:00", "09:00", 0, true));
        assert!(f("09:00", "09:00", 4, true));
        assert!(!f("09:00", "09:00", 5, true), "Saturday gated");
        assert!(!f("09:00", "09:01", 0, false));
        assert!(!f("", "09:00", 0, false), "empty = disabled");
        assert!(f("10:30", "10:30", 6, false), "daily includes Sunday");
    }

    #[tokio::test]
    async fn set_active_arms_disarms_persists_and_summarizes() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("migrations for test pool");
        let event_tx = Arc::new(crate::routes::events::new_event_bus());
        let e = Arc::new(engine());
        // Arm is refused while AI is inactive (fixture has no model).
        assert!(set_active(&e, &pool, &event_tx, true, None).await.is_err());
        // With no events, disarm returns no summary.
        assert_eq!(
            set_active(&e, &pool, &event_tx, false, None).await.unwrap(),
            None
        );
        assert_eq!(
            crate::db::get_setting(&pool, "away.active").await.unwrap(),
            Some("false".into())
        );
    }

    #[tokio::test]
    async fn disarm_summary_counts_the_armed_window() {
        let (pool, _auth) = crate::db::create_test_dbs().await;
        crate::db::run_migrations(&pool)
            .await
            .expect("migrations for test pool");
        let e = engine();
        e.arm(1_000);
        crate::db::insert_away_event(
            &pool,
            &AwayEventRecord {
                id: 0,
                camera_id: "0".into(),
                kind: "person".into(),
                started_ms: 2_000,
                labels: "person×1".into(),
                face_name: None,
                description: Some("一名男子站在桌旁".into()),
                visitor_reply: Some("我是快递员".into()),
                snapshot: None,
                state: "answered".into(),
            },
        )
        .await
        .unwrap();
        crate::db::insert_away_event(
            &pool,
            &AwayEventRecord {
                id: 0,
                camera_id: "0".into(),
                kind: "activity".into(),
                started_ms: 3_000,
                labels: "cat×1".into(),
                face_name: None,
                description: None,
                visitor_reply: None,
                snapshot: None,
                state: "recorded".into(),
            },
        )
        .await
        .unwrap();
        // No chat engine → plain fallback text with both counts.
        let summary = disarm_summary(&e, &pool, None).await.expect("summary");
        assert!(summary.contains('2'), "total events: {summary}");
        assert!(summary.contains('1'), "visitors: {summary}");
        e.disarm();
        // After disarm (no window) there is nothing to summarize.
        assert_eq!(disarm_summary(&e, &pool, None).await, None);
    }

    #[test]
    fn observe_audio_respects_cooldown_and_link() {
        let e = engine();
        assert!(e.observe_audio("Glass", 0.8, 1_000).is_some());
        // Inside the 60 s activity cooldown → suppressed.
        assert!(e.observe_audio("Glass", 0.9, 30_000).is_none());
        assert!(e.observe_audio("Dog", 0.7, 70_000).is_some());
        // audio_link off → never.
        e.settings_share().write().audio_link = false;
        assert!(e.observe_audio("Glass", 0.9, 200_000).is_none());
    }

    /// Wire contract for the opt-in visitor push (#44): one POST with
    /// the documented JSON shape — pinned against a local listener so a
    /// field rename cannot slip out silently.
    #[tokio::test]
    async fn push_visitor_event_posts_the_documented_shape() {
        use std::io::Read;
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = req.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
            let resp = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
            sock.write_all(resp.as_bytes()).unwrap();
            body
        });
        let url = format!("http://{addr}/hook");
        push_visitor_event(&url, 42, "title-x", "body-y").await;
        let body = handle.join().unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON body");
        assert_eq!(json["device"], "mibee-eye");
        assert_eq!(json["event_id"], 42);
        assert_eq!(json["title"], "title-x");
        assert_eq!(json["body"], "body-y");
        assert!(json["timestamp_ms"].as_u64().unwrap_or(0) > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_visitor_event_fails_open_on_dead_webhook() {
        // Bind then drop the listener — the port is closed; the call must
        // simply return (fail-open) instead of panicking or hanging.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/hook");
        let () = push_visitor_event(&url, 1, "t", "b").await;
    }

    #[test]
    fn summarize_labels_ignores_person_and_unknown() {
        let labels: Vec<String> = ["cat", "dog"].iter().map(|s| s.to_string()).collect();
        let dets = vec![det("person"), det("bird"), det("cat"), det("tv")];
        assert_eq!(summarize_labels(&dets, &labels), "cat×1");
    }
}
