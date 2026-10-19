//! Process-level bootstrap — everything that must happen before any engine
//! or async runtime spawns, extracted from `main()` (M1 slice 1).
//!
//! Two concerns only: cap the OpenMP pool before any ORT session spawns it,
//! and install the rustls crypto provider exactly once. Anything needing
//! config or tracing does NOT belong here.

/// OpenMP thread cap for hosts where an oversubscribed spin-waiting pool is
/// an OOM/latency hazard (no-swap rigs): half the logical CPUs, clamped to
/// [1, 4]. Pure so the policy is pin-testable.
pub fn omp_thread_cap(logical: usize) -> usize {
    (logical / 2).clamp(1, 4)
}

/// Cap `OMP_NUM_THREADS` (a user-provided value always wins) and install
/// the rustls ring provider.
///
/// # Panics
/// Panics when the provider is already installed — that means two
/// components raced process setup, a wiring bug, not a runtime condition.
pub fn init_process_env() {
    if std::env::var_os("OMP_NUM_THREADS").is_none() {
        let logical = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        // SAFETY: single-threaded startup, before any worker exists.
        unsafe { std::env::set_var("OMP_NUM_THREADS", omp_thread_cap(logical).to_string()) };
    }

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");
}

#[cfg(test)]
mod tests {
    use super::omp_thread_cap;

    #[test]
    fn omp_cap_is_half_the_cores_clamped_to_1_and_4() {
        assert_eq!(omp_thread_cap(0), 1, "zero cores still gets one thread");
        assert_eq!(
            omp_thread_cap(1),
            1,
            "odd core count rounds down to 0, clamped up"
        );
        assert_eq!(omp_thread_cap(4), 2);
        assert_eq!(omp_thread_cap(8), 4);
        assert_eq!(omp_thread_cap(32), 4, "big hosts stay at 4");
    }
}
