//! Always-on microphone monitor producing a normalized 16 kHz mono stream.
//!
//! Unlike [`audio::AudioCapture`] (a per-session capture used by the GB28181
//! talkback upstream), the monitor is a process-lifetime singleton: it opens
//! the input device once, normalizes every callback to mono f32, resamples
//! to 16 kHz (preferably natively — most capture devices expose a 16 kHz
//! mono config; otherwise a stateful linear resampler in the worker thread),
//! and fans fixed 512-sample chunks out to any number of consumers
//! (audio-event classification today; wake-word/ASR later).
//!
//! The cpal callback stays real-time-safe: it only converts + downmixes and
//! `try_send`s into a bounded channel; slow consumers see dropped chunks via
//! the broadcast `Lagged` error, never blocked audio.

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig, SupportedStreamConfig};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Samples per monitor chunk (32 ms at 16 kHz — Silero VAD's native frame).
pub const CHUNK_SAMPLES: usize = 512;

/// Target rate of the normalized monitor stream.
pub const TARGET_RATE: u32 = 16_000;

/// One normalized monitor chunk: 16 kHz mono interleaved i16 PCM.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub samples: Arc<[i16]>,
}

/// Handle to the running monitor. Dropping it stops the stream.
pub struct AudioMonitor {
    stream: Option<cpal::Stream>,
    device_name: String,
    sample_rate: u32,
    native_16k: bool,
    /// Set when the audio fixture replaces the microphone (see
    /// [`fixture_path`]): the decoded mono samples and their rate.
    fixture: Option<std::sync::Arc<Vec<f32>>>,
}

impl AudioMonitor {
    /// Open the monitor on the default (or named) input device.
    ///
    /// `device = "default"` selects the host default input; any other value
    /// is matched by substring against the device description.
    ///
    /// With `MIBEE_EYE_AUDIO_FIXTURE=<wav>` set, no device is enumerated
    /// or opened: the monitor plays that file through the whole
    /// normalize→chunk pipeline instead (see [`fixture_path`]). An
    /// unreadable fixture is a hard error — a test facility must never
    /// silently fall back to a live microphone.
    pub fn open(device: &str) -> Result<Self> {
        if let Some(path) = fixture_path() {
            let (samples, rate) = decode_fixture(&path)?;
            info!(
                file = %path.display(),
                samples = samples.len(),
                sample_rate = rate,
                "audio monitor fixture active (no device opened)"
            );
            return Ok(Self {
                stream: None,
                device_name: format!("audio-fixture({})", path.display()),
                sample_rate: rate,
                native_16k: rate == TARGET_RATE,
                fixture: Some(std::sync::Arc::new(samples)),
            });
        }
        let host = cpal::default_host();
        let dev = if device.eq_ignore_ascii_case("default") {
            host.default_input_device()
                .context("no default audio input device (audio monitoring stays disabled)")?
        } else {
            host.input_devices()
                .context("failed to enumerate audio input devices")?
                .find(|d| {
                    d.description()
                        .map(|x| x.name().contains(device))
                        .unwrap_or(false)
                })
                .with_context(|| format!("no audio input device matching {device:?}"))?
        };
        let device_name = dev
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "<unknown>".into());

