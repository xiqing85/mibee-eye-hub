//! Hardware elasticity controller (design §10, M7; the systematic solve
//! behind issue #3): runtime signals drive tier decisions with
//! hysteresis and cooldown — memory low-water stops being a log line and
//! becomes an action (persisted LLM-tier downgrade consumed at the next
//! boot's resource gate), battery power pins the conservative tier
//! immediately, and sustained critical memory recommends the minimal
//! form (advice only — never an automatic restart).
//!
//! Every decision emits the `scale` SSE event (SPEC appendix A #48) so
//! the resource card moves from boot-time snapshot to live state.

use std::sync::{Arc, Mutex};

use web::routes::events::{CameraEvent, EventBus};

/// Settings-bag key the boot path consumes (`config.resources.tier`).
pub const SETTING_TIER: &str = "elastic.llm_tier";

/// The LLM tier ladder (down = rightward).
pub const LADDER: [&str; 3] = ["full", "mid", "lite"];

/// L2 live-swap seam: swap the running LLM to a tier's model file. Wired
/// to `ChatEngine::swap_model` (via `llm::ladder_path`) in production;
/// tests install fakes. `None` = decisions persist for next boot only.
pub type LiveSwapFn = Arc<
    dyn Fn(
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

/// `[elastic]` configuration section — policy knobs so hosts with big
/// models on small memory can tune the bands (and a kill switch).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ElasticConfig {
    /// Master switch (default on): off = no controller, no scale events.
    pub enabled: bool,
    /// Sustained-available below this → downgrade one tier.
    pub enter_low_mib: u64,
    /// Sustained-available above this → allow one upgrade.
    pub exit_low_mib: u64,
    /// Critical: recommend the minimal form (advice only).
    pub critical_mib: u64,
    /// Seconds between any two applied actions.
    pub cooldown_secs: u64,
    /// Consecutive samples inside a band before it counts (debounce).
    pub sustain_samples: u32,
}

impl Default for ElasticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enter_low_mib: 1024,
            exit_low_mib: 2560,
            critical_mib: 320,
            cooldown_secs: 180,
            sustain_samples: 3,
        }
    }
}

impl From<&ElasticConfig> for ElasticPolicy {
    fn from(cfg: &ElasticConfig) -> Self {
        Self {
            enter_low_mib: cfg.enter_low_mib,
            exit_low_mib: cfg.exit_low_mib,
            critical_mib: cfg.critical_mib,
            cooldown_secs: cfg.cooldown_secs,
            sustain_samples: cfg.sustain_samples.max(1),
        }
    }
}

/// Runtime signals (sampled; see [`sample`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ElasticSignals {
    pub mem_available_mib: u64,
    pub battery: bool,
}

/// Policy bands. Defaults carry the incident lessons: the 2026-10-12
/// PC-freeze happened around ~500 MiB available with swap exhausted, so
/// the enter band sits well above it; recovery demands real headroom to
/// avoid unload/reload flapping (design §10: 缩容有动作、扩容保守).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElasticPolicy {
    /// Sustained-available below this → downgrade one tier.
    pub enter_low_mib: u64,
    /// Sustained-available above this → allow one upgrade.
    pub exit_low_mib: u64,
    /// Critical: recommend the minimal form (advice only).
    pub critical_mib: u64,
    /// Seconds between any two applied actions.
    pub cooldown_secs: u64,
    /// Consecutive samples inside a band before it counts (debounce).
    pub sustain_samples: u32,
}

impl Default for ElasticPolicy {
    fn default() -> Self {
        Self {
            enter_low_mib: 1024,
            exit_low_mib: 2560,
            critical_mib: 320,
            cooldown_secs: 180,
            sustain_samples: 3,
        }
    }
}

/// One applied (or advised) decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElasticAction {
    /// Persisted tier downgrade (one ladder step), applied at next boot.
    TierDown { from: String, to: String },
    /// Persisted tier upgrade (one step, recovery only).
    TierUp { from: String, to: String },
    /// Battery pin: conservative tier immediately.
    BatteryPin { from: String, to: String },
    /// L3 advice: sustained critical memory — consider the minimal form.
    RecommendMinimalForm,
    /// The trigger reason rides the scale event either way.
    Noop,
}

impl ElasticAction {
    /// The `scale` event's action wire name.
    #[must_use]
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::TierDown { .. } => "tier_down",
            Self::TierUp { .. } => "tier_up",
            Self::BatteryPin { .. } => "battery_pin",
            Self::RecommendMinimalForm => "recommend_minimal_form",
            Self::Noop => "noop",
        }
    }
}

