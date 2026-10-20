//! Plugin trigger engine (design §7, SPEC appendix A #47): plugins are
//! declarative manifests — trigger rules over the device's own event
//! sources, dispatching registered tools (built-ins and MCP stdio).
//!
//! All four first-release sources are live: `alarm` and `away_state`
//! ride the SSE bus; `schedule` gets a 20-second boundary-minute tick;
//! `audio_level` matches a numeric threshold against the level stream
//! (cooldown-gated). Actions: `tool` calls through the registry, `speak`
//! via TTS (self-mute held), `notify` via the desktop integration.
//! Everything is fail-open: a failing plugin never touches the source.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use web::agent::ToolRegistry;
use web::routes::events::{CameraEvent, EventBus};

use crate::desktop::Desktop;

/// One trigger source (SPEC #47 first-release set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Alarm,
    AwayState,
    Schedule,
    AudioLevel,
}

impl TriggerSource {}

/// One action to run when a rule fires (SPEC #47 amendment): exactly one
/// of `tool` / `speak` / `notify` per action object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// Registered tool name (built-in or `mcp:<server>` namespace).
    #[serde(default)]
    pub tool: Option<String>,
    /// Arguments object forwarded verbatim with `tool`.
    #[serde(default)]
    pub args: serde_json::Value,
    /// TTS utterance (self-mute held for the playback).
    #[serde(default)]
    pub speak: Option<String>,
    /// Desktop notification title (headless = ok no-op).
    #[serde(default)]
    pub notify: Option<String>,
}

/// One trigger rule (SPEC #47).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerRule {
    pub on: TriggerSource,
    /// `when` filter semantics per source: away_state → "armed" |
    /// "disarmed" | empty = both; alarm → "ai" | "audio" | "zone" |
    /// empty = any; schedule/audio_level → reserved (next slice).
    #[serde(default)]
    pub when: String,
    /// Same-rule re-fire window; 0 means the config default (60 s).
    #[serde(default)]
    pub cooldown_secs: u64,
    pub run: Vec<Action>,
}

/// The permission declaration (minimal event set + egress allowlist).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginPermissions {
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub egress: Vec<String>,
}

/// One plugin manifest (`[plugin.<id>]`, SPEC #47).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub version: u32,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<TriggerRule>,
    #[serde(default)]
    pub permissions: PluginPermissions,
}

impl PluginManifest {
    /// Whether the manifest is internally consistent: every trigger
    /// source appears in the declared event allowlist. Inconsistent
    /// manifests are skipped at load (fail-open), not silently run.
    #[must_use]
    pub fn consistent(&self) -> bool {
        self.triggers.iter().all(|r| {
            let name = match r.on {
                TriggerSource::Alarm => "alarm",
                TriggerSource::AwayState => "away_state",
                TriggerSource::Schedule => "schedule",
                TriggerSource::AudioLevel => "audio_level",
            };
            self.permissions.events.iter().any(|e| e == name)
        })
    }
}

/// Default cooldown when a rule declares none/zero.
const DEFAULT_COOLDOWN_SECS: u64 = 60;

/// The live trigger occurrence the engine derives from the event bus.
#[derive(Debug, Clone, PartialEq)]
pub struct Trigger {
    pub source: TriggerSource,
    /// away_state: "armed"/"disarmed"; alarm: the SSE `source` field.
    pub discriminator: String,
    pub at_ms: u64,
}

/// Pure rule matching (unit-pinned): does this rule fire for this
/// trigger? Per-source `when` semantics: alarm/away_state compare the
/// discriminator (empty = any); schedule compares "HH:MM"; audio_level
/// parses `when` as a numeric threshold and fires at level >= it.
#[must_use]
pub fn rule_fires(rule: &TriggerRule, trigger: &Trigger) -> bool {
    if rule.on != trigger.source {
        return false;
    }
    match rule.on {
        TriggerSource::AudioLevel => {
            let Ok(threshold) = rule.when.trim().parse::<f32>() else {
                return false; // non-numeric when never fires (load warns)
            };
            let Ok(level) = trigger.discriminator.parse::<f32>() else {
                return false;
            };
            level >= threshold
        }
        // Schedule requires the exact boundary minute — empty when is
        // rejected at load and never fires here either.
        TriggerSource::Schedule => !rule.when.is_empty() && rule.when == trigger.discriminator,
        _ => rule.when.is_empty() || rule.when == trigger.discriminator,
    }
}

