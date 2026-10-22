//! Form capability profiles (design §2/§3, roadmap #6 M2 form split).
//!
//! A form is a *coarse capability gate*, applied to the loaded config
//! before any engine is built: sections outside the form's families are
//! force-disabled, the engines then construct inactive (fail-open), no
//! models load, and the capabilities document shrinks by itself — the
//! same mechanism the resource gate (#40) uses one level down.
//!
//! Profiles are wired into the composition root one form at a time, as
//! each form's assembly diverges (cam first). `Eye` is the ungated
//! reference: the profile is the identity and `apply` is a no-op.

use crate::config::AppConfig;

/// Which capability families a form carries. Field-per-family so a wrong
/// `true` is visible in review, not buried in a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormProfile {
    /// Visual AI on the camera stream: detection, zones, face, OCR.
    pub vision_ai: bool,
    /// Microphone pickup: sound events, ASR, KWS, speaker verification.
    pub audio: bool,
    /// Spoken interaction: TTS playback and the voice bridge.
    pub voice_out: bool,
    /// Dialogue family: LLM, VLM, decision triage, agent tools,
    /// conversations log, meeting diarization.
    pub dialogue: bool,
}

impl FormProfile {
    /// The full form: every family.
    pub const EYE: FormProfile = FormProfile {
        vision_ai: true,
        audio: true,
        voice_out: true,
        dialogue: true,
    };

    /// The camera form (design §3: 采集/AI/RTSP/RTMP/GB28181/ONVIF/录像).
    /// Visual AI rides the camera; the audio family is admitted for
    /// **ambient-sound listening** (SPEC appendix A #49, 2026-10-22):
    /// `[audio_ai]` gives a cam deployment YAMNet sound events plus the
    /// real-mic `audio_level` stream for the ambient waveform — the
    /// recommended cam setup. `[voice]` (wake-word stack) is also admitted
    /// by the family gate but pointless without dialogue (wake with no
    /// answer); deployments leave it off. Away keeps its camera-side watch
    /// (it degrades to the no-voice visitor record on its own).
    pub const CAM: FormProfile = FormProfile {
        vision_ai: true,
        audio: true,
        voice_out: false,
        dialogue: false,
    };

    /// The audio-pickup form (design §2 basic form 2): microphone, sound
    /// events, ASR transcribe records, KWS/speaker. No camera (hence no
    /// visual AI), no speaker output, no dialogue — the extension form
    /// (speech-to-text records) rides the audio family alone.
    pub const MIC: FormProfile = FormProfile {
        vision_ai: false,
        audio: true,
        voice_out: false,
        dialogue: false,
    };

    /// The intercom form (design §2 basic form 3): talkback both ways —
    /// microphone upstream plus speaker playback — and the PTZ proxy.
    /// Audio in and out; no camera AI, no dialogue.
    pub const TALK: FormProfile = FormProfile {
        vision_ai: false,
        audio: true,
        voice_out: true,
        dialogue: false,
    };

    /// Profile lookup by the bin's form tag. Unknown tags (a future form
    /// before its wiring lands) fall back to the full profile — failing
    /// open keeps an unknown binary useful instead of silently crippled.
    pub fn for_form(form: &str) -> FormProfile {
        match form {
            "cam" => Self::CAM,
            "mic" => Self::MIC,
            "talk" => Self::TALK,
            _ => Self::EYE,
        }
    }