/// Controller state across ticks (hysteresis counters + cooldown).
#[derive(Debug, Default)]
pub struct ElasticState {
    pub tier: String,
    low_streak: u32,
    high_streak: u32,
    critical_streak: u32,
    battery_latched: bool,
    last_action_at_ms: u64,
}

impl ElasticState {
    #[must_use]
    pub fn new(initial_tier: &str) -> Self {
        Self {
            tier: initial_tier.to_string(),
            ..Self::default()
        }
    }
}

/// Pure decision core — unit-pinned. Battery dominates everything (a
/// notebook on battery should not be running the 4B chat model); then
/// critical advice; then the low/high hysteresis bands; cooldown gates
/// every applied action but never the advice.
#[must_use]
pub fn decide(
    signals: ElasticSignals,
    state: &mut ElasticState,
    policy: &ElasticPolicy,
    now_ms: u64,
) -> ElasticAction {
    // Battery pin: immediate, once per battery session.
    if signals.battery && !state.battery_latched {
        state.battery_latched = true;
        state.high_streak = 0;
        state.low_streak = 0;
        if state.tier != "lite" {
            let from = state.tier.clone();
            state.tier = "lite".into();
            state.last_action_at_ms = now_ms;
            return ElasticAction::BatteryPin {
                to: "lite".into(),
                from,
            };
        }
        return ElasticAction::Noop;
    }
    if !signals.battery {
        state.battery_latched = false;
    } else {
        // Still on battery with the pin latched: no recovery counting —
        // the conservative tier stands until real AC headroom returns.
        return ElasticAction::Noop;
    }

    // Critical band: advice (L3) regardless of cooldown.
    if signals.mem_available_mib < policy.critical_mib {
        state.critical_streak += 1;
        state.low_streak = 0;
        state.high_streak = 0;
        if state.critical_streak >= policy.sustain_samples {
            return ElasticAction::RecommendMinimalForm;
        }
        return ElasticAction::Noop;
    }
    state.critical_streak = 0;

    let cooldown_open = state.last_action_at_ms == 0
        || now_ms.saturating_sub(state.last_action_at_ms) >= policy.cooldown_secs * 1000;

    if signals.mem_available_mib < policy.enter_low_mib {
        state.low_streak += 1;
        state.high_streak = 0;
        if state.low_streak >= policy.sustain_samples
            && cooldown_open
            && let Some(to) = step_down(&state.tier)
        {
            let from = state.tier.clone();
            state.tier = to.to_string();
            state.last_action_at_ms = now_ms;
            state.low_streak = 0;
            return ElasticAction::TierDown {
                from,
                to: to.to_string(),
            };
        }
        return ElasticAction::Noop;
    }
    state.low_streak = 0;

    if signals.mem_available_mib > policy.exit_low_mib {
        state.high_streak += 1;
        if state.high_streak >= policy.sustain_samples
            && cooldown_open
            && let Some(to) = step_up(&state.tier)
        {
            let from = state.tier.clone();
            state.tier = to.to_string();
            state.last_action_at_ms = now_ms;
            state.high_streak = 0;
            return ElasticAction::TierUp {
                from,
                to: to.to_string(),
            };
        }
    } else {
        state.high_streak = 0;
    }
    ElasticAction::Noop
}

fn step_down(tier: &str) -> Option<&'static str> {
    match tier {
        "full" => Some("mid"),
        "mid" => Some("lite"),
        _ => None,
    }
}

fn step_up(tier: &str) -> Option<&'static str> {
    match tier {
        "lite" => Some("mid"),
        "mid" => Some("full"),
        _ => None,
    }
}

/// One sample of the runtime signals.
#[must_use]
pub fn sample() -> ElasticSignals {
    let mem = streaming::tools::memory_mib().map_or(0, |(avail, _)| avail);
    let battery = streaming::feature_gate::HardwareProfile::detect().battery;
    ElasticSignals {
        mem_available_mib: mem,
        battery,
    }
}

/// The wired controller: ticks, decides, persists tier decisions to the
/// settings bag (the boot path consumes `elastic.llm_tier`) and reports
/// every action on the SSE bus. With a live-swap seam installed (L2),
/// tier decisions also hot-swap the running model — the persistence
/// stays as the cross-boot floor either way.
pub struct ElasticController {
    pool: sqlx::SqlitePool,
    state: Mutex<ElasticState>,
    policy: ElasticPolicy,
    swap: Option<LiveSwapFn>,
}

