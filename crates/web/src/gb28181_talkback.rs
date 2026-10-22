//! GB28181 voice-talkback receive pipeline (device side, GB/T 28181-2022
//! §9.2): the gb28181-rs [`AudioTalkbackSink`] decodes G.711 RTP payloads
//! to 8 kHz mono PCM and feeds a cpal output stream.
//!
//! Fail-open posture: when talkback playback is disabled or the host has
//! no usable audio output, no sink is registered and the library refuses
//! talkback INVITEs with 488 — never a 200 OK with silently dropped
//! audio.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result};
use gb28181_rs::AudioCodec;
use gb28181_rs::server::AudioTalkbackSink;
use parking_lot::Mutex;
use protocols::audio_codec::{alaw_to_pcm, mulaw_to_pcm};
#[cfg(test)]
use protocols::audio_codec::{pcm_to_alaw, pcm_to_mulaw};

/// Decoded-PCM destination: a cpal ring buffer in production, a collector
/// in tests.
pub trait PcmSink: Send + Sync {
    /// Append 8 kHz mono samples.
    fn push(&self, samples: &[i16]);
}

/// Decodes G.711 talkback packets into 8 kHz mono PCM. One instance
/// serves the whole GB28181 runtime; the per-packet codec comes from the
/// offer-negotiated payload type (A-law vs μ-law).
pub struct TalkbackDecoder {
    out: Arc<dyn PcmSink>,
}

impl TalkbackDecoder {
    pub fn new(out: Arc<dyn PcmSink>) -> Self {
        Self { out }
    }

    fn decode(payload: &[u8], codec: AudioCodec) -> Vec<i16> {
        match codec {
            AudioCodec::Pcma => payload.iter().map(|&b| alaw_to_pcm(b)).collect(),
            AudioCodec::Pcmu => payload.iter().map(|&b| mulaw_to_pcm(b)).collect(),
        }
    }
}

impl AudioTalkbackSink for TalkbackDecoder {
    fn on_audio(&self, payload: &[u8], _ssrc: u32) {
        // Only reached for sinks that do not override on_audio_codec;
        // this one does. A-law is the dominant GB28181 variant if the
        // codec-aware path ever regresses.
        self.out.push(&Self::decode(payload, AudioCodec::Pcma));
    }

    fn on_audio_codec(&self, payload: &[u8], _ssrc: u32, codec: AudioCodec) {
        self.out.push(&Self::decode(payload, codec));
    }
}

/// Bounded mono PCM ring (≈2 s at 8 kHz). Live audio: drop-oldest keeps
/// playback latency bounded instead of accumulating a backlog when the
/// consumer stalls.
struct PcmRing {
    buf: Mutex<VecDeque<i16>>,
    cap: usize,
}

impl PcmRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: Mutex::new(VecDeque::with_capacity(cap)),
            cap,
        }
    }

    fn write(&self, samples: &[i16]) {
        let mut buf = self.buf.lock();
        buf.extend(samples.iter().copied());
        while buf.len() > self.cap {
            buf.pop_front();
        }
    }

    /// Drain up to `out.len()` samples (FIFO) into `out`; returns the
    /// number filled. The remainder is zero-filled (output silence).
    fn drain_into(&self, out: &mut [i16]) -> usize {
        let mut buf = self.buf.lock();
        let n = out.len().min(buf.len());
        for slot in out.iter_mut().take(n) {
            *slot = buf.pop_front().unwrap_or(0);
        }
        for slot in out.iter_mut().skip(n) {
            *slot = 0;
        }
        n
    }
}

/// Expand mono i16 samples to interleaved N-channel i16.
fn expand_i16(mono: &[i16], channels: usize, out: &mut Vec<i16>) {
    out.clear();
    out.reserve(mono.len() * channels);
    for &s in mono {
        for _ in 0..channels {
            out.push(s);
        }
    }
}

/// Expand mono i16 samples to interleaved N-channel f32.
fn expand_f32(mono: &[i16], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(mono.len() * channels);
    for &s in mono {
        let v = f32::from(s) / 32768.0;
        for _ in 0..channels {
            out.push(v);
        }
    }
}

