//! Microphone level meter for the real-time voice waveform (SPEC §6
//! `audio_level` event, notebook dialect #36).
//!
//! A lightweight tap on the 16 kHz mono monitor broadcast: RMS per window,
//! perceptual (dB) scaling, attack/release smoothing, and a minimum emit
//! interval so the SSE bus carries at most ~10 updates/second regardless
//! of the chunk cadence. Pure state machine — the main.rs wiring just
//! feeds chunks in and sends whatever comes out.
//!
//! The published `level` is NOT linear RMS: real speech through a laptop
//! mic sits around RMS 0.03–0.1, which is invisible on a linear bar. The
//! raw RMS is mapped from dBFS (−45…−5 dB → 0…1) so ordinary conversation
//! reads mid-scale and only a closed noise gate publishes exact zero.

/// Smoothed mic level 0..=1 plus the emit policy.
pub struct LevelMeter {
    /// Samples accumulated since the last emit window.
    pending: usize,
    pending_sq: f64,
    /// Samples at/near full scale accumulated in the pending window.
    pending_clips: usize,
    /// Sustained-clipping episode state (see [`ClipDetector`]).
    clips: ClipDetector,
    /// Last emitted smoothed level (attack fast, release slow).
    level: f32,
    /// Monotonic ms at last emit; `None` until the first emit (the very
    /// first full window is never throttled). Caller-supplied clock keeps
    /// the throttle deterministic in tests.
    last_emit_ms: Option<u64>,
}

/// Sustained-input-clip detector (2026-10-12 incident guard). OS-side mic
/// gain above 100% on SOF laptop mics corrupts the capture into
/// near-full-scale garbage: the KWS hit rate collapses with zero log
/// output while this meter happily reads "loud". A window where ≥1/10 of
/// samples sit within 0.5% of full scale counts as clipped; 5 consecutive
/// clipped windows (~0.5 s) fire one warn per episode, re-armed by the
/// first clean window.
#[derive(Debug, Default)]
struct ClipDetector {
    streak: u32,
    warned: bool,
}

impl ClipDetector {
    const CLIP_SAMPLE: i16 = 32_600; // ≥ 99.5% of full scale
    const RATIO: usize = 10; // ≥1/10 samples clipped → clipped window
    const WARN_STREAK: u32 = 5; // ~0.5 s of continuous clipping

    /// Feed one completed window; returns `true` exactly once per episode
    /// (on the call that crosses the warn streak) so the caller can log.
    fn feed(&mut self, clipped_samples: usize, window_samples: usize) -> bool {
        if window_samples > 0 && clipped_samples * Self::RATIO >= window_samples {
            self.streak = self.streak.saturating_add(1);
            if self.streak >= Self::WARN_STREAK && !self.warned {
                self.warned = true;
                return true;
            }
        } else {
            self.streak = 0;
            self.warned = false;
        }
        false
    }
}

const WINDOW_SAMPLES: usize = 1600; // 100 ms @ 16 kHz
const MIN_EMIT_INTERVAL_MS: u64 = 100;
const ATTACK: f32 = 0.6;
const RELEASE: f32 = 0.3;
/// Release-tail cutoff on the scaled axis: below this the bar snaps to 0
/// instead of crawling down asymptotically.
const FLOOR: f32 = 0.01;
/// Noise gate: room noise sits under −45 dBFS and must not flicker the bar.
const MIN_DB: f32 = -45.0;
/// Full scale for the mapping — close, loud speech saturates here.
const MAX_DB: f32 = -5.0;