        // Prefer a native 16 kHz mono config so no resampling is needed.
        let (config, native_16k) = pick_config(&dev);
        let sample_rate = config.sample_rate();
        info!(
            device = %device_name,
            sample_rate,
            channels = config.channels(),
            format = ?config.sample_format(),
            native_16k,
            "audio monitor configured"
        );
        Ok(Self {
            stream: None,
            device_name,
            sample_rate,
            native_16k,
            fixture: None,
        })
    }

    /// Human-readable device description (for status logging).
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Whether the device was opened natively at 16 kHz (no resampling).
    #[must_use]
    pub fn is_native_16k(&self) -> bool {
        self.native_16k
    }

    /// Start streaming. Returns the chunk broadcast; the monitor keeps the
    /// cpal stream alive until dropped.
    pub fn start(
        &mut self,
    ) -> Result<(
        broadcast::Receiver<AudioChunk>,
        broadcast::Sender<AudioChunk>,
    )> {
        if let Some(samples) = self.fixture.clone() {
            return self.start_fixture(samples);
        }
        // Re-resolve the device: cpal devices are not clonable, and the
        // config we picked must belong to a live device handle.
        let dev = resolve_device(&self.device_name)?;
        let supported = pick_config(&dev).0;
        let stream_config: StreamConfig = supported.config();
        // Widen the device period deliberately: old onboard codecs
        // (ALC269-class) overrun their tiny default buffers whenever
        // inference threads preempt the audio callback on weak CPUs.
        // 4096 frames ≈ 93 ms at 44.1 kHz — latency is irrelevant to a
        // ≥ 32 ms-chunk classifier/wake-word stream, dropped audio is not.
        // (.41 field data: two voice stacks on a 2C/4T i3 still saw ~1.3
        // ALSA xruns/hour at 2048 — doubling the period doubles the
        // preemption tolerance; 2026-10-29.)
        //
        // The enumerated range can lie: PipeWire-era ALSA plugins advertise
        // a wide range but enforce the current quantum at open time, so the
        // preferred request may be rejected outright — fall back to the
        // device default (which the device picks within its true
        // constraints) instead of losing sound for the whole run.
        let preferred =
            if let cpal::SupportedBufferSize::Range { min, max } = supported.buffer_size() {
                Some(cpal::BufferSize::Fixed(4096u32.clamp(*min, *max)))
            } else {
                None
            };
        let channels = supported.channels();
        let sample_rate = supported.sample_rate();
        let format = supported.sample_format();

        let (raw_tx, raw_rx) = std_mpsc::sync_channel::<Vec<f32>>(64);
        let send_mono = move |data: &[f32], tx: &std_mpsc::SyncSender<Vec<f32>>| {
            let mono = downmix(data, channels);
            if tx.try_send(mono).is_err() {
                // Consumer behind — drop this callback's audio. The windowed
                // classifier tolerates gaps; blocking here would glitch.
                debug!("audio monitor: raw queue full, dropping callback");
            }
        };
        let build_stream = |buffer_size: cpal::BufferSize| -> Result<cpal::Stream> {
            let mut cfg = stream_config;
            cfg.buffer_size = buffer_size;
            let stream = match format {
                SampleFormat::F32 => dev
                    .build_input_stream(
                        cfg,
                        {
                            let tx = raw_tx.clone();
                            move |data: &[f32], _| send_mono(data, &tx)
                        },
                        |err| warn!("audio monitor error (f32): {err}"),
                        None,
                    )
                    .context("failed to build audio monitor stream (f32)")?,
                SampleFormat::I16 => dev
                    .build_input_stream(
                        cfg,
                        {
                            let tx = raw_tx.clone();
                            move |data: &[i16], _| {
                                let f: Vec<f32> =
                                    data.iter().map(|&s| s as f32 / 32_768.0).collect();
                                send_mono(&f, &tx);
                            }
                        },
                        |err| warn!("audio monitor error (i16): {err}"),
                        None,
                    )
                    .context("failed to build audio monitor stream (i16)")?,
                SampleFormat::U16 => dev
                    .build_input_stream(
                        cfg,
                        {
                            let tx = raw_tx.clone();
                            move |data: &[u16], _| {
                                let f: Vec<f32> = data
                                    .iter()
                                    .map(|&s| (s as f32 - 32_768.0) / 32_768.0)
                                    .collect();
                                send_mono(&f, &tx);
                            }
                        },
                        |err| warn!("audio monitor error (u16): {err}"),
                        None,
                    )
                    .context("failed to build audio monitor stream (u16)")?,
                SampleFormat::I8 => dev
                    .build_input_stream(
                        cfg,
                        {
                            let tx = raw_tx.clone();
                            move |data: &[i8], _| {
                                let f: Vec<f32> =
                                    data.iter().map(|&s| f32::from(s) / 128.0).collect();
                                send_mono(&f, &tx);
                            }
                        },
                        |err| warn!("audio monitor error (i8): {err}"),
                        None,
                    )
                    .context("failed to build audio monitor stream (i8)")?,
                other => bail!("audio monitor: unsupported sample format {other:?}"),
            };
            Ok(stream)
        };
        let stream = match preferred {
            Some(preferred) => match build_stream(preferred) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        error = format!("{e:#}"),
                        "audio monitor: period request rejected — falling back to the device default"
                    );
                    build_stream(cpal::BufferSize::Default)
                        .context("failed to build audio monitor stream (device default)")?
                }
            },
            None => build_stream(cpal::BufferSize::Default)
                .context("failed to build audio monitor stream (device default)")?,
        };
        stream
            .play()
            .context("failed to start audio monitor stream")?;
        self.stream = Some(stream);

        let (chunk_tx, chunk_rx) = broadcast::channel::<AudioChunk>(32);
        let worker_tx = chunk_tx.clone();
        let ratio = f64::from(sample_rate) / f64::from(TARGET_RATE);
        std::thread::Builder::new()
            .name("audio-monitor".into())
            .spawn(move || {
                let mut resampler = LinearResampler::new(ratio);
                let mut acc: Vec<i16> = Vec::with_capacity(CHUNK_SAMPLES * 2);
                while let Ok(mono) = raw_rx.recv() {
                    let out = resampler.process(&mono);
                    for s in out {
                        let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
                        acc.push(v);
                    }
                    while acc.len() >= CHUNK_SAMPLES {
                        let rest = acc.split_off(CHUNK_SAMPLES);
                        let chunk = AudioChunk {
                            samples: Arc::from(acc.as_slice()),
                        };
                        acc = rest;
                        if worker_tx.send(chunk).is_err() {
                            return; // every receiver dropped — stop the worker
                        }
                    }
                }
                debug!("audio monitor worker exited (stream closed)");
            })
            .context("failed to spawn audio monitor worker")?;

        self.sample_rate = sample_rate;
        info!(
            device = %self.device_name,
            sample_rate,
            "audio monitor started ({} chunks/s)",
            TARGET_RATE / CHUNK_SAMPLES as u32
        );
        Ok((chunk_rx, chunk_tx))
    }

    /// Fixture variant of [`Self::start`]: a pacing thread plays the decoded
    /// samples through the same normalize→chunk pipeline at real-time pace —
    /// one [`CHUNK_SAMPLES`]-sample (32 ms) window per tick, looping from the
    /// start when the file runs out. A late wake on a busy machine delivers
    /// the chunk late, never early: the deadline advances on the clock, not
    /// on the previous sleep (drift-free over long runs).
    fn start_fixture(
        &mut self,
        samples: std::sync::Arc<Vec<f32>>,
    ) -> Result<(
        broadcast::Receiver<AudioChunk>,
        broadcast::Sender<AudioChunk>,
    )> {
        let rate = self.sample_rate;
        let (chunk_tx, chunk_rx) = broadcast::channel::<AudioChunk>(32);
        let worker_tx = chunk_tx.clone();
        let ratio = f64::from(rate) / f64::from(TARGET_RATE);
        // Source samples consumed per emitted 32 ms chunk (rounded up; the
        // stateful resampler absorbs the rounding).
        let window = (CHUNK_SAMPLES as f64 * ratio).ceil() as usize;
        let period = std::time::Duration::from_millis(
            (CHUNK_SAMPLES as u64 * 1000) / u64::from(TARGET_RATE),
        );
        std::thread::Builder::new()
            .name("audio-monitor-fixture".into())
            .spawn(move || {
                let mut resampler = LinearResampler::new(ratio);
                let mut acc: Vec<i16> = Vec::with_capacity(CHUNK_SAMPLES * 2);
                let mut pos = 0usize;
                // Absolute pacing: the first window is due immediately.
                let mut next_due = std::time::Instant::now();
                // Boot grace: `start()` returns the receiver before the
                // engine workers subscribe (µs later on the caller side);
                // a send with zero subscribers would error and kill this
                // worker before anyone listens. The real cpal path never
                // hits this — its first chunk waits 32 ms+ of device
                // warmup. Wait for the first subscriber, then a drop to
                // zero really means "everyone left".
                let mut seen_subscriber = false;
                loop {
                    if worker_tx.receiver_count() == 0 {
                        if seen_subscriber {
                            return; // every receiver dropped — stop
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        next_due = std::time::Instant::now();
                        continue;
                    }
                    seen_subscriber = true;
                    let mut input = Vec::with_capacity(window);
                    while input.len() < window {
                        let remaining = &samples[pos..];
                        let take = remaining.len().min(window - input.len());
                        input.extend_from_slice(&remaining[..take]);
                        pos += take;
                        if pos >= samples.len() {
                            pos = 0; // loop the clip from its start
                        }
                    }
                    for s in resampler.process(&input) {
                        let v = (s.clamp(-1.0, 1.0) * 32_767.0) as i16;
                        acc.push(v);
                    }
                    while acc.len() >= CHUNK_SAMPLES {
                        let rest = acc.split_off(CHUNK_SAMPLES);
                        let chunk = AudioChunk {
                            samples: Arc::from(acc.as_slice()),
                        };
                        acc = rest;
                        if worker_tx.send(chunk).is_err() {
                            return;
                        }
                    }
                    next_due += period;
                    let now = std::time::Instant::now();
                    if next_due > now {
                        std::thread::sleep(next_due - now);
                    }
                }
            })
            .context("failed to spawn audio monitor fixture worker")?;
        info!(
            device = %self.device_name,
            sample_rate = rate,
            "audio monitor fixture started ({} chunks/s)",
            TARGET_RATE / CHUNK_SAMPLES as u32
        );
        Ok((chunk_rx, chunk_tx))
    }
}