/// cpal-backed PCM sink: writes into the ring drained by the output
/// stream callback. The returned [`cpal::Stream`] must be kept alive for
/// playback to continue.
pub struct CpalTalkbackOutput {
    ring: Arc<PcmRing>,
}

impl CpalTalkbackOutput {
    /// Open the default output device at G.711's native layout
    /// (8 kHz mono); when the host refuses it, retry with the device's
    /// default config, adapting channels and sample format (still at
    /// 8 kHz — no resampling). Any other rate is an error so the caller
    /// can fail open (no sink → 488).
    pub fn open() -> Result<(Arc<Self>, cpal::Stream)> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        use cpal::{SampleFormat, StreamConfig};

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no audio output device")?;

        // Preferred: 8 kHz mono i16, the G.711 native layout.
        let native = StreamConfig {
            channels: 1,
            sample_rate: 8000,
            buffer_size: cpal::BufferSize::Default,
        };
        let ring = Arc::new(PcmRing::new(16_000));
        let ring_i16 = Arc::clone(&ring);
        if let Ok(stream) = device.build_output_stream(
            native,
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                ring_i16.drain_into(data);
            },
            |err| tracing::warn!(error = %err, "talkback output stream error"),
            None,
        ) {
            stream.play().context("start talkback output stream")?;
            return Ok((Self::spawn(ring), stream));
        }

        // Fallback: the device's default config, 8 kHz only.
        let supported = device
            .default_output_config()
            .context("query default output config")?;
        if supported.sample_rate() != 8000 {
            anyhow::bail!(
                "output device cannot run at 8 kHz (default {} Hz, channels {})",
                supported.sample_rate(),
                supported.channels()
            );
        }
        let channels = supported.channels() as usize;
        let config = supported.config();
        let stream = match supported.sample_format() {
            SampleFormat::I16 => {
                let ring_cb = Arc::clone(&ring);
                let mut mono = Vec::new();
                let mut interleaved = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        mono.clear();
                        mono.resize(data.len() / channels, 0);
                        ring_cb.drain_into(&mut mono);
                        expand_i16(&mono, channels, &mut interleaved);
                        data.copy_from_slice(&interleaved);
                    },
                    |err| tracing::warn!(error = %err, "talkback output stream error"),
                    None,
                )
            }
            SampleFormat::F32 => {
                let ring_cb = Arc::clone(&ring);
                let mut mono = Vec::new();
                let mut interleaved = Vec::new();
                device.build_output_stream(
                    config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        mono.clear();
                        mono.resize(data.len() / channels, 0);
                        ring_cb.drain_into(&mut mono);
                        expand_f32(&mono, channels, &mut interleaved);
                        data.copy_from_slice(&interleaved);
                    },
                    |err| tracing::warn!(error = %err, "talkback output stream error"),
                    None,
                )
            }
            other => anyhow::bail!("unsupported output sample format: {other:?}"),
        }
        .context("build talkback output stream (default config)")?;
        stream.play().context("start talkback output stream")?;
        Ok((Self::spawn(ring), stream))
    }

    fn spawn(ring: Arc<PcmRing>) -> Arc<Self> {
        Arc::new(Self { ring })
    }
}

impl PcmSink for CpalTalkbackOutput {
    fn push(&self, samples: &[i16]) {
        self.ring.write(samples);
    }
}

/// Resolve the talkback sink per config: `None` when disabled; when
/// enabled, a decoder feeding the local output device. Errors bubble up
/// so the caller can fail open (no sink → the library answers 488).
pub fn open_sink(enabled: bool) -> Result<Option<(Arc<dyn AudioTalkbackSink>, cpal::Stream)>> {
    if !enabled {
        return Ok(None);
    }
    let (out, stream) = CpalTalkbackOutput::open()?;
    Ok(Some((Arc::new(TalkbackDecoder::new(out)), stream)))
}

// ── Talkback upstream (mic → platform, §9.2 send half) ───────────────────────

