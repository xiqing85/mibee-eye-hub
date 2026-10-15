use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// A rate-limit tracking entry for a single IP.
#[derive(Debug, Clone)]
struct RateLimitEntry {
    /// Number of attempts in the current window.
    count: usize,
    /// Window start time as seconds since UNIX epoch.
    window_start: u64,
}

static STATE: std::sync::LazyLock<Mutex<HashMap<String, RateLimitEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Hard ceiling on tracked client keys.
///
/// The client key is derived from request headers, so it is only as
/// trustworthy as the proxy in front of us. A cap makes the worst case
/// (attacker-supplied keys) bounded even when a legitimate deployment
/// legitimately has many clients behind one NAT.
const MAX_TRACKED_CLIENTS: usize = 8192;

/// Check whether `ip` is allowed to proceed under the rate limit.
///
/// `max` is the maximum number of attempts allowed within a sliding `window_secs` window.
/// Returns `true` if the request should be allowed, `false` if rate-limited.
pub fn check_rate_limit(ip: &str, max: usize, window_secs: u64) -> bool {
    let now = unix_secs();
    let mut state = STATE.lock();

    // Opportunistic prune before touching the entry. Without this the map
    // only ever shrinks via `reset_rate_limit` (a successful login), so a
    // stream of unauthenticated requests — each with a distinct client key —
    // grew this `HashMap<String, _>` without bound for the process lifetime.
    // `retain` over the whole map is cheap because the map is now small, and
    // it also gives us an early `capacity` signal without a separate counter.
    if state.len() >= MAX_TRACKED_CLIENTS / 2 {
        state.retain(|_, e| !is_window_expired(e, now, window_secs));
    }
    if state.len() >= MAX_TRACKED_CLIENTS {
        // Still saturated after pruning (many clients inside one window).
        // Fail closed for this key rather than growing without bound.
        tracing::warn!(
            tracked = state.len(),
            "rate-limit table saturated; rejecting new client key"
        );
        return false;
    }

    match state.get_mut(ip) {
        Some(ref mut entry) if !is_window_expired(entry, now, window_secs) => {
            if entry.count < max {
                entry.count += 1;
                true
            } else {
                false
            }
        }
        Some(ref mut entry) => {
            // Window expired -- reset
            entry.count = 1;
            entry.window_start = now;
            true
        }
        None => {
            state.insert(
                ip.to_string(),
                RateLimitEntry {
                    count: 1,
                    window_start: now,
                },
            );
            true
        }
    }
}

/// Seconds since the UNIX epoch, saturating at 0 for a pre-1970 clock.
///
/// The previous `SystemTime::now().duration_since(UNIX_EPOCH).unwrap()`
/// panicked outright if the system clock was ever set before 1970, taking
/// down every request that passes through the limiter.
fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reset rate-limit state for a given IP (e.g. after successful login).
pub fn reset_rate_limit(ip: &str) {
    let mut state = STATE.lock();
    state.remove(ip);
}

/// Clear all rate-limit state (for testing).
pub fn reset_all() {
    let mut state = STATE.lock();
    state.clear();
}

/// Check whether the entry's window has expired.
#[inline]
fn is_window_expired(entry: &RateLimitEntry, now: u64, window_secs: u64) -> bool {
    now >= entry.window_start.wrapping_add(window_secs)
}
/// Rate-limit configuration from app config.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitConfig {
    /// Maximum number of requests allowed in the window.
    pub max_requests: usize,
    /// Window duration in seconds.
    pub window_secs: u64,
}

static RATE_LIMIT_CONFIG: OnceLock<RateLimitConfig> = OnceLock::new();
static DEFAULT_RATE_LIMIT_CONFIG: RateLimitConfig = RateLimitConfig {
    max_requests: 20,
    window_secs: 60,
};

/// Initialize the rate limit configuration from app config.
pub fn init_rate_limit_config(max_requests: usize, window_secs: u64) {
    let _ = RATE_LIMIT_CONFIG.set(RateLimitConfig {
        max_requests,
        window_secs,
    });
}

/// Get the current rate limit configuration, or default if not initialized.
pub fn get_rate_limit_config() -> &'static RateLimitConfig {
    RATE_LIMIT_CONFIG
        .get()
        .unwrap_or(&DEFAULT_RATE_LIMIT_CONFIG)
}

