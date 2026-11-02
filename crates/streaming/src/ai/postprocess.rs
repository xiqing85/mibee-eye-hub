//! GFL (Generalized Focal Loss) post-processing for NanoDet ONNX models.
//!
//! Ported from `mibee-eye-raspi-rs` (same model, same semantics) so all
//! MiBee cameras decode identically:
//!
//! 1. **Classification** — the first `NUM_CLASSES` channels per point are
//!    class scores (the `nanodet-plus-m_320.onnx` export folds the sigmoid
//!    into the graph); argmax picks the label.
//! 2. **GFL regression** — the remaining `4 × (REG_MAX + 1)` channels hold a
//!    discrete distance distribution per box side (l, t, r, b); the expected
//!    distance is `dot(softmax(bins), [0..=REG_MAX])` in stride units.
//! 3. **Bbox** — `x1 = (grid_x - l) * stride`, … clamped to the input frame.
//! 4. **NMS** — per-class non-maximum suppression at IoU 0.5.
//!
//! Model output layout: `[1, 2125, 112]` (point-major) — 2125 points =
//! 40² + 20² + 10² + 5² at strides [8, 16, 32, 64]; 112 channels = 80 COCO
//! classes + 32 regression.

use super::Detection;
use anyhow::{Result, bail};

/// Number of COCO object classes.
const NUM_CLASSES: usize = 80;
/// Maximum bin index of the GFL regression distribution (8 bins: 0..=7).
const REG_MAX: usize = 7;
/// Number of distance bins per box side.
const NUM_BINS: usize = REG_MAX + 1;
/// Regression channels: 4 sides × 8 bins.
const NUM_REGRESSION: usize = 4 * NUM_BINS;
/// Total channels per grid point.
const NUM_CHANNELS: usize = NUM_CLASSES + NUM_REGRESSION;
/// Model input size (square, 320×320).
const INPUT_SIZE: u32 = 320;
/// Feature-map strides of the FPN levels.
const STRIDES: [u32; 4] = [8, 16, 32, 64];
/// Feature-map width/height per stride level (320 / stride).
const GRID_SIZES: [usize; 4] = [40, 20, 10, 5];
/// Cumulative point offset of each stride level.
const LEVEL_OFFSETS: [usize; 4] = [0, 1600, 2000, 2100];
/// Total number of grid points across all levels.
const NUM_POINTS: usize = 2125;
/// NMS intersection-over-union threshold.
const NMS_IOU_THRESHOLD: f32 = 0.5;