/// A valid schedule `when` is "HH:MM" (00:00–23:59, local time).
#[must_use]
pub fn valid_schedule_when(when: &str) -> bool {
    let Some((h, m)) = when.split_once(':') else {
        return false;
    };
    h.len() == 2
        && m.len() == 2
        && h.chars().all(|c| c.is_ascii_digit())
        && m.chars().all(|c| c.is_ascii_digit())
        && h.parse::<u32>().is_ok_and(|v| v < 24)
        && m.parse::<u32>().is_ok_and(|v| v < 60)
}

/// The current local "HH:MM" for the schedule tick (chrono, same clock
/// the away scheduler uses for its boundary edges).
#[must_use]
pub fn local_hhmm(unix_secs: i64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs.max(0) as u64),
    )
    .format("%H:%M")
    .to_string()
}

/// Cooldown bookkeeping: last fire per (plugin, rule index).
#[derive(Default)]
struct Cooldowns {
    last_fired_ms: HashMap<(String, usize), u64>,
}

fn cooldown_open(
    cds: &mut Cooldowns,
    key: (String, usize),
    now_ms: u64,
    cooldown_secs: u64,
) -> bool {
    let window_ms = if cooldown_secs == 0 {
        DEFAULT_COOLDOWN_SECS
    } else {
        cooldown_secs
    } * 1000;
    let open = match cds.last_fired_ms.get(&key) {
        Some(last) => now_ms.saturating_sub(*last) >= window_ms,
        None => true,
    };
    if open {
        cds.last_fired_ms.insert(key, now_ms);
    }
    open
}

/// One dispatch outcome, mirrored onto the SSE bus (SPEC #47).
#[derive(Debug, Clone, Serialize)]
pub struct InvocationRecord {
    pub plugin: String,
    pub rule: usize,
    pub action: String,
    pub outcome: String,
    pub timestamp_ms: u64,
}

/// The trigger engine: subscribes the event bus, matches rules,
/// dispatches tool calls through the registry, emits invocations.
pub struct TriggerEngine {
    plugins: HashMap<String, PluginManifest>,
    registry: Arc<ToolRegistry>,
    tts: Arc<streaming::tts::TtsEngine>,
    desktop: Arc<Desktop>,
    cooldowns: Mutex<Cooldowns>,
}

impl TriggerEngine {
    #[must_use]
    pub fn new(
        plugins: HashMap<String, PluginManifest>,
        registry: Arc<ToolRegistry>,
        tts: Arc<streaming::tts::TtsEngine>,
        desktop: Arc<Desktop>,
    ) -> Self {
        let mut plugins = plugins;
        plugins.retain(|id, m| {
            let ok = m.consistent();
            if !ok {
                tracing::warn!(plugin = %id, "plugin: manifest inconsistent (source not allowed or invalid when) — skipped");
            }
            ok
        });
        Self {
            plugins,
            registry,
            tts,
            desktop,
            cooldowns: Mutex::new(Cooldowns::default()),
        }
    }

    /// Number of loaded (consistent) manifests — the capabilities count.
    #[must_use]
    pub fn loaded_count(&self) -> usize {
        self.plugins.len()
    }