/// Serialises every test in this crate that touches the rate-limit table.
///
/// `STATE` is a process-global table and both this module's tests and
/// `middleware`'s rate-limit tests reset it and assert on its contents. Left
/// to run in parallel they interleave: the bounded-table test deliberately
/// inserts `MAX_TRACKED_CLIENTS * 2` distinct keys, and while it is
/// mid-flood the table is saturated, so a concurrent test's
/// `check_rate_limit` takes the "reject new client key" branch and fails on
/// an assertion unrelated to what it was testing.
///
/// This is not hypothetical — the `voice-plus-llm` CI leg caught exactly
/// that, and only because that leg started *running* tests instead of just
/// compiling them.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Take [`TEST_LOCK`] and reset the table. Hold the returned guard for the
/// rest of the test.
///
/// `parking_lot::Mutex` is not reentrant, so take it exactly once per test;
/// use `reset_all()` for a mid-test reset.
#[cfg(test)]
pub(crate) fn serialise() -> parking_lot::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock();
    reset_all();
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Take the module lock and reset the table, returning the guard.
    fn setup() -> parking_lot::MutexGuard<'static, ()> {
        serialise()
    }
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_allows_first_request() {
        let _serialised = setup();
        assert!(check_rate_limit("192.168.1.1", 5, 60));
    }

    #[test]
    fn test_allows_under_limit() {
        let _serialised = setup();
        let ip = "10.0.0.1";
        for _ in 0..4 {
            assert!(check_rate_limit(ip, 5, 60), "Should allow under limit");
        }
    }

    #[test]
    fn test_rejects_at_limit() {
        let _serialised = setup();
        let ip = "10.0.0.2";
        for _ in 0..5 {
            check_rate_limit(ip, 5, 60);
        }
        // 6th attempt should be blocked
        assert!(!check_rate_limit(ip, 5, 60), "Should reject over limit");
    }

    #[test]
    fn test_window_resets() {
        let _serialised = setup();
        let ip = "10.0.0.3";

        // Exhaust limit
        for _ in 0..5 {
            check_rate_limit(ip, 5, 60);
        }
        assert!(!check_rate_limit(ip, 5, 60));

        // Wait for window to expire
        thread::sleep(Duration::from_millis(100));

        // The window hasn't actually expired (100ms < 60s), so still blocked.
        // Prove it, then reset the table. `reset_all()` rather than a second
        // `setup()`: the module lock is a `parking_lot::Mutex`, which is not
        // reentrant, so taking it twice in one test would deadlock.
        assert!(!check_rate_limit(ip, 5, 60));
        reset_all();
        assert!(check_rate_limit(ip, 5, 60));
    }

    #[test]
    fn test_short_window_expiry() {
        let _serialised = setup();
        let ip = "10.0.0.4";

        // Exhaust limit with short 50ms window
        for _ in 0..3 {
            assert!(check_rate_limit(ip, 3, 1)); // 1 sec window
        }
        assert!(!check_rate_limit(ip, 3, 1));

        // Wait just over 1 second for window to expire
        thread::sleep(Duration::from_secs(1));

        // Should be allowed again (window expired and reset)
        assert!(check_rate_limit(ip, 3, 1));
    }

    #[test]
    fn test_reset_rate_limit() {
        let _serialised = setup();
        let ip = "10.0.0.5";

        for _ in 0..5 {
            check_rate_limit(ip, 5, 60);
        }
        assert!(!check_rate_limit(ip, 5, 60));

        reset_rate_limit(ip);
        assert!(check_rate_limit(ip, 5, 60), "Should allow after reset");
    }

    #[test]
    fn test_different_ips_independent() {
        let _serialised = setup();

        let ip_a = "10.0.0.10";
        let ip_b = "10.0.0.20";

        // Exhaust ip_a
        for _ in 0..3 {
            check_rate_limit(ip_a, 3, 60);
        }
        assert!(!check_rate_limit(ip_a, 3, 60));

        // ip_b should still be allowed
        assert!(check_rate_limit(ip_b, 3, 60));
        assert!(check_rate_limit(ip_b, 3, 60));
        assert!(check_rate_limit(ip_b, 3, 60));
        assert!(!check_rate_limit(ip_b, 3, 60));
    }

    // ---------------------------------------------------------------------
    // Bounded-table regression (2026-10-15): the tracked-key map used to
    // grow without bound because entries were only ever removed by a
    // successful login, while the key itself came from a request header.
    // ---------------------------------------------------------------------

    #[test]
    fn tracked_table_stays_bounded_under_key_flood() {
        let _serialised = setup();
        // Distinct "clients" arriving inside one long window — exactly what a
        // spoofed header or a very large NAT produces.
        for i in 0..(MAX_TRACKED_CLIENTS * 2) {
            check_rate_limit(&format!("10.1.{}.{}", i / 256, i % 256), 5, 3600);
        }
        let tracked = STATE.lock().len();
        assert!(
            tracked <= MAX_TRACKED_CLIENTS,
            "tracked-key table grew past the cap: {tracked} > {MAX_TRACKED_CLIENTS}"
        );
    }

    #[test]
    fn expired_entries_are_reclaimed_so_a_flood_never_saturates() {
        let _serialised = setup();
        // Flood with a window already expired for every entry, then confirm
        // the next lookup still succeeds — i.e. the flood did not wedge the
        // table into permanent "reject everything".
        let past = unix_secs();
        assert!(check_rate_limit("10.2.0.1", 5, 0));
        assert!(past > 0);
        for i in 0..(MAX_TRACKED_CLIENTS * 2) {
            check_rate_limit(&format!("10.3.{}.{}", i / 256, i % 256), 5, 0);
        }
        assert!(
            check_rate_limit("10.9.9.9", 5, 60),
            "a fresh client must still be admitted after a flood"
        );
    }

    #[test]
    fn configured_limit_is_readable_from_the_once_lock() {
        // `init_rate_limit_config` is the path production actually uses
        // (middleware calls `check_rate_limit` with these values); before this
        // test nothing asserted the OnceLock plumbing at all.
        init_rate_limit_config(7, 42);
        let cfg = get_rate_limit_config();
        assert_eq!(cfg.max_requests, 7);
        assert_eq!(cfg.window_secs, 42);
        // A second init must not silently override an already-set config.
        init_rate_limit_config(99, 99);
        assert_eq!(get_rate_limit_config().max_requests, 7);
    }
}
