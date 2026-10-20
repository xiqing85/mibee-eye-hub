//! Plugin trigger engine (design §7, SPEC appendix A #47): plugins are
//! declarative manifests — trigger rules over the device's own event
//! sources, dispatching registered tools (built-ins and MCP stdio).
//!
//! First slice: the `alarm` and `away_state` sources are live (the
//! engine subscribes the SSE bus); `schedule` and `audio_level` are
//! parsed and validated but their wiring lands with the next slice.
//! Actions are tool calls; TTS/desktop/push actions follow. Everything
//! is fail-open: a failing plugin never touches the trigger source.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use web::agent::ToolRegistry;
use web::routes::events::{CameraEvent, EventBus};

/// One trigger source (SPEC #47 first-release set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    Alarm,
    AwayState,
    Schedule,
    AudioLevel,
}

impl TriggerSource {
    /// The sources whose event wiring is live in this slice.
    #[must_use]
    pub fn wired(&self) -> bool {
        matches!(self, Self::Alarm | Self::AwayState)
    }
}

/// One action to run when a rule fires. First-release: tool calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    /// Registered tool name (built-in or `mcp:<server>` namespace).
    pub tool: String,
    /// Arguments object forwarded verbatim.
    #[serde(default)]
    pub args: serde_json::Value,
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
/// trigger? `when` empty matches everything for the source.
#[must_use]
pub fn rule_fires(rule: &TriggerRule, trigger: &Trigger) -> bool {
    rule.on == trigger.source && (rule.when.is_empty() || rule.when == trigger.discriminator)
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
    cooldowns: Mutex<Cooldowns>,
}

impl TriggerEngine {
    #[must_use]
    pub fn new(plugins: HashMap<String, PluginManifest>, registry: Arc<ToolRegistry>) -> Self {
        let mut plugins = plugins;
        plugins.retain(|id, m| {
            let ok = m.consistent();
            if !ok {
                tracing::warn!(plugin = %id, "plugin: manifest inconsistent (trigger source not in permissions.events) — skipped");
            }
            ok
        });
        Self {
            plugins,
            registry,
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
    }

    /// Match + dispatch every rule for one trigger. Fail-open per action.
    pub async fn dispatch(&self, trigger: Trigger, event_tx: &EventBus) {
        let now = trigger.at_ms;
        for (plugin_id, manifest) in &self.plugins {
            for (idx, rule) in manifest.triggers.iter().enumerate() {
                if !rule.on.wired() {
                    continue;
                }
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
                    let outcome = match self
                        .registry
                        .call(&action.tool, action.args.clone(), Duration::from_secs(15))
                        .await
                    {
                        Ok(_) => "ok".to_string(),
                        Err(e) => format!("error:{e}"),
                    };
                    emit(
                        event_tx,
                        &InvocationRecord {
                            plugin: plugin_id.clone(),
                            rule: idx,
                            action: action.tool.clone(),
                            outcome,
                            timestamp_ms: now,
                        },
                    );
                }
            }
        }
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
        assert_eq!(m.triggers[0].run[0].tool, "lamp.switch");
        assert_eq!(m.triggers[0].run[0].args["room"], "客厅");
        assert!(m.consistent());
        assert!(m.triggers[0].on.wired());
        assert!(
            !TriggerSource::Schedule.wired(),
            "schedule wired next slice"
        );
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
                        tool: "time.now".into(),
                        args: serde_json::json!({}),
                    }],
                }],
                permissions: PluginPermissions {
                    events: vec!["away_state".into()],
                    egress: vec![],
                },
            },
        );
        let engine = TriggerEngine::new(plugins, Arc::clone(&registry));
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
}
