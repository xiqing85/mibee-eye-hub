//! Output trait and concrete output adapters.
//!
//! [`Output`] is the consumer side of the stream pipeline — it receives
//! [`MediaFrame`]s  and delivers them to a downstream destination
//! (RTSP clients, RTMP push target, etc.).
//!
//! Concrete adapters:
//! - [`RtspOutput`] — feeds frames into an RTSP server for client distribution
//! - [`RtmpOutput`] — pushes frames via RTMP to an ingest point (e.g. MiBee NVR)

pub mod file;
#[cfg(test)]
pub mod mock;
pub mod rtmp;
pub mod rtsp;

pub use file::FileOutput;
pub use file::SegmentSlot;
#[cfg(test)]
pub use mock::MockOutput;
pub use rtmp::RtmpOutput;
pub use rtsp::RtspOutput;

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::source::MediaFrame;
use protocols::h264;

// ── Output trait ───────────────────────────────────────────────────────────────

/// Async consumer of media frames.
///
/// Implementors take [`MediaFrame`] values and deliver them somewhere
/// (network stream, file, another process, …).
///
/// # Lifetimes
///
/// Like [`Source`](crate::source::Source), each method returns a pinned
/// boxed future tied to `&mut self` for trait-object safety.
pub trait Output: Send + 'static {
    /// Start the output (open connection / bind listener).
    fn start(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Deliver a frame to the output.
    ///
    /// Blocks (asynchronously) until the frame has been handed off.
    /// Returns [`Err`] on permanent failure (output should be stopped).
    fn send_frame(
        &mut self,
        frame: &MediaFrame,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Stop the output and release resources.
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

/// Parse H.264 data into individual NAL units, handling both Annex B and
/// AVCC formats.
pub fn parse_h264_nal_units(data: &[u8]) -> Vec<Vec<u8>> {
    if data.len() < 4 {
        return vec![data.to_vec()];
    }

    // Detect format: check for Annex B start code (0x00 0x00 0x01 or
    // 0x00 0x00 0x00 0x01) anywhere in the data.
    let is_annex_b = data.windows(3).any(|w| w == [0x00, 0x00, 0x01]);

    if is_annex_b {
        h264::split_nal_units(data)
            .iter()
            .map(|n| n.to_vec())
            .collect()
    } else {
        // Assume AVCC format (4-byte length prefix)
        h264::split_nal_units_avcc(data)
            .iter()
            .map(|n| n.to_vec())
            .collect()
    }
}

// Preserve the `crate::output::tests::MockOutput` path used by hub.rs tests.
#[cfg(test)]
pub(crate) mod tests {
    pub(crate) use super::mock::MockOutput;
}

#[cfg(test)]
mod wire_tests {
    use super::parse_h264_nal_units;

    #[test]
    fn short_input_passes_through_whole() {
        assert_eq!(parse_h264_nal_units(&[0x00, 0x01]), vec![vec![0x00, 0x01]]);
        assert_eq!(parse_h264_nal_units(b""), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn annex_b_start_codes_split() {
        // Two NAL units with 4-byte and 3-byte start codes.
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x65, 0x11, 0x00, 0x00, 0x01, 0x41, 0x22,
        ];
        let nals = parse_h264_nal_units(&data);
        assert_eq!(nals.len(), 2, "start codes must split: {nals:?}");
        assert_eq!(nals[0], vec![0x65, 0x11]);
        assert_eq!(nals[1], vec![0x41, 0x22]);
    }

    #[test]
    fn avcc_length_prefixes_split() {
        // No Annex B start code → AVCC: each unit is [len: u32 BE][payload].
        // Length prefixes are chosen so no [0,0,1] byte triple appears —
        // a length of 1 would encode as 00 00 00 01 and route this data
        // down the Annex B path instead (the detector cannot tell a
        // 4-byte start code from a length prefix that happens to be 1).
        let mut data = Vec::new();
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&[0x65, 0x11]);
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&[0x41, 0x42, 0x43, 0x44]);
        let nals = parse_h264_nal_units(&data);
        assert_eq!(nals.len(), 2, "length prefixes must split: {nals:?}");
        assert_eq!(nals[0], vec![0x65, 0x11]);
        assert_eq!(nals[1], vec![0x41, 0x42, 0x43, 0x44]);
    }
}
