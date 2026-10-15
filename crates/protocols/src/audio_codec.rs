/// G.711 constants (ITU-T G.711 §2 — μ-law bias/clip; A-law 13-bit folding)
const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

/// Compress 16-bit PCM sample to 8-bit μ-law (ITU-T G.711).
///
/// Standard 16-bit formulation: clip, add BIAS (0x84), pick the segment from
/// the thresholds `0x0100 << k`, and invert all bits of the composed code.
/// Linear 0 therefore encodes to 0xFF and full scale to 0x80 / 0x00, matching
/// the G.711 tables byte-for-byte.
pub fn pcm_to_mulaw(sample: i16) -> u8 {
    let sign = if sample < 0 { 0x80u8 } else { 0u8 };
    let mut mag = (sample.unsigned_abs() as i32).min(CLIP);
    mag += BIAS;

    let segment = if mag >= 0x4000 {
        7
    } else if mag >= 0x2000 {
        6
    } else if mag >= 0x1000 {
        5
    } else if mag >= 0x0800 {
        4
    } else if mag >= 0x0400 {
        3
    } else if mag >= 0x0200 {
        2
    } else if mag >= 0x0100 {
        1
    } else {
        0
    };

    let mantissa = ((mag >> (segment + 3)) & 0x0F) as u8;
    let encoded = sign | ((segment as u8) << 4) | mantissa;
    !encoded
}

/// Decompress 8-bit μ-law to 16-bit PCM sample (ITU-T G.711).
///
/// `((mantissa << 3) + BIAS) << segment - BIAS` reproduces the G.711 decoder
/// table: 0xFF → 0, 0xFE → 8, …, 0x80 → 32124.
pub fn mulaw_to_pcm(encoded: u8) -> i16 {
    let u = !(encoded as i32) & 0xFF;
    let sign = u & 0x80;
    let segment = (u >> 4) & 0x07;
    let mantissa = u & 0x0F;

    let mag = (((mantissa << 3) + BIAS) << segment) - BIAS;
    if sign != 0 { (-mag) as i16 } else { mag as i16 }
}

/// Compress 16-bit PCM sample to 8-bit A-law (ITU-T G.711).
///
/// Classic 13-bit formulation (as in the ITU / Sun reference): fold to 13
/// bits, negate-as-complement for negatives, segment on `0x20 << k`, extract
/// the step (`>> 1` in chords 0–1, `>> segment` above), and XOR with the
/// sign mask (0xD5 positive / 0x55 negative) so the code word carries even
/// parity. Linear 0 encodes to 0xD5 and full scale to 0xAA / 0x2A.
pub fn pcm_to_alaw(sample: i16) -> u8 {
    let pcm = (sample as i32) >> 3; // fold to 13 bits
    let mask: u8 = if pcm >= 0 { 0xD5 } else { 0x55 };
    let mag = if pcm >= 0 { pcm } else { -pcm - 1 };

    let segment = if mag >= 0x0800 {
        7
    } else if mag >= 0x0400 {
        6
    } else if mag >= 0x0200 {
        5
    } else if mag >= 0x0100 {
        4
    } else if mag >= 0x0080 {
        3
    } else if mag >= 0x0040 {
        2
    } else if mag >= 0x0020 {
        1
    } else {
        0
    };

    let aval = if segment >= 8 {
        0x7F
    } else {
        let step = if segment < 2 {
            (mag >> 1) & 0x0F
        } else {
            (mag >> segment) & 0x0F
        };
        ((segment as u8) << 4) | step as u8
    };
    aval ^ mask
}