    /// Spawn the bus subscription (alarm + away_state sources).
    pub fn spawn(self: &Arc<Self>, event_tx: Arc<EventBus>) {
        let engine = Arc::clone(self);
        let ticker_tx = Arc::clone(&event_tx);
        let mut events = event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                let ev = match events.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let Some(trigger) = trigger_from_event(&ev) else {
                    continue;
                };
                engine.dispatch(trigger, &event_tx).await;
            }
        });

        // Schedule ticker (SPEC #47 amendment): a 20-second tick whose
        // per-rule cooldown dedups the boundary minute — the default 60 s
        // window admits exactly one fire. Spawned only when some manifest
        // declares schedule rules.
        if self
            .plugins
            .values()
            .any(|m| m.triggers.iter().any(|r| r.on == TriggerSource::Schedule))
        {
            let engine = Arc::clone(self);
            let event_tx = ticker_tx;
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
                loop {
                    tick.tick().await;
                    let now_secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let hhmm = local_hhmm(now_secs);
                    engine
                        .dispatch(
                            Trigger {
                                source: TriggerSource::Schedule,
                                discriminator: hhmm,
                                at_ms: now_secs.saturating_mul(1000) as u64,
                            },
                            &event_tx,
                        )
                        .await;
                }
            });
        }
    }

    /// Match + dispatch every rule for one trigger. Fail-open per action.
    pub async fn dispatch(&self, trigger: Trigger, event_tx: &EventBus) {
        let now = trigger.at_ms;
        for (plugin_id, manifest) in &self.plugins {
            for (idx, rule) in manifest.triggers.iter().enumerate() {
                if !rule_fires(rule, &trigger) {
                    continue;
                }
                let key = (plugin_id.clone(), idx);
                let open = {
                    let Ok(mut cds) = self.cooldowns.lock() else {
                        continue;
                    };
                    cooldown_open(&mut cds, key, now, rule.cooldown_secs)
                };
                if !open {
                    emit(
                        event_tx,
                        &InvocationRecord {
                            plugin: plugin_id.clone(),
                            rule: idx,
                            action: String::new(),
                            outcome: "skipped:cooldown".into(),
                            timestamp_ms: now,
                        },
                    );
                    continue;
                }
                for action in &rule.run {
                    let (name, outcome) = self.run_action(action).await;
                    emit(
                        event_tx,
                        &InvocationRecord {
                            plugin: plugin_id.clone(),
                            rule: idx,
                            action: name,
                            outcome,
                            timestamp_ms: now,
                        },
                    );
                }
            }
        }
    }
}

impl TriggerEngine {
    /// Run one action (tool | speak | notify) and report (name, outcome).
    async fn run_action(&self, action: &Action) -> (String, String) {
        if let Some(tool) = &action.tool {
            let outcome = match self
                .registry
                .call(tool, action.args.clone(), Duration::from_secs(15))
                .await
            {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("error:{e}"),
            };
            return (tool.clone(), outcome);
        }
        if let Some(text) = &action.speak {
            if !self.tts.is_active() {
                return ("speak".into(), "skipped:tts-inactive".into());
            }
            // Playback self-mute held for the utterance (the #45
            // self-talk line — plugin speech must not wake the device).
            let tts = Arc::clone(&self.tts);
            let text = text.clone();
            let outcome = match tokio::task::spawn_blocking(move || tts.speak(&text)).await {
                Ok(Ok(_)) => "ok".to_string(),
                Ok(Err(e)) => format!("error:{e}"),
                Err(e) => format!("error:{e}"),
            };
            return ("speak".into(), outcome);
        }
        if let Some(title) = &action.notify {
            // Headless hosts have no session bus — the desktop layer is a
            // documented no-op there, so this honestly reports ok.
            self.desktop.notify_alarm("plugin", 1, Some(title));
            return ("notify".into(), "ok".into());
        }
        ("empty".into(), "skipped:no-action".into())
    }
}

fn emit(event_tx: &EventBus, record: &InvocationRecord) {
    let _ = event_tx.send(CameraEvent::PluginInvocation {
        plugin: record.plugin.clone(),
        rule: record.rule,
        action: record.action.clone(),
        outcome: record.outcome.clone(),
        timestamp_ms: record.timestamp_ms,
    });
}

