//! Per-capability model routing (design §6, M5): one declarative place
//! for "which provider serves this capability, and which direction does
//! a failure fall back". Chat and the HTTP legs converge here; the
//! voice bridge's agent chain adopts it next.
//!
//! Defaults follow the privacy decision (拍板 2026-10-19): the private
//! surfaces (vision/ASR) are local-first, chat may go cloud-first, TTS
//! stays local until cloud voices ship. Every capability is overridable
//! in `[models.<cap>]`.

use serde::{Deserialize, Serialize};

/// One inference provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Local engines: llama.cpp / ONNX / sherpa (real token accounting).
    #[default]
    Local,
    /// The existing cloud gateway (cloud.chat / cloud.vision).
    Cloud,
}

/// Operator declaration for one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteSelection {
    /// Primary provider.
    #[serde(default)]
    pub provider: Provider,
    /// Failure-fallback direction (the #35 fail-back semantics).
    #[serde(default)]
    pub fallback: Provider,
}

impl RouteSelection {
    /// Cloud-first with local fallback — the historic chat posture.
    pub const CLOUD_FIRST: RouteSelection = RouteSelection {
        provider: Provider::Cloud,
        fallback: Provider::Local,
    };

    /// Local-only (the historic private-surface posture).
    pub const LOCAL_ONLY: RouteSelection = RouteSelection {
        provider: Provider::Local,
        fallback: Provider::Local,
    };
}

/// The routing family under `[models]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Dialogue: cloud-first by default (historic posture), local fallback.
    #[serde(default = "chat_default")]
    pub chat: RouteSelection,
    /// Vision (privacy surface): local VLM first; cloud only on explicit
    /// override.
    #[serde(default = "local_only_default")]
    pub vision: RouteSelection,
    /// Speech-to-text: local paraformer.
    #[serde(default = "local_only_default")]
    pub asr: RouteSelection,
    /// Text-to-speech: local melo (cloud voices are the 变音 third face,
    /// M7 scope).
    #[serde(default = "local_only_default")]
    pub tts: RouteSelection,
}

const fn chat_default() -> RouteSelection {
    RouteSelection::CLOUD_FIRST
}

const fn local_only_default() -> RouteSelection {
    RouteSelection::LOCAL_ONLY
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            chat: chat_default(),
            vision: local_only_default(),
            asr: local_only_default(),
            tts: local_only_default(),
        }
    }
}

impl RoutingConfig {
    /// Effective primary provider for a capability, degrade-aware: a
    /// cloud primary with the cloud unavailable routes local (the same
    /// fail-open the live legs already practice).
    #[must_use]
    pub fn effective(&self, cap: &str, cloud_available: bool) -> Provider {
        let sel = self.selection(cap);
        if sel.provider == Provider::Cloud && !cloud_available {
            sel.fallback
        } else {
            sel.provider
        }
    }

    /// The operator's declaration for a capability (unknown caps are
    /// local-only — fail toward the private surface).
    #[must_use]
    pub fn selection(&self, cap: &str) -> RouteSelection {
        match cap {
            "chat" => self.chat,
            "vision" => self.vision,
            "asr" => self.asr,
            "tts" => self.tts,
            _ => RouteSelection::LOCAL_ONLY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_privacy_decision() {
        let r = RoutingConfig::default();
        assert_eq!(
            r.chat,
            RouteSelection::CLOUD_FIRST,
            "chat keeps cloud-first"
        );
        assert_eq!(
            r.vision,
            RouteSelection::LOCAL_ONLY,
            "privacy surfaces local"
        );
        assert_eq!(r.asr, RouteSelection::LOCAL_ONLY);
        assert_eq!(r.tts, RouteSelection::LOCAL_ONLY);
    }

    #[test]
    fn effective_degrades_to_fallback_when_cloud_unavailable() {
        let r = RoutingConfig::default();
        assert_eq!(r.effective("chat", true), Provider::Cloud);
        assert_eq!(
            r.effective("chat", false),
            Provider::Local,
            "cloud down routes local"
        );
    }

    #[test]
    fn operator_override_flips_a_capability() {
        let r = RoutingConfig {
            vision: RouteSelection {
                provider: Provider::Cloud,
                fallback: Provider::Local,
            },
            ..RoutingConfig::default()
        };
        assert_eq!(r.effective("vision", true), Provider::Cloud);
        assert_eq!(r.effective("chat", true), Provider::Cloud);
        assert_eq!(r.effective("asr", true), Provider::Local);
    }

    #[test]
    fn unknown_capability_is_local_only() {
        let r = RoutingConfig::default();
        assert_eq!(r.selection("diarize"), RouteSelection::LOCAL_ONLY);
        assert_eq!(r.effective("diarize", true), Provider::Local);
    }

    #[test]
    fn parses_from_toml_nested_tables() {
        let raw = r#"
[chat]
provider = "cloud"
fallback = "local"
[vision]
provider = "cloud"
"#;
        let r: RoutingConfig = toml::from_str(raw).unwrap();
        assert_eq!(r.chat.provider, Provider::Cloud);
        assert_eq!(r.chat.fallback, Provider::Local);
        assert_eq!(r.vision.provider, Provider::Cloud);
        assert_eq!(
            r.vision.fallback,
            Provider::Local,
            "absent key = type default"
        );
    }
}