/// Decompress 8-bit A-law to 16-bit PCM sample (ITU-T G.711).
///
/// XOR off the parity mask, then `t = (step << 4) + 8` for chord 0 or
/// `t = (step << 4) + 0x108) << (chord - 1)` above — the G.711 decoder table
/// (0xD5 → 8, 0x8A → 8064, 0xAA → 32256, 0x2A → −32256).
pub fn alaw_to_pcm(encoded: u8) -> i16 {
    let a = (encoded ^ 0x55) as i32;
    let sign = a & 0x80;
    let segment = (a >> 4) & 0x07;
    let step = a & 0x0F;

    let mut t = step << 4;
    if segment == 0 {
        t += 8;
    } else {
        t = (t + 0x108) << (segment - 1);
    }

    if sign != 0 { t as i16 } else { (-t) as i16 }
}

/// RTP header structure
#[derive(Debug, Clone)]
struct RtpHeader {
    version: u8,
    padding: bool,
    extension: bool,
    csrc_count: u8,
    marker: bool,
    payload_type: u8,
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
}

impl RtpHeader {
    fn new(payload_type: u8, sequence_number: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            version: 2,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
        }
    }

    fn to_bytes(&self) -> [u8; 12] {
        let mut bytes = [0u8; 12];

        // Byte 0: V (2) + P (1) + X (1) + CC (4)
        bytes[0] = (self.version << 6)
            | ((self.padding as u8) << 5)
            | ((self.extension as u8) << 4)
            | self.csrc_count;

        // Byte 1: M (1) + PT (7)
        bytes[1] = ((self.marker as u8) << 7) | self.payload_type;

        // Bytes 2-3: Sequence number
        bytes[2..4].copy_from_slice(&self.sequence_number.to_be_bytes());

        // Bytes 4-7: Timestamp
        bytes[4..8].copy_from_slice(&self.timestamp.to_be_bytes());

        // Bytes 8-11: SSRC
        bytes[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        bytes
    }
}

/// RTP audio packetizer for G.711 μ-law/A-law
#[derive(Debug, Clone)]
pub struct RtpAudioPacketizer {
    sequence_number: u16,
}

impl RtpAudioPacketizer {
    /// Create a new RTP audio packetizer
    pub fn new() -> Self {
        Self { sequence_number: 0 }
    }

    /// Reset sequence number
    pub fn reset(&mut self) {
        self.sequence_number = 0;
    }

    /// Packetize PCM samples into RTP packets with μ-law encoding
    ///
    /// # Arguments
    /// * `pcm_samples` - Raw 16-bit PCM samples
    /// * `sample_rate` - Sample rate in Hz (8000 for G.711 standard)
    /// * `channels` - Number of audio channels (1 for mono, 2 for stereo)
    /// * `timestamp` - Initial RTP timestamp
    /// * `ssrc` - Synchronization source identifier
    ///
    /// # Returns
    /// Vector of RTP packets (header + payload)
    ///
    /// # Note
    /// G.711 is designed for 8kHz audio. For other sample rates, this function
    /// will document the limitation but will still packetize (may require resampling).
    pub fn packetize_pcm_ulaw(
        &mut self,
        pcm_samples: &[i16],
        sample_rate: u32,
        channels: u16,
        timestamp: u32,
        ssrc: u32,
    ) -> Vec<Vec<u8>> {
        self.packetize_pcm(pcm_samples, sample_rate, channels, timestamp, ssrc, 0) // Payload type 0 = PCMU
    }

    /// Packetize PCM samples into RTP packets with A-law encoding
    ///
    /// Same parameters as `packetize_pcm_ulaw` but uses A-law (payload type 8)
    pub fn packetize_pcm_alaw(
        &mut self,
        pcm_samples: &[i16],
        sample_rate: u32,
        channels: u16,
        timestamp: u32,
        ssrc: u32,
    ) -> Vec<Vec<u8>> {
        self.packetize_pcm(pcm_samples, sample_rate, channels, timestamp, ssrc, 8) // Payload type 8 = PCMA
    }

