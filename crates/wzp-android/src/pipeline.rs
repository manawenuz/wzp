//! Codec pipeline — encode/decode with FEC and jitter buffer.
//!
//! Runs on a dedicated thread, processing 20 ms frames at 48 kHz.
//! The pipeline is NOT Send/Sync (Opus encoder state) — it is owned
//! exclusively by the codec thread.

use tracing::{debug, warn};
use wzp_codec::{AdaptiveDecoder, AdaptiveEncoder, AutoGainControl, EchoCanceller};
use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::jitter::{JitterBuffer, PlayoutResult};
use wzp_proto::quality::AdaptiveQualityController;
use wzp_proto::traits::{AudioDecoder, AudioEncoder, FecDecoder, FecEncoder};
use wzp_proto::traits::QualityController;
use wzp_proto::{MediaPacket, QualityProfile};

use crate::audio_android::FRAME_SAMPLES;

/// Maximum encoded frame size (Opus worst case at highest bitrate).
const MAX_ENCODED_BYTES: usize = 1275;

/// Pipeline statistics snapshot.
#[derive(Clone, Debug, Default)]
pub struct PipelineStats {
    pub frames_encoded: u64,
    pub frames_decoded: u64,
    pub underruns: u64,
    pub jitter_depth: usize,
    pub quality_tier: u8,
}

/// The codec pipeline: encode, FEC, jitter buffer, decode.
///
/// This struct is owned by the codec thread and not shared.
pub struct Pipeline {
    encoder: AdaptiveEncoder,
    decoder: AdaptiveDecoder,
    fec_encoder: RaptorQFecEncoder,
    fec_decoder: RaptorQFecDecoder,
    jitter_buffer: JitterBuffer,
    quality_ctrl: AdaptiveQualityController,
    /// Acoustic echo canceller applied before encoding.
    aec: EchoCanceller,
    /// Automatic gain control applied before encoding.
    agc: AutoGainControl,
    /// Last decoded PCM frame, used as the AEC far-end reference.
    last_decoded_farend: Option<Vec<i16>>,
    // Pre-allocated scratch buffers
    capture_buf: Vec<i16>,
    #[allow(dead_code)]
    playout_buf: Vec<i16>,
    encode_out: Vec<u8>,
    // Stats counters
    frames_encoded: u64,
    frames_decoded: u64,
    underruns: u64,
}

impl Pipeline {
    /// Create a new pipeline configured for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, anyhow::Error> {
        let encoder = AdaptiveEncoder::new(profile)
            .map_err(|e| anyhow::anyhow!("encoder init: {e}"))?;
        let decoder = AdaptiveDecoder::new(profile)
            .map_err(|e| anyhow::anyhow!("decoder init: {e}"))?;
        let fec_encoder =
            RaptorQFecEncoder::with_defaults(profile.frames_per_block as usize);
        let fec_decoder =
            RaptorQFecDecoder::with_defaults(profile.frames_per_block as usize);
        let jitter_buffer = JitterBuffer::new(10, 250, 3);
        let quality_ctrl = AdaptiveQualityController::new();

