//! RTSP/1.0 server (RFC 2326)
//!
//! Provides:
//! - Configurable TCP listener (default port 8554)
//! - OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN handlers
//! - Digest authentication (RFC 2617)
//! - Multi-stream support via URL path matching
//! - RTP interleaved mode (TCP) for streaming (RFC 2326 §10.12)
//!
//! This is a **hand-written** implementation — no GStreamer or heavy media
//! framework. Reuses types from the sibling `rtsp` client module.

#![cfg_attr(test, deny(warnings))]

mod auth;
mod framing;
mod server;

// ── Public API re-exports ─────────────────────────────────────────────────────

pub use framing::TransportInfo;
pub use framing::TransportInfo as RtpTransportInfo;
pub use framing::build_interleaved_frame;
pub use server::RtspMethod;

use base64::engine::Engine;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{debug, error, info};

// ═══════════════════════════════════════════════════════════════════════════════
// Configuration
// ═══════════════════════════════════════════════════════════════════════════════

/// Configuration for the RTSP server.
#[derive(Debug, Clone)]
pub struct RtspServerConfig {
    /// TCP port to listen on (default: 8554)
    pub port: u16,
    /// Require Digest authentication for all requests
    pub auth_required: bool,
    /// Authentication realm (used in WWW-Authenticate challenges)
    pub realm: String,
    /// Allowed username for Digest auth
    pub username: String,
    /// Allowed password for Digest auth
    pub password: String,
}

impl Default for RtspServerConfig {
    fn default() -> Self {
        Self {
            port: 8554,
            auth_required: false,
            realm: "mibee-eye RTSP Server".to_string(),
            username: String::new(),
            password: String::new(),
        }
    }
}

/// Describes a stream that the RTSP server can serve.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// URL path for this stream (e.g., "webcam", "camera/1")
    pub path: String,
    /// Full SDP body returned for DESCRIBE requests
    pub sdp_body: String,
    /// SSRC identifier for RTP packets
    pub ssrc: u32,
}

impl StreamConfig {
    /// Create a new stream configuration.
    #[tracing::instrument(skip_all)]
    pub fn new(path: &str, sdp_body: &str, ssrc: u32) -> Self {
        Self {
            path: path.to_string(),
            sdp_body: sdp_body.to_string(),
            ssrc,
        }
    }

    /// Returns the URL path for DESCRIBE matching: `/stream/{path}` or `/{path}`
    fn url_path(&self) -> String {
        if self.path.starts_with('/') {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        }
    }