impl Drop for AudioMonitor {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream);
            debug!(device = %self.device_name, "audio monitor stream stopped");
        }
    }
}

/// `MIBEE_EYE_AUDIO_FIXTURE`: a WAV file that stands in for the microphone,
/// so tests and benchmarks drive the real normalize→chunk pipeline (and
/// everything downstream: KWS, ASR, VAD, the level meter) without recording
/// anyone and without audio hardware. Read once per process; empty unsets.
fn fixture_path() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    {
        // Tests swap fixtures; the process-wide env cache below is for
        // production determinism only.
        if let Some(p) = tests::fixture_override() {
            return Some(p);
        }
    }
    static PATH: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os("MIBEE_EYE_AUDIO_FIXTURE")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    })
    .clone()
}

/// Decode a fixture WAV to mono f32 at its source rate. Any bit depth
/// hound reads is accepted; stereo+ is averaged down to mono.
fn decode_fixture(path: &std::path::Path) -> Result<(Vec<f32>, u32)> {
    let mut reader = hound::WavReader::open(path)
        .with_context(|| format!("fixture open failed: {}", path.display()))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    if channels == 0 {
        bail!("audio fixture has zero channels: {}", path.display());
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("fixture decode failed (f32)")?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("fixture decode failed (int)")?
        }
    };
    if samples.is_empty() {
        bail!("audio fixture is empty: {}", path.display());
    }
    let mono = downmix(&samples, spec.channels);
    Ok((mono, spec.sample_rate))
}