    /// Force the config into this form's capability subset. Runs before
    /// the resource gate, which then sees (and charges) only what the
    /// form admits. Idempotent; logs one line per family it disables so
    /// a misconfigured deployment says why its AI did not load.
    pub fn apply(&self, form: &str, config: &mut AppConfig) {
        if !self.audio {
            if config.audio_ai.enabled {
                tracing::info!(form, "form gate: audio_ai disabled (not in this form)");
            }
            if config.voice.enabled {
                tracing::info!(form, "form gate: voice disabled (not in this form)");
            }
            config.audio_ai.enabled = false;
            config.voice.enabled = false;
        }
        if !self.voice_out && config.tts.enabled {
            tracing::info!(form, "form gate: tts disabled (not in this form)");
            config.tts.enabled = false;
        }
        if !self.dialogue {
            for (name, on) in [
                ("llm", config.llm.enabled),
                ("vlm", config.vlm.enabled),
                ("decision", config.decision.enabled),
                ("meeting", config.meeting.enabled),
                ("conversations", config.conversations.enabled),
                ("agent", config.agent.enabled),
            ] {
                if on {
                    tracing::info!(
                        form,
                        section = name,
                        "form gate: disabled (not in this form)"
                    );
                }
            }
            config.llm.enabled = false;
            config.vlm.enabled = false;
            config.decision.enabled = false;
            config.meeting.enabled = false;
            config.conversations.enabled = false;
            config.agent.enabled = false;
        }
        if !self.vision_ai {
            for (name, on) in [
                ("ai", config.ai.enabled),
                ("face", config.face.enabled),
                ("ocr", config.ocr.enabled),
            ] {
                if on {
                    tracing::info!(
                        form,
                        section = name,
                        "form gate: disabled (not in this form)"
                    );
                }
            }
            config.ai.enabled = false;
            config.face.enabled = false;
            config.ocr.enabled = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn everything_on() -> AppConfig {
        let mut c = AppConfig::default();
        c.ai.enabled = true;
        c.face.enabled = true;
        c.ocr.enabled = true;
        c.audio_ai.enabled = true;
        c.voice.enabled = true;
        c.tts.enabled = true;
        c.llm.enabled = true;
        c.vlm.enabled = true;
        c.decision.enabled = true;
        c.meeting.enabled = true;
        c.conversations.enabled = true;
        c.agent.enabled = true;
        c
    }

    #[test]
    fn cam_admits_audio_disables_dialogue_keeps_vision() {
        let mut config = everything_on();
        FormProfile::CAM.apply("cam", &mut config);
        assert!(config.ai.enabled, "visual detection is camera-domain");
        assert!(config.face.enabled);
        assert!(config.ocr.enabled);
        // Audio family admitted for ambient listening (SPEC #49): the
        // config decides whether that is audio_ai (recommended) or voice.
        assert!(
            config.audio_ai.enabled,
            "ambient-sound listening is cam-admitted"
        );
        assert!(
            config.voice.enabled,
            "audio family gate covers both sections"
        );
        for off in [
            config.tts.enabled,
            config.llm.enabled,
            config.vlm.enabled,
            config.decision.enabled,
            config.meeting.enabled,
            config.conversations.enabled,
            config.agent.enabled,
        ] {
            assert!(!off);
        }
    }

    #[test]
    fn cam_with_voice_off_keeps_audio_ai() {
        // The recommended cam deployment (SPEC #49): ambient listening via
        // audio_ai only, wake-word stack explicitly off.
        let mut config = everything_on();
        config.voice.enabled = false;
        FormProfile::CAM.apply("cam", &mut config);
        assert!(config.audio_ai.enabled);
        assert!(
            !config.voice.enabled,
            "form gate never re-enables a section"
        );
    }

    #[test]
    fn eye_apply_is_the_identity() {
        let mut config = everything_on();
        let before = config.clone();
        FormProfile::EYE.apply("eye", &mut config);
        assert_eq!(config, before);
    }

    #[test]
    fn apply_is_idempotent_for_cam() {
        let mut config = everything_on();
        FormProfile::CAM.apply("cam", &mut config);
        let once = config.clone();
        FormProfile::CAM.apply("cam", &mut config);
        assert_eq!(config, once);
    }

    #[test]
    fn unknown_form_falls_back_to_eye() {
        assert_eq!(FormProfile::for_form("eye"), FormProfile::EYE);
        assert_eq!(FormProfile::for_form("whatever"), FormProfile::EYE);
        assert_eq!(FormProfile::for_form("cam"), FormProfile::CAM);
        assert_eq!(FormProfile::for_form("mic"), FormProfile::MIC);
        assert_eq!(FormProfile::for_form("talk"), FormProfile::TALK);
    }

    #[test]
    fn mic_keeps_audio_only() {
        let mut config = everything_on();
        FormProfile::MIC.apply("mic", &mut config);
        // The pickup family: microphone pipeline + sound events.
        assert!(config.audio_ai.enabled);
        assert!(config.voice.enabled, "ASR/KWS ride the voice engine");
        // No speaker, no camera AI, no dialogue.
        for off in [
            config.tts.enabled,
            config.ai.enabled,
            config.face.enabled,
            config.ocr.enabled,
            config.llm.enabled,
            config.vlm.enabled,
            config.decision.enabled,
            config.meeting.enabled,
            config.conversations.enabled,
            config.agent.enabled,
        ] {
            assert!(!off);
        }
    }

    #[test]
    fn talk_keeps_audio_in_and_out() {
        let mut config = everything_on();
        FormProfile::TALK.apply("talk", &mut config);
        // Intercom: microphone upstream + speaker playback.
        assert!(config.audio_ai.enabled);
        assert!(config.voice.enabled);
        assert!(config.tts.enabled, "the output-device family stays on");
        // No camera AI, no dialogue.
        for off in [
            config.ai.enabled,
            config.face.enabled,
            config.ocr.enabled,
            config.llm.enabled,
            config.vlm.enabled,
            config.decision.enabled,
            config.meeting.enabled,
            config.conversations.enabled,
            config.agent.enabled,
        ] {
            assert!(!off);
        }
    }

    #[test]
    fn apply_is_idempotent_for_mic_and_talk() {
        for (tag, profile) in [("mic", FormProfile::MIC), ("talk", FormProfile::TALK)] {
            let mut config = everything_on();
            profile.apply(tag, &mut config);
            let once = config.clone();
            profile.apply(tag, &mut config);
            assert_eq!(config, once, "{tag} apply must be idempotent");
        }
    }
}