/// Derive a trigger occurrence from a bus event (the wired sources).
fn trigger_from_event(ev: &CameraEvent) -> Option<Trigger> {
    match ev {
        CameraEvent::Alarm {
            source,
            timestamp_ms,
            ..
        } => Some(Trigger {
            source: TriggerSource::Alarm,
            discriminator: source.clone(),
            at_ms: *timestamp_ms,
        }),
        CameraEvent::AwayState {
            active, since_ms, ..
        } => Some(Trigger {
            source: TriggerSource::AwayState,
            discriminator: if *active {
                "armed".into()
            } else {
                "disarmed".into()
            },
            at_ms: since_ms.unwrap_or_else(|| 0),
        }),
        CameraEvent::AudioLevel {
            level,
            timestamp_ms,
            ..
        } => Some(Trigger {
            source: TriggerSource::AudioLevel,
            discriminator: format!("{level}"),
            at_ms: *timestamp_ms,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(on: TriggerSource, when: &str, cooldown: u64) -> TriggerRule {
        TriggerRule {
            on,
            when: when.into(),
            cooldown_secs: cooldown,
            run: vec![],
        }
    }

    fn trigger(source: TriggerSource, disc: &str, at: u64) -> Trigger {
        Trigger {
            source,
            discriminator: disc.into(),
            at_ms: at,
        }
    }

    #[test]
    fn rule_matching_per_source_and_when() {
        let armed = rule(TriggerSource::AwayState, "armed", 0);
        assert!(rule_fires(
            &armed,
            &trigger(TriggerSource::AwayState, "armed", 1)
        ));
        assert!(!rule_fires(
            &armed,
            &trigger(TriggerSource::AwayState, "disarmed", 1)
        ));
        let any = rule(TriggerSource::AwayState, "", 0);
        assert!(rule_fires(
            &any,
            &trigger(TriggerSource::AwayState, "disarmed", 1)
        ));
        let ai_alarm = rule(TriggerSource::Alarm, "ai", 0);
        assert!(rule_fires(
            &ai_alarm,
            &trigger(TriggerSource::Alarm, "ai", 1)
        ));
        assert!(!rule_fires(
            &ai_alarm,
            &trigger(TriggerSource::Alarm, "audio", 1)
        ));
        assert!(!rule_fires(
            &ai_alarm,
            &trigger(TriggerSource::AwayState, "ai", 1)
        ));
    }

    #[test]
    fn cooldown_gates_refires_within_window() {
        let mut cds = Cooldowns::default();
        let key = ("p".into(), 0);
        assert!(cooldown_open(&mut cds, key.clone(), 1_000, 60));
        assert!(
            !cooldown_open(&mut cds, key.clone(), 30_000, 60),
            "inside window"
        );
        assert!(
            cooldown_open(&mut cds, key.clone(), 61_000, 60),
            "window elapsed"
        );
        // Declared zero falls back to the 60 s default.
        let mut cds0 = Cooldowns::default();
        assert!(cooldown_open(&mut cds0, key.clone(), 0, 0));
        assert!(!cooldown_open(&mut cds0, key, 59_999, 0));
    }

    #[test]
    fn inconsistent_manifests_are_detected() {
        let mut m = PluginManifest {
            version: 1,
            tools: vec![],
            triggers: vec![rule(TriggerSource::AwayState, "armed", 0)],
            permissions: PluginPermissions::default(),
        };
        assert!(!m.consistent(), "no events allowed = inconsistent");
        m.permissions.events = vec!["away_state".into()];
        assert!(m.consistent());
        m.triggers.push(rule(TriggerSource::Alarm, "", 0));
        assert!(!m.consistent(), "second source not allowed");
    }

    #[test]
    fn manifest_parses_from_toml() {
        let raw = r#"
version = 1
tools = ["mcp:home"]
[[triggers]]
on = "away_state"
when = "armed"
cooldown_secs = 120
[[triggers.run]]
tool = "lamp.switch"
args = { room = "客厅", on = true }
[permissions]
events = ["away_state"]
egress = []
"#;
        let m: PluginManifest = toml::from_str(raw).unwrap();
        assert_eq!(m.triggers.len(), 1);
        assert_eq!(m.triggers[0].when, "armed");
        assert_eq!(m.triggers[0].cooldown_secs, 120);
        assert_eq!(m.triggers[0].run[0].tool.as_deref(), Some("lamp.switch"));
        assert_eq!(m.triggers[0].run[0].args["room"], "客厅");
        assert!(m.consistent());
    }

    #[tokio::test]
    async fn dispatch_runs_a_builtin_tool_and_reports_ok() {
        let tools: web::server::SharedTools = Arc::new(std::sync::RwLock::new(
            streaming::tools::ToolsConfig::default(),
        ));
        let registry = Arc::new(ToolRegistry::new(
            tools,
            &web::agent::AgentConfig::default(),
        ));
        registry.attach_streams(Arc::new(web::stream_manager::StreamManager::new()));
        let mut plugins = HashMap::new();
        plugins.insert(
            "clock-link".into(),
            PluginManifest {
                version: 1,
                tools: vec![],
                triggers: vec![TriggerRule {
                    on: TriggerSource::AwayState,
                    when: "armed".into(),
                    cooldown_secs: 60,
                    run: vec![Action {
                        tool: Some("time.now".into()),
                        args: serde_json::json!({}),
                        speak: None,
                        notify: None,
                    }],
                }],
                permissions: PluginPermissions {
                    events: vec!["away_state".into()],
                    egress: vec![],
                },
            },
        );
        // Player pinned empty — no test ever spawns a real player.
        let tts = Arc::new(streaming::tts::TtsEngine::from_config(
            &streaming::tts::TtsConfig {
                player: String::new(),
                ..Default::default()
            },
        ));
        let desktop = crate::desktop::Desktop::new(&crate::config::DesktopConfig::default());
        let engine = TriggerEngine::new(plugins, Arc::clone(&registry), tts, desktop);
        assert_eq!(engine.loaded_count(), 1);
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        engine
            .dispatch(trigger(TriggerSource::AwayState, "armed", 1_000), &tx)
            .await;
        let first = rx.recv().await.expect("invocation event");
        match first {
            CameraEvent::PluginInvocation {
                plugin, outcome, ..
            } => {
                assert_eq!(plugin, "clock-link");
                assert_eq!(outcome, "ok", "time.now is a registered built-in");
            }
            other => panic!("unexpected event {other:?}"),
        }
        // Cooldown: the second trigger inside the window reports skipped.
        engine
            .dispatch(trigger(TriggerSource::AwayState, "armed", 2_000), &tx)
            .await;
        let second = rx.recv().await.expect("cooldown event");
        match second {
            CameraEvent::PluginInvocation { outcome, .. } => {
                assert_eq!(outcome, "skipped:cooldown")
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn schedule_when_validation() {
        assert!(valid_schedule_when("00:00"));
        assert!(valid_schedule_when("23:59"));
        assert!(!valid_schedule_when("24:00"), "hour overflow");
        assert!(!valid_schedule_when("12:60"), "minute overflow");
        assert!(!valid_schedule_when("9:05"), "two-digit required");
        assert!(!valid_schedule_when("1200"), "missing colon");
        assert!(!valid_schedule_when(""), "empty");
    }

    #[test]
    fn audio_level_threshold_matching() {
        let loud = rule(TriggerSource::AudioLevel, "0.5", 60);
        assert!(rule_fires(
            &loud,
            &trigger(TriggerSource::AudioLevel, "0.62", 1)
        ));
        assert!(!rule_fires(
            &loud,
            &trigger(TriggerSource::AudioLevel, "0.3", 1)
        ));
        // Non-numeric when never fires (manifests skip at load, but the
        // matcher stays total and safe).
        let bad = rule(TriggerSource::AudioLevel, "loud", 0);
        assert!(!rule_fires(
            &bad,
            &trigger(TriggerSource::AudioLevel, "0.9", 1)
        ));
    }

    #[test]
    fn schedule_rules_fire_on_the_boundary_minute() {
        let at = rule(TriggerSource::Schedule, "09:00", 0);
        assert!(rule_fires(
            &at,
            &trigger(TriggerSource::Schedule, "09:00", 1)
        ));
        assert!(!rule_fires(
            &at,
            &trigger(TriggerSource::Schedule, "09:01", 1)
        ));
        // The away-scheduler convention: empty when would be any-minute —
        // rejected at load; the matcher treats it as never-firing.
        let any = rule(TriggerSource::Schedule, "", 0);
        assert!(!rule_fires(
            &any,
            &trigger(TriggerSource::Schedule, "09:00", 1)
        ));
    }

    #[tokio::test]
    async fn speak_on_inactive_tts_reports_skipped() {
        let tools: web::server::SharedTools = Arc::new(std::sync::RwLock::new(
            streaming::tools::ToolsConfig::default(),
        ));
        let registry = Arc::new(ToolRegistry::new(
            tools,
            &web::agent::AgentConfig::default(),
        ));
        let tts = Arc::new(streaming::tts::TtsEngine::from_config(
            &streaming::tts::TtsConfig {
                player: String::new(),
                ..Default::default()
            },
        )); // inactive
        let desktop = crate::desktop::Desktop::new(&crate::config::DesktopConfig::default());
        let engine = TriggerEngine::new(HashMap::new(), registry, tts, desktop);
        let (name, outcome) = engine
            .run_action(&Action {
                tool: None,
                args: serde_json::json!({}),
                speak: Some("你好".into()),
                notify: None,
            })
            .await;
        assert_eq!(
            (name.as_str(), outcome.as_str()),
            ("speak", "skipped:tts-inactive")
        );
    }

    #[test]
    fn local_hhmm_formats_two_digits() {
        let s = local_hhmm(0);
        assert_eq!(s.len(), 5);
        assert_eq!(s.as_bytes()[2], b':');
        assert!(s[..2].chars().all(|c| c.is_ascii_digit()));
        assert!(s[3..].chars().all(|c| c.is_ascii_digit()));
    }
}
