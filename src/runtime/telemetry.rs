//! Audio telemetry tagging (design §5, SPEC appendix A #46): the level
//! tap's capture-context classifier.
//!
//! Every `audio_level` event carries two additive labels decided here:
//! `capture_mode` — active iff the system is inside a functional capture
//! window (wake / follow-up / away listen / talkback / transcription;
//! all of them arm the voice engine's shared listen deadline) — and
//! `source` — whose voice the window is hearing: the device itself
//! (TTS playback self-mute), an enrolled speaker (latest voiceprint
//! hit), an unregistered voice, or plain ambience.
//!
//! The classification core is pure; [`CaptureTagger`] wires it to the
//! engine atomics plus a voice-event observer that tracks the latest
//! enrolled hit. Real-time per-person clustering is phase 2 (design §5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use streaming::voice::VoiceEngine;

/// How long a speaker (or unregistered-voice) observation keeps tagging
/// samples after the voice event fired. The event lands after
/// transcription, so the TTL covers the tail of the utterance plus the
/// usual pause; the next utterance refreshes it.
const SOURCE_HIT_TTL_MS: u64 = 4_000;

/// Wire names (SPEC §6).
pub const MODE_ACTIVE: &str = "active";
pub const MODE_PASSIVE: &str = "passive";

/// Whose voice the sample hears (SPEC #46 priority order applies in
/// [`classify`]; the name rides separately for the `Speaker` kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    /// The device's own TTS output (playback self-mute window).
    SelfOut,
    /// An enrolled speaker matched the latest voice event.
    Speaker,
    /// A voice event without an enrolled match.
    Others,
    /// No recent voice at all.
    Ambient,
}

/// Pure classification core — unit-pinned below.
fn classify(
    now_ms: u64,
    listen_until_ms: u64,
    mute_until_ms: u64,
    speaker_hit_at: Option<u64>,
    unregistered_at: Option<u64>,
) -> (bool, SourceKind) {
    let active = listen_until_ms > 0 && now_ms < listen_until_ms;
    // Priority per SPEC #46: self-mute wins (the device's own output),
    // then the latest enrolled hit, then any voice, then ambience.
    let fresh = |at: Option<u64>| at.is_some_and(|a| now_ms.saturating_sub(a) <= SOURCE_HIT_TTL_MS);
    let kind = if mute_until_ms > 0 && now_ms < mute_until_ms {
        SourceKind::SelfOut
    } else if fresh(speaker_hit_at) {
        SourceKind::Speaker
    } else if fresh(unregistered_at) {
        SourceKind::Others
    } else {
        SourceKind::Ambient
    };
    (active, kind)
}

/// Latest voice observations fed by the voice-event observer.
#[derive(Default)]
struct VoiceObs {
    /// (name, at_ms) of the latest enrolled-speaker hit.
    speaker: Option<(String, u64)>,
    /// at_ms of the latest voice event without an enrolled hit.
    unregistered: Option<u64>,
}

/// Wired tagger: engine atomics + the voice-event observer.
pub struct CaptureTagger {
    listen_until: Arc<AtomicU64>,
    mute_until: Arc<AtomicU64>,
    obs: Arc<Mutex<VoiceObs>>,
}

impl CaptureTagger {
    /// Build from the voice engine. An inactive engine's atomics simply
    /// stay zero: every sample reads passive/ambient — the honest label
    /// for a form without the voice family.
    #[must_use]
    pub fn new(voice: &VoiceEngine) -> Self {
        Self {
            listen_until: voice.listen_deadline_share(),
            mute_until: voice.playback_mute_share(),
            obs: Arc::new(Mutex::new(VoiceObs::default())),
        }
    }

    /// Classify one level sample into its wire strings.
    pub fn tag(&self, now_ms: u64) -> (String, String) {
        let obs = self.obs.lock().expect("telemetry obs lock");
        let (active, kind) = classify(
            now_ms,
            self.listen_until.load(Ordering::SeqCst),
            self.mute_until.load(Ordering::SeqCst),
            obs.speaker.as_ref().map(|(_, at)| *at),
            obs.unregistered,
        );
        let mode = if active { MODE_ACTIVE } else { MODE_PASSIVE };
        let source = match kind {
            SourceKind::SelfOut => "self".to_string(),
            SourceKind::Speaker => {
                format!("speaker:{}", obs.speaker.as_ref().expect("fresh hit").0)
            }
            SourceKind::Others => "others".to_string(),
            SourceKind::Ambient => "ambient".to_string(),
        };
        (mode.to_string(), source)
    }

    /// Spawn the voice-event observer that keeps the speaker/unregistered
    /// observations fresh. Fail-open: a closed voice stream simply stops
    /// refreshing — samples degrade to ambient, never to wrong owners.
    pub fn spawn_observer(&self, voice: &VoiceEngine) {
        let mut events = voice.subscribe_events();
        let obs = Arc::clone(&self.obs);
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => {
                        let mut o = obs.lock().expect("telemetry obs lock");
                        if ev.speaker.is_empty() {
                            o.unregistered = Some(ev.timestamp_ms);
                        } else {
                            o.speaker = Some((ev.speaker.clone(), ev.timestamp_ms));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagger_with(listen_until: u64, mute_until: u64) -> CaptureTagger {
        CaptureTagger {
            listen_until: Arc::new(AtomicU64::new(listen_until)),
            mute_until: Arc::new(AtomicU64::new(mute_until)),
            obs: Arc::new(Mutex::new(VoiceObs::default())),
        }
    }

    #[test]
    fn idle_engine_reads_passive_ambient() {
        let t = tagger_with(0, 0);
        assert_eq!(t.tag(1_000), ("passive".into(), "ambient".into()));
    }

    #[test]
    fn listen_window_reads_active() {
        let t = tagger_with(10_000, 0);
        assert_eq!(t.tag(9_999).0, "active");
        assert_eq!(t.tag(10_000).0, "passive", "deadline itself is past");
        assert_eq!(t.tag(10_001).0, "passive");
    }

    #[test]
    fn playback_mute_tags_self_and_wins_over_listen() {
        let t = tagger_with(10_000, 5_000);
        let (mode, source) = t.tag(4_000);
        assert_eq!((mode.as_str(), source.as_str()), ("active", "self"));
        // After the mute lifts (still listening): no voice obs yet.
        assert_eq!(t.tag(6_000).1, "ambient");
    }

    #[test]
    fn speaker_hit_tags_within_ttl_then_decays() {
        let t = tagger_with(1_000_000, 0);
        t.obs.lock().unwrap().speaker = Some(("alice".into(), 100_000));
        assert_eq!(t.tag(103_999).1, "speaker:alice");
        assert_eq!(
            t.tag(104_001).1,
            "ambient",
            "hit older than the TTL must not own the sample"
        );
    }

    #[test]
    fn unregistered_voice_tags_others() {
        let t = tagger_with(1_000_000, 0);
        t.obs.lock().unwrap().unregistered = Some(100_000);
        assert_eq!(t.tag(101_000).1, "others");
        assert_eq!(t.tag(200_000).1, "ambient");
    }

    #[test]
    fn speaker_wins_over_unregistered() {
        let t = tagger_with(1_000_000, 0);
        let mut o = t.obs.lock().unwrap();
        o.speaker = Some(("bob".into(), 100_000));
        o.unregistered = Some(100_500);
        drop(o);
        assert_eq!(t.tag(101_000).1, "speaker:bob");
    }
}
