//! GB PTZCmd → ONVIF PTZ translation (roadmap M4-a, design §"外接云台
//! 代理"): a GB/T 28181 platform steers an external ONVIF camera head
//! through this hub. Direction/speed bits become ContinuousMove velocity
//! floats, presets map 1:1, a stop Move stops every axis. The hub itself
//! has no motors — the ONVIF head is the motor.
//!
//! The GB control handler fires on the SIP receive task, so dispatch is
//! copy-and-forward into an unbounded channel; a consumer task owns the
//! HTTP client (a blocking handler would stall signaling).

use std::sync::Arc;

use onvif_device_rs::client::SoapClient;
use onvif_device_rs::client::ptz::{PtzSpeed, Vector1D, Vector2D};

/// ONVIF default PTZ spaces (ONVIF PTZ Service spec, §spaces).
const PAN_TILT_SPACE: &str = "http://www.onvif.org/ver10/tptz/PanTiltSpaceNamespace";
const ZOOM_SPACE: &str = "http://www.onvif.org/ver10/tptz/ZoomSpaceNamespace";

/// `[ptz]` configuration section.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PtzConfig {
    /// Master switch: off (default) leaves `on_ptz` a no-op.
    pub enabled: bool,
    /// The external head's ONVIF device service endpoint
    /// (`http://host:port/onvif/device_service`).
    pub onvif_endpoint: String,
    /// Media profile token the PTZ commands address (default
    /// `Profile_1`, the common factory default).
    pub profile_token: String,
    /// WS-UsernameToken auth (optional — many heads run anonymous on
    /// the LAN).
    pub username: String,
    pub password: String,
}

impl Default for PtzConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            onvif_endpoint: String::new(),
            profile_token: "Profile_1".into(),
            username: String::new(),
            password: String::new(),
        }
    }
}

/// One translated command — the consumer's entire vocabulary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OnvifPtzAction {
    /// ContinuousMove at the given velocities (absent axes are omitted —
    /// they keep moving per ONVIF semantics).
    Continuous {
        pan: Option<f64>,
        tilt: Option<f64>,
        zoom: Option<f64>,
    },
    /// Stop every axis.
    Stop,
    GotoPreset(u8),
    SetPreset(u8),
    RemovePreset(u8),
}

/// Map a GB §A.3/A.4 command onto an ONVIF action. `None` = no ONVIF
/// equivalent (cruise, lens, aux, vendor-unknown, invalid) — the proxy
/// logs and drops those rather than guessing.
///
/// GB byte speeds are 0x00-0xFF slow→fast; ONVIF velocities are
/// -1.0..=1.0 signed by the direction bits.
#[must_use]
pub fn translate(cmd: &gb28181_rs::manscdp::PtzCommand) -> Option<OnvifPtzAction> {
    use gb28181_rs::manscdp::{PtzCommand, PtzPresetAction};
    match cmd {
        PtzCommand::Move {
            bits,
            pan_speed,
            tilt_speed,
            zoom_speed,
        } => {
            if bits == &0 {
                return Some(OnvifPtzAction::Stop);
            }
            let pan = axis(
                *bits & (gb28181_rs::manscdp::PTZ_RIGHT | gb28181_rs::manscdp::PTZ_LEFT) != 0,
                *bits & gb28181_rs::manscdp::PTZ_RIGHT != 0,
                *pan_speed,
            );
            let tilt = axis(
                *bits & (gb28181_rs::manscdp::PTZ_UP | gb28181_rs::manscdp::PTZ_DOWN) != 0,
                *bits & gb28181_rs::manscdp::PTZ_UP != 0,
                *tilt_speed,
            );
            let zoom = axis(
                *bits & (gb28181_rs::manscdp::PTZ_ZOOM_IN | gb28181_rs::manscdp::PTZ_ZOOM_OUT) != 0,
                *bits & gb28181_rs::manscdp::PTZ_ZOOM_IN != 0,
                *zoom_speed,
            );
            if pan.is_none() && tilt.is_none() && zoom.is_none() {
                return None;
            }
            Some(OnvifPtzAction::Continuous { pan, tilt, zoom })
        }
        PtzCommand::Preset { action, preset } => match action {
            PtzPresetAction::Call => Some(OnvifPtzAction::GotoPreset(*preset)),
            PtzPresetAction::Set => Some(OnvifPtzAction::SetPreset(*preset)),
            PtzPresetAction::Delete => Some(OnvifPtzAction::RemovePreset(*preset)),
        },
        PtzCommand::Cruise { .. }
        | PtzCommand::Lens { .. }
        | PtzCommand::AuxSwitch { .. }
        | PtzCommand::Unknown { .. }
        | PtzCommand::Invalid { .. } => None,
    }
}

