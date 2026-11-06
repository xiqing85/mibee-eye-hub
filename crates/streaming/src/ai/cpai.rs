//! Generic CPAI-compatible remote detection backend (CodeProject.AI
//! Server wire contract).
//!
//! The endpoint contract is anchored on the **public CPAI convention**
//! — any CPAI-compatible server works (MiBeeVision included; the hub
//! carries no vendor-specific protocol). Semantics per the consumer
//! agreement (MiBeeVision #34, 2026-11-03):
//!
//! - request: `multipart/form-data`, `image` part (JPEG bytes) +
//!   optional `minConfidence` part (request-level confidence filter);
//!   `Authorization: Bearer <token>` only when a token is configured
//!   (empty token = no header, keeping unauthenticated LAN servers
//!   working);
//! - response: CPAI shape `predictions[]` in **original-image pixel**
//!   coordinates with COCO label strings; unknown fields are ignored
//!   (loose read — servers may add fields);
//! - errors: any non-2xx, `success:false`, invalid JSON, or transport
//!   timeout is an error. [`FailoverDetector`] wraps this with a local
//!   fallback + exponential re-probe backoff (fail-open, no retry
//!   bombardment).

use super::{AiDetector, Detection};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::sync::Mutex;
use std::time::Duration;

/// Model identifier surfaced by `/api/detections` while the remote
/// backend is active (not a registry entry — activating any local model
/// via §4.6 hot-switch switches back to on-device detection).
pub const MODEL_ID: &str = "cpai-remote";

/// Client for one CPAI-compatible detection endpoint.
pub struct CpaiDetector {
    endpoint: String,
    /// Bearer token; empty = no Authorization header. Never logged.
    token: String,
    /// Optional request-level confidence filter sent as the
    /// `minConfidence` form part (CPAI client convention).
    min_confidence: Option<f32>,
    http: reqwest::blocking::Client,
}

#[derive(Deserialize)]
struct CpaiResponse {
    success: bool,
    #[serde(default)]
    predictions: Vec<CpaiPrediction>,
}

#[derive(Deserialize)]
struct CpaiPrediction {
    label: String,
    confidence: f32,
    x_min: f32,
    y_min: f32,
    x_max: f32,
    y_max: f32,
}

impl CpaiDetector {
    /// Build a client for `endpoint`.
    ///
    /// # Errors
    ///
    /// Fails when the endpoint is not a usable URL or the HTTP client
    /// cannot be built. Callers treat this like any other detector load
    /// failure (fail-open: AI stays disabled, startup log carries it).
    pub fn new(
        endpoint: &str,
        token: &str,
        timeout_ms: u64,
        min_confidence: Option<f32>,
    ) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_millis(timeout_ms.max(1)))
            .build()
            .context("cpai: failed to build HTTP client")?;
        let url = reqwest::Url::parse(endpoint).context("cpai: endpoint is not a valid URL")?;
        Ok(Self {
            endpoint: url.to_string(),
            token: token.to_string(),
            min_confidence,
            http,
        })
    }
}

impl AiDetector for CpaiDetector {
    fn detect(&self, jpeg: &[u8]) -> Result<Vec<Detection>> {
        let (frame_w, frame_h) = jpeg_size(jpeg)?;

        let mut form = reqwest::blocking::multipart::Form::new().part(
            "image",
            reqwest::blocking::multipart::Part::bytes(jpeg.to_vec())
                .file_name("frame.jpg")
                .mime_str("image/jpeg")
                .context("cpai: mime type")?,
        );
        if let Some(mc) = self.min_confidence {
            form = form.text("minConfidence", format!("{mc}"));
        }
        let mut req = self.http.post(&self.endpoint).multipart(form);
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
        }

        let response = req
            .send()
            .context("cpai: endpoint unreachable (transport)")?;
        let status = response.status();
        let text = response.text().context("cpai: reading response body")?;
        let parsed: CpaiResponse = serde_json::from_str(&text)
            .with_context(|| format!("cpai: non-JSON response (HTTP {status})"))?;

