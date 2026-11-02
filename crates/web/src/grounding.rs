//! Live scene grounding for the chat engines (SPEC appendix A #29).
//!
//! The notebook device runs several perception loops independently —
//! NanoDet detection (every ~1 s), VLM alarm descriptions (tens of
//! seconds, only on alarm edges). This module is the cheap junction
//! box: it remembers the latest of each per camera and renders a
//! compact 【画面】 context block that the chat routes prepend to the
//! LLM turns, so "你能看到我吗" is answered from what the cameras
//! actually see instead of a blind language model.
//!
//! Zero new inference is ever triggered from here — recording is fed
//! by the existing loops, and stale entries expire by timestamp.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use streaming::ai::Detection;

/// Detection labels stay useful for a few seconds; the AI loop refreshes
/// them about once per second, so anything older means the loop stopped.
const DETECTION_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct CameraGrounding {
    /// Unix-ms stamp of the detection event that produced `labels`.
    detection_ts_ms: u64,
    /// Label counts rendered at record time (e.g. `2×person, 1×chair`).
    labels: String,
    /// Unix-ms stamp + text of the last VLM alarm description.
    vlm_ts_ms: Option<u64>,
    vlm_text: Option<String>,
    /// Last zone event (name + ts) — recent zone activity feeds the
    /// scene block for multi-dimensional visual judgment.
    zone_ts_ms: Option<u64>,
    zone_text: Option<String>,
    /// Last face-recognition labels (#33) — e.g. `张三×1、未识别×1`.
    face_ts_ms: Option<u64>,
    face_text: Option<String>,
}

/// Shared, lock-guarded per-camera grounding state.
#[derive(Debug, Default)]
pub struct GroundingState {
    cameras: Mutex<HashMap<String, CameraGrounding>>,
}

/// Horizontal position bucket of a bbox center (#30 vision context:
/// lets the model answer WHERE, not just WHAT). `frame_w` 0 → no suffix.
fn position_suffix(d: &Detection, frame_w: u32) -> &'static str {
    if frame_w == 0 {
        return "";
    }
    let center = (d.bbox[0] + d.bbox[2]) as f64 / 2.0 / f64::from(frame_w);
    if center < 0.34 {
        "（左侧）"
    } else if center > 0.66 {
        "（右侧）"
    } else {
        "（中间）"
    }
}

