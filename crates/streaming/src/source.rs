//! Source trait and the unit of media data flowing through the pipeline.
//!
//! Defines [`MediaFrame`] — the unit of media data flowing through the
//! pipeline — and [`Source`], the async trait that produces frames.
//! Only local capture sources (via [`CaptureSource`]) are used.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

// ── MediaFrame ─────────────────────────────────────────────────────────────────

/// A single media frame — either video (H.264 NAL unit data) or audio (PCM/G.711).
#[derive(Debug, Clone, PartialEq)]
pub enum MediaFrame {
    /// Video frame: H.264 NAL unit data.
    Video {
        /// Whether this frame is a keyframe (IDR).
        keyframe: bool,
        /// Raw NAL unit data (Annex B or AVCC format).
        data: Vec<u8>,
        /// Presentation timestamp in milliseconds.
        timestamp: u64,
    },
    /// Audio frame: PCM or G.711 encoded data.
    Audio {
        /// Raw audio data.
        data: Vec<u8>,
        /// Presentation timestamp in milliseconds.
        timestamp: u64,
    },
}

impl MediaFrame {
    /// Return the timestamp of this frame.
    pub fn timestamp(&self) -> u64 {
        match self {
            MediaFrame::Video { timestamp, .. } | MediaFrame::Audio { timestamp, .. } => *timestamp,
        }
    }

    /// Return the raw data of this frame.
    pub fn data(&self) -> &[u8] {
        match self {
            MediaFrame::Video { data, .. } | MediaFrame::Audio { data, .. } => data,
        }
    }
}

// ── Source trait ───────────────────────────────────────────────────────────────

/// Async source of media frames.
///
/// Implementors wrap a capture device, network stream, or file and produce
/// [`MediaFrame`] values via [`next_frame`](Source::next_frame).
///
/// # Lifetimes
///
/// Each method returns a pinned, boxed future whose lifetime is tied to
/// `&mut self` — the future may borrow `self` while pending. This makes
/// the trait fully object-safe for `Box<dyn Source>`.
pub trait Source: Send + 'static {
    /// Start the source (open device / connect to stream).
    fn start(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Produce the next frame.
    ///
    /// Blocks (asynchronously) until a frame is available.
    /// Returns [`Err`] on permanent failure (the source should be stopped).
    fn next_frame(&mut self) -> Pin<Box<dyn Future<Output = Result<MediaFrame>> + Send + '_>>;

    /// Stop the source and release resources.
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

// ── Tests ──────────────────────────────────────────────────────────────────────

/// Test seam (2026-10-21): a scripted [`Source`] for integration tests in
/// dependent crates — the stream manager's pipeline tests feed it canned
/// frames the way the AI/voice engines take fake detectors. Compiled
/// always: it is a few dozen lines, and feature-gating a test double
/// costs more surface than the double itself. Production config never
/// selects it; see stream_manager's `"test"` camera type.
#[doc(hidden)]
pub mod testing {
    use super::{MediaFrame, Result, Source};
    use std::future::Future;
    use std::pin::Pin;

    /// A source that yields predefined frames, then reports exhaustion.
    pub struct MockSource {
        frames: Vec<MediaFrame>,
        started: bool,
        index: usize,
    }

    impl MockSource {
        pub fn new(frames: Vec<MediaFrame>) -> Self {
            Self {
                frames,
                started: false,
                index: 0,
            }
        }

        pub fn empty() -> Self {
            Self::new(vec![])
        }
    }

    impl Source for MockSource {
        fn start(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.started = true;
                Ok(())
            })
        }

        fn next_frame(&mut self) -> Pin<Box<dyn Future<Output = Result<MediaFrame>> + Send + '_>> {
            Box::pin(async move {
                if !self.started {
                    anyhow::bail!("MockSource not started");
                }
                if self.index >= self.frames.len() {
                    anyhow::bail!("MockSource exhausted");
                }
                let frame = self.frames[self.index].clone();
                self.index += 1;
                Ok(frame)
            })
        }

        fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            Box::pin(async move {
                self.started = false;
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::MockSource;
    use super::*;

    // ── MediaFrame tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_media_frame_video() {
        let frame = MediaFrame::Video {
            keyframe: true,
            data: vec![0x00, 0x00, 0x00, 0x01, 0x67],
            timestamp: 1000,
        };
        assert!(frame.timestamp() == 1000);
        assert!(frame.data() == [0x00, 0x00, 0x00, 0x01, 0x67]);
        assert!(matches!(frame, MediaFrame::Video { .. }));
    }

    #[test]
    fn test_media_frame_audio() {
        let frame = MediaFrame::Audio {
            data: vec![0xFF; 160],
            timestamp: 500,
        };
        assert!(frame.timestamp() == 500);
        assert!(frame.data().len() == 160);
        assert!(matches!(frame, MediaFrame::Audio { .. }));
    }

    #[test]
    fn test_media_frame_clone_eq() {
        let a = MediaFrame::Video {
            keyframe: false,
            data: vec![1, 2, 3],
            timestamp: 0,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── MockSource tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_mock_source_lifecycle() {
        let frames = vec![
            MediaFrame::Video {
                keyframe: true,
                data: vec![0x67],
                timestamp: 0,
            },
            MediaFrame::Audio {
                data: vec![0xAA],
                timestamp: 33,
            },
        ];
        let mut src = MockSource::new(frames.clone());

        // Start
        src.start().await.unwrap();

        // Read first frame
        let f1 = src.next_frame().await.unwrap();
        assert_eq!(f1, frames[0]);

        // Read second frame
        let f2 = src.next_frame().await.unwrap();
        assert_eq!(f2, frames[1]);

        // Source exhausted
        assert!(src.next_frame().await.is_err());

        // Stop
        src.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_mock_source_not_started() {
        let mut src = MockSource::new(vec![MediaFrame::Video {
            keyframe: false,
            data: vec![],
            timestamp: 0,
        }]);
        assert!(src.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn test_mock_source_empty() {
        let mut src = MockSource::empty();
        src.start().await.unwrap();
        assert!(src.next_frame().await.is_err());
    }
}