fn axis(active: bool, positive: bool, speed: u8) -> Option<f64> {
    if !active {
        return None;
    }
    let magnitude = f64::from(speed) / 255.0;
    Some(if positive { magnitude } else { -magnitude })
}

/// The dispatch side (cheap, SIP-task-safe) plus the consumer task that
/// owns the SOAP client.
pub struct PtzProxy {
    tx: tokio::sync::mpsc::UnboundedSender<OnvifPtzAction>,
}

impl PtzProxy {
    /// Build the proxy and spawn the consumer. Fails closed — an
    /// unbuildable client (bad endpoint) returns `Err` and the caller
    /// proceeds without PTZ (the GB side stays a no-op ack).
    pub fn start(config: &PtzConfig) -> anyhow::Result<Arc<Self>> {
        let mut client = SoapClient::new(&config.onvif_endpoint)
            .map_err(|e| anyhow::anyhow!("ptz: endpoint: {e}"))?;
        if !config.username.is_empty() {
            client = client.with_credentials(&config.username, &config.password);
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let profile = config.profile_token.clone();
        tokio::spawn(async move {
            while let Some(action) = rx.recv().await {
                if let Err(e) = apply(&client, &profile, action).await {
                    tracing::warn!(error = %e, "ptz: ONVIF call failed (head keeps its last state)");
                }
            }
        });
        Ok(Arc::new(Self { tx }))
    }

    /// Forward one GB command (no-op when the channel is gone).
    pub fn dispatch(&self, cmd: &gb28181_rs::manscdp::PtzCommand) {
        match translate(cmd) {
            Some(action) => {
                let _ = self.tx.send(action);
            }
            None => tracing::debug!(?cmd, "ptz: no ONVIF equivalent — dropped"),
        }
    }
}

async fn apply(
    client: &SoapClient,
    profile: &str,
    action: OnvifPtzAction,
) -> Result<(), onvif_device_rs::client::ClientError> {
    match action {
        OnvifPtzAction::Continuous { pan, tilt, zoom } => {
            let velocity = PtzSpeed {
                pan_tilt: pan.or(tilt).map(|_| Vector2D {
                    x: pan.unwrap_or(0.0),
                    y: tilt.unwrap_or(0.0),
                    space: PAN_TILT_SPACE.into(),
                }),
                zoom: zoom.map(|z| Vector1D {
                    x: z,
                    space: ZOOM_SPACE.into(),
                }),
            };
            client.ptz_continuous_move(profile, &velocity, "").await
        }
        OnvifPtzAction::Stop => client.ptz_stop(profile, true, true).await,
        OnvifPtzAction::GotoPreset(n) => {
            client
                .ptz_goto_preset(profile, &preset_token(n), None)
                .await
        }
        OnvifPtzAction::SetPreset(n) => client
            .ptz_set_preset(profile, &format!("gb-preset-{n}"), &preset_token(n))
            .await
            .map(|_| ()),
        OnvifPtzAction::RemovePreset(n) => {
            client.ptz_remove_preset(profile, &preset_token(n)).await
        }
    }
}

/// GB preset numbers are 1-255; ONVIF preset tokens are strings. The
/// proxy's stable convention keeps the two address spaces 1:1.
fn preset_token(n: u8) -> String {
    format!("gb-preset-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gb28181_rs::manscdp::PtzCommand;

    fn mv(bits: u8, p: u8, t: u8, z: u8) -> PtzCommand {
        PtzCommand::Move {
            bits,
            pan_speed: p,
            tilt_speed: t,
            zoom_speed: z,
        }
    }

    #[test]
    fn translation_truth_table() {
        use gb28181_rs::manscdp::{PTZ_DOWN, PTZ_LEFT, PTZ_RIGHT, PTZ_UP, PTZ_ZOOM_IN};
        // Stop Move (no bits) → Stop.
        assert_eq!(translate(&mv(0, 200, 200, 0)), Some(OnvifPtzAction::Stop));
        // Right at full speed: pan +1.0, other axes omitted.
        assert_eq!(
            translate(&mv(PTZ_RIGHT, 255, 0, 0)),
            Some(OnvifPtzAction::Continuous {
                pan: Some(1.0),
                tilt: None,
                zoom: None
            })
        );
        // Left at half speed: −0.5.
        assert_eq!(
            translate(&mv(PTZ_LEFT, 128, 0, 0)),
            Some(OnvifPtzAction::Continuous {
                pan: Some(-128.0 / 255.0),
                tilt: None,
                zoom: None
            })
        );
        // Up-right diagonal + zoom in: all three axes.
        assert_eq!(
            translate(&mv(PTZ_UP | PTZ_RIGHT | PTZ_ZOOM_IN, 64, 192, 255)),
            Some(OnvifPtzAction::Continuous {
                pan: Some(64.0 / 255.0),
                tilt: Some(192.0 / 255.0),
                zoom: Some(1.0)
            })
        );
        // Down is negative tilt.
        assert_eq!(
            translate(&mv(PTZ_DOWN, 0, 255, 0)),
            Some(OnvifPtzAction::Continuous {
                pan: None,
                tilt: Some(-1.0),
                zoom: None
            })
        );
        // Presets map 1:1.
        assert_eq!(
            translate(&PtzCommand::Preset {
                action: gb28181_rs::manscdp::PtzPresetAction::Call,
                preset: 5
            }),
            Some(OnvifPtzAction::GotoPreset(5))
        );
        // Cruise / lens / aux / unknown / invalid have no ONVIF face.
        assert_eq!(
            translate(&PtzCommand::Cruise {
                action: gb28181_rs::manscdp::PtzCruiseAction::Start,
                group: 1,
                value: 2
            }),
            None
        );
        assert_eq!(translate(&PtzCommand::Invalid { raw: "zz".into() }), None);
    }

    /// The consumer E2E against a local ONVIF SOAP endpoint: dispatched GB
    /// commands arrive as the right SOAP action with the right body.
    #[tokio::test]
    async fn dispatched_commands_reach_the_soap_endpoint() {
        use axum::routing::post;
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let s2 = Arc::clone(&seen);
        let app = axum::Router::new().route(
            "/onvif/device_service",
            post(
                move |headers: axum::http::HeaderMap, body: String| async move {
                    let action = headers
                        .get("SOAPAction")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    s2.lock().push(format!("{action}|{body}"));
                    axum::http::StatusCode::OK
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let proxy = PtzProxy::start(&PtzConfig {
            enabled: true,
            onvif_endpoint: format!("http://{addr}/onvif/device_service"),
            profile_token: "Profile_7".into(),
            ..PtzConfig::default()
        })
        .expect("proxy starts");

        use gb28181_rs::manscdp::{PTZ_LEFT, PTZ_ZOOM_IN};
        proxy.dispatch(&mv(PTZ_LEFT | PTZ_ZOOM_IN, 255, 0, 255));
        proxy.dispatch(&mv(0, 0, 0, 0));
        proxy.dispatch(&PtzCommand::Preset {
            action: gb28181_rs::manscdp::PtzPresetAction::Call,
            preset: 3,
        });
        // Dropped (no ONVIF face) — must not reach the endpoint.
        proxy.dispatch(&PtzCommand::Invalid { raw: "x".into() });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if seen.lock().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let seen = seen.lock();
        assert_eq!(seen.len(), 3, "exactly the mapped commands: {seen:?}");
        assert!(
            seen[0].contains("ContinuousMove")
                && seen[0].contains("Profile_7")
                && seen[0].contains(&format!("-{}", 1.0_f64))
                && seen[0].contains("Zoom"),
            "left+zoom-in velocities: {seen:?}"
        );
        assert!(seen[1].contains("Stop"), "{seen:?}");
        assert!(
            seen[2].contains("GotoPreset") && seen[2].contains("gb-preset-3"),
            "{seen:?}"
        );
    }

    #[test]
    fn bad_endpoint_fails_closed() {
        assert!(
            PtzProxy::start(&PtzConfig {
                enabled: true,
                onvif_endpoint: "ftp://nope".into(),
                ..PtzConfig::default()
            })
            .is_err()
        );
    }
}