/// COCO 80-class labels in model output order (index = class id).
const COCO_LABELS: [&str; NUM_CLASSES] = [
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

/// A decoded detection candidate before NMS.
#[derive(Debug, Clone)]
struct Candidate {
    label: usize,
    confidence: f32,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

/// Decode the ONNX output tensor into detections.
///
/// `output` is the flat `[1, 2125, 112]` tensor data; `confidence_threshold`
/// is the pre-NMS filter applied to the (already sigmoid'd) class scores.
pub fn postprocess(output: &[f32], confidence_threshold: f32) -> Result<Vec<Detection>> {
    if output.len() != NUM_POINTS * NUM_CHANNELS {
        bail!(
            "Unexpected ONNX output length: got {}, expected {} ({} points × {} channels)",
            output.len(),
            NUM_POINTS * NUM_CHANNELS,
            NUM_POINTS,
            NUM_CHANNELS
        );
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for (point_idx, point) in output.as_chunks::<NUM_CHANNELS>().0.iter().enumerate() {
        let (_level, stride, grid_x, grid_y) = grid_coords(point_idx);

        // Classification: argmax over the class channels.
        let mut label = 0usize;
        let mut confidence = point[0];
        for (class, &score) in point[..NUM_CLASSES].iter().enumerate().skip(1) {
            if score > confidence {
                label = class;
                confidence = score;
            }
        }
        if confidence < confidence_threshold {
            continue;
        }

        let distances = decode_distances(&point[NUM_CLASSES..]);

        let x1 = (grid_x as f32 - distances[0]) * stride as f32;
        let y1 = (grid_y as f32 - distances[1]) * stride as f32;
        let x2 = (grid_x as f32 + distances[2]) * stride as f32;
        let y2 = (grid_y as f32 + distances[3]) * stride as f32;

        candidates.push(Candidate {
            label,
            confidence,
            x1: x1.max(0.0),
            y1: y1.max(0.0),
            x2: x2.min(INPUT_SIZE as f32),
            y2: y2.min(INPUT_SIZE as f32),
        });
    }

    candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let kept = nms(&candidates, NMS_IOU_THRESHOLD);

    Ok(kept
        .into_iter()
        .map(|c| Detection {
            label: COCO_LABELS[c.label].to_string(),
            confidence: c.confidence,
            bbox: [
                c.x1.round() as u32,
                c.y1.round() as u32,
                (c.x2 - c.x1).round() as u32,
                (c.y2 - c.y1).round() as u32,
            ],
        })
        .collect())
}

/// YOLOX row width: 4 box (dx, dy, log_w, log_h) + 1 objectness + 80
/// COCO classes (same class order as [`COCO_LABELS`]).
const YOLOX_ROW: usize = 85;

/// Grid cells at stride 32 implied by a YOLOX row count. The three
/// pyramid levels contribute `21·side²` cells total: `(4s)² + (2s)² + s²`
/// with strides 8/16/32. 416-input exports → 3549 rows (side 13),
/// 640-input → 8400 (side 20).
#[must_use]
pub fn yolox_side_from_rows(rows: usize) -> Option<usize> {
    let side2 = rows / 21;
    if side2 == 0 || !rows.is_multiple_of(21) {
        return None;
    }
    let side = (side2 as f64).sqrt().round() as usize;
    (side > 0 && 21 * side * side == rows).then_some(side)
}

/// Decode a YOLOX-family output into detections.
///
/// The Megvii ONNX exports ship the **raw head output**: rows are grid
/// cells (stride 8/16/32 levels, row-major within a level) carrying
/// `[dx, dy, log_w, log_h, obj(sigmoid), 80×cls(sigmoid)]` — the
/// official demo decodes with `cx=(dx+gx)·stride`, `w=exp(log_w)·stride`
/// (`yolox/utils/demo_utils.py::demo_postprocess`), and this port does
/// the same, then reuses the greedy per-label NMS below.
///
/// `confidence_threshold` gates the final `obj × cls` score; bboxes are
/// clamped to the square model-input space (the letterbox inverse is
/// [`scale_yolox_to_frame`], applied by the caller).
pub fn postprocess_yolox(
    output: &[f32],
    side_cells: usize,
    confidence_threshold: f32,
) -> Result<Vec<Detection>> {
    let rows = 21 * side_cells * side_cells;
    if output.len() != rows * YOLOX_ROW {
        bail!(
            "Unexpected YOLOX output length: got {}, expected {} ({} rows × {})",
            output.len(),
            rows * YOLOX_ROW,
            rows,
            YOLOX_ROW
        );
    }
    let input_px = (side_cells * 32) as f32;
    let levels = [
        (side_cells * 4, 8usize),
        (side_cells * 2, 16),
        (side_cells, 32),
    ];
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut row = 0usize;
    for &(grid, stride) in &levels {
        for gy in 0..grid {
            for gx in 0..grid {
                let base = row * YOLOX_ROW;
                row += 1;
                let obj = output[base + 4];
                if obj < confidence_threshold {
                    continue; // score = obj × cls ≤ obj
                }
                let mut label = 0usize;
                let mut best = output[base + 5];
                for (class, &s) in output[base + 5..base + YOLOX_ROW]
                    .iter()
                    .enumerate()
                    .skip(1)
                {
                    if s > best {
                        best = s;
                        label = class;
                    }
                }
                let score = obj * best;
                if score < confidence_threshold {
                    continue;
                }
                let cx = (output[base] + gx as f32) * stride as f32;
                let cy = (output[base + 1] + gy as f32) * stride as f32;
                let w = output[base + 2].exp() * stride as f32;
                let h = output[base + 3].exp() * stride as f32;
                candidates.push(Candidate {
                    label,
                    confidence: score,
                    x1: (cx - w / 2.0).max(0.0),
                    y1: (cy - h / 2.0).max(0.0),
                    x2: (cx + w / 2.0).min(input_px),
                    y2: (cy + h / 2.0).min(input_px),
                });
            }
        }
    }
    candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let kept = nms(&candidates, NMS_IOU_THRESHOLD);
    Ok(kept
        .into_iter()
        .map(|c| Detection {
            label: COCO_LABELS[c.label].to_string(),
            confidence: c.confidence,
            bbox: [
                c.x1.round() as u32,
                c.y1.round() as u32,
                (c.x2 - c.x1).round() as u32,
                (c.y2 - c.y1).round() as u32,
            ],
        })
        .collect())
}

/// Invert the YOLOX letterbox: model-pixel bboxes → frame pixels.
/// Padding sits bottom/right only, so `frame = model / scale` with a
/// clamp; boxes stay inside the frame like
/// [`scale_detections_to_frame`].
#[must_use]
pub fn scale_yolox_to_frame(
    detections: Vec<Detection>,
    scale: f32,
    frame_w: u32,
    frame_h: u32,
) -> Vec<Detection> {
    if scale <= 0.0 || frame_w == 0 || frame_h == 0 {
        return detections;
    }
    detections
        .into_iter()
        .map(|mut det| {
            let [x, y, w, h] = det.bbox;
            let x = ((x as f32 / scale).round() as u32).min(frame_w.saturating_sub(1));
            let y = ((y as f32 / scale).round() as u32).min(frame_h.saturating_sub(1));
            let w = ((w as f32 / scale).round() as u32).min(frame_w - x);
            let h = ((h as f32 / scale).round() as u32).min(frame_h - y);
            det.bbox = [x, y, w, h];
            det
        })
        .collect()
}

/// Scale detections from model-input pixel space back to the source video
/// frame's pixel space (SPEC v1 §4.6: bboxes are in **video pixel**
/// coordinates). The preprocessing stretches the frame into the square
/// model input without letterboxing, so x and y scale independently.
/// Boxes are clamped to the frame bounds so rounding cannot push an edge
/// one pixel outside.
pub fn scale_detections_to_frame(
    detections: Vec<Detection>,
    model_w: u32,
    model_h: u32,
    frame_w: u32,
    frame_h: u32,
) -> Vec<Detection> {
    if model_w == 0 || model_h == 0 || frame_w == 0 || frame_h == 0 {
        return detections;
    }
    let scale_x = frame_w as f32 / model_w as f32;
    let scale_y = frame_h as f32 / model_h as f32;
    detections
        .into_iter()
        .map(|mut det| {
            let [x, y, w, h] = det.bbox;
            let x = ((x as f32 * scale_x).round() as u32).min(frame_w.saturating_sub(1));
            let y = ((y as f32 * scale_y).round() as u32).min(frame_h.saturating_sub(1));
            let w = ((w as f32 * scale_x).round() as u32).min(frame_w - x);
            let h = ((h as f32 * scale_y).round() as u32).min(frame_h - y);
            det.bbox = [x, y, w, h];
            det
        })
        .collect()
}

/// Map a flat point index to its (level, stride, grid_x, grid_y).
///
/// Points are ordered level-major (stride 8 first) and row-major within
/// each level (y outer, x inner), matching NanoDet's
/// `generate_grid_center_priors`.
fn grid_coords(point_idx: usize) -> (usize, u32, usize, usize) {
    let level = LEVEL_OFFSETS
        .iter()
        .rposition(|&offset| offset <= point_idx)
        .unwrap_or(0);
    let offset = LEVEL_OFFSETS[level];
    let local = point_idx - offset;
    let grid_w = GRID_SIZES[level];
    (level, STRIDES[level], local % grid_w, local / grid_w)
}

/// Decode the 4 box-side distances (l, t, r, b) from the regression
/// channels: each side's bins form a discrete distribution over distances
/// `0..=REG_MAX` (stride units); the expected distance is the
/// softmax-weighted sum.
fn decode_distances(regression: &[f32]) -> [f32; 4] {
    let mut distances = [0.0f32; 4];
    for (side, distance) in distances.iter_mut().enumerate() {
        let bins = &regression[side * NUM_BINS..(side + 1) * NUM_BINS];
        let weights = softmax(bins);
        *distance = weights
            .iter()
            .enumerate()
            .map(|(bin, &weight)| bin as f32 * weight)
            .sum();
    }
    distances
}

/// Numerically stable softmax over a slice.
fn softmax(values: &[f32]) -> Vec<f32> {
    let max = values.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = values.iter().map(|&v| (v - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.iter().map(|&e| e / sum).collect()
}

/// Per-class non-maximum suppression.
///
/// Candidates must be sorted by confidence descending. A candidate is kept
/// if its IoU with every already-kept candidate of the same class is ≤
/// threshold.
fn nms(candidates: &[Candidate], iou_threshold: f32) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        let suppressed = kept
            .iter()
            .any(|k| k.label == candidate.label && iou(k, candidate) > iou_threshold);
        if !suppressed {
            kept.push(candidate.clone());
        }
    }
    kept
}

/// Intersection-over-union of two boxes.
fn iou(a: &Candidate, b: &Candidate) -> f32 {
    let inter_w = a.x2.min(b.x2) - a.x1.max(b.x1);
    let inter_h = a.y2.min(b.y2) - a.y1.max(b.y1);
    if inter_w <= 0.0 || inter_h <= 0.0 {
        return 0.0;
    }
    let inter = inter_w * inter_h;
    let union = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - inter;
    inter / union
}

// ── Tests (ported from mibee-eye-raspi-rs — same model, same numbers) ────────

#[cfg(test)]
mod tests {
    use super::*;

    mod yolox {
        use super::*;

        /// One hot cell in a synthetic 416-input tensor (side 13, 3549
        /// rows): stride-8 level, grid (10, 20), a 32×32 box centered
        /// there. Verifies the grid/stride/exp decode and the letterbox
        /// inverse end to end.
        #[test]
        fn decode_and_scale_one_hot_cell() {
            let side = 13;
            let rows = 21 * side * side;
            let mut tensor = vec![0f32; rows * YOLOX_ROW];
            // Row index: level stride 8 first → grid (gx=10, gy=20) at
            // 20*52 + 10.
            let row = 20 * 52 + 10;
            let base = row * YOLOX_ROW;
            tensor[base] = 0.0; // dx → cx = (0+10)*8 = 80
            tensor[base + 1] = 0.0; // dy → cy = (0+20)*8 = 160
            tensor[base + 2] = 2f32.ln(); // log w → w = exp(ln2)*8 = 16
            tensor[base + 3] = 2f32.ln(); // h = 16
            tensor[base + 4] = 0.9; // obj
            tensor[base + 5 + 16] = 0.9; // cls 16 = dog
            let dets = postprocess_yolox(&tensor, side, 0.5).expect("decode");
            assert_eq!(dets.len(), 1);
            let d = &dets[0];
            assert_eq!(d.label, "dog");
            assert!((d.confidence - 0.81).abs() < 1e-6, "{:?}", d.confidence);
            // Model-space xywh: center (80,160), 16×16 → (72,152,16,16).
            assert_eq!(d.bbox, [72, 152, 16, 16]);
            // Letterbox inverse: 1280×720 frame into 416×416 → scale
            // = min(416/1280, 416/720) = 0.325; model px / 0.325.
            let scaled = scale_yolox_to_frame(vec![d.clone()], 416.0 / 1280.0, 1280, 720);
            let s = &scaled[0];
            // 72/0.325 ≈ 221.5 → 222 (round); 152/0.325 ≈ 467.7 → 468.
            assert_eq!(s.bbox[0], 222);
            assert_eq!(s.bbox[1], 468);
        }

        /// Duplicate overlapping cells collapse under NMS; a far cell
        /// survives.
        #[test]
        fn nms_collapses_overlapping_cells() {
            let side = 13;
            let rows = 21 * side * side;
            let mut tensor = vec![0f32; rows * YOLOX_ROW];
            for (row, conf) in [(100, 0.9f32), (101, 0.8), (3000, 0.7)] {
                let base = row * YOLOX_ROW;
                tensor[base] = 0.5;
                tensor[base + 1] = 0.5;
                tensor[base + 2] = 2f32.ln();
                tensor[base + 3] = 2f32.ln();
                tensor[base + 4] = conf;
                tensor[base + 5] = conf; // class 0 person
            }
            // Rows 100/101 are adjacent grid cells (stride-8 level,
            // gx 48→(48,1) vs 49→(49,1)): same object, near-identical
            // boxes → NMS keeps the stronger. Row 3000 sits in the
            // stride-16 level, far away → kept.
            let dets = postprocess_yolox(&tensor, side, 0.5).expect("decode");
            assert_eq!(dets.len(), 2, "{dets:?}");
            assert!(dets.iter().all(|d| d.label == "person"));
        }

        #[test]
        fn side_from_rows_round_trips_both_exports() {
            assert_eq!(yolox_side_from_rows(3549), Some(13)); // 416
            assert_eq!(yolox_side_from_rows(8400), Some(20)); // 640
            assert_eq!(yolox_side_from_rows(3548), None);
            assert_eq!(yolox_side_from_rows(21), Some(1)); // 32px input: legal decomposition
        }

        #[test]
        fn wrong_length_rejected() {
            assert!(postprocess_yolox(&[0f32; 100], 13, 0.5).is_err());
        }
    }

    mod scale_to_frame {
        use super::*;

        /// Real-world case: 1280×720 frame stretched into a 320×320 model
        /// input (x scale 4.0, y scale 2.25).
        #[test]
        fn test_maps_model_box_to_video_pixels() {
            let dets = vec![Detection {
                label: "chair".to_string(),
                confidence: 0.65,
                bbox: [272, 272, 48, 48],
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 1280, 720);
            assert_eq!(scaled[0].bbox, [1088, 612, 192, 108]);
        }

        #[test]
        fn test_identity_when_sizes_match() {
            let dets = vec![Detection {
                label: "person".to_string(),
                confidence: 0.9,
                bbox: [10, 20, 30, 40],
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 320, 320);
            assert_eq!(scaled[0].bbox, [10, 20, 30, 40]);
        }

        #[test]
        fn test_clamps_to_frame_bounds() {
            let dets = vec![Detection {
                label: "person".to_string(),
                confidence: 0.9,
                bbox: [300, 300, 30, 30],
            }];
            let scaled = scale_detections_to_frame(dets, 320, 320, 1280, 720);
            let [x, y, w, h] = scaled[0].bbox;
            assert_eq!((x, y), (1200, 675));
            assert!(y + h <= 720, "y+h={}", y + h);
            assert!(x + w <= 1280, "x+w={}", x + w);
        }
    }

    /// Class 0 ("person") at grid (x=20, y=20) on the stride-8 level
    /// (point 820), one-hot regression at distances (2, 3, 4, 5) → bbox
    /// (144, 136, 192, 200).
    fn synthetic_output_with_detection() -> Vec<f32> {
        let mut output = vec![0.0f32; NUM_POINTS * NUM_CHANNELS];
        let base = (20 * 40 + 20) * NUM_CHANNELS;
        output[base] = 0.9;
        for class in 1..NUM_CLASSES {
            output[base + class] = 0.01;
        }
        for (side, distance) in [2usize, 3, 4, 5].iter().enumerate() {
            output[base + NUM_CLASSES + side * NUM_BINS + distance] = 20.0;
        }
        output
    }

    #[test]
    fn test_synthetic_detection_decodes_bbox_within_2px() {
        let output = synthetic_output_with_detection();
        let detections = postprocess(&output, 0.4).expect("postprocess");
        assert_eq!(detections.len(), 1);
        let detection = &detections[0];
        assert_eq!(detection.label, "person");
        assert!((detection.confidence - 0.9).abs() < 1e-6);
        let [x, y, w, h] = detection.bbox;
        assert!((x as i32 - 144).abs() <= 2, "x={x}");
        assert!((y as i32 - 136).abs() <= 2, "y={y}");
        assert!((w as i32 - 48).abs() <= 2, "w={w}");
        assert!((h as i32 - 64).abs() <= 2, "h={h}");
    }

    #[test]
    fn test_all_zero_output_yields_no_detections() {
        let output = vec![0.0f32; NUM_POINTS * NUM_CHANNELS];
        assert!(postprocess(&output, 0.4).expect("postprocess").is_empty());
    }

    #[test]
    fn test_short_output_returns_error() {
        let output = vec![0.0f32; NUM_POINTS * NUM_CHANNELS - 1];
        let err = postprocess(&output, 0.4);
        assert!(err.is_err());
    }

    #[test]
    fn test_grid_coords() {
        assert_eq!(grid_coords(0), (0, 8, 0, 0));
        assert_eq!(grid_coords(820), (0, 8, 20, 20));
        assert_eq!(grid_coords(1599), (0, 8, 39, 39));
        assert_eq!(grid_coords(1600), (1, 16, 0, 0));
        assert_eq!(grid_coords(2000), (2, 32, 0, 0));
        assert_eq!(grid_coords(2100), (3, 64, 0, 0));
        assert_eq!(grid_coords(2124), (3, 64, 4, 4));
    }

    #[test]
    fn test_nms_suppresses_same_class_overlap() {
        let candidates = vec![
            Candidate {
                label: 0,
                confidence: 0.9,
                x1: 10.0,
                y1: 10.0,
                x2: 100.0,
                y2: 100.0,
            },
            Candidate {
                label: 0,
                confidence: 0.8,
                x1: 20.0,
                y1: 20.0,
                x2: 110.0,
                y2: 110.0,
            },
        ];
        let kept = nms(&candidates, 0.5);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].confidence, 0.9);
    }

    #[test]
    fn test_nms_keeps_different_classes() {
        let candidates = vec![
            Candidate {
                label: 0,
                confidence: 0.9,
                x1: 10.0,
                y1: 10.0,
                x2: 100.0,
                y2: 100.0,
            },
            Candidate {
                label: 2,
                confidence: 0.8,
                x1: 20.0,
                y1: 20.0,
                x2: 110.0,
                y2: 110.0,
            },
        ];
        let kept = nms(&candidates, 0.5);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn test_coco_labels() {
        assert_eq!(COCO_LABELS.len(), 80);
        assert_eq!(COCO_LABELS[0], "person");
        assert_eq!(COCO_LABELS[2], "car");
        assert_eq!(COCO_LABELS[16], "dog");
        assert_eq!(COCO_LABELS[79], "toothbrush");
    }
}
