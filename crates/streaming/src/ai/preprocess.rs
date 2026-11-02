//! JPEG → RGB → resize → normalization preprocessing for AI inference.
//!
//! Byte-for-byte the same normalization semantics as the
//! `mibee-eye-raspi-rs` pipeline (which consumes YUV420 directly): BT.601
//! full-range color, BGR channel order, ImageNet mean/std applied to raw
//! [0, 255] pixel values (no /255 scaling), nearest-neighbor resize, NCHW
//! f32 output. The JPEG decoder's YCbCr → RGB conversion uses the same
//! BT.601 full-range coefficients, so both paths feed the model equivalent
//! inputs.

use anyhow::{Context, Result, bail};

/// Normalization constants for ImageNet-style preprocessing (BGR order,
/// matching the NanoDet training config used by the raspi cameras).
pub const MEAN: [f32; 3] = [103.53, 116.28, 123.675];
pub const STD: [f32; 3] = [57.375, 57.12, 58.395];

/// Convert an RGB8 frame to the model's NCHW f32 input.
///
/// Pipeline: nearest-neighbor resize to `dst_w × dst_h`, then
/// `(pixel - mean[c]) / std[c]` in **BGR** channel order, laid out
/// channel-first (`[B-plane, G-plane, R-plane]`).
///
/// The source frame is stretched into the destination without letterboxing,
/// exactly like the raspi pipeline — x and y therefore carry independent
/// scale factors, which [`super::postprocess::scale_detections_to_frame`]
/// inverts.
pub fn preprocess_rgb8(
    rgb: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Result<Vec<f32>> {
    let (src_w, src_h, dst_w, dst_h) = (
        src_w as usize,
        src_h as usize,
        dst_w as usize,
        dst_h as usize,
    );
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        bail!("preprocess: zero-sized frame ({src_w}x{src_h} → {dst_w}x{dst_h})");
    }
    if rgb.len() < src_w * src_h * 3 {
        bail!(
            "preprocess: RGB frame too short: got {} bytes, need {} ({}x{}x3)",
            rgb.len(),
            src_w * src_h * 3,
            src_w,
            src_h
        );
    }

    let scale_x = src_w as f32 / dst_w as f32;
    let scale_y = src_h as f32 / dst_h as f32;

    let mut output = vec![0.0f32; 3 * dst_w * dst_h];
    let plane = dst_w * dst_h;

    for dy in 0..dst_h {
        let sy = (dy as f32 * scale_y) as usize;
        let row = sy * src_w;
        for dx in 0..dst_w {
            let sx = (dx as f32 * scale_x) as usize;
            let idx = (row + sx) * 3;
            let (r, g, b) = (rgb[idx] as f32, rgb[idx + 1] as f32, rgb[idx + 2] as f32);
            let pixel_idx = dy * dst_w + dx;
            output[pixel_idx] = (b - MEAN[0]) / STD[0];
            output[plane + pixel_idx] = (g - MEAN[1]) / STD[1];
            output[2 * plane + pixel_idx] = (r - MEAN[2]) / STD[2];
        }
    }
    Ok(output)
}

/// Decode a JPEG frame and preprocess it to the model's NCHW f32 input.
///
/// Returns the tensor plus the decoded `(width, height)` — the bbox
/// coordinate space the detections must be scaled back to (SPEC v1 §4.6:
/// video pixel coordinates of the native stream).
pub fn preprocess_jpeg(jpeg: &[u8], dst_w: u32, dst_h: u32) -> Result<(Vec<f32>, u32, u32)> {
    let (rgb, w, h) = decode_jpeg_rgb(jpeg)?;
    let tensor = preprocess_rgb8(&rgb, w, h, dst_w, dst_h)?;
    Ok((tensor, w, h))
}

