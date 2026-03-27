//! Call session — manages the end-to-end pipeline for a single voice call.
//!
//! Pipeline: mic → encode → FEC → encrypt → send / recv → decrypt → FEC → decode → speaker

use bytes::Bytes;
use tracing::{debug, warn};

use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::jitter::{JitterBuffer, PlayoutResult};
use wzp_proto::packet::{MediaHeader, MediaPacket};
use wzp_proto::quality::AdaptiveQualityController;
use wzp_proto::traits::{
    AudioDecoder, AudioEncoder, FecDecoder, FecEncoder,
};
use wzp_proto::QualityProfile;

/// Configuration for a call session.
pub struct CallConfig {
    /// Initial quality profile.
    pub profile: QualityProfile,
    /// Jitter buffer target depth.
    pub jitter_target: usize,
    /// Jitter buffer max depth.
    pub jitter_max: usize,
    /// Jitter buffer min depth before playout.
    pub jitter_min: usize,
}

impl Default for CallConfig {
    fn default() -> Self {
        Self {
            profile: QualityProfile::GOOD,
            jitter_target: 50,
            jitter_max: 250,
            jitter_min: 25,
        }
    }
}

/// Manages the encode/send side of a call.
pub struct CallEncoder {
    /// Audio encoder (Opus or Codec2).
    audio_enc: Box<dyn AudioEncoder>,
    /// FEC encoder.
    fec_enc: RaptorQFecEncoder,
    /// Current profile.
    profile: QualityProfile,
    /// Outbound sequence counter.
    seq: u16,
    /// Current FEC block.
    block_id: u8,
    /// Frame index within current block.
    frame_in_block: u8,
    /// Timestamp counter (ms).
    timestamp_ms: u32,
}

impl CallEncoder {
    pub fn new(config: &CallConfig) -> Self {
        Self {
            audio_enc: wzp_codec::create_encoder(config.profile),
            fec_enc: wzp_fec::create_encoder(&config.profile),
            profile: config.profile,
            seq: 0,
            block_id: 0,
            frame_in_block: 0,
            timestamp_ms: 0,
        }
    }

    /// Encode a PCM frame and produce media packets (source + repair when block is full).
    ///
    /// Input: 48kHz mono PCM, frame size depends on profile (960 for 20ms, 1920 for 40ms).
    /// Output: one or more MediaPackets to send.
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<MediaPacket>, anyhow::Error> {
        // Encode audio
        let mut encoded = vec![0u8; self.audio_enc.max_frame_bytes()];
        let enc_len = self.audio_enc.encode(pcm, &mut encoded)?;
        encoded.truncate(enc_len);

        // Build source media packet
        let source_pkt = MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: self.profile.codec,
                has_quality_report: false,
                fec_ratio_encoded: MediaHeader::encode_fec_ratio(self.profile.fec_ratio),
                seq: self.seq,
                timestamp: self.timestamp_ms,
                fec_block: self.block_id,
                fec_symbol: self.frame_in_block,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from(encoded.clone()),
            quality_report: None,
        };

        self.seq = self.seq.wrapping_add(1);
        self.timestamp_ms = self
            .timestamp_ms
            .wrapping_add(self.profile.frame_duration_ms as u32);

        let mut output = vec![source_pkt];

        // Add to FEC encoder
        self.fec_enc.add_source_symbol(&encoded)?;
        self.frame_in_block += 1;

        // If block is full, generate repair and finalize
        if self.frame_in_block >= self.profile.frames_per_block {
            if let Ok(repairs) = self.fec_enc.generate_repair(self.profile.fec_ratio) {
                for (sym_idx, repair_data) in repairs {
                    output.push(MediaPacket {
                        header: MediaHeader {
                            version: 0,
                            is_repair: true,
                            codec_id: self.profile.codec,
                            has_quality_report: false,
                            fec_ratio_encoded: MediaHeader::encode_fec_ratio(
                                self.profile.fec_ratio,
                            ),
                            seq: self.seq,
                            timestamp: self.timestamp_ms,
                            fec_block: self.block_id,
                            fec_symbol: sym_idx,
                            reserved: 0,
                            csrc_count: 0,
                        },
                        payload: Bytes::from(repair_data),
                        quality_report: None,
                    });
                    self.seq = self.seq.wrapping_add(1);
                }
            }
            let _ = self.fec_enc.finalize_block();
            self.block_id = self.block_id.wrapping_add(1);
            self.frame_in_block = 0;
        }