/// GB28181 voice is 8 kHz mono; the library expects 20 ms G.711 frames
/// (160 bytes) pushed at real-time cadence.
const UPSTREAM_FRAME_BYTES: usize = 160;
/// Upstream capture buffer: ~100 ms of frames per callback. With
/// `BufferSize::Default` the Sandy Bridge talk unit xrun'ed ~2×/s
/// (cpal default period is a few ms — any scheduler hiccup drops a
/// chunk); 100 ms rides out jitter for ~100 ms of extra upstream
/// latency, clamped to whatever the device actually supports.
#[must_use]
pub fn upstream_buffer_frames(rate: u32, min: Option<u32>, max: Option<u32>) -> u32 {
    let want = (rate / 10).max(UPSTREAM_FRAME_BYTES as u32);
    match (min, max) {
        (Some(lo), Some(hi)) => want.clamp(lo.max(1), hi.max(lo.max(1))),
        (Some(lo), None) => want.max(lo),
        (None, Some(hi)) => want.min(hi),
        (None, None) => want,
    }
}

/// Bounded pre-session backlog (~1 s of audio). The library only drains
/// the channel while a talkback session is up; a longer backlog would
/// both leak memory and replay stale mic audio on connect.
const UPSTREAM_BACKLOG_FRAMES: usize = 50;

/// Downmix to mono and linearly resample to 8 kHz.
#[must_use]
pub fn resample_to_8k_mono(samples: &[i16], rate: f64, channels: u16) -> Vec<i16> {
    let mono: Vec<i16> = if channels >= 2 {
        let ch = channels as usize;
        samples
            .chunks(ch)
            .map(|c| {
                let sum: i32 = c.iter().map(|&s| i32::from(s)).sum();
                (sum / ch as i32) as i16
            })
            .collect()
    } else {
        samples.to_vec()
    };
    if (rate - 8000.0).abs() < 0.5 {
        return mono;
    }
    let ratio = rate / 8000.0;
    let out_len = ((mono.len() as f64) / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let i0 = pos.floor() as usize;
            let i1 = (i0 + 1).min(mono.len() - 1);
            let frac = pos - i0 as f64;
            let a = i32::from(mono[i0]);
            let b = i32::from(mono[i1]);
            (a as f64 + (b - a) as f64 * frac).round() as i16
        })
        .collect()
}

/// Accumulates A-law bytes and yields only complete 20 ms frames,
/// retaining the remainder for the next push.
pub struct G711Chunker {
    buf: Vec<u8>,
}

impl G711Chunker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(UPSTREAM_FRAME_BYTES * 2),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.buf.extend_from_slice(bytes);
        while self.buf.len() >= UPSTREAM_FRAME_BYTES {
            out.push(self.buf[..UPSTREAM_FRAME_BYTES].to_vec());
            self.buf.drain(..UPSTREAM_FRAME_BYTES);
        }
        out
    }
}

