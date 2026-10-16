//! Admission control for the asynchronous VLM alarm-frame description.
//!
//! # Why this exists
//!
//! A visual alarm is accepted, and then — separately and without delaying
//! that alarm — a vision-language model is asked to describe the triggering
//! frame. Two things make that dangerous if left unguarded:
//!
//! **Stacking.** A noisy detector produces rising edges every cooldown
//! period (30 s by default) while a single description takes 35–60 s on CPU.
//! Without a gate, each new edge spawns another `spawn_blocking` call that
//! holds a full llama context. On a 4-core host that saturates the machine;
//! on a host where the model is near the memory guardrail it OOM-aborts the
//! process. That is not hypothetical — it is why this gate exists, added
//! after the 2026-09-28 incident where load 31 and a `ggml` alloc-abort
//! took the service down.
//!
//! **Leaking the lease.** The previous implementation released the in-flight
//! flag by calling `VLM_DESC_IN_FLIGHT.store(false)` immediately after
//! `describe_jpeg()` returned. If inference *panicked* — which a malformed
//! frame or an OOM inside the model can do — that store never ran. The flag
//! stayed `true` for the life of the process and every later description was
//! dropped as "already in flight", silently, with no log line explaining why
//! the feature had quietly stopped working. Nothing in the test suite could
//! catch it, because the whole decision lived in a 2 900-line `main.rs` with
//! no tests at all.
//!
//! So the lease is an RAII guard: it is released by `Drop`, which runs on the
//! normal path, on an early return, and on unwind. There is no way to take
//! the lease and forget to give it back.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Why a description request was not admitted.
///
/// Returned instead of a bool so the caller's log says *which* gate closed —
/// "something is running" and "we ran recently" are different operational
/// stories, and conflating them is how a throttle bug looks like a stuck lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescDecision {
    /// Admitted. The returned guard must be held for the whole run.
    Admitted,
    /// Another description is already running. Never queue: a queued frame is
    /// stale by the time a slot frees, and the queue is unbounded memory.
    Busy,
    /// A description started less than `min_interval` ago. This is the
    /// backstop for the case where a run finished quickly and the detector is
    /// still flapping.
    Throttled,
}

impl DescDecision {
    /// Short label for logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Busy => "busy",
            Self::Throttled => "throttled",
        }
    }
}

/// Held for the duration of one description. Releases the in-flight flag on
/// drop, including on unwind.
///
/// Deliberately not `Clone` and deliberately not `Send`-to-many: exactly one
/// guard may exist per admitted run, and two of them would mean the second
/// drop releases a lease the first still owns.
#[derive(Debug)]
pub struct DescLease<'g> {
    in_flight: &'g AtomicBool,
    /// Set on drop so a test can assert the release actually happened.
    released: bool,
}

impl DescLease<'_> {
    /// Whether the lease is still held. Always `false` once dropped, which
    /// means this is only useful before the drop.
    #[must_use]
    pub fn is_held(&self) -> bool {
        !self.released
    }
}

impl Drop for DescLease<'_> {
    fn drop(&mut self) {
        self.in_flight.store(false, Ordering::SeqCst);
        self.released = true;
    }
}

/// Single-flight plus a minimum-interval floor, over caller-supplied atomics.
///
/// The state is passed in rather than living in a `static` so the logic is
/// testable without a process-wide singleton: several gates can coexist, and
/// a test can give itself its own private gate instead of racing every other
/// test in the binary. `main.rs` keeps the single process-wide gate it
/// actually uses.
pub struct DescGate {
    in_flight: AtomicBool,
    last_start_ms: AtomicU64,
    /// Whether `last_start_ms` holds a real value.
    ///
    /// Without this the very first request is throttled whenever the timebase
    /// starts near zero: `0 - 0 = 0`, and `0 < min_interval` reads as "inside
    /// the floor". With the wall clock in milliseconds that never happens, so
    /// the bug stays invisible until someone drives the gate with a monotonic
    /// or test-relative clock — at which point the VLM silently never runs
    /// once. An explicit "have we ever started" flag makes the first request
    /// admissible on any timebase, which is the intended contract.
    has_started: AtomicBool,
    min_interval_ms: u64,
}

impl DescGate {
    /// Create an owned gate. `const` so the product can hold one in a
    /// `static` without a lazy initialiser — a lazy one would need a
    /// `OnceLock` and a branch on every alarm frame.
    ///
    /// Dropping it releases nothing: the atomics go with it.
    #[must_use]
    pub const fn new(min_interval_ms: u64) -> Self {
        Self {
            in_flight: AtomicBool::new(false),
            last_start_ms: AtomicU64::new(0),
            has_started: AtomicBool::new(false),
            min_interval_ms,
        }
    }