    /// Check if a request URI matches this stream.
    fn matches_uri(&self, uri: &str) -> bool {
        let url_path = self.url_path();
        uri == url_path || uri.ends_with(&url_path) || uri.contains(&format!("/{}", self.path))
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Server
// ═══════════════════════════════════════════════════════════════════════════════

/// Entry for a dynamically registered live stream.
struct LiveStreamEntry {
    /// Broadcast sender for H.264 NAL unit data from RtspOutput.
    /// Each RTSP client calls `subscribe()` to get its own receiver.
    frame_tx: broadcast::Sender<Vec<u8>>,
    /// SDP body describing the stream (codec, payload type, etc.).
    /// Built dynamically when SPS/PPS become available.
    sdp_body: String,
    /// SSRC for RTP packets.
    ssrc: u32,
    /// Cached SPS NAL (type 7) for SDP sprop-parameter-sets.
    cached_sps: Option<Vec<u8>>,
    /// Cached PPS NAL (type 8) for SDP sprop-parameter-sets.
    cached_pps: Option<Vec<u8>>,
    /// A parked receiver, kept solely so `broadcast::Sender::send` cannot
    /// fail with `SendError` (and therefore cannot panic the encoder task)
    /// between "stream registered" and "first RTSP client subscribes".
    ///
    /// This replaces a `std::mem::forget(rx)` that nothing ever dropped:
    /// each register/stop cycle leaked a 300-slot ring plus whatever frames
    /// it still held. Holding it here ties the channel's lifetime to the
    /// *entry*, so [`RtspServer::unregister_live_stream`] releases both.
    #[allow(dead_code)]
    keepalive_rx: broadcast::Receiver<Vec<u8>>,
}

/// Internal server state shared across connections.
struct RtspServerInner {
    config: RtspServerConfig,
    streams: HashMap<String, StreamConfig>,
    /// Dynamically registered live stream entries, keyed by path.
    live_streams: Mutex<HashMap<String, LiveStreamEntry>>,
}

/// The RTSP server instance.
///
/// Serves configured streams via the RTSP/1.0 protocol.
/// Each stream is identified by a URL path and provides an SDP description.
///
/// # Example
///
/// ```ignore
/// use protocols::rtsp_server::{RtspServer, RtspServerConfig, StreamConfig};
///
/// let config = RtspServerConfig {
///     port: 8554,
///     auth_required: false,
///     ..Default::default()
/// };
///
/// let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=Test\r\n\
///            c=IN IP4 0.0.0.0\r\nt=0 0\r\n\
///            m=video 0 RTP/AVP 96\r\n\
///            a=rtpmap:96 H264/90000\r\n"
///     .to_string();
///
/// let server = RtspServer::new(config)
///     .with_stream(StreamConfig::new("webcam", &sdp, 0x12345678));
/// ```
#[derive(Clone)]
pub struct RtspServer {
    inner: Arc<RtspServerInner>,
}

impl RtspServer {
    /// Create a new RTSP server with the given configuration.
    #[tracing::instrument(skip_all)]
    pub fn new(config: RtspServerConfig) -> Self {
        Self {
            inner: Arc::new(RtspServerInner {
                config,
                streams: HashMap::new(),
                live_streams: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Add a stream to the server.
    #[tracing::instrument(skip_all)]
    pub fn with_stream(mut self, stream: StreamConfig) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("RtspServer::with_stream called after sharing")
            .streams
            .insert(stream.path.clone(), stream);
        self
    }

    /// Add a stream to the server by mutating self.
    #[tracing::instrument(skip_all)]
    pub fn add_stream(&mut self, stream: StreamConfig) {
        Arc::get_mut(&mut self.inner)
            .expect("RtspServer::add_stream called after sharing")
            .streams
            .insert(stream.path.clone(), stream);
    }

    /// Register a live stream that receives H.264 NAL data via a broadcast channel.
    ///
    /// Returns a `Sender` that [`RtspOutput`] can use to push video frames.
    /// The server will deliver frames to any RTSP client that PLAYS this stream.
    #[tracing::instrument(skip_all, fields(stream_name = path))]
    pub fn register_live_stream(
        &self,
        path: String,
        sdp_body: String,
        ssrc: u32,
    ) -> broadcast::Sender<Vec<u8>> {
        let (tx, rx) = broadcast::channel(300);
        let entry = LiveStreamEntry {
            frame_tx: tx.clone(),
            sdp_body,
            ssrc,
            cached_sps: None,
            cached_pps: None,
            keepalive_rx: rx,
        };
        // Re-registering the same path replaces the previous entry, which
        // drops that entry's parked receiver together with its ring.
        //
        // The old code only ever inserted here — never removed — so a stopped
        // mount stayed DESCRIBE-able and a client could PLAY a dead stream
        // and hang instead of getting an error.
        self.inner.live_streams.lock().insert(path, entry);
        tx
    }

    /// Remove a stream from the registry.
    ///
    /// Without this the entry (and its parked broadcast receiver) outlived
    /// the stream, and clients could still DESCRIBE / PLAY a mount that no
    /// longer produces frames.
    pub fn unregister_live_stream(&self, path: &str) -> bool {
        self.inner.live_streams.lock().remove(path).is_some()
    }

    /// Update the SPS/PPS NAL units for a live stream.
    /// This enables the server to build a complete SDP with sprop-parameter-sets.
    pub fn update_sps_pps(&self, path: &str, sps: Vec<u8>, pps: Vec<u8>) {
        if let Some(entry) = self.inner.live_streams.lock().get_mut(path) {
            entry.cached_sps = Some(sps);
            entry.cached_pps = Some(pps);
        }
    }

    /// Build a complete SDP string with sprop-parameter-sets if available.
    fn build_live_sdp(entry: &LiveStreamEntry) -> String {
        let base_sdp = &entry.sdp_body;
        if let (Some(sps), Some(pps)) = (&entry.cached_sps, &entry.cached_pps) {
            // Build SDP with sprop-parameter-sets so ffmpeg clients can
            // determine H.264 codec parameters (resolution, profile) without
            // waiting for the first keyframe from the RTP stream.
            let sps_b64 = base64::engine::general_purpose::STANDARD.encode(sps);
            let pps_b64 = base64::engine::general_purpose::STANDARD.encode(pps);
            // Replace the minimal fmtp line with one that includes sprop-parameter-sets
            let fmtp_with_sprop = format!(
                "a=fmtp:96 packetization-mode=1; sprop-parameter-sets={},{}\r\n",
                sps_b64, pps_b64
            );
            // Find and replace the existing fmtp line
            if let Some(idx) = base_sdp.find("a=fmtp:96") {
                let line_end = base_sdp[idx..]
                    .find("\r\n")
                    .map(|e| idx + e + 2)
                    .unwrap_or(base_sdp.len());
                let mut sdp = base_sdp[..idx].to_string();
                sdp.push_str(&fmtp_with_sprop);
                sdp.push_str(&base_sdp[line_end..]);
                return sdp;
            }
        }
        base_sdp.to_string()
    }

    /// Start the server and listen for connections.
    ///
    /// Binds to the configured port and accepts incoming RTSP connections.
    /// Each connection is handled in a separate tokio task.
    #[tracing::instrument(skip_all)]
    pub async fn run(&self) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", self.inner.config.port);
        let listener = TcpListener::bind(&addr).await?;
        info!("RTSP server listening on {addr}");

        let inner = self.inner.clone();

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    debug!("RTSP connection from {peer_addr}");
                    let inner = inner.clone();
                    tokio::spawn(async move {
                        server::handle_connection(stream, inner).await;
                    });
                }
                Err(e) => {
                    error!("Error accepting RTSP connection: {e}");
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::auth::*;
    use super::framing::*;
    use super::server::*;
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::sync::broadcast;

    // ─── MD5 Tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_md5_empty() {
        let digest = md5_hash(b"");
        assert_eq!(hex_encode(&digest), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn test_md5_hello() {
        let digest = md5_hash(b"hello");
        assert_eq!(hex_encode(&digest), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn test_md5_rfc2617_example() {
        // HA1 = MD5("Mufasa:testrealm@host.com:Circle Of Life")
        let ha1 = md5_hash(b"Mufasa:testrealm@host.com:Circle Of Life");
        assert_eq!(hex_encode(&ha1), "939e7578ed9e3c518a452acee763bce9");
    }

    // ─── Digest Auth Tests ──────────────────────────────────────────────────

    #[test]
    fn test_verify_digest_auth_valid() {
        // Test with known values
        let username = "admin";
        let password = "secret";
        let realm = "test";
        let method = "DESCRIBE";
        let uri = "rtsp://localhost:8554/webcam";
        let nonce = "abc123";

        // Manually construct the expected Authorization header
        let ha1 = md5_hex(format!("{username}:{realm}:{password}").as_bytes());
        let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
        let expected_response = md5_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes());

        let auth_header = format!(
            r#"Digest username="{username}", realm="{realm}", nonce="{nonce}", uri="{uri}", response="{expected_response}""#
        );

        assert!(verify_digest_auth(
            &auth_header,
            method,
            uri,
            username,
            password,
            realm,
        ));
    }

    #[test]
    fn test_verify_digest_auth_wrong_password() {
        let auth_header = r#"Digest username="admin", realm="test", nonce="abc123", uri="rtsp://localhost/webcam", response="deadbeef""#;

        assert!(!verify_digest_auth(
            auth_header,
            "DESCRIBE",
            "rtsp://localhost/webcam",
            "admin",
            "wrongpass",
            "test",
        ));
    }

    #[test]
    fn test_verify_digest_auth_wrong_username() {
        let auth_header = r#"Digest username="other", realm="test", nonce="abc123", uri="rtsp://localhost/webcam", response="deadbeef""#;

        assert!(!verify_digest_auth(
            auth_header,
            "DESCRIBE",
            "rtsp://localhost/webcam",
            "admin",
            "secret",
            "test",
        ));
    }

    #[test]
    fn test_verify_digest_auth_with_qop() {
        let username = "admin";
        let password = "secret";
        let realm = "test";
        let method = "PLAY";
        let uri = "rtsp://localhost:8554/webcam";
        let nonce = "abc123";
        let qop = "auth";
        let nc = "00000001";
        let cnonce = "deadbeef";

        let ha1 = md5_hex(format!("{username}:{realm}:{password}").as_bytes());
        let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
        let expected_response =
            md5_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}").as_bytes());

        let auth_header = format!(
            r#"Digest username="{username}", realm="{realm}", nonce="{nonce}", uri="{uri}", qop={qop}, nc={nc}, cnonce="{cnonce}", response="{expected_response}""#
        );

        assert!(verify_digest_auth(
            &auth_header,
            method,
            uri,
            username,
            password,
            realm,
        ));
    }

    #[test]
    fn test_verify_digest_auth_invalid_header() {
        // Invalid format
        assert!(!verify_digest_auth(
            "Basic xxx",
            "DESCRIBE",
            "rtsp://localhost/stream",
            "admin",
            "secret",
            "test"
        ));

        // Empty
        assert!(!verify_digest_auth(
            "",
            "DESCRIBE",
            "rtsp://localhost/stream",
            "admin",
            "secret",
            "test"
        ));
    }

    // ─── Auth Params Parsing ────────────────────────────────────────────────

    #[test]
    fn test_parse_auth_params_basic() {
        let params = parse_auth_params(r#"username="admin", realm="test", nonce="abc""#).unwrap();
        assert_eq!(params.get("username").unwrap(), "admin");
        assert_eq!(params.get("realm").unwrap(), "test");
        assert_eq!(params.get("nonce").unwrap(), "abc");
    }

    #[test]
    fn test_parse_auth_params_unquoted_qop() {
        let params = parse_auth_params(r#"realm="test", qop=auth, nc=00000001"#).unwrap();
        assert_eq!(params.get("realm").unwrap(), "test");
        assert_eq!(params.get("qop").unwrap(), "auth");
        assert_eq!(params.get("nc").unwrap(), "00000001");
    }

    // ─── Stream Config Tests ────────────────────────────────────────────────

    #[test]
    fn test_stream_config_matches_uri() {
        let stream = StreamConfig::new(
            "webcam",
            "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=Test\r\nt=0 0\r\n",
            0x1234,
        );
        assert!(stream.matches_uri("/webcam"));
        assert!(stream.matches_uri("rtsp://localhost:8554/webcam"));
        assert!(stream.matches_uri("rtsp://localhost:8554/stream/webcam"));
        assert!(!stream.matches_uri("/other"));
        assert!(!stream.matches_uri("rtsp://localhost:8554/other"));
    }

    #[test]
    fn test_stream_config_url_path() {
        let stream = StreamConfig::new("webcam", "", 0);
        assert_eq!(stream.url_path(), "/webcam");

        let stream2 = StreamConfig::new("/camera/1", "", 0);
        assert_eq!(stream2.url_path(), "/camera/1");
    }

    // ─── Response Building Tests ────────────────────────────────────────────

    #[test]
    fn test_build_response_basic() {
        let resp = build_response(1, 200, "OK", &[("Server", "mibee-eye")], b"");
        let s = String::from_utf8(resp).unwrap();
        assert!(s.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(s.contains("CSeq: 1\r\n"));
        assert!(s.contains("Server: mibee-eye\r\n"));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_build_response_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=Test\r\n";
        let resp = build_response(2, 200, "OK", &[("Content-Type", "application/sdp")], body);
        let s = String::from_utf8(resp).unwrap();
        assert!(s.contains("Content-Length"));
        assert!(s.contains(&body.len().to_string()));
        assert!(s.ends_with("s=Test\r\n"));
    }

    #[test]
    fn test_build_unauthorized_response() {
        let resp = build_unauthorized_response(1, "test-realm", "nonce-123");
        let s = String::from_utf8(resp).unwrap();
        assert!(s.contains("401 Unauthorized"));
        assert!(s.contains("WWW-Authenticate: Digest"));
        assert!(s.contains("realm=\"test-realm\""));
        assert!(s.contains("nonce=\"nonce-123\""));
    }

    #[test]
    fn test_build_not_found_response() {
        let resp = build_not_found_response(5);
        let s = String::from_utf8(resp).unwrap();
        assert!(s.contains("404 Not Found"));
        assert!(s.contains("CSeq: 5"));
    }

    #[test]
    fn test_options_response() {
        let resp = handle_options(1);
        let s = String::from_utf8(resp).unwrap();
        assert!(s.contains("200 OK"));
        assert!(s.contains("Public: DESCRIBE"));
        assert!(s.contains("SETUP"));
        assert!(s.contains("PLAY"));
        assert!(s.contains("TEARDOWN"));
    }

    // ─── Interleaved Frame Tests ────────────────────────────────────────────

    #[test]
    fn test_build_interleaved_frame() {
        let data = vec![0u8; 100];
        let frame = build_interleaved_frame(0, &data).unwrap();
        // Format: $<channel:1><len:2><data>
        assert_eq!(frame[0], 0x24); // '$'
        assert_eq!(frame[1], 0); // channel
        assert_eq!(u16::from_be_bytes([frame[2], frame[3]]), 100); // length
        assert_eq!(&frame[4..], &data[..]);
    }

    #[test]
    fn test_build_interleaved_frame_channel_1() {
        let data = vec![0xFFu8; 50];
        let frame = build_interleaved_frame(1, &data).unwrap();
        assert_eq!(frame[0], 0x24);
        assert_eq!(frame[1], 1);
        assert_eq!(u16::from_be_bytes([frame[2], frame[3]]), 50);
        assert_eq!(&frame[4..], &data[..]);
    }

    #[test]
    fn test_build_interleaved_frame_too_large() {
        let data = vec![0u8; 70000]; // > u16::MAX
        assert!(build_interleaved_frame(0, &data).is_err());
    }

    // ─── Error Response Tests ───────────────────────────────────────────────

    #[test]
    fn test_invalid_state_response() {
        let resp = build_invalid_state_response(3);
        let s = String::from_utf8(resp).unwrap();
        assert!(s.contains("455 Method Not Valid In This State"));
    }

    #[test]
    fn test_unsupported_transport_response() {
        let resp = build_unsupported_transport_response(4);
        let s = String::from_utf8(resp).unwrap();
        assert!(s.contains("461 Unsupported Transport"));
    }

    // ─── Server Logic Tests ─────────────────────────────────────────────────

    /// Helper to create a test SDP for webcam stream
    fn test_webcam_sdp() -> String {
        "v=0\r\no=- 1234567890 1234567890 IN IP4 192.168.1.1\r\n\
         s=Live Stream\r\n\
         c=IN IP4 0.0.0.0\r\n\
         t=0 0\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 packetization-mode=1;profile-level-id=42C01E\r\n\
         a=control:track1\r\n"
            .to_string()
    }

    /// Helper to create a test server with one stream
    fn test_server() -> RtspServer {
        RtspServer::new(RtspServerConfig::default()).with_stream(StreamConfig::new(
            "webcam",
            &test_webcam_sdp(),
            0xdeadbeef,
        ))
    }

    #[test]
    fn test_server_create() {
        let server = test_server();
        assert_eq!(server.inner.streams.len(), 1);
        assert!(server.inner.streams.contains_key("webcam"));
    }

    #[test]
    fn test_server_with_multiple_streams() {
        let mut server = RtspServer::new(RtspServerConfig::default());
        server.add_stream(StreamConfig::new("webcam", &test_webcam_sdp(), 0x1));
        server.add_stream(StreamConfig::new("camera/1", &test_webcam_sdp(), 0x2));
        server.add_stream(StreamConfig::new("camera/2", &test_webcam_sdp(), 0x3));
        assert_eq!(server.inner.streams.len(), 3);
    }

    #[test]
    fn test_find_stream_by_uri() {
        let mut server = RtspServer::new(RtspServerConfig::default());
        server.add_stream(StreamConfig::new("webcam", &test_webcam_sdp(), 0x1));
        server.add_stream(StreamConfig::new("camera/1", &test_webcam_sdp(), 0x2));

        assert!(find_stream_by_uri("/webcam", &server.inner.streams).is_some());
        assert!(
            find_stream_by_uri("rtsp://localhost:8554/webcam", &server.inner.streams).is_some()
        );
        assert!(find_stream_by_uri("/camera/1", &server.inner.streams).is_some());
        assert!(find_stream_by_uri("/other", &server.inner.streams).is_none());
    }

    // ─── Request/Response Protocol Flow Tests ──────────────────────────────

    /// Helper: send an RTSP request and read the response from a duplex stream
    async fn send_rtsp_and_recv(
        writer: &mut (impl AsyncWrite + Unpin),
        reader: &mut (impl AsyncRead + Unpin),
        request: &[u8],
    ) -> Vec<u8> {
        writer.write_all(request).await.unwrap();

        // Read response: read until \r\n\r\n headers are complete, then grab body
        let mut resp_buf = Vec::new();
        let mut buf = [0u8; 1];

        // Read headers first
        let mut header_end = None;
        while header_end.is_none() {
            if reader.read(&mut buf).await.unwrap() == 0 {
                break;
            }
            resp_buf.push(buf[0]);
            if resp_buf.len() >= 4 && resp_buf[resp_buf.len() - 4..] == *b"\r\n\r\n" {
                header_end = Some(resp_buf.len());
            }
        }

        // Parse Content-Length from headers to get body size
        let header_str = String::from_utf8_lossy(&resp_buf);
        let content_length = header_str
            .lines()
            .find_map(|line| {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("Content-Length")
                {
                    return value.trim().parse::<usize>().ok();
                }
                None
            })
            .unwrap_or(0);

        // Read body
        if content_length > 0 {
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await.unwrap();
            resp_buf.extend_from_slice(&body);
        }

        resp_buf
    }

    #[tokio::test]
    async fn test_full_rtsp_handshake() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        // Spawn connection handler on server side
        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // 1. OPTIONS
        let request = b"OPTIONS rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.contains("200 OK"), "OPTIONS should return 200");
        assert!(
            resp_str.contains("Public:"),
            "OPTIONS should have Public header"
        );
        assert!(resp_str.contains("CSeq: 1"));

        // 2. DESCRIBE
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "DESCRIBE should return 200, got: {resp_str}"
        );
        assert!(resp_str.contains("Content-Type: application/sdp"));
        assert!(resp_str.contains("H264"), "SDP should contain H264");

        // 3. SETUP
        let request = b"SETUP rtsp://localhost:8554/webcam/track1 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "SETUP should return 200, got: {resp_str}"
        );
        assert!(resp_str.contains("Transport:"));
        assert!(resp_str.contains("Session:"));

        // Extract session ID
        let session_id = resp_str
            .lines()
            .find_map(|line| {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("Session")
                {
                    return Some(value.trim().to_string());
                }
                None
            })
            .expect("SETUP response should have Session header");

        // 4. PLAY
        let play_request = format!(
            "PLAY rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\nRange: npt=0.000-\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            play_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "PLAY should return 200, got: {resp_str}"
        );

        // 5. TEARDOWN
        let teardown_request = format!(
            "TEARDOWN rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 5\r\nSession: {session_id}\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            teardown_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "TEARDOWN should return 200, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_describe_nonexistent_stream() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        let request = b"DESCRIBE rtsp://localhost:8554/nonexistent RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("404 Not Found"),
            "Should return 404 for unknown stream, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_setup_without_transport_header() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // First describe
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let _resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;

        // SETUP without Transport header
        let request = b"SETUP rtsp://localhost:8554/webcam/track1 RTSP/1.0\r\nCSeq: 2\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("461 Unsupported Transport"),
            "SETUP without Transport should return 461, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_play_without_setup() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Describe
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let _resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;

        // PLAY without SETUP (should fail)
        let request =
            b"PLAY rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 2\r\nSession: nosession\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("455 Method Not Valid In This State"),
            "PLAY without SETUP should return 455, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_unauthorized_access() {
        let config = RtspServerConfig {
            port: 8554,
            auth_required: true,
            realm: "test-realm".to_string(),
            username: "admin".to_string(),
            password: "secret".to_string(),
        };
        let server = RtspServer::new(config).with_stream(StreamConfig::new(
            "webcam",
            &test_webcam_sdp(),
            0x1234,
        ));

        let (client, server_stream) = tokio::io::duplex(4096);
        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Request without auth
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("401 Unauthorized"),
            "Should return 401 without auth, got: {resp_str}"
        );
        assert!(
            resp_str.contains("WWW-Authenticate: Digest"),
            "Should send Digest challenge"
        );
    }

    #[tokio::test]
    async fn test_authorized_access() {
        let config = RtspServerConfig {
            port: 8554,
            auth_required: true,
            realm: "test-realm".to_string(),
            username: "admin".to_string(),
            password: "secret".to_string(),
        };
        let server = RtspServer::new(config).with_stream(StreamConfig::new(
            "webcam",
            &test_webcam_sdp(),
            0x1234,
        ));

        let (client, server_stream) = tokio::io::duplex(4096);
        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // First request to get challenge
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);

        // Extract nonce from WWW-Authenticate
        let nonce = resp_str
            .lines()
            .find_map(|line| {
                if line.contains("WWW-Authenticate") {
                    let params = line.split(':').nth(1)?;
                    let params = params.trim().trim_start_matches("Digest ");
                    // Extract nonce
                    let nonce_start = params.find(r#"nonce=""#)? + 7;
                    let nonce_end = params[nonce_start..].find('"')? + nonce_start;
                    return Some(params[nonce_start..nonce_end].to_string());
                }
                None
            })
            .expect("Should have nonce in challenge");

        // Build authenticated request
        let uri = "rtsp://localhost:8554/webcam";
        let method = "DESCRIBE";
        let ha1 = md5_hex(b"admin:test-realm:secret");
        let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
        let response = md5_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes());

        let auth_request = format!(
            "DESCRIBE {uri} RTSP/1.0\r\n\
             CSeq: 2\r\n\
             Accept: application/sdp\r\n\
             Authorization: Digest username=\"admin\", realm=\"test-realm\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\"\r\n\
             \r\n"
        );

        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            auth_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "Should return 200 with valid auth, got: {resp_str}"
        );
        assert!(resp_str.contains("application/sdp"), "Should return SDP");
    }

    #[tokio::test]
    async fn test_get_parameter() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        let request = b"GET_PARAMETER rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "GET_PARAMETER should return 200, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_options_then_describe() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Send OPTIONS
        let opts_request = b"OPTIONS rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, opts_request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.contains("200 OK"));

        // Then DESCRIBE
        let desc_request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, desc_request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.contains("200 OK"));
        assert!(resp_str.contains("H264"));
    }

    #[tokio::test]
    async fn test_pause_before_play() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Describe + Setup first
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let _ = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;

        let request = b"SETUP rtsp://localhost:8554/webcam/track1 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        let session_id = resp_str
            .lines()
            .find_map(|line| {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("Session")
                {
                    return Some(value.trim().to_string());
                }
                None
            })
            .unwrap_or_default();

        // PAUSE before PLAY — should fail (not in playing state)
        let pause_request = format!(
            "PAUSE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 3\r\nSession: {session_id}\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            pause_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("455 Method Not Valid In This State"),
            "PAUSE before PLAY should return 455, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_play_then_pause() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Full handshake: OPTIONS, DESCRIBE, SETUP, PLAY
        let request = b"DESCRIBE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let _ = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;

        let request = b"SETUP rtsp://localhost:8554/webcam/track1 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        let session_id = resp_str
            .lines()
            .find_map(|line| {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("Session")
                {
                    return Some(value.trim().to_string());
                }
                None
            })
            .unwrap_or_default();

        // PLAY
        let play_request = format!(
            "PLAY rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 3\r\nSession: {session_id}\r\nRange: npt=0.000-\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            play_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.contains("200 OK"), "PLAY should succeed");

        // PAUSE after PLAY — should succeed
        let pause_request = format!(
            "PAUSE rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            pause_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "PAUSE after PLAY should return 200, got: {resp_str}"
        );
    }

    #[tokio::test]
    async fn test_connection_close_on_teardown() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // OPTIONS
        let request = b"OPTIONS rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        assert!(String::from_utf8_lossy(&resp).contains("200 OK"));

        // TEARDOWN immediately
        let request =
            b"TEARDOWN rtsp://localhost:8554/webcam RTSP/1.0\r\nCSeq: 2\r\nSession: \r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.contains("200 OK"));

        // Connection should close; reading should return 0 bytes
        let mut buf = [0u8; 1];
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            client_reader.read(&mut buf),
        )
        .await;

        match result {
            Ok(Ok(0)) => {} // Expected: connection closed
            Ok(Ok(n)) => panic!("Expected EOF after TEARDOWN, got {n} bytes"),
            Ok(Err(e)) => panic!("Error after TEARDOWN: {e}"),
            Err(_) => {} // Timeout is also OK if it takes a moment to close
        }
    }

    #[tokio::test]
    async fn test_missing_cseq_handling() {
        let server = test_server();
        let (client, server_stream) = tokio::io::duplex(4096);

        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Request without CSeq
        let request = b"OPTIONS rtsp://localhost:8554/webcam RTSP/1.0\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        // Should still respond (CSeq defaults to 0)
        assert!(resp_str.contains("200 OK"));
        assert!(resp_str.contains("CSeq: 0"));
    }

    // ─── Live Stream Tests ──────────────────────────────────────────────────

    #[test]
    fn test_register_live_stream() {
        let server = RtspServer::new(RtspServerConfig::default());
        let tx = server.register_live_stream(
            "livecam".to_string(),
            "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=Live\r\nt=0 0\r\n".to_string(),
            0x12345678,
        );
        // Verify sender works by sending data
        let result = tx.send(vec![0x00, 0x00, 0x00, 0x01, 0x67]);
        assert!(
            result.is_ok(),
            "Should be able to send to live stream channel"
        );
        // Verify entry is stored in server
        let live_map = server.inner.live_streams.lock();
        assert!(live_map.contains_key("livecam"));
        let entry = live_map.get("livecam").unwrap();
        assert_eq!(entry.ssrc, 0x12345678);
        assert!(entry.sdp_body.contains("s=Live"));
    }

    #[test]
    fn test_find_live_stream() {
        let mut live_map = HashMap::new();
        let (tx, rx) = broadcast::channel(300);
        live_map.insert(
            "livecam".to_string(),
            LiveStreamEntry {
                frame_tx: tx,
                sdp_body: "s=Live".to_string(),
                ssrc: 1,
                cached_sps: None,
                cached_pps: None,
                keepalive_rx: rx,
            },
        );
        assert!(find_live_stream("/livecam", &live_map).is_some());
        assert!(find_live_stream("rtsp://localhost:8554/livecam", &live_map).is_some());
        assert!(find_live_stream("/other", &live_map).is_none());
    }

    #[test]
    fn test_find_live_stream_longest_match_wins() {
        // Substream mounts (SPEC appendix A #20): `/live/{id}/sub` must
        // resolve to its own entry regardless of HashMap iteration order.
        let mut live_map = HashMap::new();
        let (tx_main, rx_main) = broadcast::channel(300);
        let (tx_sub, rx_sub) = broadcast::channel(300);
        live_map.insert(
            "live/cam-1".to_string(),
            LiveStreamEntry {
                frame_tx: tx_main,
                sdp_body: "s=Main".to_string(),
                ssrc: 1,
                cached_sps: None,
                cached_pps: None,
                keepalive_rx: rx_main,
            },
        );
        live_map.insert(
            "live/cam-1/sub".to_string(),
            LiveStreamEntry {
                frame_tx: tx_sub,
                sdp_body: "s=Sub".to_string(),
                ssrc: 2,
                cached_sps: None,
                cached_pps: None,
                keepalive_rx: rx_sub,
            },
        );

        let (path, entry) = find_live_stream("/live/cam-1/sub", &live_map).unwrap();
        assert_eq!(path, "live/cam-1/sub");
        assert_eq!(
            entry.ssrc, 2,
            "the sub mount must win over the shorter main path"
        );

        let (path, entry) = find_live_stream("/live/cam-1", &live_map).unwrap();
        assert_eq!(path, "live/cam-1");
        assert_eq!(entry.ssrc, 1, "exact main path still resolves to main");
    }

    /// Regression for the registry leak: `live_streams` was insert-only, so a
    /// stopped mount stayed DESCRIBE-able and a client could PLAY a stream
    /// that produces nothing and hang instead of erroring.
    #[test]
    fn unregister_removes_the_stream_from_the_registry() {
        let server = RtspServer::new(RtspServerConfig {
            port: 0,
            auth_required: false,
            ..Default::default()
        });

        let tx =
            server.register_live_stream("live/stop-me".to_string(), "s=Live\r\n".to_string(), 7);
        assert!(
            server
                .inner
                .live_streams
                .lock()
                .contains_key("live/stop-me"),
            "registered stream must be visible to DESCRIBE"
        );
        // The parked `keepalive_rx` means the encoder can push frames before
        // any RTSP client has subscribed — `broadcast::Sender::send` fails
        // (and the caller may panic on it) with zero receivers.
        assert!(tx.send(vec![0u8; 4]).is_ok());

        assert!(server.unregister_live_stream("live/stop-me"));
        assert!(
            !server
                .inner
                .live_streams
                .lock()
                .contains_key("live/stop-me"),
            "an unregistered stream must no longer be DESCRIBE-able"
        );
        // Idempotent: a second unregister reports "was not there" rather than
        // panicking or double-freeing the parked receiver.
        assert!(!server.unregister_live_stream("live/stop-me"));
        // With the entry gone, the parked receiver went with it — the sender
        // correctly reports no recipients instead of silently buffering.
        assert!(tx.send(vec![0u8; 4]).is_err());

        // Re-registering the same path must work and replace cleanly.
        let tx2 =
            server.register_live_stream("live/stop-me".to_string(), "s=Live\r\n".to_string(), 8);
        assert_eq!(
            server.inner.live_streams.lock()["live/stop-me"].ssrc,
            8,
            "re-registration must replace, not silently keep, the old entry"
        );
        assert!(tx2.send(vec![0u8; 4]).is_ok());
    }

    #[tokio::test]
    async fn test_live_stream_describe_and_play() {
        let server = RtspServer::new(RtspServerConfig::default());

        // Register a live stream
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=LiveCam\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
        let tx = server.register_live_stream("livecam".to_string(), sdp.to_string(), 0xdeadbeef);

        let (client, server_stream) = tokio::io::duplex(65536);
        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // 1. OPTIONS
        let request = b"OPTIONS rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        assert!(String::from_utf8_lossy(&resp).contains("200 OK"));

        // 2. DESCRIBE — should find the live stream
        let request = b"DESCRIBE rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "DESCRIBE should return 200 for live stream, got: {resp_str}"
        );
        assert!(resp_str.contains("H264"), "SDP should contain H264");

        // 3. SETUP
        let request = b"SETUP rtsp://localhost:8554/livecam/track1 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "SETUP should return 200, got: {resp_str}"
        );