        Ok(output)
    }

    /// Update the quality profile (codec switch, FEC ratio change).
    pub fn set_profile(&mut self, profile: QualityProfile) -> Result<(), anyhow::Error> {
        self.audio_enc.set_profile(profile)?;
        self.fec_enc = wzp_fec::create_encoder(&profile);
        self.profile = profile;
        self.frame_in_block = 0;
        Ok(())
    }
}

/// Manages the recv/decode side of a call.
pub struct CallDecoder {
    /// Audio decoder.
    audio_dec: Box<dyn AudioDecoder>,
    /// FEC decoder.
    fec_dec: RaptorQFecDecoder,
    /// Jitter buffer.
    jitter: JitterBuffer,
    /// Quality controller (used when ingesting quality reports).
    pub quality: AdaptiveQualityController,
    /// Current profile.
    profile: QualityProfile,
}

impl CallDecoder {
    pub fn new(config: &CallConfig) -> Self {
        Self {
            audio_dec: wzp_codec::create_decoder(config.profile),
            fec_dec: wzp_fec::create_decoder(&config.profile),
            jitter: JitterBuffer::new(config.jitter_target, config.jitter_max, config.jitter_min),
            quality: AdaptiveQualityController::new(),
            profile: config.profile,
        }
    }

    /// Feed a received media packet into the decode pipeline.
    pub fn ingest(&mut self, packet: MediaPacket) {
        // Feed to FEC decoder
        let _ = self.fec_dec.add_symbol(
            packet.header.fec_block,
            packet.header.fec_symbol,
            packet.header.is_repair,
            &packet.payload,
        );

        // If not a repair packet, also feed directly to jitter buffer
        if !packet.header.is_repair {
            self.jitter.push(packet);
        }
    }

    /// Decode the next audio frame from the jitter buffer.
    ///
    /// Returns PCM samples (48kHz mono) or None if not ready.
    pub fn decode_next(&mut self, pcm: &mut [i16]) -> Option<usize> {
        match self.jitter.pop() {
            PlayoutResult::Packet(pkt) => {
                match self.audio_dec.decode(&pkt.payload, pcm) {
                    Ok(n) => Some(n),
                    Err(e) => {
                        warn!("decode error: {e}, using PLC");
                        self.audio_dec.decode_lost(pcm).ok()
                    }
                }
            }
            PlayoutResult::Missing { seq } => {
                // Only generate PLC if there are still packets buffered ahead.
                // Otherwise we've drained everything — return None to stop.
                if self.jitter.depth() > 0 {
                    debug!(seq, "packet loss, generating PLC");
                    self.audio_dec.decode_lost(pcm).ok()
                } else {
                    None
                }
            }
            PlayoutResult::NotReady => None,
        }
    }

    /// Get the current quality profile.
    pub fn profile(&self) -> QualityProfile {
        self.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wzp_proto::CodecId;

    #[test]
    fn encoder_produces_packets() {
        let config = CallConfig::default();
        let mut enc = CallEncoder::new(&config);

        // 20ms at 48kHz = 960 samples
        let pcm = vec![0i16; 960];
        let packets = enc.encode_frame(&pcm).unwrap();
        assert!(!packets.is_empty());
        assert_eq!(packets[0].header.seq, 0);
        assert!(!packets[0].header.is_repair);
    }

    #[test]
    fn encoder_generates_repair_on_full_block() {
        let config = CallConfig {
            profile: QualityProfile::GOOD, // 5 frames/block
            ..Default::default()
        };
        let mut enc = CallEncoder::new(&config);
        let pcm = vec![0i16; 960];

        let mut total_packets = 0;
        let mut repair_count = 0;
        for _ in 0..5 {
            let packets = enc.encode_frame(&pcm).unwrap();
            for p in &packets {
                if p.header.is_repair {
                    repair_count += 1;
                }
            }
            total_packets += packets.len();
        }
        assert!(repair_count > 0, "should have repair packets after full block");
        assert!(total_packets > 5, "total {total_packets} should exceed 5 source");
    }

    #[test]
    fn decoder_handles_ingest() {
        let config = CallConfig::default();
        let mut dec = CallDecoder::new(&config);

        let pkt = MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: CodecId::Opus24k,
                has_quality_report: false,
                fec_ratio_encoded: 0,
                seq: 0,
                timestamp: 0,
                fec_block: 0,
                fec_symbol: 0,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from(vec![0u8; 60]),
            quality_report: None,
        };
        dec.ingest(pkt);
        // Not enough buffered yet (min_depth = 25)
        let mut pcm = vec![0i16; 960];
        assert!(dec.decode_next(&mut pcm).is_none());
    }
}