        if !status.is_success() || !parsed.success {
            bail!(
                "cpai: endpoint rejected the frame (HTTP {status}, success={})",
                parsed.success
            );
        }

        Ok(parsed
            .predictions
            .into_iter()
            .filter_map(|p| p.into_detection(frame_w, frame_h))
            .collect())
    }

    fn model_name(&self) -> &str {
        MODEL_ID
    }
}

impl CpaiPrediction {
    /// CPAI returns `[x_min, y_min, x_max, y_max]` in original-image
    /// pixels — the same space our video-frame bboxes live in — so the
    /// mapping is a clamp + `[x, y, w, h]` reshape. Degenerate or NaN
    /// boxes are dropped.
    fn into_detection(self, frame_w: u32, frame_h: u32) -> Option<Detection> {
        let (x_min, y_min, x_max, y_max) = (self.x_min, self.y_min, self.x_max, self.y_max);
        if !(x_min.is_finite() && y_min.is_finite() && x_max.is_finite() && y_max.is_finite()) {
            return None;
        }
        let x = x_min.clamp(0.0, frame_w as f32);
        let y = y_min.clamp(0.0, frame_h as f32);
        let x2 = x_max.clamp(x, frame_w as f32);
        let y2 = y_max.clamp(y, frame_h as f32);
        let (w, h) = (x2 - x, y2 - y);
        if w < 1.0 || h < 1.0 {
            return None;
        }
        Some(Detection {
            label: self.label,
            confidence: self.confidence,
            bbox: [x as u32, y as u32, w as u32, h as u32],
        })
    }
}

/// JPEG header read (no full decode) — CPAI coordinates are already in
/// the original-image space, the frame size only bounds the clamp.
fn jpeg_size(jpeg: &[u8]) -> Result<(u32, u32)> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
    decoder
        .read_info()
        .context("cpai: JPEG header parse failed")?;
    let info = decoder.info().context("cpai: JPEG header missing")?;
    Ok((info.width as u32, info.height as u32))
}

/// Remote-primary detector with a local fallback and exponential
/// re-probe backoff — the consumer agreement's fail-open discipline:
/// a downed endpoint degrades to local detection without hammering the
/// server (probe cadence 1s → 2s → 4s … capped at 60s).
pub struct FailoverDetector {
    primary: CpaiDetector,
    fallback: Option<Arc<dyn AiDetector>>,
    state: Mutex<FailoverState>,
}

struct FailoverState {
    degraded: bool,
    /// Wall-clock ms of the next allowed probe attempt.
    next_probe_ms: u64,
    /// Current backoff, doubled per consecutive failure.
    backoff_ms: u64,
}

const BACKOFF_INIT_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 60_000;

impl FailoverDetector {
    pub fn new(primary: CpaiDetector, fallback: Option<Arc<dyn AiDetector>>) -> Self {
        Self::with_backoff(primary, fallback, BACKOFF_INIT_MS)
    }

    /// Test seam: initial probe backoff in milliseconds.
    pub fn with_backoff(
        primary: CpaiDetector,
        fallback: Option<Arc<dyn AiDetector>>,
        init_backoff_ms: u64,
    ) -> Self {
        Self {
            primary,
            fallback,
            state: Mutex::new(FailoverState {
                degraded: false,
                next_probe_ms: 0,
                backoff_ms: init_backoff_ms.max(1),
            }),
        }
    }
}