impl ElasticController {
    #[must_use]
    pub fn new(pool: sqlx::SqlitePool, initial_tier: &str) -> Self {
        Self {
            pool,
            state: Mutex::new(ElasticState::new(initial_tier)),
            policy: ElasticPolicy::default(),
            swap: None,
        }
    }

    /// Tune the policy bands from `[elastic]` config.
    #[must_use]
    pub fn with_policy(mut self, policy: ElasticPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Install the L2 live-swap seam (no-op without it: decisions only
    /// persist for the next boot).
    #[must_use]
    pub fn with_live_swap(mut self, swap: LiveSwapFn) -> Self {
        self.swap = Some(swap);
        self
    }

    /// Spawn the 30-second tick.
    pub fn spawn(self: &Arc<Self>, event_tx: Arc<EventBus>) {
        let ctrl = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let action = {
                    let Ok(mut st) = ctrl.state.lock() else {
                        continue;
                    };
                    decide(sample(), &mut st, &ctrl.policy, now_ms)
                };
                if matches!(action, ElasticAction::Noop) {
                    continue;
                }
                ctrl.apply_and_report(action, now_ms, &event_tx).await;
            }
        });
    }

    /// Persist tier decisions, attempt the L2 live swap, and emit the
    /// `scale` event (SPEC #48). The live swap is best-effort: a failed
    /// swap never cancels the decision — the persisted tier still applies
    /// at the next boot.
    pub async fn apply_and_report(&self, action: ElasticAction, now_ms: u64, event_tx: &EventBus) {
        let reason = match &action {
            ElasticAction::TierDown { .. } => "memory_low",
            ElasticAction::TierUp { .. } => "memory_recovered",
            ElasticAction::BatteryPin { .. } => "battery_powered",
            ElasticAction::RecommendMinimalForm => "memory_critical",
            ElasticAction::Noop => return,
        };
        if let ElasticAction::TierDown { to, .. }
        | ElasticAction::TierUp { to, .. }
        | ElasticAction::BatteryPin { to, .. } = &action
        {
            // L2 first (the running process benefits immediately); the
            // same-path no-op inside the swap keeps repeated decisions
            // cheap. A failure falls through to persistence — next boot
            // still lands on the decided tier.
            if let Some(swap) = &self.swap {
                match swap(to.clone()).await {
                    Ok(()) => {
                        tracing::info!(tier = %to, reason, "elastic: LLM live-swapped (no restart)");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, tier = %to, "elastic: live swap failed — decision persists for next boot");
                    }
                }
            }
            if let Err(e) = web::db::set_setting(&self.pool, SETTING_TIER, to).await {
                tracing::warn!(error = %e, "elastic: tier persist failed (decision stands in-memory)");
            } else if self.swap.is_none() {
                tracing::info!(tier = %to, reason, "elastic: tier decision persisted (applies at next boot)");
            }
        } else if matches!(action, ElasticAction::RecommendMinimalForm) {
            tracing::warn!(
                "elastic: sustained critical memory — consider the minimal (cam) form for this host"
            );
        }
        let (from, to) = match &action {
            ElasticAction::TierDown { from, to } | ElasticAction::TierUp { from, to } => {
                (from.clone(), to.clone())
            }
            ElasticAction::BatteryPin { to, .. } => (String::new(), to.clone()),
            _ => (String::new(), String::new()),
        };
        let _ = event_tx.send(CameraEvent::Scale {
            action: action.wire_name().to_string(),
            from,
            to,
            reason: reason.to_string(),
            timestamp_ms: now_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POL: ElasticPolicy = ElasticPolicy {
        enter_low_mib: 1024,
        exit_low_mib: 2560,
        critical_mib: 320,
        cooldown_secs: 180,
        sustain_samples: 3,
    };

    fn sigs(mem: u64) -> ElasticSignals {
        ElasticSignals {
            mem_available_mib: mem,
            battery: false,
        }
    }

    #[test]
    fn healthy_memory_is_noop() {
        let mut st = ElasticState::new("full");
        for _ in 0..10 {
            assert_eq!(
                decide(sigs(4096), &mut st, &POL, 1_000),
                ElasticAction::Noop
            );
        }
    }

    #[test]
    fn sustained_low_downgrades_one_step_with_hysteresis() {
        let mut st = ElasticState::new("full");
        // Two samples inside the band: still noop (debounce).
        assert_eq!(decide(sigs(900), &mut st, &POL, 1_000), ElasticAction::Noop);
        assert_eq!(decide(sigs(900), &mut st, &POL, 2_000), ElasticAction::Noop);
        // Third sample: one step down.
        assert_eq!(
            decide(sigs(900), &mut st, &POL, 3_000),
            ElasticAction::TierDown {
                from: "full".into(),
                to: "mid".into()
            }
        );
        // Immediately low again: cooldown holds (180 s).
        assert_eq!(decide(sigs(500), &mut st, &POL, 4_000), ElasticAction::Noop);
        // After the cooldown, low samples re-accumulate the streak (one
        // already counted at 4_000) — the third total sample fires.
        let _ = decide(sigs(500), &mut st, &POL, 200_000);
        assert_eq!(
            decide(sigs(500), &mut st, &POL, 201_000),
            ElasticAction::TierDown {
                from: "mid".into(),
                to: "lite".into()
            }
        );
        // Floor reached: noop forever.
        assert_eq!(
            decide(sigs(100), &mut st, &POL, 400_000),
            ElasticAction::Noop
        );
    }

    #[test]
    fn band_exit_resets_the_streak() {
        let mut st = ElasticState::new("full");
        let _ = decide(sigs(900), &mut st, &POL, 1_000);
        let _ = decide(sigs(900), &mut st, &POL, 2_000);
        let _ = decide(sigs(4096), &mut st, &POL, 3_000); // band exit resets
        assert_eq!(decide(sigs(900), &mut st, &POL, 4_000), ElasticAction::Noop);
        assert_eq!(decide(sigs(900), &mut st, &POL, 5_000), ElasticAction::Noop);
    }

    #[test]
    fn recovery_upgrades_conservatively() {
        let mut st = ElasticState::new("lite");
        // Three sustained-high samples satisfy the debounce; the third
        // itself applies the step (cooldown is open — no prior action).
        assert_eq!(
            decide(sigs(4096), &mut st, &POL, 1_000),
            ElasticAction::Noop
        );
        assert_eq!(
            decide(sigs(4096), &mut st, &POL, 2_000),
            ElasticAction::Noop
        );
        assert_eq!(
            decide(sigs(4096), &mut st, &POL, 3_000),
            ElasticAction::TierUp {
                from: "lite".into(),
                to: "mid".into()
            }
        );
        // Inside the cooldown: no second step however high the memory.
        assert_eq!(
            decide(sigs(8192), &mut st, &POL, 4_000),
            ElasticAction::Noop
        );
    }

    #[test]
    fn critical_memory_advises_minimal_form_even_in_cooldown() {
        let mut st = ElasticState::new("full");
        let _ = decide(sigs(900), &mut st, &POL, 1_000); // low streak 1
        assert_eq!(decide(sigs(100), &mut st, &POL, 2_000), ElasticAction::Noop);
        assert_eq!(decide(sigs(100), &mut st, &POL, 3_000), ElasticAction::Noop);
        assert_eq!(
            decide(sigs(100), &mut st, &POL, 4_000),
            ElasticAction::RecommendMinimalForm
        );
        // Advice repeats while critical — visibility over silence.
        assert_eq!(
            decide(sigs(100), &mut st, &POL, 5_000),
            ElasticAction::RecommendMinimalForm
        );
    }

    #[test]
    fn battery_pins_lite_immediately_and_resets_on_ac() {
        let mut st = ElasticState::new("full");
        let b = ElasticSignals {
            mem_available_mib: 8192,
            battery: true,
        };
        assert_eq!(
            decide(b, &mut st, &POL, 1_000),
            ElasticAction::BatteryPin {
                to: "lite".into(),
                from: "full".into()
            }
        );
        // Still on battery: latched, no repeat.
        assert_eq!(decide(b, &mut st, &POL, 2_000), ElasticAction::Noop);
        // Back on AC with headroom: recovery climbs after the battery
        // pin's cooldown elapses (three samples + cooldown window).
        assert_eq!(
            decide(sigs(4096), &mut st, &POL, 5_000),
            ElasticAction::Noop
        );
        assert_eq!(
            decide(sigs(4096), &mut st, &POL, 200_000),
            ElasticAction::Noop
        );
        assert_eq!(
            decide(sigs(4096), &mut st, &POL, 201_000),
            ElasticAction::TierUp {
                from: "lite".into(),
                to: "mid".into()
            }
        );
    }

    #[tokio::test]
    async fn tier_decisions_persist_and_scale_events_fire() {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "mibee-elastic-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ));
        for ext in ["", ".db-wal", ".db-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", p.to_str().unwrap()));
        }
        let pool = web::db::init_pool(p.to_str().unwrap()).await.expect("pool");
        let ctrl = ElasticController::new(pool.clone(), "full");
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        ctrl.apply_and_report(
            ElasticAction::TierDown {
                from: "full".into(),
                to: "mid".into(),
            },
            42,
            &tx,
        )
        .await;
        assert_eq!(
            web::db::get_setting(&pool, SETTING_TIER)
                .await
                .unwrap()
                .as_deref(),
            Some("mid"),
            "decision persisted for the next boot"
        );
        match rx.recv().await.expect("scale event") {
            CameraEvent::Scale {
                action,
                from,
                to,
                reason,
                timestamp_ms,
            } => {
                assert_eq!(action, "tier_down");
                assert_eq!((from.as_str(), to.as_str()), ("full", "mid"));
                assert_eq!(reason, "memory_low");
                assert_eq!(timestamp_ms, 42);
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    // ── L2 wiring: live swap + persistence + scale event ─────────────

    async fn mem_pool() -> sqlx::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(":memory:")
            .await
            .expect("test pool");
        sqlx::query(
            "CREATE TABLE settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await
        .expect("settings table");
        pool
    }

    fn recording_swap(seen: Arc<Mutex<Vec<String>>>) -> LiveSwapFn {
        Arc::new(move |tier| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                seen.lock().expect("swap recorder").push(tier);
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn tier_down_live_swaps_persists_and_emits_scale() {
        let pool = mem_pool().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ctrl = ElasticController::new(pool.clone(), "full")
            .with_live_swap(recording_swap(Arc::clone(&seen)));
        let bus = web::routes::events::new_event_bus();
        let mut rx = bus.subscribe();
        ctrl.apply_and_report(
            ElasticAction::TierDown {
                from: "full".into(),
                to: "mid".into(),
            },
            123_456,
            &bus,
        )
        .await;
        assert_eq!(*seen.lock().expect("seen"), vec!["mid".to_string()]);
        assert_eq!(
            web::db::get_setting(&pool, SETTING_TIER).await.unwrap(),
            Some("mid".into())
        );
        match rx.try_recv() {
            Ok(web::routes::events::CameraEvent::Scale {
                action,
                from,
                to,
                reason,
                timestamp_ms,
            }) => {
                assert_eq!(action, "tier_down");
                assert_eq!(from, "full");
                assert_eq!(to, "mid");
                assert_eq!(reason, "memory_low");
                assert_eq!(timestamp_ms, 123_456);
            }
            other => panic!("expected scale event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_live_swap_still_persists_the_decision() {
        let pool = mem_pool().await;
        let failing: LiveSwapFn =
            Arc::new(|_| Box::pin(async { anyhow::bail!("no ladder path configured") }));
        let ctrl = ElasticController::new(pool.clone(), "full").with_live_swap(failing);
        let bus = web::routes::events::new_event_bus();
        ctrl.apply_and_report(
            ElasticAction::BatteryPin {
                from: "full".into(),
                to: "lite".into(),
            },
            1,
            &bus,
        )
        .await;
        assert_eq!(
            web::db::get_setting(&pool, SETTING_TIER).await.unwrap(),
            Some("lite".into())
        );
    }

    #[test]
    fn elastic_config_defaults_and_policy_projection() {
        let cfg = ElasticConfig::default();
        assert!(cfg.enabled);
        assert_eq!(ElasticPolicy::from(&cfg), ElasticPolicy::default());
        let tuned = ElasticConfig {
            enter_low_mib: 9000,
            sustain_samples: 0,
            ..Default::default()
        };
        let pol = ElasticPolicy::from(&tuned);
        assert_eq!(pol.enter_low_mib, 9000);
        assert_eq!(pol.sustain_samples, 1, "zero sustain clamps to 1");
    }

    #[test]
    fn elastic_config_parses_toml_with_defaults() {
        // Section-body shape (the [elastic] header is consumed by
        // AppConfig; serde(default) fills whatever the body omits).
        let cfg: ElasticConfig =
            toml::from_str("enabled = false\nenter_low_mib = 9000\n").expect("parse");
        assert!(!cfg.enabled);
        assert_eq!(cfg.enter_low_mib, 9000);
        assert_eq!(cfg.exit_low_mib, 2560, "unspecified keeps default");
    }
}
