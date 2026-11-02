//! ONNX Runtime detector (NanoDet-Plus), loaded dynamically.
//!
//! The `ort` crate is configured with `load-dynamic`, so the binary itself
//! stays portable: at runtime it dlopens `libonnxruntime.so`, searched via
//! the `ORT_DYLIB_PATH` environment variable or the system library paths
//! (same deployment model as the raspi cameras).

use super::postprocess::{
    postprocess, postprocess_yolox, scale_detections_to_frame, scale_yolox_to_frame,
    yolox_side_from_rows,
};
use super::preprocess::{preprocess_jpeg, preprocess_yolox_jpeg};
use super::{AiDetector, Detection};
use anyhow::{Result, bail};
use ort::session::Session;
use ort::value::Tensor;
use std::path::Path;
use std::sync::Mutex;

/// Pre-NMS confidence filter applied inside post-processing. The configured
/// `confidence_threshold` is applied by the worker after `detect()` returns;
/// this lower pre-filter only trims obviously-empty grid points to keep NMS
/// cheap.
const PRE_NMS_CONFIDENCE: f32 = 0.05;

/// Which decode family a loaded ONNX model speaks, auto-detected from
/// the output shape so a model swap is a pure `model_path` change:
/// `[1, N, 85]` = YOLOX raw head (Megvii exports), anything else keeps
/// the NanoDet GFL path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFamily {
    Nanodet,
    Yolox { side_cells: usize },
}

fn detect_family(shape: &[i64]) -> OutputFamily {
    // [batch, rows, 85] — the 85-row signature is unambiguous for the
    // YOLOX COCO head; the row count must also decompose into grid
    // levels (21·side²) or we fall back rather than guess.
    if shape.len() == 3
        && shape[2] == 85
        && shape[0] == 1
        && let Some(side) = yolox_side_from_rows(shape[1].max(0) as usize)
    {
        return OutputFamily::Yolox { side_cells: side };
    }
    OutputFamily::Nanodet
}

/// ONNX Runtime-based AI detector.
#[derive(Debug)]
pub struct OrtDetector {
    /// Path to the ONNX model file (also serves as the model identifier).
    model_path: String,
    /// ONNX Runtime session. Wrapped in a mutex because `run()` takes
    /// `&mut self`.
    session: Mutex<Session>,
    /// Name of the model's input tensor.
    input_name: String,
    /// Name of the model's output tensor.
    output_name: String,
    /// Model input width (read from the session's input shape).
    input_width: u32,
    /// Model input height (read from the session's input shape).
    input_height: u32,
    /// Output decode family (auto-detected at load; see [`detect_family`]).
    family: OutputFamily,
}

impl OrtDetector {
    /// Create a detector from an ONNX model file.
    ///
    /// # Errors
    ///
    /// Fails if the model file is missing, the ONNX Runtime library cannot
    /// be loaded, or the model's input/output metadata is unusable. Callers
    /// treat this as "AI disabled" (fail-open), never fatal.
    pub fn new(model_path: &str) -> Result<Self> {
        if !Path::new(model_path).exists() {
            bail!("ONNX model file not found: {model_path}");
        }

        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create ONNX session builder: {e}"))?
            .with_intra_threads(2)
            .map_err(|e| anyhow::anyhow!("failed to set intra-op threads: {e}"))?
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("failed to load ONNX model {model_path}: {e}"))?;

        let input = session
            .inputs()
            .first()
            .ok_or_else(|| anyhow::anyhow!("ONNX model has no inputs"))?;
        let output = session
            .outputs()
            .first()
            .ok_or_else(|| anyhow::anyhow!("ONNX model has no outputs"))?;

        let input_name = input.name().to_string();
        let output_name = output.name().to_string();

        // NCHW layout: [batch, channels, height, width].
        let shape = input
            .dtype()
            .tensor_shape()
            .ok_or_else(|| anyhow::anyhow!("ONNX model input is not a tensor"))?;
        let input_height = shape.get(2).copied().unwrap_or(-1);
        let input_width = shape.get(3).copied().unwrap_or(-1);
        if input_height <= 0 || input_width <= 0 {
            bail!("ONNX model input shape has dynamic dimensions: {shape:?}");
        }

        let out_shape = output
            .dtype()
            .tensor_shape()
            .ok_or_else(|| anyhow::anyhow!("ONNX model output is not a tensor"))?;
        let family = detect_family(out_shape);

        Ok(Self {
            model_path: model_path.to_string(),
            session: Mutex::new(session),
            input_name,
            output_name,
            input_width: input_width as u32,
            input_height: input_height as u32,
            family,
        })
    }
}

impl AiDetector for OrtDetector {
    fn detect(&self, jpeg: &[u8]) -> Result<Vec<Detection>> {
        match self.family {
            OutputFamily::Yolox { side_cells } => self.detect_yolox(jpeg, side_cells),
            OutputFamily::Nanodet => self.detect_nanodet(jpeg),
        }
    }

    fn model_name(&self) -> &str {
        &self.model_path
    }
}

