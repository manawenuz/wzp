//! Media processing pipeline for the relay.
//!
//! The relay pipeline processes media packets in both directions:
//! - **Inbound** (from client/upstream): recv → decrypt → FEC decode → jitter buffer
//! - **Outbound** (to downstream/remote): jitter pop → FEC encode → encrypt → send
//!
//! The relay does NOT decode/re-encode audio — it operates on encrypted,
//! FEC-protected packets. The crypto and FEC layers are the relay's concern;
//! the actual audio codec is end-to-end between client and destination.

use tracing::{debug, info};

use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::jitter::{JitterBuffer, PlayoutResult};
use wzp_proto::packet::{MediaHeader, MediaPacket};
use wzp_proto::quality::AdaptiveQualityController;
use wzp_proto::traits::{FecDecoder, FecEncoder, QualityController};
use wzp_proto::QualityProfile;

/// Configuration for a relay pipeline instance.
pub struct PipelineConfig {
    pub initial_profile: QualityProfile,
    pub jitter_target: usize,
    pub jitter_max: usize,
    pub jitter_min: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            initial_profile: QualityProfile::GOOD,
            jitter_target: 50,
            jitter_max: 250,
            jitter_min: 25,
        }
    }
}

/// A relay media pipeline for one direction of a call session.
///
/// Each call has two pipelines: client→destination and destination→client.
pub struct RelayPipeline {
    /// FEC encoder for outbound packets.
    fec_encoder: RaptorQFecEncoder,
    /// FEC decoder for inbound packets.
    fec_decoder: RaptorQFecDecoder,
    /// Jitter buffer for reordering and smoothing.
    jitter: JitterBuffer,
    /// Adaptive quality controller.
    quality: AdaptiveQualityController,
    /// Current quality profile.
    profile: QualityProfile,
    /// Outbound sequence counter.
    out_seq: u16,
    /// Packets processed count.
    stats: PipelineStats,
}

/// Pipeline statistics.
#[derive(Clone, Debug, Default)]
pub struct PipelineStats {
    pub packets_received: u64,
    pub packets_forwarded: u64,
    pub packets_fec_recovered: u64,
    pub packets_lost: u64,
    pub profile_changes: u64,
}

impl RelayPipeline {
    /// Create a new relay pipeline with the given configuration.
    pub fn new(config: PipelineConfig) -> Self {
        let (fec_enc, fec_dec) = wzp_fec::create_fec_pair(&config.initial_profile);

        Self {
            fec_encoder: fec_enc,
            fec_decoder: fec_dec,
            jitter: JitterBuffer::new(config.jitter_target, config.jitter_max, config.jitter_min),
            quality: AdaptiveQualityController::new(),
            profile: config.initial_profile,
            out_seq: 0,
            stats: PipelineStats::default(),
        }
    }

    /// Process an incoming media packet from the upstream side.
    ///
    /// The packet is fed into the FEC decoder and jitter buffer.
    /// Returns decoded packets ready for forwarding (if any).
    pub fn ingest(&mut self, packet: MediaPacket) -> Vec<MediaPacket> {
        self.stats.packets_received += 1;

        // Feed quality report if present
        if let Some(ref qr) = packet.quality_report {
            if let Some(new_profile) = self.quality.observe(qr) {
                info!(
                    tier = ?self.quality.tier(),
                    codec = ?new_profile.codec,
                    fec_ratio = new_profile.fec_ratio,
                    "quality tier change"
                );
                self.profile = new_profile;
                self.stats.profile_changes += 1;
                // Reconfigure FEC for new profile
                let (enc, dec) = wzp_fec::create_fec_pair(&new_profile);
                self.fec_encoder = enc;
                self.fec_decoder = dec;
            }
        }

        // Feed packet into FEC decoder
        let header = &packet.header;
        let _ = self.fec_decoder.add_symbol(
            header.fec_block,
            header.fec_symbol,
            header.is_repair,
            &packet.payload,
        );

        // Try to decode the FEC block
        let mut output = Vec::new();
        if let Ok(Some(frames)) = self.fec_decoder.try_decode(header.fec_block) {
            debug!(
                block = header.fec_block,
                frames = frames.len(),
                "FEC block decoded"
            );
            // Each recovered frame becomes a media packet for the jitter buffer
            for (i, frame) in frames.into_iter().enumerate() {
                let reconstructed = MediaPacket {
                    header: MediaHeader {
                        version: 0,
                        is_repair: false,
                        codec_id: header.codec_id,
                        has_quality_report: false,
                        fec_ratio_encoded: header.fec_ratio_encoded,
                        // Reconstruct seq from block + symbol index
                        seq: (header.fec_block as u16)
                            .wrapping_mul(self.profile.frames_per_block as u16)
                            .wrapping_add(i as u16),
                        timestamp: header
                            .timestamp
                            .wrapping_add((i as u32) * (header.codec_id.frame_duration_ms() as u32)),
                        fec_block: header.fec_block,
                        fec_symbol: i as u8,
                        reserved: 0,
                        csrc_count: 0,
                    },
                    payload: bytes::Bytes::from(frame),
                    quality_report: None,
                };
                self.jitter.push(reconstructed);
            }
        }

        // Pop from jitter buffer
        loop {
            match self.jitter.pop() {
                PlayoutResult::Packet(pkt) => {
                    self.stats.packets_forwarded += 1;
                    output.push(pkt);
                }
                PlayoutResult::Missing { seq } => {
                    self.stats.packets_lost += 1;
                    debug!(seq, "jitter buffer: missing packet");
                    // Continue popping — the next packet might be available
                }
                PlayoutResult::NotReady => break,
            }
        }

        output
    }

