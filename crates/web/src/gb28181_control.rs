//! DeviceControl / DeviceConfig host handlers (gb28181-rs seams).
//!
//! Installing a control handler accepts the whole DeviceControl family:
//! the library acks recognized sub-commands and routes them here.
//! notebook has no pan-tilt hardware — PTZ / HomePosition / DragZoom and
//! the guard / teleboot family stay ack-only via the trait defaults.
//! RecordCmd gates local recording through a shared pause flag; IFrameCmd
//! forces the next OpenH264-encoded frame to an IDR.
//!
//! The DeviceConfig glue toggles the alarm-NOTIFY gate and the runtime
//! FrameMirror flags (A.2.3.2.9); BasicParam stays unimplemented and is
//! therefore rejected — same posture as the raspi twins.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gb28181_rs::server::{DeviceConfigHandler, DeviceControlHandler};
use streaming::capture_source::Flips;

/// DeviceControl family handler (A.2.3.1).
pub struct Gb28181ControlHandler {
    /// Shared with the recording `FileOutput`s: `true` pauses local
    /// recording (platform RecordCmd StopRecord), `false` resumes.
    recording_paused: Arc<AtomicBool>,
    /// Shared with the camera encode loops (OpenH264): setting this makes
    /// the next encoded frame an IDR (platform IFrameCmd).
    force_idr: Arc<AtomicBool>,
    /// External ONVIF head (M4-a): GB PTZCmd forwards here; `None` keeps
    /// the historical no-op ack.
    ptz_proxy: Option<Arc<crate::ptz_proxy::PtzProxy>>,
}

impl Gb28181ControlHandler {
    pub fn new(recording_paused: Arc<AtomicBool>, force_idr: Arc<AtomicBool>) -> Self {
        Self {
            recording_paused,
            force_idr,
            ptz_proxy: None,
        }
    }

    /// Attach the external-head PTZ proxy (M4-a).
    #[must_use]
    pub fn with_ptz_proxy(mut self, proxy: Option<Arc<crate::ptz_proxy::PtzProxy>>) -> Self {
        self.ptz_proxy = proxy;
        self
    }

    /// Snapshot of the pause gate (mirrors the shared flag).
    pub fn recording_paused(&self) -> bool {
        self.recording_paused.load(Ordering::SeqCst)
    }
}

impl DeviceControlHandler for Gb28181ControlHandler {
    fn on_force_iframe(&self) {
        self.force_idr.store(true, Ordering::SeqCst);
        tracing::info!("DeviceControl IFrameCmd: keyframe request queued for the encoder");
    }

    fn on_record(&self, start: bool) {
        let prev = self.recording_paused.swap(!start, Ordering::SeqCst);
        if prev != !start {
            tracing::info!(
                record = start,
                "DeviceControl RecordCmd: local recording gate updated"
            );
        }
    }

    fn on_ptz(&self, cmd: &gb28181_rs::manscdp::PtzCommand) {
        if let Some(proxy) = &self.ptz_proxy {
            proxy.dispatch(cmd);
        }
    }

    // on_guard / on_reset_alarm / on_teleboot / on_ptz / on_home_position /
    // on_drag_zoom: no actuators on a notebook — the trait defaults ack
    // these as documented no-ops.
}

/// DeviceConfig glue (A.2.3.2): the platform's AlarmReport dual switches
/// toggle whether AI alarms go out as GB28181 Alarm NOTIFY (SSE `alarm`
/// events are unaffected; the initial value comes from the
/// `alarm_notify_enabled` config key), and FrameMirror flips the runtime
/// mirror flags shared with every capture loop (A.2.1.22 mode semantics
/// via [`mirror_mode_to_flips`]). Runtime-only state — the per-camera
/// `hflip`/`vflip` config still governs boot. BasicParam is left to the
/// trait default (reject), matching the raspi twins.
#[derive(Debug)]
pub struct DeviceConfigGlue {
    pub alarm: Arc<AtomicBool>,
    pub gb_flips: Arc<Flips>,
}