    fn packetize_pcm(
        &mut self,
        pcm_samples: &[i16],
        sample_rate: u32,
        channels: u16,
        timestamp: u32,
        ssrc: u32,
        payload_type: u8,
    ) -> Vec<Vec<u8>> {
        // G.711 standard is 8kHz. Document limitation for other rates.
        if sample_rate != 8000 {
            tracing::warn!(
                "G.711 is designed for 8kHz audio. Current rate: {}Hz. May require resampling.",
                sample_rate
            );
        }

        // Calculate samples per packet (20ms = 1/50 second).
        //
        // Both divisions below are guarded. Previously `sample_rate / 50`
        // truncated to 0 for any rate under 50 Hz, which made `end_offset`
        // equal `sample_offset` on every iteration — an infinite loop that
        // pushed a new packet per spin and grew the Vec until the allocator
        // gave up, with `channels == 0` panicking one line earlier.
        let samples_per_packet = ((sample_rate / 50).max(1)) as usize;
        debug_assert!(channels > 0, "channel count must be non-zero");

        let mut packets = Vec::new();
        let mut sample_offset = 0;

        while sample_offset < pcm_samples.len() {
            let end_offset = (sample_offset + samples_per_packet).min(pcm_samples.len());
            let chunk = &pcm_samples[sample_offset..end_offset];

            // Encode to μ-law or A-law
            let encoded: Vec<u8> = if payload_type == 0 {
                chunk.iter().map(|&s| pcm_to_mulaw(s)).collect()
            } else {
                chunk.iter().map(|&s| pcm_to_alaw(s)).collect()
            };

            // Create RTP header. `max(1)` keeps a degenerate 0 Hz rate from
            // dividing by zero — the timestamp just advances on the caller's
            // own clock instead of panicking the media task.
            let current_timestamp =
                timestamp + (sample_offset as u32).saturating_mul(8000) / sample_rate.max(1); // Scale to 8kHz clock
            let header =
                RtpHeader::new(payload_type, self.sequence_number, current_timestamp, ssrc);

            // Build packet: header (12 bytes) + payload
            let mut packet = Vec::with_capacity(12 + encoded.len());
            packet.extend_from_slice(&header.to_bytes());
            packet.extend_from_slice(&encoded);

            packets.push(packet);

            self.sequence_number = self.sequence_number.wrapping_add(1);
            sample_offset = end_offset;
        }

        packets
    }
}