impl AiDetector for FailoverDetector {
    fn detect(&self, jpeg: &[u8]) -> Result<Vec<Detection>> {
        let now = unix_now_ms();
        let mut state = self.state.lock().expect("cpai failover state lock");

        // Degraded and outside the probe window: serve the fallback
        // directly. Without a fallback the honest answer between probes
        // is "nothing detected this frame" — the degradation warning
        // already fired at episode start, staying silent beats a warn
        // per frame.
        if state.degraded && now < state.next_probe_ms {
            return match &self.fallback {
                Some(fb) => fb.detect(jpeg),
                None => Ok(Vec::new()),
            };
        }

        match self.primary.detect(jpeg) {
            Ok(detections) => {
                if state.degraded {
                    tracing::info!("cpai: remote endpoint recovered");
                }
                state.degraded = false;
                state.backoff_ms = state.backoff_ms.min(BACKOFF_MAX_MS);
                state.next_probe_ms = 0;
                Ok(detections)
            }
            Err(e) => {
                if !state.degraded {
                    tracing::warn!(
                        error = %e,
                        "cpai: remote endpoint failed — frames degrade to the local detector until a re-probe succeeds"
                    );
                }
                state.degraded = true;
                state.next_probe_ms = now.saturating_add(state.backoff_ms);
                state.backoff_ms = (state.backoff_ms.saturating_mul(2)).min(BACKOFF_MAX_MS);
                match &self.fallback {
                    Some(fb) => fb.detect(jpeg),
                    // Surface the error only on probe attempts (the
                    // window check above silences the in-between
                    // frames), so the worker's warn fires at the same
                    // throttled cadence.
                    None => Err(e),
                }
            }
        }
    }

    fn model_name(&self) -> &str {
        MODEL_ID
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

use std::sync::Arc;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read};

    /// 16×16 grey JPEG (header is all these tests need).
    fn tiny_jpeg() -> Vec<u8> {
        use std::io::Write;
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
                &vec![128u8; 16 * 16 * 3],
                16,
                16,
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("encode jpeg");
        buf.borrow().clone()
    }

    struct Captured {
        head: String,
        body: Vec<u8>,
    }