/// Decode a JPEG to interleaved RGB8 plus its dimensions (shared by the
/// NanoDet and YOLOX preprocess paths).
fn decode_jpeg_rgb(jpeg: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
    let pixels = decoder.decode().context("ai: JPEG decode failed")?;
    let info = decoder
        .info()
        .context("ai: JPEG header missing after decode")?;
    let (w, h) = (info.width as u32, info.height as u32);

    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => pixels,
        jpeg_decoder::PixelFormat::L8 => {
            // Grayscale: replicate the luma channel into RGB.
            let mut rgb = Vec::with_capacity(pixels.len() * 3);
            for &y in &pixels {
                rgb.extend_from_slice(&[y, y, y]);
            }
            rgb
        }
        other => bail!("ai: unsupported JPEG pixel format {other:?}"),
    };
    Ok((rgb, w, h))
}

/// YOLOX-family preprocess (Megvii Apache-2.0 ONNX exports; semantics
/// verified 2026-11-02 against the official `demo/ONNXRuntime`
/// pipeline): **gray-114 letterbox** keeping aspect ratio (image at
/// top-left, padding bottom/right), nearest resize, **BGR** channel
/// order, **raw 0-255 values** (the /255 normalization is baked into
/// the exported graph — dividing again produces a black input), NCHW
/// f32 output.
///
/// Returns `(tensor, frame_w, frame_h, scale)` where `scale` maps
/// frame pixels to model pixels (the letterbox inverse for bboxes).
pub fn preprocess_yolox_jpeg(
    jpeg: &[u8],
    dst_w: u32,
    dst_h: u32,
) -> Result<(Vec<f32>, u32, u32, f32)> {
    let (rgb, src_w, src_h) = decode_jpeg_rgb(jpeg)?;
    let (sw, sh, dw, dh) = (
        src_w as usize,
        src_h as usize,
        dst_w as usize,
        dst_h as usize,
    );
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        bail!("yolox preprocess: zero-sized frame ({sw}x{sh} → {dw}x{dh})");
    }
    let scale = (dw as f32 / sw as f32).min(dh as f32 / sh as f32);
    let (rw, rh) = (
        ((sw as f32 * scale) as usize).min(dw),
        ((sh as f32 * scale) as usize).min(dh),
    );

    // Letterboxed image, HWC BGR, gray-114 padding.
    let mut img = vec![114u8; dw * dh * 3];
    for y in 0..rh {
        // Inverse-scale sample centers, mirroring INTER_LINEAR's
        // half-pixel convention with a nearest pick (cheap and stable).
        let sy =
            (((y as f32 + 0.5) / scale - 0.5).round() as isize).clamp(0, sh as isize - 1) as usize;
        for x in 0..rw {
            let sx = (((x as f32 + 0.5) / scale - 0.5).round() as isize).clamp(0, sw as isize - 1)
                as usize;
            let s = (sy * sw + sx) * 3;
            let d = (y * dw + x) * 3;
            img[d] = rgb[s + 2]; // B
            img[d + 1] = rgb[s + 1]; // G
            img[d + 2] = rgb[s]; // R
        }
    }

    // HWC → NCHW planes, raw 0-255.
    let mut tensor = vec![0f32; 3 * dw * dh];
    for y in 0..dh {
        for x in 0..dw {
            let s = (y * dw + x) * 3;
            tensor[y * dw + x] = img[s] as f32; // B plane
            tensor[dw * dh + y * dw + x] = img[s + 1] as f32; // G
            tensor[2 * dw * dh + y * dw + x] = img[s + 2] as f32; // R
        }
    }
    Ok((tensor, src_w, src_h, scale))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 320×320 grey frame: all pixels convert to RGB (128, 128, 128) and
    /// every output channel equals (128 - mean) / std.
    #[test]
    fn test_normalization_formula() {
        let src = vec![128u8; 320 * 320 * 3];
        let out = preprocess_rgb8(&src, 320, 320, 320, 320).expect("preprocess");
        assert_eq!(out.len(), 307_200);
        let expected_b = (128.0 - MEAN[0]) / STD[0];
        let expected_g = (128.0 - MEAN[1]) / STD[1];
        let expected_r = (128.0 - MEAN[2]) / STD[2];
        assert!((out[0] - expected_b).abs() < 0.001);
        assert!((out[320 * 320] - expected_g).abs() < 0.001);
        assert!((out[2 * 320 * 320] - expected_r).abs() < 0.001);
        // Same cross-check constant as the raspi pipeline: (128-103.53)/57.375.
        assert!((expected_b - 0.4266).abs() < 0.001);
    }

    #[test]
    fn test_channel_order_is_bgr() {
        // Pure red pixel: B and G channels land below their means, R above.
        let src = vec![0u8; 2 * 2 * 3];
        let mut src = src;
        for px in src.as_chunks_mut::<3>().0 {
            px[0] = 255; // R
        }
        let out = preprocess_rgb8(&src, 2, 2, 2, 2).expect("preprocess");
        assert!(out[0] < 0.0, "B plane first, red pixel → negative B");
        assert!(out[4] < 0.0, "G plane second, red pixel → negative G");
        assert!(out[8] > 0.0, "R plane last, red pixel → positive R");
    }

    #[test]
    fn test_short_input_rejected() {
        let err = preprocess_rgb8(&[0u8; 10], 320, 320, 320, 320);
        assert!(err.is_err());
    }

    #[test]
    fn test_nearest_neighbor_picks_top_left_on_downsample() {
        // 2×2 checkerboard → 1×1 must sample the (0,0) pixel (white).
        let mut src = vec![0u8; 2 * 2 * 3];
        for (i, px) in src.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let v = if i == 0 { 255 } else { 0 };
            px.copy_from_slice(&[v, v, v]);
        }
        let out = preprocess_rgb8(&src, 2, 2, 1, 1).expect("preprocess");
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|&v| v > 0.0), "sampled the white pixel");
    }

    #[test]
    fn test_various_sizes() {
        for (sw, sh) in [(640u32, 480u32), (1280, 720), (1920, 1080)] {
            let src = vec![128u8; sw as usize * sh as usize * 3];
            let out = preprocess_rgb8(&src, sw, sh, 320, 320).expect("preprocess");
            assert_eq!(out.len(), 307_200);
        }
    }

    /// End-to-end JPEG path with a real encoded frame: a 16×16 grey JPEG
    /// must decode and normalize to the same values as the raw-RGB path
    /// (within JPEG compression tolerance).
    #[test]
    fn test_preprocess_jpeg_roundtrip() {
        use std::io::Write;
        let (w, h) = (16u16, 16u16);
        let jpeg_bytes: Vec<u8> = {
            struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
            impl Write for Shared {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.0.borrow_mut().extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let buf = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let encoder = jpeg_encoder::Encoder::new(Shared(std::rc::Rc::clone(&buf)), 90);
            encoder
                .encode(
                    &vec![128u8; w as usize * h as usize * 3],
                    w,
                    h,
                    jpeg_encoder::ColorType::Rgb,
                )
                .expect("encode jpeg");
            buf.borrow().clone()
        };

        let (tensor, dw, dh) = preprocess_jpeg(&jpeg_bytes, 320, 320).expect("preprocess jpeg");
        assert_eq!((dw, dh), (16, 16));
        assert_eq!(tensor.len(), 307_200);
        // Grey 128 survives JPEG compression closely (quality 90).
        let expected_b = (128.0 - MEAN[0]) / STD[0];
        assert!(
            (tensor[0] - expected_b).abs() < 0.15,
            "tensor[0]={}, expected≈{expected_b}",
            tensor[0]
        );
    }

    #[test]
    fn test_preprocess_jpeg_rejects_garbage() {
        assert!(preprocess_jpeg(&[1, 2, 3], 320, 320).is_err());
    }
}