impl Default for G711Chunker {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode PCM to the negotiated G.711 variant. A-law is the de-facto GB
/// platform codec (PCMA, PT 8); a PCMU offer flips the law for that
/// session and the encoder follows.
fn encode_g711(pcm: &[i16], law: gb28181_rs::client::AudioCodec) -> Vec<u8> {
    match law {
        gb28181_rs::client::AudioCodec::Pcma => pcm
            .iter()
            .map(|&s| protocols::audio_codec::pcm_to_alaw(s))
            .collect(),
        gb28181_rs::client::AudioCodec::Pcmu => pcm
            .iter()
            .map(|&s| protocols::audio_codec::pcm_to_mulaw(s))
            .collect(),
    }
}

/// The live law the cpal callback encodes with: the library's source
/// handle once the runtime has installed the channel (late-bound — the
/// mic opens before the builder exists), PCMA until then, matching the
/// library default.
#[must_use]
pub fn current_law(
    slot: &std::sync::OnceLock<Arc<gb28181_rs::server::TalkbackSource>>,
) -> gb28181_rs::client::AudioCodec {
    slot.get()
        .map_or(gb28181_rs::client::AudioCodec::Pcma, |s| s.law())
}

/// Callback-side send: never blocks (cpal callbacks must not); a full
/// backlog drops the frame — session start replays at most ~1 s.
fn push_frames(tx: &std::sync::mpsc::SyncSender<Vec<u8>>, chunker: &mut G711Chunker, g711: &[u8]) {
    for frame in chunker.push(g711) {
        let _ = tx.try_send(frame);
    }
}

/// Late-bound negotiated-law handle: the runtime installs the library's
/// live source after handing the channel to the builder.
pub type UpstreamLawSlot = std::sync::OnceLock<Arc<gb28181_rs::server::TalkbackSource>>;

/// Everything the runtime needs to wire the upstream half: the frame
/// channel for the builder, the live capture stream to hold for the
/// protocol's lifetime, and the late-bound law slot to bind after the
/// builder installs the channel.
pub struct UpstreamHandle {
    pub frames: std::sync::mpsc::Receiver<Vec<u8>>,
    pub stream: cpal::Stream,
    pub law_slot: Arc<UpstreamLawSlot>,
}

/// Resolve the talkback source per config: `None` when disabled; when
/// enabled, a cpal input stream (8 kHz mono preferred, resampled
/// otherwise) feeding G.711 20 ms frames into the channel the library
/// consumes during recvonly talkback sessions. The law follows the
/// session's negotiated offer via the late-bound slot (PCMA default;
/// the runtime installs the library's live handle after the builder
/// takes the channel). Errors bubble up so the caller can fail open
/// (no source → the library answers 488).
pub fn open_upstream(enabled: bool) -> anyhow::Result<Option<UpstreamHandle>> {
    if !enabled {
        return Ok(None);
    }
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default audio input device available")?;

    // Prefer a native 8 kHz config; fall back to the device default and
    // resample in-callback.
    let native_8k = device
        .supported_input_configs()?
        .into_iter()
        .find(|c| {
            let fmt = c.sample_format();
            (fmt == cpal::SampleFormat::I16 || fmt == cpal::SampleFormat::F32)
                && c.min_sample_rate() <= 8000
                && c.max_sample_rate() >= 8000
        })
        .map(|c| c.with_sample_rate(8000));
    let config = match native_8k {
        Some(c) => c,
        None => device.default_input_config()?,
    };
    let mut fixed_config = config.config();
    let (min_buf, max_buf) = match config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => (Some(*min), Some(*max)),
        cpal::SupportedBufferSize::Unknown => (None, None),
    };
    fixed_config.buffer_size = cpal::BufferSize::Fixed(upstream_buffer_frames(
        fixed_config.sample_rate,
        min_buf,
        max_buf,
    ));

    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(UPSTREAM_BACKLOG_FRAMES);
    let chunker = Arc::new(parking_lot::Mutex::new(G711Chunker::new()));
    // Late-bound law slot: the runtime installs the library's live
    // TalkbackSource handle after handing the channel to the builder;
    // until then (and per default) the encoder uses PCMA.
    let law_slot = Arc::new(std::sync::OnceLock::new());

    // Prefer the larger Fixed period (jitter headroom); when the host
    // rejects it — observed: dsnoop advertising a hard 170..=171 frame
    // range @8 kHz, and Unknown ranges that the device still enforces —
    // retry with the device default rather than failing open to 488.
    let stream = match build_upstream_stream(
        &device,
        &fixed_config,
        config.sample_format(),
        &tx,
        &chunker,
        &law_slot,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "fixed-buffer upstream capture rejected — retrying with device default"
            );
            build_upstream_stream(
                &device,
                &config.config(),
                config.sample_format(),
                &tx,
                &chunker,
                &law_slot,
            )?
        }
    };
    stream
        .play()
        .context("failed to start talkback upstream capture")?;
    Ok(Some(UpstreamHandle {
        frames: rx,
        stream,
        law_slot,
    }))
}