impl DeviceConfigGlue {
    pub fn new(alarm: Arc<AtomicBool>, gb_flips: Arc<Flips>) -> Self {
        Self { alarm, gb_flips }
    }

    pub fn alarm_enabled(&self) -> bool {
        self.alarm.load(Ordering::SeqCst)
    }
}

impl DeviceConfigHandler for DeviceConfigGlue {
    fn on_alarm_report(&self, motion_detection: u32, field_detection: u32) {
        let enabled = motion_detection > 0 || field_detection > 0;
        let prev = self.alarm.swap(enabled, Ordering::SeqCst);
        if prev != enabled {
            tracing::info!(
                enabled,
                "DeviceConfig AlarmReport: alarm notify gate updated"
            );
        }
    }

    fn on_frame_mirror(&self, mode: u32) {
        let (hflip, vflip) = mirror_mode_to_flips(mode);
        self.gb_flips.set(hflip, vflip);
        tracing::info!(
            mode,
            hflip,
            vflip,
            "DeviceConfig FrameMirror: runtime mirror flags updated"
        );
    }
}

/// A.2.1.22 frameMirrorCfgType: 0 不启用, 1 水平镜像, 2 上下镜像, 3 中心
/// (both). Anything else leaves the frames untouched (defensive — the
/// library only decodes 0-3).
#[must_use]
pub fn mirror_mode_to_flips(mode: u32) -> (bool, bool) {
    match mode {
        1 => (true, false),
        2 => (false, true),
        3 => (true, true),
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_cmd_flips_pause_gate() {
        let flag = Arc::new(AtomicBool::new(false));
        let handler =
            Gb28181ControlHandler::new(Arc::clone(&flag), Arc::new(AtomicBool::new(false)));
        assert!(!handler.recording_paused());

        handler.on_record(false); // StopRecord
        assert!(handler.recording_paused());
        assert!(flag.load(Ordering::SeqCst));

        handler.on_record(true); // Record
        assert!(!handler.recording_paused());
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[test]
    fn alarm_report_gate_follows_dual_switches() {
        let gate =
            DeviceConfigGlue::new(Arc::new(AtomicBool::new(true)), Arc::new(Flips::default()));
        assert!(gate.alarm_enabled());

        gate.on_alarm_report(0, 0);
        assert!(!gate.alarm_enabled());

        gate.on_alarm_report(1, 0);
        assert!(gate.alarm_enabled());

        gate.on_alarm_report(0, 1);
        assert!(gate.alarm_enabled());
    }

    #[test]
    fn frame_mirror_updates_shared_runtime_flags() {
        let flips = Arc::new(Flips::default());
        let glue = DeviceConfigGlue::new(Arc::new(AtomicBool::new(true)), Arc::clone(&flips));

        glue.on_frame_mirror(1);
        assert_eq!(flips.load(), (true, false));

        glue.on_frame_mirror(3);
        assert_eq!(flips.load(), (true, true));

        glue.on_frame_mirror(0);
        assert_eq!(flips.load(), (false, false));
    }

    #[test]
    fn mirror_mode_table_matches_a_2_1_22() {
        assert_eq!(mirror_mode_to_flips(0), (false, false));
        assert_eq!(mirror_mode_to_flips(1), (true, false));
        assert_eq!(mirror_mode_to_flips(2), (false, true));
        assert_eq!(mirror_mode_to_flips(3), (true, true));
        // Defensive: unknown modes leave frames untouched.
        assert_eq!(mirror_mode_to_flips(4), (false, false));
    }

    #[test]
    fn force_idr_sets_shared_latch() {
        let flag = Arc::new(AtomicBool::new(false));
        let handler =
            Gb28181ControlHandler::new(Arc::new(AtomicBool::new(false)), Arc::clone(&flag));
        assert!(!flag.load(Ordering::SeqCst));
        handler.on_force_iframe();
        assert!(flag.load(Ordering::SeqCst));
    }
}
