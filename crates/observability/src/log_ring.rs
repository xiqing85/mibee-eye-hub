//! In-process log ring backing `GET /api/logs` (SPEC v1 §3.2).
//!
//! A [`tracing_subscriber::Layer`] captures every event into a bounded
//! ring (newest in, oldest evicted) so the web API can serve recent logs
//! without reading journald or shipping to Loki. Real-time only: the ring
//! holds the last 1000 entries and nothing else.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing_subscriber::Layer;

/// One captured log entry as served by `/api/logs`.
#[derive(Debug, Clone, Serialize)]
pub struct RingEntry {
    pub ts: i64,
    pub level: String,
    pub target: String,
    pub message: String,
}

const RING_CAPACITY: usize = 1000;

struct Inner {
    entries: Mutex<VecDeque<RingEntry>>,
}

static RING: OnceLock<Inner> = OnceLock::new();

fn ring() -> &'static Inner {
    RING.get_or_init(|| Inner {
        entries: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
    })
}

/// Push one entry, evicting the oldest at capacity.
pub fn push(entry: RingEntry) {
    let inner = ring();
    let mut guard = inner.entries.lock().expect("log ring poisoned");
    if guard.len() == RING_CAPACITY {
        guard.pop_front();
    }
    guard.push_back(entry);
}

/// Snapshot of the ring, newest first (capped by `limit` after `min_level`
/// filtering).
pub fn newest_first(limit: usize, min_level: u8) -> Vec<RingEntry> {
    let inner = ring();
    let guard = inner.entries.lock().expect("log ring poisoned");
    guard
        .iter()
        .rev()
        .filter(|e| level_rank(&e.level) >= min_level)
        .take(limit)
        .cloned()
        .collect()
}

/// Rank a lowercase level name; unknown levels rank as debug (0).
pub fn level_rank(level: &str) -> u8 {
    match level {
        "info" => 1,
        "warn" | "warning" => 2,
        "error" => 3,
        _ => 0,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A [`tracing_subscriber::Layer`] that tees every event into the ring.
///
/// The message field is rendered like `fmt`'s default; remaining fields are
/// appended as `key=value` pairs. Attach it with
/// `.with(log_ring::layer())` alongside the fmt/OTLP/Loki layers.
pub fn layer<S>() -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    RingLayer.boxed()
}

struct RingLayer;

struct FieldVisitor {
    message: Option<String>,
    extras: Vec<(String, String)>,
}

impl tracing_subscriber::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.extras
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.extras
                .push((field.name().to_string(), value.to_string()));
        }
    }
}

impl<S> Layer<S> for RingLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor {
            message: None,
            extras: Vec::new(),
        };
        event.record(&mut visitor);
        let mut message = visitor.message.unwrap_or_default();
        for (k, v) in visitor.extras {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(&format!("{k}={v}"));
        }
        push(RingEntry {
            ts: now_secs(),
            level: event.metadata().level().to_string().to_lowercase(),
            target: event.metadata().target().to_string(),
            message,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_capacity_and_order() {
        // The global ring is shared across parallel tests: assert relative
        // behaviour (order + capacity), not absolute counts.
        for i in 0..25 {
            push(RingEntry {
                ts: i,
                level: "info".into(),
                target: "t".into(),
                message: format!("m{i}"),
            });
        }
        let entries = newest_first(usize::MAX, 0);
        assert!(entries.len() >= 25);
        // Relative order only: parallel tests share the global ring and can
        // interleave pushes after m24 (CI flake 2026-10-20: "filter-in" from
        // level_filter_and_ranking landed first) — absolute head assertions
        // violate this test's own sharing rule.
        let pos24 = entries.iter().position(|e| e.message == "m24").unwrap();
        let pos23 = entries.iter().position(|e| e.message == "m23").unwrap();
        let pos0 = entries.iter().position(|e| e.message == "m0").unwrap();
        assert!(pos24 < pos23, "newer entries come first");
        assert!(pos23 < pos0, "oldest of the batch comes last");
    }

    #[test]
    fn level_filter_and_ranking() {
        push(RingEntry {
            ts: 1,
            level: "info".into(),
            target: "t".into(),
            message: "filter-out".into(),
        });
        push(RingEntry {
            ts: 2,
            level: "warn".into(),
            target: "t".into(),
            message: "filter-in".into(),
        });
        let warns = newest_first(usize::MAX, level_rank("warn"));
        assert!(warns.iter().any(|e| e.message == "filter-in"));
        assert!(!warns.iter().any(|e| e.message == "filter-out"));
    }

    #[test]
    fn layer_captures_events() {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::Registry::default().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "ring_test", camera = 7, "hello ring");
        });
        let entries = newest_first(usize::MAX, 0);
        let ours = entries
            .iter()
            .find(|e| e.target == "ring_test" && e.message.contains("hello ring"))
            .expect("event must be captured");
        assert_eq!(ours.level, "warn");
        assert!(ours.message.contains("camera=7"));
    }
}