fn resolve_device(name: &str) -> Result<Device> {
    let host = cpal::default_host();
    let by_default = || {
        host.default_input_device()
            .context("no default audio input device")
    };
    if name == "<unknown>" {
        return by_default();
    }
    // The default device's description may be a placeholder ("Default
    // Audio Device") that never matches a real device name — re-resolve
    // by default when the stored name is that placeholder.
    if let Ok(dev) = &by_default()
        && let Ok(desc) = dev.description()
        && desc.name() == name
    {
        return by_default();
    }
    host.input_devices()
        .context("failed to enumerate audio input devices")?
        .find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
        .with_context(|| format!("audio device {name:?} disappeared"))
}

/// Pick the best config: exact 16 kHz mono when offered, else the device
/// default (the worker resamples). Returns the config and whether it is
/// natively 16 kHz mono.
fn pick_config(dev: &Device) -> (SupportedStreamConfig, bool) {
    if let Ok(configs) = dev.supported_input_configs() {
        // Prefer a 16 kHz mono range in a directly handled format; fall
        // back to any 16 kHz mono range (the I8 branch below converts).
        let usable = |f: cpal::SampleFormat| {
            matches!(
                f,
                cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
            )
        };
        let mut any_16k_mono = None;
        for range in configs {
            if range.channels() == 1
                && range.min_sample_rate() <= TARGET_RATE
                && range.max_sample_rate() >= TARGET_RATE
            {
                let cfg = range.with_sample_rate(TARGET_RATE);
                if usable(cfg.sample_format()) {
                    return (cfg, true);
                }
                any_16k_mono.get_or_insert(cfg);
            }
        }
        if let Some(cfg) = any_16k_mono {
            return (cfg, true);
        }
    }
    match dev.default_input_config() {
        Ok(cfg) => (cfg, false),
        Err(e) => {
            warn!("audio monitor: no input config ({e}); using 16k mono best-effort");
            // Last resort: synthesize a 16 kHz mono config and hope the
            // device accepts it.
            let cfg = SupportedStreamConfig::new(
                1,
                TARGET_RATE,
                cpal::SupportedBufferSize::Range { min: 64, max: 8192 },
                SampleFormat::F32,
            );
            (cfg, true)
        }
    }
}

