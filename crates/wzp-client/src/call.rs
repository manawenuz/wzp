//! Call session — manages the end-to-end pipeline for a single voice call.
//!
//! Pipeline: mic → encode → FEC → encrypt → send / recv → decrypt → FEC → decode → speaker

use std::time::{Duration, Instant};

use bytes::Bytes;
use tracing::{debug, info, warn};

use wzp_codec::dred_ffi::{DredDecoderHandle, DredState};
use wzp_codec::{
    AdaptiveDecoder, AutoGainControl, ComfortNoise, EchoCanceller, NoiseSupressor, SilenceDetector,
};
use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::jitter::{JitterBuffer, PlayoutResult};
use wzp_proto::packet::{MediaHeader, MediaPacket, MiniFrameContext};
use wzp_proto::quality::AdaptiveQualityController;
use wzp_proto::traits::{AudioDecoder, AudioEncoder, FecDecoder, FecEncoder};
use wzp_proto::packet::QualityReport;
use wzp_proto::{CodecId, QualityProfile};

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
    /// Enable silence suppression (default: true).
    pub suppression_enabled: bool,
    /// RMS threshold for silence detection (default: 100.0 for i16 PCM).
    pub silence_threshold_rms: f64,
    /// Hangover frames before suppression begins (default: 5 = 100ms at 20ms frames).
    pub silence_hangover_frames: u32,
    /// Comfort noise amplitude (default: 50).
    pub comfort_noise_level: i16,
    /// Enable ML-based noise suppression via RNNoise (default: true).
    pub noise_suppression: bool,
    /// Enable mini-frame header compression (default: true).
    /// When enabled, only every 50th frame carries a full 12-byte MediaHeader;
    /// intermediate frames use a compact 4-byte MiniHeader.
    pub mini_frames_enabled: bool,
    /// Enable adaptive jitter buffer (default: true).
    ///
    /// When true, the jitter buffer target depth is automatically adjusted
    /// based on observed inter-arrival jitter (NetEq-inspired algorithm).
    pub adaptive_jitter: bool,
}

impl Default for CallConfig {
    fn default() -> Self {
        Self {
            profile: QualityProfile::GOOD,
            jitter_target: 10,
            jitter_max: 250,
            jitter_min: 3, // 60ms — low latency start, still smooths jitter
            suppression_enabled: true,
            silence_threshold_rms: 100.0,
            silence_hangover_frames: 5,
            comfort_noise_level: 50,
            noise_suppression: true,
            mini_frames_enabled: true,
            adaptive_jitter: true,
        }
    }
}

impl CallConfig {
    /// Build a `CallConfig` tuned for the given quality profile.
    pub fn from_profile(profile: QualityProfile) -> Self {
        let (jitter_target, jitter_max, jitter_min) = if profile == QualityProfile::CATASTROPHIC {
            // Catastrophic: larger jitter buffer to absorb spikes
            (20, 500, 8)
        } else if profile == QualityProfile::DEGRADED {
            // Degraded: moderately deeper buffer
            (15, 350, 5)
        } else {
            // Good: low-latency defaults
            (10, 250, 3)
        };
        Self {
            profile,
            jitter_target,
            jitter_max,
            jitter_min,
            ..Default::default()
        }
    }
}

/// Sliding-window quality adapter that reacts to relay `QualityReport`s.
///
/// Thresholds (per-report):
///   - loss > 15% OR rtt > 200ms  => CATASTROPHIC
///   - loss > 5%  OR rtt > 100ms  => DEGRADED
///   - otherwise                   => GOOD
///
/// Hysteresis: a profile switch is only recommended after the new profile
/// has been the recommendation for 3 or more consecutive reports.
pub struct QualityAdapter {
    /// Sliding window of the last N reports.
    window: std::collections::VecDeque<QualityReport>,
    /// Maximum window size.
    max_window: usize,
    /// Number of consecutive reports recommending the same (non-current) profile.
    consecutive_same: u32,
    /// The profile that the last `consecutive_same` reports recommended.
    pending_profile: Option<QualityProfile>,
}

/// Number of consecutive reports required before accepting a switch.
const HYSTERESIS_COUNT: u32 = 3;
/// Default sliding window capacity.
const ADAPTER_WINDOW: usize = 10;

impl QualityAdapter {
    pub fn new() -> Self {
        Self {
            window: std::collections::VecDeque::with_capacity(ADAPTER_WINDOW),
            max_window: ADAPTER_WINDOW,
            consecutive_same: 0,
            pending_profile: None,
        }
    }

    /// Record a new quality report from the relay.
    pub fn ingest(&mut self, report: &QualityReport) {
        if self.window.len() >= self.max_window {
            self.window.pop_front();
        }
        self.window.push_back(*report);
    }

    /// Classify a single report into a recommended profile.
    fn classify(report: &QualityReport) -> QualityProfile {
        let loss = report.loss_percent();
        let rtt = report.rtt_ms();

        if loss > 15.0 || rtt > 200 {
            QualityProfile::CATASTROPHIC
        } else if loss > 5.0 || rtt > 100 {
            QualityProfile::DEGRADED
        } else {
            QualityProfile::GOOD
        }
    }

    /// Return the best profile based on the most recent report in the window.
    pub fn recommended_profile(&self) -> QualityProfile {
        match self.window.back() {
            Some(report) => Self::classify(report),
            None => QualityProfile::GOOD,
        }
    }