impl Default for RtpAudioPacketizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ITU-T G.711 standard anchors ──────────────────────────────────────
    // The codec must match the G.711 tables byte-for-byte: silence in μ-law
    // is 0xFF (not an ad-hoc code), and every code must re-encode to itself.

    #[test]
    fn mulaw_encodes_standard_anchor_values() {
        assert_eq!(pcm_to_mulaw(0), 0xFF, "μ-law silence must be 0xFF");
        assert_eq!(pcm_to_mulaw(32124), 0x80, "μ-law full scale must be 0x80");
        assert_eq!(
            pcm_to_mulaw(-32124),
            0x00,
            "μ-law negative full scale must be 0x00"
        );
    }

    #[test]
    fn mulaw_decodes_standard_anchor_values() {
        assert_eq!(mulaw_to_pcm(0xFF), 0);
        assert_eq!(mulaw_to_pcm(0xFE), 8);
        assert_eq!(mulaw_to_pcm(0x80), 32124);
        assert_eq!(mulaw_to_pcm(0x00), -32124);
    }

    #[test]
    fn mulaw_roundtrips_the_entire_code_space() {
        for b in 0..=255u8 {
            // 0x7F is μ-law "negative zero": it decodes to 0 like 0xFF, but
            // the encoder only ever emits 0xFF for linear 0 (G.711 keeps the
            // negative-zero slot as a decoder-side alias, not an encoding).
            if b == 0x7F {
                assert_eq!(mulaw_to_pcm(0x7F), 0, "−0 must decode to 0");
                continue;
            }
            assert_eq!(
                pcm_to_mulaw(mulaw_to_pcm(b)),
                b,
                "re-encoding decoded 0x{b:02X} diverged"
            );
        }
    }

    #[test]
    fn alaw_encodes_standard_anchor_values() {
        assert_eq!(pcm_to_alaw(0), 0xD5, "A-law silence must be 0xD5");
        assert_eq!(pcm_to_alaw(32256), 0xAA, "A-law full scale must be 0xAA");
        assert_eq!(
            pcm_to_alaw(-32256),
            0x2A,
            "A-law negative full scale must be 0x2A"
        );
    }

    #[test]
    fn alaw_decodes_standard_anchor_values() {
        assert_eq!(alaw_to_pcm(0xD5), 8);
        assert_eq!(alaw_to_pcm(0x8A), 8064);
        assert_eq!(alaw_to_pcm(0xAA), 32256);
        assert_eq!(alaw_to_pcm(0x2A), -32256);
    }

    #[test]
    fn alaw_roundtrips_the_entire_code_space() {
        for b in 0..=255u8 {
            assert_eq!(
                pcm_to_alaw(alaw_to_pcm(b)),
                b,
                "re-encoding decoded 0x{b:02X} diverged"
            );
        }
    }

    #[test]
    fn test_pcm_to_mulaw_range() {
        // Test that μ-law encoding always produces 0-255
        for sample in -32768i16..32767 {
            let _ = pcm_to_mulaw(sample);
        }
    }

    #[test]
    fn test_pcm_to_alaw_range() {
        // Test that A-law encoding always produces 0-255
        for sample in -32768i16..32767 {
            let _ = pcm_to_alaw(sample);
        }
    }

    #[test]
    fn test_mulaw_roundtrip_within_tolerance() {
        // Test round-trip: PCM → μ-law → PCM
        let tolerance = 792; // μ-law has ~12-bit dynamic range, some loss expected

        for sample in [-10000, -5000, -1000, 0, 1000, 5000, 10000].iter() {
            let encoded = pcm_to_mulaw(*sample);
            let decoded = mulaw_to_pcm(encoded);
            let diff = (decoded - sample).abs();
            assert!(
                diff <= tolerance,
                "Round-trip failed: original={}, decoded={}, diff={}",
                sample,
                decoded,
                diff
            );
        }
    }

    #[test]
    fn test_alaw_roundtrip_within_tolerance() {
        // Test round-trip: PCM → A-law → PCM
        let tolerance = 800; // A-law tolerance

        for sample in [-10000, -5000, -1000, 0, 1000, 5000, 10000].iter() {
            let encoded = pcm_to_alaw(*sample);
            let decoded = alaw_to_pcm(encoded);
            let diff = (decoded - sample).abs();
            assert!(
                diff <= tolerance,
                "Round-trip failed: original={}, decoded={}, diff={}",
                sample,
                decoded,
                diff
            );
        }
    }

    #[test]
    fn test_mulaw_zero_point() {
        // Zero should encode/decode reasonably close
        let encoded = pcm_to_mulaw(0);
        let decoded = mulaw_to_pcm(encoded);
        assert!(decoded.abs() < 100, "Zero point drift: {}", decoded);
    }

    #[test]
    fn test_alaw_zero_point() {
        // Zero should encode/decode reasonably close
        let encoded = pcm_to_alaw(0);
        let decoded = alaw_to_pcm(encoded);
        assert!(decoded.abs() < 800, "Zero point drift: {}", decoded);
    }

    #[test]
    fn test_rtp_audio_packetizer_ulaw_payload_type() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 160];
        let packets = packetizer.packetize_pcm_ulaw(&samples, 8000, 1, 0, 12345);

        assert_eq!(packets.len(), 1, "Should produce 1 packet for 160 samples");

        // Check payload type (byte 1, lower 7 bits)
        let payload_type = packets[0][1] & 0x7F;
        assert_eq!(payload_type, 0, "Payload type should be 0 for PCMU");
    }

    #[test]
    fn test_rtp_audio_packetizer_alaw_payload_type() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 160];
        let packets = packetizer.packetize_pcm_alaw(&samples, 8000, 1, 0, 12345);

        assert_eq!(packets.len(), 1, "Should produce 1 packet for 160 samples");

        // Check payload type (byte 1, lower 7 bits)
        let payload_type = packets[0][1] & 0x7F;
        assert_eq!(payload_type, 8, "Payload type should be 8 for PCMA");
    }

    #[test]
    fn test_rtp_audio_packetizer_packet_count() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 320]; // 320 samples = 2 packets at 8kHz/20ms
        let packets = packetizer.packetize_pcm_ulaw(&samples, 8000, 1, 0, 12345);

        assert_eq!(packets.len(), 2, "Should produce 2 packets for 320 samples");
    }

    #[test]
    fn test_rtp_audio_packetizer_header_structure() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 160];
        let packets = packetizer.packetize_pcm_ulaw(&samples, 8000, 1, 1000, 0xABCD1234);

        assert!(!packets.is_empty(), "Should produce at least 1 packet");
        let packet = &packets[0];

        // Verify packet is at least header (12 bytes) + payload
        assert!(packet.len() >= 12, "Packet too short: {}", packet.len());

        // Check version (bits 6-7 of byte 0)
        let version = packet[0] >> 6;
        assert_eq!(version, 2, "RTP version should be 2");

        // Check SSRC (bytes 8-11)
        let ssrc_bytes: [u8; 4] = [packet[8], packet[9], packet[10], packet[11]];
        let ssrc = u32::from_be_bytes(ssrc_bytes);
        assert_eq!(ssrc, 0xABCD1234, "SSRC mismatch");
    }

    #[test]
    fn test_rtp_audio_packetizer_sequence_number() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 320]; // 2 packets
        let packets = packetizer.packetize_pcm_ulaw(&samples, 8000, 1, 0, 12345);

        assert_eq!(packets.len(), 2);

        // Extract sequence numbers (bytes 2-3)
        let seq1 = u16::from_be_bytes([packets[0][2], packets[0][3]]);
        let seq2 = u16::from_be_bytes([packets[1][2], packets[1][3]]);

        assert_eq!(seq1, 0, "First sequence number should be 0");
        assert_eq!(seq2, 1, "Second sequence number should be 1");
    }

    #[test]
    fn test_rtp_audio_packetizer_reset() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 160];

        packetizer.packetize_pcm_ulaw(&samples, 8000, 1, 0, 12345);
        packetizer.reset();

        let packets = packetizer.packetize_pcm_ulaw(&samples, 8000, 1, 0, 12345);
        let seq = u16::from_be_bytes([packets[0][2], packets[0][3]]);
        assert_eq!(seq, 0, "Sequence number should reset to 0");
    }

    #[test]
    fn test_rtp_audio_packetizer_stereo() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 320]; // 160 stereo frames = 320 samples
        let packets = packetizer.packetize_pcm_ulaw(&samples, 8000, 2, 0, 12345);

        assert_eq!(
            packets.len(),
            2,
            "Stereo: 320 samples should produce 2 packets"
        );
        assert_eq!(
            packets[0].len(),
            12 + 160,
            "First packet: header + 160 encoded bytes"
        );
    }

    #[test]
    fn test_rtp_audio_packetizer_non_standard_sample_rate() {
        let mut packetizer = RtpAudioPacketizer::new();
        let samples = vec![0i16; 160];
        // 16kHz should work (though non-standard for G.711)
        let _ = packetizer.packetize_pcm_ulaw(&samples, 16000, 1, 0, 12345);

        // 48kHz should also work (may need resampling in real use)
        let _ = packetizer.packetize_pcm_ulaw(&samples, 48000, 1, 0, 12345);
    }
}