    /// The floor between consecutive starts, in milliseconds.
    #[must_use]
    pub fn min_interval_ms(&self) -> u64 {
        self.min_interval_ms
    }

    /// Whether a description is running right now.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Ask to run a description at `now_ms`.
    ///
    /// On [`DescDecision::Admitted`] the caller **must** keep the returned
    /// lease alive for the whole run. Dropping it releases the gate; leaking
    /// it (e.g. with `std::mem::forget`) is the one way to still get the old
    /// stuck-forever behaviour, which is why [`DescLease`] documents it.
    ///
    /// Ownership of the lease is part of the decision: when the throttle
    /// closes, this function releases *its own* acquisition only. A run
    /// already in flight belongs to someone else's lease, and clearing the
    /// flag for them would let a second description start beside the first —
    /// the exact stacking this gate exists to prevent.
    pub fn try_admit(&self, now_ms: u64) -> (DescDecision, Option<DescLease<'_>>) {
        // test-then-set: only one caller can observe `false` and win the swap.
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return (DescDecision::Busy, None);
        }
        let ever_started = self.has_started.load(Ordering::SeqCst);
        let since_last = now_ms.saturating_sub(self.last_start_ms.load(Ordering::SeqCst));
        if ever_started && since_last < self.min_interval_ms {
            // We hold the flag but no run is ours to give up: undo our own
            // acquisition and leave any other run alone.
            self.in_flight.store(false, Ordering::SeqCst);
            return (DescDecision::Throttled, None);
        }
        self.last_start_ms.store(now_ms, Ordering::SeqCst);
        self.has_started.store(true, Ordering::SeqCst);
        (
            DescDecision::Admitted,
            Some(DescLease {
                in_flight: &self.in_flight,
                released: false,
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_INTERVAL: u64 = 120_000;

    #[test]
    fn admits_the_first_request_and_holds_the_lease() {
        let gate = DescGate::new(MIN_INTERVAL);
        let (decision, lease) = gate.try_admit(1_000);
        assert_eq!(decision, DescDecision::Admitted);
        let lease = lease.expect("admitted must hand back a lease");
        assert!(gate.is_busy());
        assert!(lease.is_held());
    }

    #[test]
    fn rejects_a_second_request_while_one_runs_and_never_queues_it() {
        let gate = DescGate::new(MIN_INTERVAL);
        let (_d, lease) = gate.try_admit(1_000);
        let (decision, second) = gate.try_admit(1_001);
        assert_eq!(decision, DescDecision::Busy);
        assert!(
            second.is_none(),
            "a rejected request must not hand out a lease"
        );
        // Crucially, the first run's lease is still valid and the gate is
        // still held by it.
        assert!(gate.is_busy());
        assert!(lease.expect("first lease").is_held());
    }

    /// The regression from the 2026-09-28 incident: three rising edges inside
    /// one description must produce exactly one run.
    #[test]
    fn concurrent_triggers_produce_exactly_one_run() {
        let gate = DescGate::new(MIN_INTERVAL);
        let mut admitted = 0;
        let mut leases = Vec::new();
        for i in 0..3 {
            let (decision, lease) = gate.try_admit(1_000 + i);
            if decision == DescDecision::Admitted {
                admitted += 1;
                leases.push(lease.expect("admitted"));
            }
        }
        assert_eq!(admitted, 1, "stacked descriptions are what OOM-aborted us");
        drop(leases);
    }

    #[test]
    fn dropping_the_lease_releases_the_gate() {
        let gate = DescGate::new(MIN_INTERVAL);
        {
            let (_d, lease) = gate.try_admit(0);
            drop(lease);
        }
        assert!(!gate.is_busy());
    }

    /// The bug the old `store(false)`-after-the-call shape could not survive.
    ///
    /// A panic inside inference left the flag set for the life of the
    /// process: every later description was dropped as "busy", silently, and
    /// the feature simply stopped working with nothing in the log explaining
    /// why. `Drop` runs during unwind, so the lease is returned.
    /// The first request must be admissible on *any* timebase.
    ///
    /// A `last_start_ms` of zero means "a run started at the epoch", so
    /// `now - 0` under a test-relative or monotonic clock is small, reads as
    /// "inside the floor", and the VLM never runs even once. With the wall
    /// clock the arithmetic accidentally works out, which is exactly why this
    /// would have shipped.
    #[test]
    fn the_first_request_is_admitted_on_any_timebase() {
        for now in [0, 1, 42, MIN_INTERVAL - 1] {
            let gate = DescGate::new(MIN_INTERVAL);
            let (decision, lease) = gate.try_admit(now);
            assert_eq!(
                decision,
                DescDecision::Admitted,
                "first request at t={now} must not be throttled"
            );
            drop(lease);
        }
    }

    #[test]
    fn a_panicking_run_releases_the_lease() {
        let gate = DescGate::new(MIN_INTERVAL);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (_d, _lease) = gate.try_admit(0);
            // The guard is deliberately *not* dropped explicitly below: the
            // panic must do it.
            panic!("inference blew up");
        }));

        assert!(result.is_err(), "the test must actually panic");
        assert!(
            !gate.is_busy(),
            "a panicking run must not leave the gate stuck — that silently \
             killed every future description"
        );

        // And the gate is genuinely usable again, not just readable. Past the
        // floor, so the throttle does not mask the result.
        let (decision, lease) = gate.try_admit(MIN_INTERVAL);
        assert_eq!(decision, DescDecision::Admitted);
        drop(lease);
    }