/// Perceptual mapping dBFS → 0..=1 (−45…−5 dB, clamped).
fn scale_level(rms: f32) -> f32 {
    if rms <= f32::EPSILON {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl LevelMeter {
    pub fn new() -> Self {
        Self {
            pending: 0,
            pending_sq: 0.0,
            pending_clips: 0,
            clips: ClipDetector::default(),
            level: 0.0,
            last_emit_ms: None,
        }
    }

    /// Feed one monitor chunk; returns the smoothed level when a new value
    /// should be published (window full AND the emit interval elapsed).
    pub fn update(&mut self, samples: &[i16], now_ms: u64) -> Option<f32> {
        for s in samples {
            let f = f64::from(*s) / 32768.0;
            self.pending_sq += f * f;
            if s.unsigned_abs() >= ClipDetector::CLIP_SAMPLE.unsigned_abs() {
                self.pending_clips += 1;
            }
        }
        self.pending += samples.len();
        if self.pending < WINDOW_SAMPLES {
            return None;
        }
        if self
            .last_emit_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < MIN_EMIT_INTERVAL_MS)
        {
            // Interval not elapsed: keep accumulating (the window average
            // just spans slightly more than 100 ms — harmless).
            return None;
        }
        let rms = (self.pending_sq / self.pending as f64).sqrt() as f32;
        if self.clips.feed(self.pending_clips, self.pending) {
            tracing::warn!(
                "audio: mic input clipping sustained — check the OS source gain \
                 (SOF laptop mics corrupt above 100%); KWS degrades silently"
            );
        }
        self.pending = 0;
        self.pending_sq = 0.0;
        self.pending_clips = 0;
        self.last_emit_ms = Some(now_ms);
        let target = scale_level(rms);
        // Speech bursts must light up instantly; decay can lag — a
        // symmetric filter makes the wave look sluggish on stop.
        let alpha = if target > self.level { ATTACK } else { RELEASE };
        self.level = self.level + alpha * (target - self.level);
        if self.level < FLOOR {
            self.level = 0.0;
        }
        Some(self.level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silence(n: usize) -> Vec<i16> {
        vec![0; n]
    }

    fn loud(n: usize) -> Vec<i16> {
        // Near-full-scale triangle wave: RMS ≈ 0.4 — like close speech.
        (0..n)
            .map(|i| ((i % 7) as f32 / 7.0 * 60000.0 - 30000.0) as i16)
            .collect()
    }

    /// Sine at the given peak amplitude — RMS = peak/√2.
    fn sine(n: usize, peak: f32) -> Vec<i16> {
        (0..n)
            .map(|i| (peak * (i as f32 * 0.25).sin()) as i16)
            .collect()
    }

    #[test]
    fn typical_speech_rms_reads_mid_scale() {
        // The whole point of the dB mapping: linear RMS at real speech
        // levels (a few hundredths of full scale) is invisible on a bar.
        // A sine with peak 2400 → RMS ≈ 0.052 (−25.7 dBFS) must land
        // visibly above a quarter of the scale after one window.
        assert!((scale_level(0.052) - 0.48).abs() < 0.02);
        let mut m = LevelMeter::new();
        let l = m.update(&sine(1600, 2400.0), 0).unwrap();
        assert!(l > 0.25, "typical speech lights the bar: {l}");
        // Close/loud speech saturates near the top of the scale.
        assert!(scale_level(0.3) > 0.85);
        // Room noise (−54 dBFS) is under the gate → exact zero.
        assert_eq!(scale_level(0.002), 0.0);
        assert_eq!(scale_level(0.0), 0.0);
    }

    #[test]
    fn emits_at_most_every_100ms_and_needs_a_full_window() {
        let mut m = LevelMeter::new();
        // Half a window → nothing.
        assert!(m.update(&silence(800), 0).is_none());
        // The FIRST full window emits immediately (no throttle before the
        // first publish — the UI must not wait 100 ms of dead air).
        assert!(m.update(&silence(800), 0).is_some());
        // Window full again but only +50ms since the last emit → throttled.
        assert!(m.update(&loud(1600), 50).is_none());
        // At +120ms → emits again.
        assert!(m.update(&loud(1600), 130).is_some());
    }

    #[test]
    fn attack_is_fast_release_is_slow() {
        let mut m = LevelMeter::new();
        let l1 = m.update(&loud(1600), 0).unwrap();
        let l2 = m.update(&loud(1600), 150).unwrap();
        let l3 = m.update(&loud(1600), 300).unwrap();
        assert!(l1 > 0.1, "first loud window lights up: {l1}");
        assert!(
            l2 >= l1 && l3 >= l2,
            "sustained input rises: {l1} {l2} {l3}"
        );
        // Input stops: decays but not instantly to zero.
        let d1 = m.update(&silence(1600), 450).unwrap();
        assert!(d1 < l3 && d1 > 0.0, "release is gradual: {l3} -> {d1}");
        // Long silence eventually floors to exactly zero (idle suppression).
        let mut m2 = LevelMeter::new();
        m2.update(&loud(1600), 0);
        let mut last = 1.0;
        for t in (0..25).map(|i| 150 + i * 150) {
            if let Some(v) = m2.update(&silence(1600), t) {
                last = v;
            }
        }
        assert_eq!(last, 0.0, "long silence floors the level");
    }

    #[test]
    fn silent_start_emits_zero_not_noise() {
        let mut m = LevelMeter::new();
        assert_eq!(m.update(&silence(3200), 0), Some(0.0));
    }

    #[test]
    fn clip_detector_fires_once_per_episode_and_rearms() {
        let mut d = ClipDetector::default();
        for _ in 0..4 {
            assert!(!d.feed(1600, 1600), "below the warn streak");
        }
        assert!(d.feed(1600, 1600), "5th fully-clipped window fires");
        assert!(!d.feed(1600, 1600), "no re-warn inside one episode");
        assert!(!d.feed(0, 1600), "clean window resets the streak");
        for _ in 0..4 {
            assert!(!d.feed(1600, 1600));
        }
        assert!(d.feed(1600, 1600), "warn re-arms after a clean window");
    }

    #[test]
    fn clip_detector_ignores_sparse_peaks_and_empty_windows() {
        let mut d = ClipDetector::default();
        // 6% of samples clipped (isolated pops) never trips the ratio.
        for _ in 0..100 {
            assert!(!d.feed(100, 1600));
        }
        // Degenerate windows are never clipped.
        assert!(!d.feed(1600, 0));
        assert!(!d.feed(0, 0));
    }

    #[test]
    fn full_scale_windows_drive_the_detector() {
        // The 2026-10-12 incident shape: OS mic gain >100% turned the whole
        // capture into near-full-scale garbage while every other subsystem
        // looked healthy. The meter must flag it.
        let mut m = LevelMeter::new();
        let clipped: Vec<i16> = vec![32_700; 1600];
        let mut fired = false;
        for i in 0..6 {
            m.update(&clipped, i * 150);
            fired |= m.clips.warned;
        }
        assert!(fired, "sustained full-scale input sets the clip flag");
        // A normal-speech window clears the episode.
        m.update(&sine(1600, 2400.0), 6 * 150);
        assert_eq!(m.clips.streak, 0, "clean window resets the streak");
        assert!(!m.clips.warned, "warn re-arms");
    }
}