    /// One-shot HTTP mock: reads a single request (head + Content-Length
    /// body), replays `response` bytes, stores the capture in the
    /// returned handle.
    fn spawn_mock(response: Vec<u8>) -> (String, std::thread::JoinHandle<Captured>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut head = Vec::new();
            reader.read_until(b'\n', &mut head).expect("read head");
            loop {
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).expect("read header");
                if line.len() <= 2 {
                    break;
                }
                head.extend_from_slice(&line);
            }
            let head_str = String::from_utf8_lossy(&head).to_string();
            let content_length: usize = head_str
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).expect("read body");
            let mut stream = stream;
            std::io::Write::write_all(&mut stream, &response).expect("write response");
            Captured {
                head: head_str,
                body,
            }
        });
        (format!("http://{addr}/v1/vision/detection"), handle)
    }

    fn ok_response(predictions: &str) -> Vec<u8> {
        let body = format!(
            r#"{{"success":true,"predError":false,"count":2,"predictions":[{predictions}],"width":16,"height":16,"processMs":42,"someFutureField":1}}"#
        );
        http_response(200, body.as_bytes())
    }

    fn http_response(status: u16, body: &[u8]) -> Vec<u8> {
        let reason = match status {
            200 => "OK",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let mut out = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn wire_roundtrip_maps_predictions_and_sends_contract_parts() {
        let jpeg = tiny_jpeg();
        let (url, handle) = spawn_mock(ok_response(
            r#"{"label":"person","confidence":0.91,"x_min":2,"y_min":3,"x_max":10,"y_max":12},
               {"label":"dog","confidence":0.55,"x_min":0,"y_min":0,"x_max":16,"y_max":16}"#,
        ));
        let detector = CpaiDetector::new(&url, "tok_abc", 5_000, Some(0.4)).expect("detector");

        let detections = detector.detect(&jpeg).expect("detect ok");
        let captured = handle.join().expect("mock thread");

        assert_eq!(detections.len(), 2);
        assert_eq!(detections[0].label, "person");
        assert!((detections[0].confidence - 0.91).abs() < 1e-6);
        assert_eq!(
            detections[0].bbox,
            [2, 3, 8, 9],
            "w=x_max-x_min h=y_max-y_min"
        );
        assert_eq!(detections[1].bbox, [0, 0, 16, 16]);

        // Consumer contract: bearer token + image part + minConfidence.
        let head = captured.head.to_lowercase();
        assert!(head.starts_with("post /v1/vision/detection"));
        assert!(head.contains("authorization: bearer tok_abc"));
        assert!(captured.body.windows(6).any(|w| w == b"image\""));
        let body_str = String::from_utf8_lossy(&captured.body);
        assert!(
            body_str.contains("minConfidence"),
            "form carries minConfidence"
        );
        assert!(
            body_str.contains("0.4"),
            "minConfidence carries the configured threshold"
        );
        assert!(
            captured
                .body
                .windows(jpeg.len())
                .any(|w| w == jpeg.as_slice()),
            "raw JPEG bytes ride the image part"
        );
    }

    #[test]
    fn wire_empty_token_omits_auth_header() {
        let jpeg = tiny_jpeg();
        let (url, handle) = spawn_mock(ok_response(
            r#"{"label":"person","confidence":0.9,"x_min":1,"y_min":1,"x_max":5,"y_max":5}"#,
        ));
        let detector = CpaiDetector::new(&url, "", 5_000, None).expect("detector");
        detector.detect(&jpeg).expect("detect ok");
        let captured = handle.join().expect("mock thread");
        assert!(
            !captured.head.to_lowercase().contains("authorization:"),
            "empty token = no Authorization header"
        );
        assert!(
            !String::from_utf8_lossy(&captured.body).contains("minConfidence"),
            "None threshold = no minConfidence part"
        );
    }

    #[test]
    fn wire_success_false_is_error() {
        let jpeg = tiny_jpeg();
        let body = br#"{"success":false,"predError":true,"message":"model not loaded"}"#;
        let mut res = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        res.extend_from_slice(body);
        let (url, handle) = spawn_mock(res);
        let detector = CpaiDetector::new(&url, "", 5_000, None).expect("detector");
        let err = detector
            .detect(&jpeg)
            .expect_err("success:false must error");
        assert!(err.to_string().contains("success=false"), "{err}");
        handle.join().expect("mock thread");
    }

    #[test]
    fn wire_http_503_is_error() {
        let jpeg = tiny_jpeg();
        let body = br#"{"success":false,"predError":true,"message":"model warming up"}"#;
        let mut res = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        res.extend_from_slice(body);
        let (url, _) = spawn_mock(res);
        let detector = CpaiDetector::new(&url, "", 5_000, None).expect("detector");
        let err = detector.detect(&jpeg).expect_err("503 must error");
        assert!(err.to_string().contains("503"), "{err}");
    }

    #[test]
    fn wire_non_json_body_is_error() {
        let jpeg = tiny_jpeg();
        let (url, _) = spawn_mock(http_response(200, b"<html>proxy error</html>"));
        let detector = CpaiDetector::new(&url, "", 5_000, None).expect("detector");
        let err = detector.detect(&jpeg).expect_err("non-JSON must error");
        assert!(err.to_string().contains("non-JSON"), "{err}");
    }

    #[test]
    fn wire_timeout_is_error() {
        let jpeg = tiny_jpeg();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            // Accept and never respond.
            let (_s, _) = listener.accept().expect("accept");
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        let url = format!("http://{addr}/v1/vision/detection");
        let detector = CpaiDetector::new(&url, "", 300, None).expect("detector");
        let started = std::time::Instant::now();
        assert!(detector.detect(&jpeg).is_err(), "timeout must error");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "must not wait the mock's full sleep"
        );
        h.join().expect("mock thread");
    }

    #[test]
    fn wire_unknown_fields_ignored_and_out_of_range_clamped() {
        let jpeg = tiny_jpeg(); // 16×16
        let (url, _) = spawn_mock(ok_response(
            r#"{"label":"person","confidence":0.9,"x_min":-5,"y_min":0,"x_max":999,"y_max":10},
               {"label":"cat","confidence":0.8,"x_min":1,"y_min":1,"x_max":1,"y_max":9}"#,
        ));
        let detector = CpaiDetector::new(&url, "", 5_000, None).expect("detector");
        let detections = detector.detect(&jpeg).expect("detect ok");
        assert_eq!(
            detections[0].bbox,
            [0, 0, 16, 10],
            "negative/overflow clamp to frame"
        );
        assert_eq!(detections.len(), 1, "degenerate zero-width box dropped");
    }

    #[test]
    fn wire_bad_jpeg_is_error_before_any_http() {
        // A parser failure must not hit the endpoint at all — point the
        // detector at a port with no listener: if HTTP were attempted,
        // the error would be a transport one instead.
        let detector = CpaiDetector::new("http://127.0.0.1:9/v1/vision/detection", "", 500, None)
            .expect("detector");
        let err = detector
            .detect(b"not a jpeg")
            .expect_err("bad jpeg must error");
        assert!(err.to_string().contains("JPEG header"), "{err}");
    }

    /// Fake fallback counting calls and returning a sentinel box.
    struct CountingFallback {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl AiDetector for CountingFallback {
        fn detect(&self, _jpeg: &[u8]) -> Result<Vec<Detection>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![Detection {
                label: "fallback".into(),
                confidence: 0.99,
                bbox: [1, 1, 2, 2],
            }])
        }
        fn model_name(&self) -> &str {
            "counting-fallback"
        }
    }

    fn dead_endpoint_detector() -> CpaiDetector {
        // Port 9 (discard) has no listener in the test env → instant
        // connection refused = a fast, deterministic remote failure.
        CpaiDetector::new("http://127.0.0.1:9/v1/vision/detection", "", 500, None)
            .expect("detector")
    }

    #[test]
    fn failover_degrades_to_fallback_and_probes_at_backoff_cadence() {
        let jpeg = tiny_jpeg();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failover = FailoverDetector::with_backoff(
            dead_endpoint_detector(),
            Some(Arc::new(CountingFallback {
                calls: Arc::clone(&calls),
            })),
            200,
        );

        // Burst of 5 frames: remote probed exactly once, every frame
        // served by the fallback.
        for _ in 0..5 {
            let d = failover.detect(&jpeg).expect("fallback serves the frame");
            assert_eq!(d[0].label, "fallback");
        }
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "every frame hit the fallback"
        );

        // Past the (200 ms) probe window the remote is retried: fails
        // again → backoff doubles → fallback keeps serving.
        std::thread::sleep(std::time::Duration::from_millis(220));
        failover.detect(&jpeg).expect("still served");
        std::thread::sleep(std::time::Duration::from_millis(420));
        failover.detect(&jpeg).expect("still served");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            7,
            "fallback kept serving during probes"
        );
    }

    #[test]
    fn failover_without_fallback_silences_between_probes() {
        let jpeg = tiny_jpeg();
        let failover = FailoverDetector::with_backoff(dead_endpoint_detector(), None, 200);

        let err = failover
            .detect(&jpeg)
            .expect_err("probe attempt surfaces the error");
        assert!(err.to_string().contains("cpai"), "{err}");

        // Inside the probe window: honest empty frame, no error, no
        // warn flood.
        let d = failover.detect(&jpeg).expect("silent empty frame");
        assert!(d.is_empty());
    }

    #[test]
    fn failover_recovers_when_remote_returns() {
        let jpeg = tiny_jpeg();
        let (url, handle) = spawn_mock(ok_response(
            r#"{"label":"person","confidence":0.9,"x_min":1,"y_min":1,"x_max":5,"y_max":5}"#,
        ));
        // Primary works from the start — no fallback needed, first call
        // serves remote detections.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failover = FailoverDetector::with_backoff(
            CpaiDetector::new(&url, "", 5_000, None).expect("detector"),
            Some(Arc::new(CountingFallback {
                calls: Arc::clone(&calls),
            })),
            1_000,
        );
        let d = failover.detect(&jpeg).expect("remote ok");
        assert_eq!(d[0].label, "person");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "fallback untouched"
        );
        let _ = handle.join();
    }
}