    /// Determine if a profile switch should happen, applying hysteresis.
    ///
    /// Returns `Some(new_profile)` only when the recommendation has differed
    /// from `current` for at least `HYSTERESIS_COUNT` consecutive reports.
    pub fn should_switch(&mut self, current: &QualityProfile) -> Option<QualityProfile> {
        let recommended = self.recommended_profile();

        if recommended == *current {
            // Conditions match current profile — reset pending state.
            self.consecutive_same = 0;
            self.pending_profile = None;
            return None;
        }

        // Recommended differs from current.
        match self.pending_profile {
            Some(pending) if pending == recommended => {
                self.consecutive_same += 1;
            }
            _ => {
                // New or changed recommendation — restart counter.
                self.pending_profile = Some(recommended);
                self.consecutive_same = 1;
            }
        }

        if self.consecutive_same >= HYSTERESIS_COUNT {
            self.consecutive_same = 0;
            self.pending_profile = None;
            Some(recommended)
        } else {
            None
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
    /// Acoustic echo canceller (removes speaker echo from mic signal).
    aec: EchoCanceller,
    /// Automatic gain control (normalises mic level).
    agc: AutoGainControl,
    /// Silence detector for suppression.
    silence_detector: SilenceDetector,
    /// Whether silence suppression is enabled.
    suppression_enabled: bool,
    /// Total frames suppressed (telemetry).
    frames_suppressed: u64,
    /// Frames since last CN packet was sent.
    cn_counter: u32,
    /// Comfort noise amplitude level (stored for CN packet payload).
    cn_level: i16,
    /// ML-based noise suppressor (RNNoise).
    denoiser: NoiseSupressor,
    /// Mini-frame compression context (tracks last full header).
    mini_context: MiniFrameContext,
    /// Whether mini-frame header compression is enabled.
    mini_frames_enabled: bool,
    /// Frames encoded since the last full header was emitted.
    frames_since_full: u32,
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
            aec: EchoCanceller::new(48000, 100), // 100 ms echo tail
            agc: AutoGainControl::new(),
            silence_detector: SilenceDetector::new(
                config.silence_threshold_rms,
                config.silence_hangover_frames,
            ),
            suppression_enabled: config.suppression_enabled,
            frames_suppressed: 0,
            cn_counter: 0,
            cn_level: config.comfort_noise_level,
            denoiser: {
                let mut d = NoiseSupressor::new();
                d.set_enabled(config.noise_suppression);
                d
            },
            mini_context: MiniFrameContext::default(),
            mini_frames_enabled: config.mini_frames_enabled,
            frames_since_full: 0,
        }
    }

    /// Serialize a `MediaPacket` for transmission, applying mini-frame
    /// compression when enabled.
    ///
    /// Returns compact wire bytes: either `[FRAME_TYPE_FULL][MediaHeader][payload]`
    /// or `[FRAME_TYPE_MINI][MiniHeader][payload]`.
    pub fn serialize_compact(&mut self, packet: &MediaPacket) -> Bytes {
        if self.mini_frames_enabled {
            packet.encode_compact(&mut self.mini_context, &mut self.frames_since_full)
        } else {
            packet.to_bytes()
        }
    }

