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
}

impl AudioMonitor {
    /// Open the monitor on the default (or named) input device.
    ///
    /// `device = "default"` selects the host default input; any other value
    /// is matched by substring against the device description.
    pub fn open(device: &str) -> Result<Self> {
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
}

impl Drop for AudioMonitor {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            drop(stream);
            debug!(device = %self.device_name, "audio monitor stream stopped");
        }
    }
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
}