        // Extract session ID
        let session_id = resp_str
            .lines()
            .find_map(|line| {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("Session")
                {
                    return Some(value.trim().to_string());
                }
                None
            })
            .expect("SETUP response should have Session header");

        // 4. PLAY
        let play_request = format!(
            "PLAY rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\nRange: npt=0.000-\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            play_request.as_bytes(),
        )
        .await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(
            resp_str.contains("200 OK"),
            "PLAY should return 200 for live stream, got: {resp_str}"
        );

        // 5. Send a video frame through the live stream, read interleaved RTP
        let nal_data = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x80];
        let _ = tx.send(nal_data.clone());

        // Read the interleaved RTP frame from the client side
        // Format: $<channel:1><len:2><data>
        let mut magic = [0u8; 1];
        client_reader.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic[0], 0x24, "Should receive interleaved RTP magic byte");

        let mut channel = [0u8; 1];
        client_reader.read_exact(&mut channel).await.unwrap();
        assert_eq!(channel[0], 0, "Should be on channel 0");

        let mut len_buf = [0u8; 2];
        client_reader.read_exact(&mut len_buf).await.unwrap();
        let frame_len = u16::from_be_bytes(len_buf) as usize;
        assert_eq!(
            frame_len,
            nal_data.len(),
            "Frame length should match NAL data"
        );

        let mut received_data = vec![0u8; frame_len];
        client_reader.read_exact(&mut received_data).await.unwrap();
        assert_eq!(
            received_data, nal_data,
            "Received data should match sent NAL data"
        );

        // 6. TEARDOWN
        let teardown_request = format!(
            "TEARDOWN rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 5\r\nSession: {session_id}\r\n\r\n"
        );
        // After TEARDOWN, the connection should close
        // We send TEARDOWN through the writer
        client_writer
            .write_all(teardown_request.as_bytes())
            .await
            .unwrap();
        // Give time for the handler to process
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // ─── RTP Sequence Gap Detection Tests ─────────────────────────────────

    #[test]
    fn test_seq_tracker_normal_sequence() {
        let mut tracker = SequenceTracker::new();
        // First packet — no gap
        assert_eq!(tracker.check(100), None);
        // Normal increment — no gap
        assert_eq!(tracker.check(101), None);
        assert_eq!(tracker.check(102), None);
        assert_eq!(tracker.check(103), None);
    }

    #[test]
    fn test_seq_tracker_gap_detected() {
        let mut tracker = SequenceTracker::new();
        tracker.check(100);
        // Gap: last=100, expected=101, got=105, gap = |105-101| = 4
        let gap = tracker.check(105);
        assert_eq!(gap, Some(4));
    }

    #[test]
    fn test_seq_tracker_large_gap() {
        let mut tracker = SequenceTracker::new();
        tracker.check(100);
        // Large jump: last=100, expected=101, got=500, gap = |500-101| = 399
        let gap = tracker.check(500);
        assert_eq!(gap, Some(399));
    }

    #[test]
    fn test_seq_tracker_reset() {
        let mut tracker = SequenceTracker::new();
        tracker.check(100);
        assert_eq!(tracker.check(105), Some(4));
        // Reset — like a stream restart
        tracker.reset();
        // First packet after reset — no gap
        assert_eq!(tracker.check(200), None);
        // Normal sequence after reset
        assert_eq!(tracker.check(201), None);
    }

    #[test]
    fn test_seq_tracker_wraparound() {
        let mut tracker = SequenceTracker::new();
        tracker.check(0xFFFE);
        // Normal increment with wraparound
        assert_eq!(tracker.check(0xFFFF), None);
        // Wrap to 0 — no gap
        assert_eq!(tracker.check(0x0000), None);
        // Continue after wrap
        assert_eq!(tracker.check(0x0001), None);
    }

    #[test]
    fn test_seq_tracker_no_gap_on_first_packet() {
        let mut tracker = SequenceTracker::new();
        // First packet should always be accepted
        assert_eq!(tracker.check(1000), None);
        // Second call is not first anymore; verify it tracks correctly
        let mut t2 = SequenceTracker::new();
        assert_eq!(t2.check(0), None);
        let mut t3 = SequenceTracker::new();
        assert_eq!(t3.check(0xFFFF), None);
    }

    #[test]
    fn test_parse_rtp_header_for_tracking() {
        // Build a minimal valid RTP header (12 bytes)
        let data = vec![
            0x80, 0x60, // V=2, P=0, X=0, CC=0 | M=0, PT=96
            0x00, 0x2A, // sequence_number = 42
            0x00, 0x00, 0x00, 0x00, // timestamp = 0
            0xDE, 0xAD, 0xBE, 0xEF, // ssrc = 0xDEADBEEF
        ];
        let info = parse_rtp_header_for_tracking(&data).unwrap();
        assert_eq!(info.sequence_number, 42);
        assert_eq!(info.ssrc, 0xDEADBEEF);
    }

    #[test]
    fn test_parse_rtp_header_for_tracking_too_short() {
        assert!(parse_rtp_header_for_tracking(&[0x80, 0x00, 0x00]).is_none());
        assert!(parse_rtp_header_for_tracking(&[]).is_none());
    }

    #[test]
    fn test_parse_rtp_header_for_tracking_invalid_version() {
        // Version 0 (invalid)
        let data = [0x00u8; 12];
        assert!(parse_rtp_header_for_tracking(&data).is_none());
    }

    // ─── Idle Timeout Tests ────────────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn test_idle_session_timeout() {
        let server = RtspServer::new(RtspServerConfig::default());
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=LiveCam\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n";
        let tx = server.register_live_stream("livecam".to_string(), sdp.to_string(), 0xdeadbeef);
        let (client, server_stream) = tokio::io::duplex(65536);
        let inner = server.inner.clone();
        tokio::spawn(async move {
            handle_connection(server_stream, inner).await;
        });
        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Full handshake to reach PLAY state
        let request = b"OPTIONS rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let _ = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;

        let request = b"DESCRIBE rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n";
        let _ = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;

        let request = b"SETUP rtsp://localhost:8554/livecam/track1 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n";
        let resp = send_rtsp_and_recv(&mut client_writer, &mut client_reader, request).await;
        let resp_str = String::from_utf8_lossy(&resp);
        assert!(resp_str.contains("200 OK"));
        let session_id = resp_str
            .lines()
            .find_map(|line| {
                if let Some((name, value)) = line.split_once(':')
                    && name.trim().eq_ignore_ascii_case("Session")
                {
                    return Some(value.trim().to_string());
                }
                None
            })
            .expect("SETUP response should have Session header");

        // PLAY — enters streaming loop
        let play_request = format!(
            "PLAY rtsp://localhost:8554/livecam RTSP/1.0\r\nCSeq: 4\r\nSession: {session_id}\r\nRange: npt=0.000-\r\n\r\n"
        );
        let resp = send_rtsp_and_recv(
            &mut client_writer,
            &mut client_reader,
            play_request.as_bytes(),
        )
        .await;
        assert!(String::from_utf8_lossy(&resp).contains("200 OK"));

        // Send a frame — resets the idle timer
        let nal_data = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x80];
        let _ = tx.send(nal_data.clone());

        // Read the interleaved frame to confirm streaming is active
        let mut magic = [0u8; 1];
        client_reader.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic[0], 0x24, "Should receive RTP data");

        // Advance time past the 60s idle timeout, then yield to let handler process
        tokio::time::advance(Duration::from_secs(90)).await;
        tokio::task::yield_now().await;

        // After timeout, the connection should be closed.
        // Try to read — should get EOF, error (closed), or TEARDOWN response.
        // Any data received must NOT be interleaved RTP (0x24).
        let mut buf = [0u8; 1];
        match client_reader.read(&mut buf).await {
            Ok(0) => {} // Clean EOF — expected
            Ok(_n) => {
                // If we got data, it should not be an interleaved RTP frame start
                assert_ne!(buf[0], 0x24, "Interleaved RTP received after timeout");
            }
            Err(_) => {} // Connection reset — expected
        }
    }
}