        Ok(Self {
            encoder,
            decoder,
            fec_encoder,
            fec_decoder,
            jitter_buffer,
            quality_ctrl,
            aec: EchoCanceller::new(48000, 100), // 100 ms echo tail
            agc: AutoGainControl::new(),
            last_decoded_farend: None,
            capture_buf: vec![0i16; FRAME_SAMPLES],
            playout_buf: vec![0i16; FRAME_SAMPLES],
            encode_out: vec![0u8; MAX_ENCODED_BYTES],
            frames_encoded: 0,
            frames_decoded: 0,
            underruns: 0,
        })
    }

    /// Encode a PCM frame into a compressed packet.
    ///
    /// If `muted` is true, a silence frame is encoded (all zeros).
    /// Returns the encoded bytes, or `None` on encoder error.
    pub fn encode_frame(&mut self, pcm: &[i16], muted: bool) -> Option<Vec<u8>> {
        let input = if muted {
            // Zero the capture buffer for silence
            for s in self.capture_buf.iter_mut() {
                *s = 0;
            }
            &self.capture_buf[..]
        } else {
            // Feed the last decoded playout as AEC far-end reference.
            if let Some(ref farend) = self.last_decoded_farend {
                self.aec.feed_farend(farend);
            }

            // Apply AEC + AGC to the captured PCM.
            let len = pcm.len().min(self.capture_buf.len());
            self.capture_buf[..len].copy_from_slice(&pcm[..len]);
            self.aec.process_frame(&mut self.capture_buf[..len]);
            self.agc.process_frame(&mut self.capture_buf[..len]);
            &self.capture_buf[..len]
        };

        match self.encoder.encode(input, &mut self.encode_out) {
            Ok(n) => {
                self.frames_encoded += 1;
                let encoded = self.encode_out[..n].to_vec();

                // Feed into FEC encoder
                if let Err(e) = self.fec_encoder.add_source_symbol(&encoded) {
                    warn!("FEC encode error: {e}");
                }

                Some(encoded)
            }
            Err(e) => {
                warn!("encode error: {e}");
                None
            }
        }
    }

    /// Feed a received media packet into the jitter buffer.
    pub fn feed_packet(&mut self, packet: MediaPacket) {
        // Feed FEC symbols if present
        let header = &packet.header;
        if header.fec_block != 0 || header.fec_symbol != 0 {
            let is_repair = header.is_repair;
            if let Err(e) = self.fec_decoder.add_symbol(
                header.fec_block,
                header.fec_symbol,
                is_repair,
                &packet.payload,
            ) {
                debug!("FEC symbol feed error: {e}");
            }
        }

        self.jitter_buffer.push(packet);
    }

    /// Decode the next frame from the jitter buffer.
    ///
    /// Returns decoded PCM samples, or `None` if the buffer is not ready.
    /// Decoded PCM is also stored as the AEC far-end reference for the next
    /// encode cycle.
    pub fn decode_frame(&mut self) -> Option<Vec<i16>> {
        let result = match self.jitter_buffer.pop() {
            PlayoutResult::Packet(pkt) => {
                let mut pcm = vec![0i16; FRAME_SAMPLES];
                match self.decoder.decode(&pkt.payload, &mut pcm) {
                    Ok(n) => {
                        self.frames_decoded += 1;
                        pcm.truncate(n);
                        Some(pcm)
                    }
                    Err(e) => {
                        warn!("decode error: {e}");
                        // Attempt PLC
                        self.generate_plc()
                    }
                }
            }
            PlayoutResult::Missing { seq } => {
                debug!(seq, "jitter buffer: missing packet, generating PLC");
                self.generate_plc()
            }
            PlayoutResult::NotReady => {
                self.underruns += 1;
                None
            }
        };

        // Save decoded PCM as far-end reference for AEC.
        if let Some(ref pcm) = result {
            self.last_decoded_farend = Some(pcm.clone());
        }

        result
    }

    /// Generate packet loss concealment output.
    fn generate_plc(&mut self) -> Option<Vec<i16>> {
        let mut pcm = vec![0i16; FRAME_SAMPLES];
        match self.decoder.decode_lost(&mut pcm) {
            Ok(n) => {
                self.frames_decoded += 1;
                pcm.truncate(n);
                Some(pcm)
            }
            Err(e) => {
                warn!("PLC error: {e}");
                None
            }
        }
    }

    /// Feed a quality report into the adaptive quality controller.
    ///
    /// Returns a new profile if a tier transition occurred.
    #[allow(unused)]
    pub fn observe_quality(
        &mut self,
        report: &wzp_proto::QualityReport,
    ) -> Option<QualityProfile> {
        let new_profile = self.quality_ctrl.observe(report);
        if let Some(ref profile) = new_profile {
            if let Err(e) = self.encoder.set_profile(*profile) {
                warn!("encoder set_profile error: {e}");
            }
            if let Err(e) = self.decoder.set_profile(*profile) {
                warn!("decoder set_profile error: {e}");
            }
        }
        new_profile
    }

    /// Force a specific quality profile.
    #[allow(unused)]
    pub fn force_profile(&mut self, profile: QualityProfile) {
        self.quality_ctrl.force_profile(profile);
        if let Err(e) = self.encoder.set_profile(profile) {
            warn!("encoder set_profile error: {e}");
        }
        if let Err(e) = self.decoder.set_profile(profile) {
            warn!("decoder set_profile error: {e}");
        }
    }

    /// Get current pipeline statistics.
    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            frames_encoded: self.frames_encoded,
            frames_decoded: self.frames_decoded,
            underruns: self.underruns,
            jitter_depth: self.jitter_buffer.stats().current_depth,
            quality_tier: self.quality_ctrl.tier() as u8,
        }
    }

    /// Enable or disable acoustic echo cancellation.
    pub fn set_aec_enabled(&mut self, enabled: bool) {
        self.aec.set_enabled(enabled);
    }

    /// Enable or disable automatic gain control.
    pub fn set_agc_enabled(&mut self, enabled: bool) {
        self.agc.set_enabled(enabled);
    }
}