/// Average interleaved samples down to mono.
fn downmix(data: &[f32], channels: u16) -> Vec<f32> {
    match channels {
        1 => data.to_vec(),
        n => {
            let n = n as usize;
            data.chunks(n)
                .map(|frame| frame.iter().sum::<f32>() / n as f32)
                .collect()
        }
    }
}

/// Stateful linear resampler (input rate / 16 kHz → step). Linear
/// interpolation is sufficient for the 64-mel/7.5 kHz-band classifiers this
/// stream feeds. Fractional phase and the interpolation neighborhood carry
/// across calls via a small leftover buffer, so chunk boundaries stay
/// continuous (no lost or duplicated samples at 1:1).
pub struct LinearResampler {
    /// Input samples per output sample (>= 1).
    step: f64,
    /// Fractional read position into `carry`.
    pos: f64,
    /// Input samples not yet consumed (usually 1-2).
    carry: Vec<f32>,
}

impl LinearResampler {
    #[must_use]
    pub fn new(ratio: f64) -> Self {
        Self {
            step: ratio.max(1e-6),
            pos: 0.0,
            carry: Vec::new(),
        }
    }

    /// Resample one buffer of input samples to the target rate. The final
    /// sample(s) may be held back until the next call when interpolating
    /// them would need future input.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        self.carry.extend_from_slice(input);
        let mut out = Vec::with_capacity((input.len() as f64 / self.step).ceil() as usize + 2);
        while self.pos + 1.0 < self.carry.len() as f64 {
            let k = self.pos.floor() as usize;
            let frac = (self.pos - k as f64) as f32;
            let prev = self.carry[k];
            let cur = self.carry[k + 1];
            out.push(prev + (cur - prev) * frac);
            self.pos += self.step;
        }
        let drop = self.pos.floor() as usize;
        self.carry.drain(..drop);
        self.pos -= drop as f64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_ratio_passthrough_with_one_sample_latency() {
        // The carry holds the final sample until the next call so it can be
        // interpolated against future input; over the stream the output is
        // the exact input (delayed by up to one sample).
        let mut r = LinearResampler::new(1.0);
        let mut all = r.process(&[0.0, 0.1, 0.2, 0.3]);
        all.extend_from_slice(&r.process(&[0.4, 0.5]));
        assert_eq!(&all[..5], &[0.0, 0.1, 0.2, 0.3, 0.4]);
        assert!(r.process(&[0.6]).is_empty() || !r.process(&[0.0]).is_empty());
    }

    #[test]
    fn downsampling_halves_count_and_keeps_signal() {
        // 32 kHz → 16 kHz: a 440 Hz sine stays a (lower-rate) sine.
        let mut r = LinearResampler::new(2.0);
        let input: Vec<f32> = (0..200)
            .map(|i| ((2.0 * std::f64::consts::PI * 440.0 * i as f64) / 32_000.0).sin() as f32)
            .collect();
        let out = r.process(&input);
        assert_eq!(out.len(), 100);
        // Energy preserved within a factor (sine, not silence).
        let energy: f32 = out.iter().map(|s| s * s).sum();
        assert!(energy > 10.0, "downsampled signal lost energy: {energy}");
    }

    #[test]
    fn continuity_across_calls() {
        // A rising ramp must stay monotonic across chunk boundaries.
        let mut r = LinearResampler::new(2.0);
        let a = r.process(&[0.0, 0.2, 0.4, 0.6, 0.8, 1.0]);
        let b = r.process(&[1.2, 1.4, 1.6, 1.8, 2.0, 2.2]);
        let mut all = a.clone();
        all.extend_from_slice(&b);
        for w in all.windows(2) {
            assert!(
                w[1] >= w[0] - 1e-4,
                "ramp not monotonic across chunks: {all:?}"
            );
        }
    }

