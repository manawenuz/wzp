//! Opus decoder wrapping the `audiopus` crate.

use audiopus::coder::Decoder;
use audiopus::{Channels, MutSignals, SampleRate};
use audiopus::packet::Packet;
use wzp_proto::{AudioDecoder, CodecError, CodecId, QualityProfile};

/// Opus decoder implementing `AudioDecoder`.
///
/// Operates at 48 kHz mono output.
pub struct OpusDecoder {
    inner: Decoder,
    codec_id: CodecId,
    frame_duration_ms: u8,
}

// SAFETY: Same reasoning as OpusEncoder — exclusive access via &mut self.
unsafe impl Sync for OpusDecoder {}

impl OpusDecoder {
    /// Create a new Opus decoder for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| CodecError::DecodeFailed(format!("opus decoder init: {e}")))?;

        Ok(Self {
            inner: decoder,
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        })
    }

    /// Expected number of output PCM samples per frame.
    pub fn frame_samples(&self) -> usize {
        (48_000 * self.frame_duration_ms as usize) / 1000
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError> {
        let expected = self.frame_samples();
        if pcm.len() < expected {
            return Err(CodecError::DecodeFailed(format!(
                "output buffer too small: need {expected}, got {}",
                pcm.len()
            )));
        }
        let packet = Packet::try_from(encoded)
            .map_err(|e| CodecError::DecodeFailed(format!("invalid packet: {e}")))?;
        let signals = MutSignals::try_from(pcm)
            .map_err(|e| CodecError::DecodeFailed(format!("output signals: {e}")))?;
        let n = self
            .inner
            .decode(Some(packet), signals, false)
            .map_err(|e| CodecError::DecodeFailed(format!("opus decode: {e}")))?;
        Ok(n)
    }

    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        let expected = self.frame_samples();
        if pcm.len() < expected {
            return Err(CodecError::DecodeFailed(format!(
                "output buffer too small: need {expected}, got {}",
                pcm.len()
            )));
        }
        let signals = MutSignals::try_from(pcm)
            .map_err(|e| CodecError::DecodeFailed(format!("output signals: {e}")))?;
        let n = self
            .inner
            .decode(None, signals, false)
            .map_err(|e| CodecError::DecodeFailed(format!("opus PLC: {e}")))?;
        Ok(n)
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            CodecId::Opus24k | CodecId::Opus16k | CodecId::Opus6k => {
                self.codec_id = profile.codec;
                self.frame_duration_ms = profile.frame_duration_ms;
                Ok(())
            }
            other => Err(CodecError::UnsupportedTransition {
                from: self.codec_id,
                to: other,
            }),
        }
    }
}