    /// Prepare a packet for outbound transmission.
    ///
    /// Adds FEC encoding and assigns a new sequence number.
    pub fn prepare_outbound(&mut self, mut packet: MediaPacket) -> Vec<MediaPacket> {
        // Assign outbound sequence number
        packet.header.seq = self.out_seq;
        self.out_seq = self.out_seq.wrapping_add(1);

        let mut output = vec![packet.clone()];

        // Add to FEC encoder
        let _ = self.fec_encoder.add_source_symbol(&packet.payload);

        // Check if block is full
        if self.fec_encoder.current_block_size() >= self.profile.frames_per_block as usize {
            // Generate repair packets
            if let Ok(repairs) = self.fec_encoder.generate_repair(self.profile.fec_ratio) {
                for (sym_idx, repair_data) in repairs {
                    let repair_packet = MediaPacket {
                        header: MediaHeader {
                            version: 0,
                            is_repair: true,
                            codec_id: packet.header.codec_id,
                            has_quality_report: false,
                            fec_ratio_encoded: MediaHeader::encode_fec_ratio(
                                self.profile.fec_ratio,
                            ),
                            seq: self.out_seq,
                            timestamp: packet.header.timestamp,
                            fec_block: self.fec_encoder.current_block_id(),
                            fec_symbol: sym_idx,
                            reserved: 0,
                            csrc_count: 0,
                        },
                        payload: bytes::Bytes::from(repair_data),
                        quality_report: None,
                    };
                    self.out_seq = self.out_seq.wrapping_add(1);
                    output.push(repair_packet);
                }
            }
            let _ = self.fec_encoder.finalize_block();
        }

        output
    }

    /// Get current pipeline statistics.
    pub fn stats(&self) -> &PipelineStats {
        &self.stats
    }

    /// Get current quality profile.
    pub fn profile(&self) -> QualityProfile {
        self.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wzp_proto::CodecId;
    use bytes::Bytes;

    fn make_media_packet(seq: u16, block: u8, symbol: u8) -> MediaPacket {
        MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: CodecId::Opus24k,
                has_quality_report: false,
                fec_ratio_encoded: 0,
                seq,
                timestamp: seq as u32 * 20,
                fec_block: block,
                fec_symbol: symbol,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from(vec![seq as u8; 60]),
            quality_report: None,
        }
    }

    #[test]
    fn pipeline_creates_successfully() {
        let pipeline = RelayPipeline::new(PipelineConfig::default());
        assert_eq!(pipeline.profile().codec, CodecId::Opus24k);
    }

    #[test]
    fn prepare_outbound_assigns_seq() {
        let mut pipeline = RelayPipeline::new(PipelineConfig::default());
        let pkt = make_media_packet(0, 0, 0);
        let out = pipeline.prepare_outbound(pkt);
        assert!(!out.is_empty());
        assert_eq!(out[0].header.seq, 0);

        let pkt2 = make_media_packet(1, 0, 1);
        let out2 = pipeline.prepare_outbound(pkt2);
        assert_eq!(out2[0].header.seq, 1);
    }

    #[test]
    fn prepare_outbound_generates_repair_on_full_block() {
        let mut pipeline = RelayPipeline::new(PipelineConfig {
            initial_profile: QualityProfile::GOOD, // 5 frames/block, 20% FEC
            ..Default::default()
        });

        // Feed 5 packets (one full block)
        let mut total_out = 0;
        for i in 0..5u16 {
            let pkt = make_media_packet(i, 0, i as u8);
            let out = pipeline.prepare_outbound(pkt);
            total_out += out.len();
        }
        // Should have 5 source + at least 1 repair packet
        assert!(total_out > 5, "expected repair packets, got {total_out}");
    }

    #[test]
    fn stats_track_packets() {
        let mut pipeline = RelayPipeline::new(PipelineConfig::default());
        let pkt = make_media_packet(0, 0, 0);
        pipeline.ingest(pkt);
        assert_eq!(pipeline.stats().packets_received, 1);
    }
}