fn render_labels(detections: &[Detection], frame_w: u32) -> String {
    let mut counts: HashMap<String, u32> = HashMap::new();
    for d in detections {
        let key = format!("{}{}", d.label, position_suffix(d, frame_w));
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut parts: Vec<String> = counts
        .into_iter()
        .map(|(label, n)| format!("{n}×{label}"))
        .collect();
    parts.sort();
    parts.join(", ")
}

impl GroundingState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed from the AI detection loop (every event, cheap). `frame_w`
    /// is the capture width for position buckets (0 = skip positions).
    pub fn record_detections(
        &self,
        camera_id: &str,
        ts_ms: u64,
        frame_w: u32,
        detections: &[Detection],
    ) {
        let labels = render_labels(detections, frame_w);
        let mut cams = self.cameras.lock().expect("grounding lock");
        let cam = cams
            .entry(camera_id.to_string())
            .or_insert(CameraGrounding {
                detection_ts_ms: 0,
                labels: String::new(),
                vlm_ts_ms: None,
                vlm_text: None,
                zone_ts_ms: None,
                zone_text: None,
                face_ts_ms: None,
                face_text: None,
            });
        // Only move the timestamp forward — events can arrive out of
        // order across threads.
        if ts_ms >= cam.detection_ts_ms {
            cam.detection_ts_ms = ts_ms;
            cam.labels = labels;
        }
    }

    /// Feed from the VLM alarm-description success path.
    pub fn record_vlm_description(&self, camera_id: &str, ts_ms: u64, text: &str) {
        let mut cams = self.cameras.lock().expect("grounding lock");
        let cam = cams
            .entry(camera_id.to_string())
            .or_insert(CameraGrounding {
                detection_ts_ms: 0,
                labels: String::new(),
                vlm_ts_ms: None,
                vlm_text: None,
                zone_ts_ms: None,
                zone_text: None,
                face_ts_ms: None,
                face_text: None,
            });
        if cam.vlm_ts_ms.is_none_or(|old| ts_ms >= old) {
            cam.vlm_ts_ms = Some(ts_ms);
            cam.vlm_text = Some(text.to_string());
        }
    }

    /// Feed from the face-recognition loop (#33): named/unknown face
    /// labels become part of the scene (「画面人员：张三×1」).
    pub fn record_face_labels(&self, camera_id: &str, ts_ms: u64, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut cams = self.cameras.lock().expect("grounding lock");
        let cam = cams
            .entry(camera_id.to_string())
            .or_insert(CameraGrounding {
                detection_ts_ms: 0,
                labels: String::new(),
                vlm_ts_ms: None,
                vlm_text: None,
                zone_ts_ms: None,
                zone_text: None,
                face_ts_ms: None,
                face_text: None,
            });
        if cam.face_ts_ms.is_none_or(|old| ts_ms >= old) {
            cam.face_ts_ms = Some(ts_ms);
            cam.face_text = Some(text.to_string());
        }
    }

    /// Feed from the zone-event bridge (#30): recent zone triggers
    /// become part of the scene ("区域「门口」1 分钟内有事件").
    pub fn record_zone_event(&self, camera_id: &str, ts_ms: u64, name: &str) {
        let mut cams = self.cameras.lock().expect("grounding lock");
        let cam = cams
            .entry(camera_id.to_string())
            .or_insert(CameraGrounding {
                detection_ts_ms: 0,
                labels: String::new(),
                vlm_ts_ms: None,
                vlm_text: None,
                zone_ts_ms: None,
                zone_text: None,
                face_ts_ms: None,
                face_text: None,
            });
        if cam.zone_ts_ms.is_none_or(|old| ts_ms >= old) {
            cam.zone_ts_ms = Some(ts_ms);
            cam.zone_text = Some(name.to_string());
        }
    }

    /// Render the 【画面】 context block for one camera. `None` when
    /// nothing fresh is known — the caller then omits the block
    /// entirely (`grounded: "none"`).
    #[must_use]
    pub fn scene_summary(&self, camera_id: &str, now_ms: u64) -> Option<String> {
        let cams = self.cameras.lock().expect("grounding lock");
        let cam = cams.get(camera_id)?;
        let mut parts: Vec<String> = Vec::new();
        if now_ms.saturating_sub(cam.detection_ts_ms) <= DETECTION_TTL.as_millis() as u64
            && !cam.labels.is_empty()
        {
            parts.push(format!("实时检测：{}", cam.labels));
        }
        if let (Some(ts), Some(text)) = (cam.vlm_ts_ms, cam.vlm_text.as_deref()) {
            let age_s = now_ms.saturating_sub(ts) / 1000;
            parts.push(format!("画面描述（{age_s} 秒前，可能滞后）：{text}"));
        }
        if let (Some(ts), Some(text)) = (cam.face_ts_ms, cam.face_text.as_deref())
            && now_ms.saturating_sub(ts) <= DETECTION_TTL.as_millis() as u64
        {
            parts.push(format!("画面人员：{text}"));
        }
        if let (Some(ts), Some(text)) = (cam.zone_ts_ms, cam.zone_text.as_deref())
            && now_ms.saturating_sub(ts) <= 60_000
        {
            parts.push(format!("区域「{text}」1 分钟内有事件"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("；"))
        }
    }

    /// Age of the cached VLM describe for a camera. `None` = unknown
    /// camera or never described.
    #[must_use]
    pub fn scene_describe_age_ms(&self, camera_id: &str, now_ms: u64) -> Option<u64> {
        let cams = self.cameras.lock().expect("grounding lock");
        let cam = cams.get(camera_id)?;
        cam.vlm_ts_ms.map(|ts| now_ms.saturating_sub(ts))
    }

    /// True only when a describe exists and is within `max_age` — the
    /// gate for the on-demand fresh look (2026-11-02).
    #[must_use]
    pub fn scene_describe_is_fresh(&self, camera_id: &str, now_ms: u64, max_age: Duration) -> bool {
        self.scene_describe_age_ms(camera_id, now_ms)
            .is_some_and(|age| age <= max_age.as_millis() as u64)
    }
}

/// Trilingual table for "the user is asking about what the camera sees
/// right now". Conservative by design — a false positive only costs one
/// deadline-guarded fresh describe.
const SCENE_QUESTION_MARKERS: &[&str] = &[
    // Mandarin
    "看到",
    "看见",
    "画面",
    "门口",
    "门外",
    "窗外",
    "谁在",
    "有人",
    "屋里",
    "房间",
    "监控",
    "现场",
    "情况怎么样",
    "什么情况",
    // Cantonese
    "睇到",
    "見到",
    "門口",
    // English
    "what do you see",
    "do you see",
    "who is",
    "at the door",
    "camera see",
];

/// Does this utterance ask about the current camera view? (#29
/// freshness gate, 2026-11-02.)
#[must_use]
pub fn is_scene_question(text: &str) -> bool {
    let lower = text.to_lowercase();
    SCENE_QUESTION_MARKERS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

/// Fresh on-demand describe for scene questions (#29 freshness
/// amendment, 2026-11-02).
///
/// Field data: the alarm-driven describe pipeline only runs on alarm
/// edges — in a quiet room the cached 【画面】 text goes arbitrarily
/// stale (a 106 s-old describe answered "你现在看到什么"), and the local
/// 4B VLM takes 60-100 s per describe: too slow for a spoken round and
/// not safely cancellable (blocking llama eval). So the fresh look goes
/// to the configured cloud vision model (auto-selected when an image
/// rides the request) under a hard deadline; any failure returns `None`
/// and the caller answers from the age-labeled cache — fail-open, no
/// regression over the pre-existing behavior.
pub async fn describe_fresh_frame(
    cloud: Option<&crate::cloud::CloudAi>,
    jpeg: &[u8],
    timeout: Duration,
) -> Option<String> {
    let cloud = cloud?;
    let turns = vec![streaming::llm::ChatTurn {
        role: "user".into(),
        content: "用一到两句话客观描述当前监控画面：场景、人物位置与动作。".into(),
    }];
    let (reply, _) = cloud
        .complete_with_usage(&turns, Some(jpeg), None, Some(timeout))
        .await
        .ok()?;
    let trimmed = reply.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Parse the pixel dimensions out of a baseline/progressive JPEG's SOF
/// marker (enough for position buckets — no decode).
#[must_use]
pub fn jpeg_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize; // skip FFD8
    while i + 9 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        // Standalone markers without a length field.
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        if i + 4 > jpeg.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        // SOF0..SOF15 except DHT (C4) / DAC (CC) / RST (C0 covers).
        let is_sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if is_sof {
            if i + 9 <= jpeg.len() {
                let h = u16::from_be_bytes([jpeg[i + 5], jpeg[i + 6]]);
                let w = u16::from_be_bytes([jpeg[i + 7], jpeg[i + 8]]);
                if w > 0 && h > 0 {
                    return Some((u32::from(w), u32::from(h)));
                }
            }
            return None;
        }
        i += 2 + seg_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(label: &str) -> Detection {
        Detection {
            label: label.to_string(),
            confidence: 0.9,
            bbox: [0, 0, 10, 10],
        }
    }

    #[test]
    fn empty_state_has_no_scene() {
        let g = GroundingState::new();
        assert_eq!(g.scene_summary("0", 1000), None);
    }

    #[test]
    fn labels_are_counted_and_sorted() {
        let g = GroundingState::new();
        g.record_detections(
            "0",
            5000,
            640,
            &[det("person"), det("person"), det("chair")],
        );
        let s = g.scene_summary("0", 8000).expect("scene");
        assert!(s.contains("2×person"), "{s}");
        assert!(s.contains("1×chair"), "{s}");
        assert!(s.contains("实时检测"), "{s}");
        // Sorted: chair before person regardless of arrival order.
        let c = s.find("1×chair").expect("chair");
        let p = s.find("2×person").expect("person");
        assert!(c < p);
    }

    #[test]
    fn detections_expire_but_vlm_survives() {
        let g = GroundingState::new();
        g.record_detections("0", 1000, 640, &[det("person")]);
        g.record_vlm_description("0", 1200, "一个人坐在桌前");
        // Fresh: both parts present.
        let s = g.scene_summary("0", 2000).expect("scene");
        assert!(
            s.contains("实时检测") && s.contains("一个人坐在桌前"),
            "{s}"
        );
        // 60 s later: detections expired, VLM description remains with age.
        let s = g.scene_summary("0", 61_200).expect("scene");
        assert!(!s.contains("实时检测"), "{s}");
        assert!(s.contains("60 秒前") && s.contains("一个人坐在桌前"), "{s}");
    }

    #[test]
    fn out_of_order_events_do_not_rewind() {
        let g = GroundingState::new();
        g.record_detections("0", 5000, 640, &[det("person")]);
        g.record_detections("0", 3000, 640, &[det("dog")]); // stale arrival
        let s = g.scene_summary("0", 6000).expect("scene");
        assert!(s.contains("person") && !s.contains("dog"), "{s}");
    }

    #[test]
    fn cameras_are_independent() {
        let g = GroundingState::new();
        g.record_detections("a", 1000, 640, &[det("person")]);
        assert!(g.scene_summary("a", 1500).is_some());
        assert_eq!(g.scene_summary("b", 1500), None);
    }

    #[test]
    fn positions_bucket_by_frame_width() {
        let g = GroundingState::new();
        let mut left = det("person");
        left.bbox = [0, 0, 100, 100];
        let mut right = det("chair");
        right.bbox = [600, 0, 630, 100];
        g.record_detections("0", 1000, 640, &[left, right]);
        let s = g.scene_summary("0", 2000).expect("scene");
        assert!(s.contains("person（左侧）"), "{s}");
        assert!(s.contains("chair（右侧）"), "{s}");
        let g = GroundingState::new();
        g.record_detections("0", 1000, 0, &[det("person")]);
        let s = g.scene_summary("0", 2000).expect("scene");
        assert!(s.contains("1×person") && !s.contains("（"), "{s}");
    }

    #[test]
    fn zone_event_joins_scene_and_expires() {
        let g = GroundingState::new();
        g.record_zone_event("0", 1000, "门口");
        let s = g.scene_summary("0", 5000).expect("scene");
        assert!(s.contains("区域「门口」"), "{s}");
        assert_eq!(g.scene_summary("0", 121_000), None);
    }

    #[test]
    fn jpeg_dimensions_reads_sof() {
        // Hand-built stream: SOI + one filler segment + SOF0 640x480.
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]); // APP0 len 4
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03]);
        assert_eq!(jpeg_dimensions(&jpeg), Some((640, 480)));
        assert_eq!(jpeg_dimensions(&[0x00, 0x01, 0x02]), None);
    }

    #[test]
    fn vlm_only_still_grounds() {
        let g = GroundingState::new();
        g.record_vlm_description("0", 1000, "空房间");
        assert!(g.scene_summary("0", 2000).is_some());
    }

    #[test]
    fn scene_describe_age_and_freshness() {
        let g = GroundingState::new();
        assert_eq!(g.scene_describe_age_ms("0", 1_000), None, "unknown camera");
        g.record_detections("0", 500, 640, &[]); // creates the record, no vlm yet
        assert_eq!(g.scene_describe_age_ms("0", 1_000), None, "never described");
        assert!(!g.scene_describe_is_fresh("0", 1_000, Duration::from_secs(20)));
        g.record_vlm_description("0", 900, "空房间");
        assert_eq!(g.scene_describe_age_ms("0", 1_000), Some(100));
        assert!(g.scene_describe_is_fresh("0", 1_000, Duration::from_secs(20)));
        // The 2026-11-02 incident shape: 106 s old must count as stale.
        assert!(!g.scene_describe_is_fresh("0", 106_900, Duration::from_secs(20)));
    }

    #[test]
    fn scene_question_markers_cover_the_field_phrases() {
        // Phrases measured in the 2026-11-02 session.
        for q in [
            "你现在看到什么",
            "帮我看看门口有人吗",
            "画面里是什么情况",
            "who is at the door",
            "What do you see right now",
        ] {
            assert!(is_scene_question(q), "should match: {q}");
        }
        // Not about the view — no fresh describe is worth its latency.
        for q in [
            "现在几点了",
            "广州天气怎么样",
            "给我讲个故事",
            "你叫什么名字",
            "把客厅的灯打开",
        ] {
            assert!(!is_scene_question(q), "should NOT match: {q}");
        }
    }
}