    #[test]
    fn a_throttled_request_is_rejected_and_the_gate_is_freed() {
        let gate = DescGate::new(MIN_INTERVAL);
        {
            let (_d, lease) = gate.try_admit(10_000);
            drop(lease);
        }
        // Well inside the floor.
        let (decision, lease) = gate.try_admit(10_500);
        assert_eq!(decision, DescDecision::Throttled);
        assert!(lease.is_none());
        assert!(
            !gate.is_busy(),
            "a throttled request must not leave the gate looking busy, or the \
             next real request would be misreported as Busy"
        );
    }

    /// A throttle rejection must not release somebody else's run.
    ///
    /// This is the subtle one. Sequence: A is admitted, A finishes and
    /// releases, then B arrives inside the floor and is throttled. If the
    /// throttle branch cleared the flag it would also be correct there — but
    /// consider A admitted, and a *concurrent* C arriving a moment later.
    /// `try_admit` for C observes `in_flight == true` and returns `Busy`
    /// without touching anything; the throttle branch is only reachable when
    /// this call itself won the swap, so it can only ever undo its own
    /// acquisition. This test pins that invariant by holding a lease and
    /// checking the gate stays held through unrelated traffic.
    #[test]
    fn unrelated_traffic_never_releases_a_running_lease() {
        let gate = DescGate::new(MIN_INTERVAL);
        let (_d, lease) = gate.try_admit(0);
        let lease = lease.expect("admitted");

        // Traffic well past the floor, and traffic inside it, must both leave
        // the running lease alone.
        for i in 1..50 {
            let _ = gate.try_admit(i * MIN_INTERVAL);
        }
        assert!(
            gate.is_busy(),
            "a running lease was released by other traffic"
        );
        assert!(lease.is_held());
    }

    #[test]
    fn the_floor_is_measured_from_the_last_start_not_the_last_finish() {
        let gate = DescGate::new(MIN_INTERVAL);
        {
            let (_d, lease) = gate.try_admit(1_000);
            drop(lease);
        }
        // Just inside the floor after a run that finished instantly.
        assert_eq!(
            gate.try_admit(1_000 + MIN_INTERVAL - 1).0,
            DescDecision::Throttled
        );
        // Exactly at the floor is admissible.
        assert_eq!(
            gate.try_admit(1_000 + MIN_INTERVAL).0,
            DescDecision::Admitted
        );
    }

    #[test]
    fn a_backwards_clock_does_not_open_the_gate_forever() {
        // `now_ms` comes from wall time; a clock step backwards must not make
        // `saturating_sub` clamp to 0 and admit forever.
        let gate = DescGate::new(MIN_INTERVAL);
        {
            let (_d, lease) = gate.try_admit(5_000_000);
            drop(lease);
        }
        // now < last_start
        let (decision, lease) = gate.try_admit(1_000);
        assert_eq!(
            decision,
            DescDecision::Throttled,
            "clamped to 0 is inside the floor"
        );
        assert!(lease.is_none());
    }

    #[test]
    fn decision_labels_are_stable_for_logging() {
        assert_eq!(DescDecision::Admitted.as_str(), "admitted");
        assert_eq!(DescDecision::Busy.as_str(), "busy");
        assert_eq!(DescDecision::Throttled.as_str(), "throttled");
    }
}