/// Build the upstream input stream for one config, cloning the shared
/// send-side state so the caller can retry with a fallback config.
fn build_upstream_stream(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    fmt: cpal::SampleFormat,
    tx: &std::sync::mpsc::SyncSender<Vec<u8>>,
    chunker: &Arc<parking_lot::Mutex<G711Chunker>>,
    law_slot: &Arc<UpstreamLawSlot>,
) -> anyhow::Result<cpal::Stream> {
    use cpal::traits::DeviceTrait;
    let err_fn = |e| tracing::warn!(error = %e, "talkback upstream capture error");
    let rate = cfg.sample_rate as f64;
    let channels = cfg.channels;
    match fmt {
        cpal::SampleFormat::I16 => {
            let tx = tx.clone();
            let chunker = Arc::clone(chunker);
            let law_slot = Arc::clone(law_slot);
            Ok(device.build_input_stream(
                *cfg,
                move |data: &[i16], _| {
                    push_frames(
                        &tx,
                        &mut chunker.lock(),
                        &encode_g711(
                            &resample_to_8k_mono(data, rate, channels),
                            current_law(&law_slot),
                        ),
                    );
                },
                err_fn,
                None,
            )?)
        }
        cpal::SampleFormat::F32 => {
            let tx = tx.clone();
            let chunker = Arc::clone(chunker);
            let law_slot = Arc::clone(law_slot);
            Ok(device.build_input_stream(
                *cfg,
                move |data: &[f32], _| {
                    let pcm: Vec<i16> = data
                        .iter()
                        .map(|&f| (f.clamp(-1.0, 1.0) * 32767.0) as i16)
                        .collect();
                    push_frames(
                        &tx,
                        &mut chunker.lock(),
                        &encode_g711(
                            &resample_to_8k_mono(&pcm, rate, channels),
                            current_law(&law_slot),
                        ),
                    );
                },
                err_fn,
                None,
            )?)
        }
        fmt => anyhow::bail!("unsupported input sample format: {fmt:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Collector(Mutex<Vec<i16>>);

    impl PcmSink for Collector {
        fn push(&self, samples: &[i16]) {
            self.0.lock().unwrap().extend_from_slice(samples);
        }
    }

    // ── Upstream helpers ───────────────────────────────────────────────

    #[test]
    fn resample_16k_to_8k_halves_and_interpolates() {
        // Ramp 0..8 at 16 kHz → 4 samples at 8 kHz, each the average of
        // the straddled pair.
        let pcm: Vec<i16> = (0..8).collect();
        let out = resample_to_8k_mono(&pcm, 16_000.0, 1);
        assert_eq!(out, vec![0, 2, 4, 6]);
    }

    #[test]
    fn resample_native_8k_passthrough_and_stereo_downmix() {
        let pcm = [100i16, 200, 300, 400];
        assert_eq!(resample_to_8k_mono(&pcm, 8000.0, 1), pcm.to_vec());
        // Stereo pairs average to mono.
        assert_eq!(resample_to_8k_mono(&pcm, 8000.0, 2), vec![150, 350]);
    }

    #[test]
    fn chunker_yields_only_complete_20ms_frames() {
        let mut chunker = G711Chunker::new();
        // 100 bytes → nothing yet.
        assert!(chunker.push(&[7u8; 100]).is_empty());
        // +100 → one complete 160-byte frame, 40 retained.
        let frames = chunker.push(&[7u8; 100]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 160);
        // +120 → another frame (40+120=160), nothing retained.
        let frames = chunker.push(&[9u8; 120]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 160);
        assert!(chunker.buf.is_empty());
    }

    #[test]
    fn upstream_disabled_returns_none() {
        assert!(open_upstream(false).unwrap().is_none());
    }

    #[test]
    fn encode_g711_follows_the_negotiated_law() {
        let pcm = [0i16, 1000, -1000, i16::MAX, i16::MIN];
        let alaw = encode_g711(&pcm, AudioCodec::Pcma);
        let mulaw = encode_g711(&pcm, AudioCodec::Pcmu);
        assert_eq!(
            alaw,
            pcm.iter().map(|&s| pcm_to_alaw(s)).collect::<Vec<_>>()
        );
        assert_eq!(
            mulaw,
            pcm.iter().map(|&s| pcm_to_mulaw(s)).collect::<Vec<_>>()
        );
        // Distinct companding laws produce distinct bytes for the same PCM.
        assert_ne!(alaw, mulaw);
    }

    #[test]
    fn current_law_defaults_to_pcma_until_bound() {
        use gb28181_rs::server::TalkbackSource;
        let slot = std::sync::OnceLock::new();
        assert_eq!(current_law(&slot), AudioCodec::Pcma);
        let (_tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let _ = slot.set(Arc::new(TalkbackSource::new(rx)));
        assert_eq!(slot.get().unwrap().law(), AudioCodec::Pcma);
    }

    #[test]
    fn decoder_decodes_pcma_payload() {
        let out = Arc::new(Collector(Mutex::new(Vec::new())));
        let decoder = TalkbackDecoder::new(Arc::clone(&out) as Arc<dyn PcmSink>);
        let sink: Arc<dyn AudioTalkbackSink> = Arc::new(decoder);
        sink.on_audio_codec(&[0xD5, 0x5A, 0xA5], 200006001, AudioCodec::Pcma);
        assert_eq!(
            out.0.lock().unwrap().as_slice(),
            [alaw_to_pcm(0xD5), alaw_to_pcm(0x5A), alaw_to_pcm(0xA5)]
        );
    }

    #[test]
    fn decoder_decodes_pcmu_payload() {
        let out = Arc::new(Collector(Mutex::new(Vec::new())));
        let decoder = TalkbackDecoder::new(Arc::clone(&out) as Arc<dyn PcmSink>);
        let sink: Arc<dyn AudioTalkbackSink> = Arc::new(decoder);
        sink.on_audio_codec(&[0xFF, 0x7F], 777, AudioCodec::Pcmu);
        assert_eq!(
            out.0.lock().unwrap().as_slice(),
            [mulaw_to_pcm(0xFF), mulaw_to_pcm(0x7F)]
        );
    }

    #[test]
    fn upstream_buffer_targets_100ms_and_clamps() {
        // 8 kHz → 800 frames (100 ms); 48 kHz → 4800.
        assert_eq!(upstream_buffer_frames(8000, None, None), 800);
        assert_eq!(upstream_buffer_frames(48_000, None, None), 4800);
        // Never below one 20 ms G.711 frame even on odd rates.
        assert_eq!(upstream_buffer_frames(1000, None, None), 160);
        // Clamped into the device's supported range.
        assert_eq!(upstream_buffer_frames(8000, Some(1024), None), 1024);
        assert_eq!(upstream_buffer_frames(8000, None, Some(512)), 512);
        assert_eq!(upstream_buffer_frames(8000, Some(64), Some(512)), 512);
        // Degenerate hi < lo must not panic — lo wins.
        assert_eq!(upstream_buffer_frames(8000, Some(1024), Some(512)), 1024);
    }

    #[test]
    fn ring_drains_fifo_and_zero_fills() {
        let ring = PcmRing::new(8);
        ring.write(&[1, 2, 3]);
        let mut out = [0i16; 5];
        assert_eq!(ring.drain_into(&mut out), 3);
        assert_eq!(out, [1, 2, 3, 0, 0]);
        // Drained empty → pure silence.
        let mut out2 = [9i16; 2];
        assert_eq!(ring.drain_into(&mut out2), 0);
        assert_eq!(out2, [0, 0]);
    }

    #[test]
    fn ring_drops_oldest_beyond_cap() {
        let ring = PcmRing::new(4);
        ring.write(&[1, 2, 3, 4, 5, 6]);
        let mut out = [0i16; 8];
        assert_eq!(ring.drain_into(&mut out), 4);
        assert_eq!(out, [3, 4, 5, 6, 0, 0, 0, 0]);
    }

    #[test]
    fn expand_i16_interleaves_channels() {
        let mut out = Vec::new();
        expand_i16(&[10, -10], 2, &mut out);
        assert_eq!(out, [10, 10, -10, -10]);
    }

    #[test]
    fn expand_f32_interleaves_and_scales() {
        let mut out = Vec::new();
        expand_f32(&[16384], 1, &mut out);
        assert!((out[0] - 0.5).abs() < 1e-6, "got {}", out[0]);
    }

    #[test]
    fn open_sink_disabled_returns_none_without_touching_audio() {
        let opened = open_sink(false).expect("disabled must not error");
        assert!(opened.is_none());
    }
}