impl OrtDetector {
    /// The YOLOX path: letterbox preprocess, raw-head grid decode,
    /// letterbox-inverse scaling.
    fn detect_yolox(&self, jpeg: &[u8], side_cells: usize) -> Result<Vec<Detection>> {
        let (input, frame_w, frame_h, scale) =
            preprocess_yolox_jpeg(jpeg, self.input_width, self.input_height)?;
        let shape = vec![1_i64, 3, self.input_height as i64, self.input_width as i64];
        let tensor = Tensor::from_array((shape, input))
            .map_err(|e| anyhow::anyhow!("failed to build input tensor: {e}"))?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("ONNX session mutex poisoned"))?;
        let outputs = session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .map_err(|e| anyhow::anyhow!("ONNX inference error: {e}"))?;
        let output = outputs
            .get(&self.output_name)
            .ok_or_else(|| anyhow::anyhow!("ONNX model produced no outputs"))?;
        let (_tensor_shape, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract output tensor: {e}"))?;
        let detections = postprocess_yolox(data, side_cells, PRE_NMS_CONFIDENCE)?;
        Ok(scale_yolox_to_frame(detections, scale, frame_w, frame_h))
    }

    /// The NanoDet-Plus path (historic default).
    fn detect_nanodet(&self, jpeg: &[u8]) -> Result<Vec<Detection>> {
        // 1. JPEG → resized + normalized NCHW f32 (plus the frame's native
        //    dimensions — the bbox output coordinate space, SPEC v1 §4.6).
        let (input, frame_w, frame_h) = preprocess_jpeg(jpeg, self.input_width, self.input_height)?;

        // 2. Build the input tensor.
        let shape = vec![1_i64, 3, self.input_height as i64, self.input_width as i64];
        let tensor = Tensor::from_array((shape, input))
            .map_err(|e| anyhow::anyhow!("failed to build input tensor: {e}"))?;

        // 3. Run inference under the session lock.
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("ONNX session mutex poisoned"))?;
        let outputs = session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .map_err(|e| anyhow::anyhow!("ONNX inference error: {e}"))?;
        let output = outputs
            .get(&self.output_name)
            .ok_or_else(|| anyhow::anyhow!("ONNX model produced no outputs"))?;

        // 4. Extract the flat f32 tensor and run NanoDet GFL post-processing.
        let (_tensor_shape, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract output tensor: {e}"))?;

        // 5. Map bboxes from model-input pixels back to video-frame pixels.
        let detections = postprocess(data, PRE_NMS_CONFIDENCE)?;
        Ok(scale_detections_to_frame(
            detections,
            self.input_width,
            self.input_height,
            frame_w,
            frame_h,
        ))
    }
}

impl OrtDetector {
    /// Square model input size in pixels (registry metadata, SPEC §4.6).
    pub fn input_size(&self) -> u32 {
        self.input_width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constructor_error_missing_file() {
        let result = OrtDetector::new("nonexistent_model.onnx");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn family_detection_by_output_shape() {
        // YOLOX 416 (3549 rows) and 640 (8400) exports decode to
        // side 13 / 20; anything else stays on the NanoDet path.
        assert_eq!(
            detect_family(&[1, 3549, 85]),
            OutputFamily::Yolox { side_cells: 13 }
        );
        assert_eq!(
            detect_family(&[1, 8400, 85]),
            OutputFamily::Yolox { side_cells: 20 }
        );
        assert_eq!(detect_family(&[1, 2125, 112]), OutputFamily::Nanodet);
        assert_eq!(
            detect_family(&[1, 3550, 85]),
            OutputFamily::Nanodet,
            "row count must decompose into grid levels"
        );
        assert_eq!(detect_family(&[1, 3549]), OutputFamily::Nanodet);
    }

    /// Real-model diagnostic (#[ignore] by default — needs the YOLOX
    /// ONNX + ORT dylib, deployment assets CI does not have). Run with:
    /// `ORT_DYLIB_PATH=… cargo test -p streaming yolox_real -- --ignored`
    /// with `YOLOX_MODEL`/`YOLOX_JPEG` env vars pointing at a model and
    /// a known-positive frame (the official demo assets/dog.jpg works:
    /// expects class 16 "dog" ≥ 0.6).
    #[test]
    #[ignore]
    fn yolox_real_model_detects_the_official_dog() {
        let model = std::env::var("YOLOX_MODEL").expect("YOLOX_MODEL env");
        let jpeg_path = std::env::var("YOLOX_JPEG").expect("YOLOX_JPEG env");
        let jpeg = std::fs::read(&jpeg_path).expect("read jpeg");
        let d = OrtDetector::new(&model).expect("load model");
        let dets = d.detect(&jpeg).expect("detect");
        let dog = dets
            .iter()
            .find(|x| x.label == "dog")
            .unwrap_or_else(|| panic!("no dog in {dets:?}"));
        assert!(dog.confidence >= 0.6, "dog {dog:?}");
    }
}