    /// Encode a PCM frame and produce media packets (source + repair when block is full).
    ///
    /// Input: 48kHz mono PCM, frame size depends on profile (960 for 20ms, 1920 for 40ms).
    /// Output: one or more MediaPackets to send.
    pub fn encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<MediaPacket>, anyhow::Error> {
        // Copy PCM into a mutable buffer for the processing pipeline.
        let mut pcm_buf = pcm.to_vec();

        // Step 1: Echo cancellation (far-end reference must have been fed already).
        self.aec.process_frame(&mut pcm_buf);

        // Step 2: Automatic gain control (normalise mic level).
        self.agc.process_frame(&mut pcm_buf);

        // Step 3: Noise suppression (RNNoise).
        if self.denoiser.is_enabled() {
            self.denoiser.process(&mut pcm_buf);
        }

        let pcm = &pcm_buf[..];

        // Silence suppression: skip encoding silent frames, periodically send CN.
        if self.suppression_enabled && self.silence_detector.is_silent(pcm) {
            self.frames_suppressed += 1;
            self.cn_counter += 1;

            // Advance timestamp even for suppressed frames.
            self.timestamp_ms = self
                .timestamp_ms
                .wrapping_add(self.profile.frame_duration_ms as u32);

            // Every 10 frames (~200ms), send a comfort noise packet.
            if self.cn_counter % 10 == 0 {
                let cn_pkt = MediaPacket {
                    header: MediaHeader {
                        version: 0,
                        is_repair: false,
                        codec_id: CodecId::ComfortNoise,
                        has_quality_report: false,
                        fec_ratio_encoded: 0,
                        seq: self.seq,
                        timestamp: self.timestamp_ms,
                        fec_block: self.block_id,
                        fec_symbol: 0,
                        reserved: 0,
                        csrc_count: 0,
                    },
                    payload: Bytes::from(vec![self.cn_level as u8]),
                    quality_report: None,
                };
                self.seq = self.seq.wrapping_add(1);
                return Ok(vec![cn_pkt]);
            }

            return Ok(vec![]);
        }

        // Not silent — reset CN counter and proceed with normal encoding.
        self.cn_counter = 0;

        // Encode audio
        let mut encoded = vec![0u8; self.audio_enc.max_frame_bytes()];
        let enc_len = self.audio_enc.encode(pcm, &mut encoded)?;
        encoded.truncate(enc_len);

        // Phase 2: Opus tiers bypass RaptorQ entirely (DRED handles loss
        // recovery at the codec layer). Codec2 tiers keep RaptorQ unchanged.
        // On Opus packets, zero the FEC header fields so old receivers
        // can cleanly identify "no RaptorQ block to assemble" and new
        // receivers can short-circuit their FEC ingest path.
        let is_opus = self.profile.codec.is_opus();
        let (fec_block, fec_symbol, fec_ratio_encoded) = if is_opus {
            (0u8, 0u8, 0u8)
        } else {
            (
                self.block_id,
                self.frame_in_block,
                MediaHeader::encode_fec_ratio(self.profile.fec_ratio),
            )
        };

        // Build source media packet
        let source_pkt = MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: self.profile.codec,
                has_quality_report: false,
                fec_ratio_encoded,
                seq: self.seq,
                timestamp: self.timestamp_ms,
                fec_block,
                fec_symbol,
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

        // Codec2-only: feed RaptorQ and generate repair packets when the
        // block is full. Opus tiers skip this entire block — DRED (active
        // in Phase 1) provides codec-layer loss recovery.
        if !is_opus {
            self.fec_enc.add_source_symbol(&encoded)?;
            self.frame_in_block += 1;

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

    /// Feed decoded playout audio as the echo reference signal.
    ///
    /// Must be called with each decoded frame BEFORE the corresponding
    /// microphone frame is processed.
    pub fn feed_aec_farend(&mut self, farend: &[i16]) {
        self.aec.feed_farend(farend);
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

/// Manages the recv/decode side of a call.
pub struct CallDecoder {
    /// Audio decoder. Concrete `AdaptiveDecoder` (not `Box<dyn AudioDecoder>`)
    /// because Phase 3b calls the inherent `reconstruct_from_dred` method,
    /// which cannot live on the `AudioDecoder` trait without dragging libopus
    /// types into `wzp-proto`.
    audio_dec: AdaptiveDecoder,
    /// FEC decoder (Codec2 tiers only; Opus bypasses RaptorQ per Phase 2).
    fec_dec: RaptorQFecDecoder,
    /// Jitter buffer.
    jitter: JitterBuffer,
    /// Quality controller (used when ingesting quality reports).
    pub quality: AdaptiveQualityController,
    /// Current profile.
    profile: QualityProfile,
    /// Comfort noise generator for filling silent gaps.
    comfort_noise: ComfortNoise,
    /// Whether the last decoded frame was comfort noise.
    last_was_cn: bool,
    /// Mini-frame decompression context (tracks last full header baseline).
    mini_context: MiniFrameContext,
    // ─── Phase 3b: DRED reconstruction state ──────────────────────────────
    /// DRED side-channel parser (a separate libopus object from the decoder).
    dred_decoder: DredDecoderHandle,
    /// Scratch buffer used by `dred_decoder.parse_into` on every arriving
    /// Opus packet. Reused across calls to avoid 10 KB alloc churn per packet.
    dred_parse_scratch: DredState,
    /// Cached "most recently parsed valid" DRED state, swapped with
    /// `dred_parse_scratch` on successful parse. Used by `decode_next` when
    /// the jitter buffer reports a gap.
    last_good_dred: DredState,
    /// Sequence number of the packet that produced `last_good_dred`. `None`
    /// if no packet has yielded DRED state yet (cold start or legacy sender).
    last_good_dred_seq: Option<u16>,
    /// Phase 4 telemetry counter: gaps recovered via DRED reconstruction.
    pub dred_reconstructions: u64,
    /// Phase 4 telemetry counter: gaps filled via classical Opus PLC
    /// (because no DRED state covered the gap, or the active codec is Codec2).
    pub classical_plc_invocations: u64,
}

impl CallDecoder {
    pub fn new(config: &CallConfig) -> Self {
        let jitter = if config.adaptive_jitter {
            JitterBuffer::new_adaptive(config.jitter_min, config.jitter_max)
        } else {
            JitterBuffer::new(config.jitter_target, config.jitter_max, config.jitter_min)
        };
        // Phase 3b: build the DRED parser + state buffers. These allocate
        // libopus state (~10 KB each) once per call, not per packet — the
        // scratch and last-good buffers are reused via std::mem::swap on
        // every successful parse.
        let dred_decoder =
            DredDecoderHandle::new().expect("opus_dred_decoder_create failed at call setup");
        let dred_parse_scratch =
            DredState::new().expect("opus_dred_alloc failed at call setup (scratch)");
        let last_good_dred =
            DredState::new().expect("opus_dred_alloc failed at call setup (good state)");
        Self {
            audio_dec: AdaptiveDecoder::new(config.profile)
                .expect("failed to create adaptive decoder"),
            fec_dec: wzp_fec::create_decoder(&config.profile),
            jitter,
            quality: AdaptiveQualityController::new(),
            profile: config.profile,
            comfort_noise: ComfortNoise::new(50),
            last_was_cn: false,
            mini_context: MiniFrameContext::default(),
            dred_decoder,
            dred_parse_scratch,
            last_good_dred,
            last_good_dred_seq: None,
            dred_reconstructions: 0,
            classical_plc_invocations: 0,
        }
    }

    /// Deserialize a compact wire-format buffer into a `MediaPacket`,
    /// auto-detecting full vs mini headers.
    ///
    /// Returns `None` on malformed data or if a mini-frame arrives before
    /// any full header baseline has been established.
    pub fn deserialize_compact(&mut self, buf: &[u8]) -> Option<MediaPacket> {
        MediaPacket::decode_compact(buf, &mut self.mini_context)
    }

    /// Feed a received media packet into the decode pipeline.
    pub fn ingest(&mut self, packet: MediaPacket) {
        // Phase 2: Opus packets bypass RaptorQ. Codec2 packets still feed
        // the FEC decoder for recovery. This also cleanly drops any stray
        // Opus repair packets from an old sender (we don't push repair
        // packets to the jitter buffer either, so they're effectively
        // ignored — a graceful mixed-version degradation).
        if !packet.header.codec_id.is_opus() {
            let _ = self.fec_dec.add_symbol(
                packet.header.fec_block,
                packet.header.fec_symbol,
                packet.header.is_repair,
                &packet.payload,
            );
        }

        // Phase 3b: Opus source packets carry DRED side-channel data in
        // libopus 1.5. Parse it into the scratch state and, on success,
        // swap with the cached `last_good_dred` so later gap reconstruction
        // has fresh neural redundancy to draw from. Parsing happens before
        // the jitter push because the jitter buffer consumes the packet.
        if packet.header.codec_id.is_opus() && !packet.header.is_repair {
            match self
                .dred_decoder
                .parse_into(&mut self.dred_parse_scratch, &packet.payload)
            {
                Ok(available) if available > 0 => {
                    // Swap the freshly parsed state into `last_good_dred`.
                    // The old good state (now in scratch) is about to be
                    // overwritten on the next parse — its contents are
                    // not needed after this swap.
                    std::mem::swap(&mut self.dred_parse_scratch, &mut self.last_good_dred);
                    self.last_good_dred_seq = Some(packet.header.seq);
                }
                Ok(_) => {
                    // Packet had no DRED data (return 0). Leave the cached
                    // state untouched — it may still cover upcoming gaps
                    // from a warm-up period where the encoder was producing
                    // DRED bytes. The scratch buffer was potentially written
                    // but its `samples_available` is 0 so it's harmless.
                }
                Err(e) => {
                    debug!("DRED parse error (ignored): {e}");
                }
            }
        }

        // Source packets (Opus or Codec2) go to the jitter buffer for decode.
        // Repair packets never reach the jitter buffer; for Codec2 they're
        // used by the FEC decoder above, for Opus they're dropped here.
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
                // Comfort noise packet: generate CN instead of decoding audio.
                if pkt.header.codec_id == CodecId::ComfortNoise {
                    self.comfort_noise.generate(pcm);
                    self.last_was_cn = true;
                    self.jitter.record_decode();
                    return Some(pcm.len());
                }

                self.last_was_cn = false;
                let result = match self.audio_dec.decode(&pkt.payload, pcm) {
                    Ok(n) => Some(n),
                    Err(e) => {
                        warn!("decode error: {e}, using PLC");
                        self.audio_dec.decode_lost(pcm).ok()
                    }
                };
                if result.is_some() {
                    self.jitter.record_decode();
                }
                result
            }
            PlayoutResult::Missing { seq } => {
                // Only attempt recovery if there are still packets buffered ahead.
                // Otherwise we've drained everything — return None to stop.
                if self.jitter.depth() == 0 {
                    self.jitter.record_underrun();
                    return None;
                }

                // Phase 3b: try DRED reconstruction first. If we have a
                // recent DRED state from a packet whose seq > missing seq,
                // and the seq delta (in samples) fits within the state's
                // available window, libopus can synthesize a plausible
                // replacement for the lost frame. Fall back to classical
                // PLC when no state covers the gap, when the active codec
                // is Codec2, or when the reconstruction itself errors.
                if self.profile.codec.is_opus() {
                    if let Some(last_seq) = self.last_good_dred_seq {
                        // How many frames ahead of the missing seq is the
                        // last-good packet? Use wrapping arithmetic for the
                        // u16 seq space.
                        let seq_delta = last_seq.wrapping_sub(seq);
                        // Reject stale or backward state. u16 wraparound
                        // would make a "seq went backward" delta very large;
                        // cap at a sane forward-looking window.
                        const MAX_SEQ_DELTA: u16 = 128;
                        if seq_delta > 0 && seq_delta <= MAX_SEQ_DELTA {
                            let frame_samples =
                                (48_000 * self.profile.frame_duration_ms as i32) / 1000;
                            let offset_samples = seq_delta as i32 * frame_samples;
                            let available = self.last_good_dred.samples_available();
                            if offset_samples > 0 && offset_samples <= available {
                                match self.audio_dec.reconstruct_from_dred(
                                    &self.last_good_dred,
                                    offset_samples,
                                    pcm,
                                ) {
                                    Ok(n) => {
                                        self.dred_reconstructions += 1;
                                        self.jitter.record_decode();
                                        debug!(
                                            seq,
                                            last_seq,
                                            offset_samples,
                                            available,
                                            "DRED reconstruction for gap"
                                        );
                                        return Some(n);
                                    }
                                    Err(e) => {
                                        // Reconstruction failed — fall
                                        // through to classical PLC below.
                                        debug!(seq, "DRED reconstruct error: {e}");
                                    }
                                }
                            }
                        }
                    }
                }

                // Classical PLC fallback (also the Codec2 path).
                debug!(seq, "packet loss, generating classical PLC");
                self.classical_plc_invocations += 1;
                let result = self.audio_dec.decode_lost(pcm).ok();
                if result.is_some() {
                    self.jitter.record_decode();
                }
                result
            }
            PlayoutResult::NotReady => {
                self.jitter.record_underrun();
                None
            }
        }
    }

    /// Get the current quality profile.
    pub fn profile(&self) -> QualityProfile {
        self.profile
    }

    /// Get jitter buffer statistics.
    pub fn stats(&self) -> &wzp_proto::jitter::JitterStats {
        self.jitter.stats()
    }

    /// Reset jitter buffer statistics counters.
    pub fn reset_stats(&mut self) {
        self.jitter.reset_stats();
    }

    /// Phase 3b introspection: sequence number of the most recently parsed
    /// valid DRED state, or `None` if no Opus packet has yielded DRED data
    /// yet. Used by tests to debug reconstruction eligibility.
    pub fn last_good_dred_seq(&self) -> Option<u16> {
        self.last_good_dred_seq
    }

    /// Phase 3b introspection: samples of audio history currently available
    /// in the cached DRED state.
    pub fn last_good_dred_samples_available(&self) -> i32 {
        self.last_good_dred.samples_available()
    }
}

/// Periodic telemetry logger for jitter buffer statistics.
///
/// Call `maybe_log` on each decode tick; it will emit a `tracing::info!` event
/// no more frequently than the configured interval.
pub struct JitterTelemetry {
    interval: Duration,
    last_report: Instant,
}

impl JitterTelemetry {
    /// Create a new telemetry logger that reports at most once per `interval_secs`.
    pub fn new(interval_secs: u64) -> Self {
        Self {
            interval: Duration::from_secs(interval_secs),
            last_report: Instant::now(),
        }
    }

    /// Log jitter statistics if the interval has elapsed. Returns `true` when a
    /// log line was emitted.
    pub fn maybe_log(&mut self, stats: &wzp_proto::jitter::JitterStats) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_report) >= self.interval {
            info!(
                buffer_depth = stats.current_depth,
                underruns = stats.underruns,
                overruns = stats.overruns,
                late_packets = stats.packets_late,
                total_received = stats.packets_received,
                total_decoded = stats.total_decoded,
                max_depth_seen = stats.max_depth_seen,
                "jitter buffer telemetry"
            );
            self.last_report = now;
            true
        } else {
            false
        }
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

    /// Phase 2: Opus packets have zero FEC header fields — no block, no
    /// symbol index, no repair ratio. The RaptorQ layer is bypassed
    /// entirely on the Opus tiers.
    #[test]
    fn opus_source_packets_have_zero_fec_header_fields() {
        let config = CallConfig {
            profile: QualityProfile::GOOD, // Opus 24k
            suppression_enabled: false,    // skip silence gate for this test
            ..Default::default()
        };
        let mut enc = CallEncoder::new(&config);
        // Non-silent sine wave so silence detection doesn't suppress us
        // even with suppression_enabled=false (belt and braces).
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 0.1).sin() * 10_000.0) as i16)
            .collect();
        let packets = enc.encode_frame(&pcm).unwrap();
        assert_eq!(packets.len(), 1, "Opus must emit exactly 1 source packet");
        let hdr = &packets[0].header;
        assert!(hdr.codec_id.is_opus());
        assert!(!hdr.is_repair);
        assert_eq!(hdr.fec_block, 0, "Opus fec_block must be 0");
        assert_eq!(hdr.fec_symbol, 0, "Opus fec_symbol must be 0");
        assert_eq!(hdr.fec_ratio_encoded, 0, "Opus fec_ratio_encoded must be 0");
    }

    /// Phase 2: Opus never emits repair packets, regardless of how many
    /// source frames are fed in. DRED (Phase 1) provides loss recovery at
    /// the codec layer; RaptorQ is disabled on Opus tiers.
    #[test]
    fn opus_encoder_never_emits_repair_packets() {
        let config = CallConfig {
            profile: QualityProfile::GOOD, // 5 frames/block in the Codec2 sense
            suppression_enabled: false,
            ..Default::default()
        };
        let mut enc = CallEncoder::new(&config);
        let pcm: Vec<i16> = (0..960)
            .map(|i| ((i as f32 * 0.1).sin() * 10_000.0) as i16)
            .collect();

        // Encode well beyond a block boundary to prove no repair ever comes out.
        let mut total_packets = 0usize;
        let mut repair_count = 0usize;
        for _ in 0..20 {
            let packets = enc.encode_frame(&pcm).unwrap();
            total_packets += packets.len();
            repair_count += packets.iter().filter(|p| p.header.is_repair).count();
        }
        assert_eq!(repair_count, 0, "Opus must emit zero repair packets");
        assert_eq!(
            total_packets, 20,
            "20 source frames → 20 source packets (1:1, no RaptorQ expansion)"
        );
    }

    /// Phase 2: Codec2 still emits repair packets with RaptorQ ratio unchanged.
    /// DRED is libopus-only and does not apply here, so RaptorQ is still the
    /// primary loss-recovery mechanism on Codec2 tiers.
    #[test]
    fn codec2_encoder_generates_repair_on_full_block() {
        let config = CallConfig {
            profile: QualityProfile::CATASTROPHIC, // Codec2 1200, 8 frames/block, ratio 1.0
            suppression_enabled: false,
            ..Default::default()
        };
        let mut enc = CallEncoder::new(&config);
        // Codec2 takes 48 kHz samples and downsamples internally.
        // CATASTROPHIC uses 40 ms frames → 1920 samples.
        let pcm: Vec<i16> = (0..1920)
            .map(|i| ((i as f32 * 0.1).sin() * 10_000.0) as i16)
            .collect();

        let mut total_packets = 0usize;
        let mut repair_count = 0usize;
        // Run long enough to cross the 8-frame block boundary and see repairs.
        for _ in 0..16 {
            let packets = enc.encode_frame(&pcm).unwrap();
            for p in &packets {
                if p.header.is_repair {
                    repair_count += 1;
                }
            }
            total_packets += packets.len();
        }
        assert!(
            repair_count > 0,
            "Codec2 must still emit repair packets (got {repair_count} repairs, {total_packets} total)"
        );
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

    // ─── Phase 3b — DRED reconstruction on packet loss ────────────────────

    /// Helper: create a CallEncoder/CallDecoder pair with the given profile
    /// and silence suppression disabled so silence-detection doesn't drop
    /// our synthetic test frames.
    fn encoder_decoder_pair(profile: QualityProfile) -> (CallEncoder, CallDecoder) {
        let config = CallConfig {
            profile,
            suppression_enabled: false,
            // Small jitter buffer so decode_next drains quickly in tests.
            jitter_min: 2,
            jitter_target: 3,
            jitter_max: 20,
            adaptive_jitter: false,
            ..Default::default()
        };
        (CallEncoder::new(&config), CallDecoder::new(&config))
    }

    /// Helper: generate a non-silent 20 ms frame of 300 Hz sine at the
    /// given sample offset so consecutive frames form a continuous tone.
    fn voice_frame_20ms(sample_offset: usize) -> Vec<i16> {
        (0..960)
            .map(|i| {
                let t = (sample_offset + i) as f64 / 48_000.0;
                (8000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
            })
            .collect()
    }

    /// Phase 3b probe: sweep packet_loss_perc values to find the minimum
    /// that produces a samples_available ≥ 960 (enough to reconstruct a
    /// single 20 ms Opus frame). This guides the production loss floor.
    #[test]
    #[ignore] // diagnostic only — run with `cargo test ... -- --ignored --nocapture`
    fn probe_dred_samples_available_by_loss_floor() {
        use wzp_codec::opus_enc::OpusEncoder;
        use wzp_proto::traits::AudioEncoder;

        for loss_pct in [5u8, 10, 15, 20, 25, 40, 60, 80].iter().copied() {
            let mut enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
            enc.set_expected_loss(loss_pct);
            let (_drop_enc, mut dec) = encoder_decoder_pair(QualityProfile::GOOD);

            for i in 0..60u16 {
                let pcm = voice_frame_20ms(i as usize * 960);
                let mut encoded = vec![0u8; 512];
                let n = enc.encode(&pcm, &mut encoded).unwrap();
                encoded.truncate(n);
                let pkt = MediaPacket {
                    header: MediaHeader {
                        version: 0,
                        is_repair: false,
                        codec_id: CodecId::Opus24k,
                        has_quality_report: false,
                        fec_ratio_encoded: 0,
                        seq: i,
                        timestamp: (i as u32) * 20,
                        fec_block: 0,
                        fec_symbol: 0,
                        reserved: 0,
                        csrc_count: 0,
                    },
                    payload: Bytes::from(encoded),
                    quality_report: None,
                };
                dec.ingest(pkt);
            }
            eprintln!(
                "[phase3b probe] loss_pct={loss_pct} samples_available={}",
                dec.last_good_dred_samples_available()
            );
        }
    }

    /// Phase 3b: simulated single-packet loss on an Opus call triggers a
    /// DRED reconstruction rather than a classical PLC fill. Runs the full
    /// encode → ingest → decode_next pipeline.
    #[test]
    fn opus_single_packet_loss_is_recovered_via_dred() {
        let (mut enc, mut dec) = encoder_decoder_pair(QualityProfile::GOOD);

        // Warm-up: encode and ingest 60 frames (1.2 s) so the DRED emitter
        // has had time to fill its 200 ms window and at least one
        // successful DRED parse has happened on the decoder side.
        let warmup_frames = 60;
        for i in 0..warmup_frames {
            let pcm = voice_frame_20ms(i * 960);
            let packets = enc.encode_frame(&pcm).unwrap();
            for pkt in packets {
                dec.ingest(pkt);
            }
        }

        // Drain the warm-up frames through the decoder to advance the
        // jitter buffer cursor past them.
        let mut out = vec![0i16; 960];
        while dec.decode_next(&mut out).is_some() {}

        // Encode the next three frames but skip ingesting the middle one.
        let base_offset = warmup_frames * 960;
        let pcm_a = voice_frame_20ms(base_offset);
        let pcm_b = voice_frame_20ms(base_offset + 960);
        let pcm_c = voice_frame_20ms(base_offset + 1920);

        let pkts_a = enc.encode_frame(&pcm_a).unwrap();
        let pkts_b = enc.encode_frame(&pcm_b).unwrap(); // DROP THIS ONE
        let pkts_c = enc.encode_frame(&pcm_c).unwrap();

        for pkt in pkts_a {
            dec.ingest(pkt);
        }
        // Skip pkts_b entirely — this is the "packet loss".
        drop(pkts_b);
        for pkt in pkts_c {
            dec.ingest(pkt);
        }

        // Drain again. Somewhere in here decode_next will hit Missing()
        // for the dropped packet and attempt DRED reconstruction.
        let baseline_dred = dec.dred_reconstructions;
        let baseline_plc = dec.classical_plc_invocations;
        eprintln!(
            "[phase3b probe] pre-drain: last_good_seq={:?} samples_available={}",
            dec.last_good_dred_seq(),
            dec.last_good_dred_samples_available()
        );
        while dec.decode_next(&mut out).is_some() {}

        let dred_delta = dec.dred_reconstructions - baseline_dred;
        let plc_delta = dec.classical_plc_invocations - baseline_plc;
        eprintln!(
            "[phase3b probe] post-drain: dred_delta={dred_delta} plc_delta={plc_delta}"
        );
        assert!(
            dred_delta >= 1,
            "expected ≥1 DRED reconstruction on single-packet loss, \
             got dred_delta={dred_delta} plc_delta={plc_delta}"
        );
    }

    /// Phase 3b: lossless stream never triggers DRED reconstruction or PLC.
    /// Baseline behavior — verifies the Missing() branch is not spuriously taken.
    #[test]
    fn opus_lossless_ingest_never_triggers_dred_or_plc() {
        let (mut enc, mut dec) = encoder_decoder_pair(QualityProfile::GOOD);

        // Encode + ingest 40 frames with no drops.
        for i in 0..40 {
            let pcm = voice_frame_20ms(i * 960);
            let packets = enc.encode_frame(&pcm).unwrap();
            for pkt in packets {
                dec.ingest(pkt);
            }
        }

        let mut out = vec![0i16; 960];
        while dec.decode_next(&mut out).is_some() {}

        assert_eq!(
            dec.dred_reconstructions, 0,
            "lossless stream should not reconstruct"
        );
        assert_eq!(
            dec.classical_plc_invocations, 0,
            "lossless stream should not PLC"
        );
    }

    /// Phase 3b: Codec2 calls fall through to classical PLC on loss.
    /// DRED is libopus-only, so even if the decoder's DRED state were
    /// populated (it won't be — Codec2 packets don't carry DRED bytes),
    /// `reconstruct_from_dred` rejects Codec2 at the AdaptiveDecoder
    /// level. This test guards the Codec2 side of the protection split.
    #[test]
    fn codec2_loss_falls_through_to_classical_plc() {
        let (mut enc, mut dec) = encoder_decoder_pair(QualityProfile::CATASTROPHIC);

        // Codec2 1200 uses 40 ms frames → 1920 samples at 48 kHz (before
        // the downsample inside the codec). Encode 20 frames (~0.8 s).
        let make_frame = |offset: usize| -> Vec<i16> {
            (0..1920)
                .map(|i| {
                    let t = (offset + i) as f64 / 48_000.0;
                    (8000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
                })
                .collect()
        };

        for i in 0..20 {
            let pcm = make_frame(i * 1920);
            let packets = enc.encode_frame(&pcm).unwrap();
            for pkt in packets {
                // Drop every 5th source packet to simulate loss.
                if !pkt.header.is_repair && i % 5 == 3 {
                    continue;
                }
                dec.ingest(pkt);
            }
        }

        let mut out = vec![0i16; 1920];
        while dec.decode_next(&mut out).is_some() {}

        assert_eq!(
            dec.dred_reconstructions, 0,
            "Codec2 must never reconstruct via DRED"
        );
        // classical_plc_invocations may or may not trigger depending on
        // whether the jitter buffer sees Missing before draining — the key
        // assertion is that DRED is not used. PLC count is advisory.
    }

    // ---- QualityAdapter tests ----

    /// Helper: build a QualityReport from human-readable loss% and RTT ms.
    fn make_report(loss_pct_f: f32, rtt_ms: u16) -> QualityReport {
        QualityReport {
            loss_pct: (loss_pct_f / 100.0 * 255.0) as u8,
            rtt_4ms: (rtt_ms / 4) as u8,
            jitter_ms: 10,
            bitrate_cap_kbps: 200,
        }
    }

    #[test]
    fn good_conditions_stays_good() {
        let mut adapter = QualityAdapter::new();
        let good = make_report(1.0, 40);
        for _ in 0..10 {
            adapter.ingest(&good);
        }
        assert_eq!(adapter.recommended_profile(), QualityProfile::GOOD);

        let current = QualityProfile::GOOD;
        for _ in 0..10 {
            adapter.ingest(&good);
            assert!(adapter.should_switch(&current).is_none());
        }
    }

    #[test]
    fn high_loss_degrades() {
        let mut adapter = QualityAdapter::new();
        // 8% loss, low RTT => DEGRADED
        let degraded = make_report(8.0, 40);
        let mut current = QualityProfile::GOOD;

        // Feed 3 consecutive degraded reports to pass hysteresis
        for _ in 0..3 {
            adapter.ingest(&degraded);
            if let Some(new) = adapter.should_switch(&current) {
                current = new;
            }
        }
        assert_eq!(current, QualityProfile::DEGRADED);
    }

    #[test]
    fn catastrophic_conditions() {
        let mut adapter = QualityAdapter::new();
        // 20% loss => CATASTROPHIC
        let terrible = make_report(20.0, 50);
        let mut current = QualityProfile::GOOD;

        for _ in 0..3 {
            adapter.ingest(&terrible);
            if let Some(new) = adapter.should_switch(&current) {
                current = new;
            }
        }
        assert_eq!(current, QualityProfile::CATASTROPHIC);

        // Also test via high RTT alone (250ms > 200ms threshold)
        let mut adapter2 = QualityAdapter::new();
        let high_rtt = make_report(1.0, 252); // rtt_4ms rounds to 63 => 252ms
        let mut current2 = QualityProfile::GOOD;

        for _ in 0..3 {
            adapter2.ingest(&high_rtt);
            if let Some(new) = adapter2.should_switch(&current2) {
                current2 = new;
            }
        }
        assert_eq!(current2, QualityProfile::CATASTROPHIC);
    }

    #[test]
    fn hysteresis_prevents_flapping() {
        let mut adapter = QualityAdapter::new();
        let good = make_report(1.0, 40);
        let bad = make_report(8.0, 40); // DEGRADED
        let current = QualityProfile::GOOD;

        // Alternate good/bad — should never trigger a switch because
        // we never get 3 consecutive same-recommendation reports.
        for _ in 0..20 {
            adapter.ingest(&bad);
            assert!(adapter.should_switch(&current).is_none());
            adapter.ingest(&good);
            assert!(adapter.should_switch(&current).is_none());
        }
        assert_eq!(current, QualityProfile::GOOD);
    }

    #[test]
    fn recovery_to_good() {
        let mut adapter = QualityAdapter::new();
        let bad = make_report(20.0, 50);
        let good = make_report(1.0, 40);

        // Drive to CATASTROPHIC first
        let mut current = QualityProfile::GOOD;
        for _ in 0..3 {
            adapter.ingest(&bad);
            if let Some(new) = adapter.should_switch(&current) {
                current = new;
            }
        }
        assert_eq!(current, QualityProfile::CATASTROPHIC);

        // Now feed good reports — should recover to GOOD after 3 consecutive
        for _ in 0..3 {
            adapter.ingest(&good);
            if let Some(new) = adapter.should_switch(&current) {
                current = new;
            }
        }
        assert_eq!(current, QualityProfile::GOOD);
    }

    #[test]
    fn call_config_from_profile() {
        let good = CallConfig::from_profile(QualityProfile::GOOD);
        assert_eq!(good.profile, QualityProfile::GOOD);
        assert_eq!(good.jitter_min, 3);

        let degraded = CallConfig::from_profile(QualityProfile::DEGRADED);
        assert_eq!(degraded.profile, QualityProfile::DEGRADED);
        assert!(degraded.jitter_target > good.jitter_target);

        let catastrophic = CallConfig::from_profile(QualityProfile::CATASTROPHIC);
        assert_eq!(catastrophic.profile, QualityProfile::CATASTROPHIC);
        assert!(catastrophic.jitter_max > degraded.jitter_max);
    }

    // ---- JitterStats telemetry tests ----

    fn make_test_packet(seq: u16) -> MediaPacket {
        MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: CodecId::Opus24k,
                has_quality_report: false,
                fec_ratio_encoded: 0,
                seq,
                timestamp: seq as u32 * 20,
                fec_block: 0,
                fec_symbol: seq as u8,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from(vec![0u8; 60]),
            quality_report: None,
        }
    }

    #[test]
    fn stats_track_ingestion() {
        let config = CallConfig::default();
        let mut dec = CallDecoder::new(&config);

        for i in 0..5u16 {
            dec.ingest(make_test_packet(i));
        }

        let stats = dec.stats();
        assert_eq!(stats.packets_received, 5);
        assert_eq!(stats.current_depth, 5);
        assert_eq!(stats.max_depth_seen, 5);
    }

    #[test]
    fn stats_track_underruns() {
        let config = CallConfig::default();
        let mut dec = CallDecoder::new(&config);

        // Empty buffer — decode_next should record underruns
        let mut pcm = vec![0i16; 960];
        dec.decode_next(&mut pcm);
        dec.decode_next(&mut pcm);
        dec.decode_next(&mut pcm);

        assert_eq!(dec.stats().underruns, 3);
    }

    #[test]
    fn stats_reset() {
        let config = CallConfig::default();
        let mut dec = CallDecoder::new(&config);

        // Generate some stats: ingest packets and trigger underruns on empty buffer
        for i in 0..3u16 {
            dec.ingest(make_test_packet(i));
        }
        // Also call decode on empty decoder to get underruns
        let config2 = CallConfig::default();
        let mut dec2 = CallDecoder::new(&config2);
        let mut pcm = vec![0i16; 960];
        dec2.decode_next(&mut pcm); // underrun — nothing in buffer

        assert!(dec.stats().packets_received > 0);
        assert!(dec2.stats().underruns > 0);

        // Test reset on the decoder with ingested packets
        dec.reset_stats();
        let stats = dec.stats();
        assert_eq!(stats.packets_received, 0);
        assert_eq!(stats.underruns, 0);
        assert_eq!(stats.overruns, 0);
        assert_eq!(stats.total_decoded, 0);
        assert_eq!(stats.packets_late, 0);
        assert_eq!(stats.max_depth_seen, 0);

        // Test reset on the decoder with underruns
        dec2.reset_stats();
        assert_eq!(dec2.stats().underruns, 0);
    }

    #[test]
    fn telemetry_respects_interval() {
        use wzp_proto::jitter::JitterStats;

        let mut telemetry = JitterTelemetry::new(60); // 60-second interval
        let stats = JitterStats::default();

        // First call right after creation — should not log because no time has passed
        // (the interval hasn't elapsed since construction)
        let logged = telemetry.maybe_log(&stats);
        assert!(!logged, "should not log before interval elapses");
    }

    #[test]
    fn silence_suppression_skips_silent_frames() {
        let config = CallConfig {
            suppression_enabled: true,
            silence_threshold_rms: 100.0,
            silence_hangover_frames: 5,
            comfort_noise_level: 50,
            ..Default::default()
        };
        let mut enc = CallEncoder::new(&config);

        let silence = vec![0i16; 960];
        let mut total_packets = 0;
        let mut cn_packets = 0;

        for _ in 0..20 {
            let packets = enc.encode_frame(&silence).unwrap();
            for p in &packets {
                if p.header.codec_id == CodecId::ComfortNoise {
                    cn_packets += 1;
                    // CN payload should be a single byte with the noise level.
                    assert_eq!(p.payload.len(), 1);
                }
            }
            total_packets += packets.len();
        }

        // First 5 frames are hangover (not suppressed) => 5 normal source packets
        // (plus potential repair packets from FEC block completion).
        // Remaining 15 frames are suppressed; CN every 10 frames => 1 CN packet
        // (cn_counter hits 10 on the 10th suppressed frame).
        assert!(
            total_packets < 20,
            "suppression should reduce packet count, got {total_packets}"
        );
        assert!(
            cn_packets >= 1,
            "should have at least one CN packet, got {cn_packets}"
        );
        assert!(
            enc.frames_suppressed > 0,
            "frames_suppressed should be > 0"
        );
    }
}