    #[test]
    fn downmix_stereo() {
        let mono = downmix(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2);
        assert_eq!(mono, vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn open_fails_cleanly_without_device_name_match() {
        // A device substring that cannot exist fails open with an error
        // (callers treat this as "monitor disabled").
        let r = AudioMonitor::open("definitely-not-a-device-xyz");
        assert!(r.is_err());
    }

    // ─── Audio fixture (MIBEE_EYE_AUDIO_FIXTURE) ────────────────────────

    /// Test seam for [`fixture_path`]. `FIXTURE` is touched only briefly
    /// (never held across an `open()` call — `fixture_override` locks it
    /// again internally); `SERIAL` serializes the fixture tests
    /// themselves.
    pub(super) static FIXTURE: std::sync::Mutex<Option<std::path::PathBuf>> =
        std::sync::Mutex::new(None);
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn fixture_override() -> Option<std::path::PathBuf> {
        FIXTURE.lock().map(|g| g.clone()).unwrap_or(None)
    }

    fn set_fixture(path: Option<std::path::PathBuf>) {
        *FIXTURE.lock().unwrap() = path;
    }

    /// Write a sine WAV at 16 kHz mono 16-bit; returns its path.
    fn write_sine_wav(dir: &std::path::Path, secs: f32, freq: f32) -> std::path::PathBuf {
        let path = dir.join(format!("fixture-{freq}.wav"));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        let n = (f64::from(TARGET_RATE) * f64::from(secs)) as u32;
        for i in 0..n {
            let t = i as f32 / TARGET_RATE as f32;
            let v = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5;
            writer.write_sample((v * 32_767.0) as i16).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn fixture_open_bypasses_devices_and_bad_paths_fail_hard() {
        let _serial = SERIAL.lock().unwrap();
        let dir = std::env::temp_dir().join("mibee-audio-fixture-test");
        std::fs::create_dir_all(&dir).unwrap();

        // A nonexistent fixture is a hard error — never a silent mic.
        set_fixture(Some(dir.join("missing.wav")));
        let err = AudioMonitor::open("default");
        assert!(err.is_err(), "missing fixture must fail open()");

        // A valid fixture replaces the device entirely.
        let wav = write_sine_wav(&dir, 0.2, 440.0);
        set_fixture(Some(wav.clone()));
        let monitor = AudioMonitor::open("definitely-not-a-device-xyz").unwrap();
        assert!(
            monitor.device_name().starts_with("audio-fixture("),
            "{}",
            monitor.device_name()
        );
        assert!(monitor.is_native_16k());
        set_fixture(None);
    }

    #[test]
    fn fixture_start_streams_512_sample_chunks_at_realtime_pace() {
        let _serial = SERIAL.lock().unwrap();
        let dir = std::env::temp_dir().join("mibee-audio-fixture-test");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = write_sine_wav(&dir, 0.2, 440.0);
        set_fixture(Some(wav));
        let mut monitor = AudioMonitor::open("default").unwrap();
        let (mut rx, _tx) = monitor.start().unwrap();
        // First chunk arrives within a couple of 32 ms windows.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        let first = loop {
            match rx.try_recv() {
                Ok(chunk) => break chunk,
                Err(broadcast::error::TryRecvError::Empty)
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    panic!("first fixture chunk within 300 ms")
                }
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => panic!("fixture worker exited"),
            }
        };
        assert_eq!(first.samples.len(), CHUNK_SAMPLES);
        let peak = first.samples.iter().map(|&s| s.abs()).max().unwrap_or(0);
        assert!(peak > 8_000, "sine at 0.5 amplitude, peak={peak}");
        set_fixture(None);
        drop(monitor);
    }

    /// Loop proof: a 0.3 s clip keeps producing chunks well past its own
    /// duration (real-time pacing + restart-from-beginning). Ignored by
    /// default because it costs ~1.2 s of wall clock.
    #[test]
    #[ignore = "wall-clock pacing test (~1.2 s)"]
    fn fixture_loops_past_the_clip_end() {
        let _serial = SERIAL.lock().unwrap();
        let dir = std::env::temp_dir().join("mibee-audio-fixture-test");
        std::fs::create_dir_all(&dir).unwrap();
        let wav = write_sine_wav(&dir, 0.3, 880.0);
        set_fixture(Some(wav));
        let mut monitor = AudioMonitor::open("default").unwrap();
        let (mut rx, _tx) = monitor.start().unwrap();
        let started = std::time::Instant::now();
        let mut chunks = 0usize;
        while started.elapsed() < std::time::Duration::from_millis(1_150) {
            match rx.try_recv() {
                Ok(_) => chunks += 1,
                Err(broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(broadcast::error::TryRecvError::Closed) => panic!("fixture worker exited"),
                Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            }
        }
        // 0.3 s of audio = ~9 chunks; 1.15 s at 31 chunks/s ≈ 36 expected
        // past the loop point. Demand comfortably more than the clip holds.
        assert!(
            chunks > 25,
            "expected looped streaming, got {chunks} chunks"
        );
        set_fixture(None);
    }
}
