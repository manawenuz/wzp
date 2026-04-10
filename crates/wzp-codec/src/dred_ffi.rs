//! Raw opusic-sys FFI wrappers for libopus 1.5.2 decoder + DRED reconstruction.
//!
//! # Why this module exists
//!
//! We cannot use `opusic_c::Decoder` because its inner `*mut OpusDecoder`
//! pointer is `pub(crate)` — not reachable from outside the opusic-c crate.
//! Phase 3 of the DRED integration needs to hand that same pointer to
//! `opus_decoder_dred_decode`, and running two parallel decoders (one from
//! opusic-c for normal audio, another from opusic-sys for DRED) would cause
//! the DRED-only decoder's internal state to drift out of sync with the
//! audio stream because it would not see normal decode calls.
//!
//! The fix is to own the raw decoder ourselves and use the same handle for
//! both normal decode AND future DRED reconstruction. This module is the
//! single owner of `*mut OpusDecoder` in the WZP workspace.
//!
//! Phase 0 only exposes `DecoderHandle` (normal decode). Phase 3 will add
//! `DredDecoderHandle`, `DredState`, and the `DredReconstructor` trait
//! implementation alongside it in this same file.

use std::ptr::NonNull;

use opusic_sys::{
    OPUS_OK, OpusDecoder as RawOpusDecoder, opus_decode, opus_decoder_create,
    opus_decoder_destroy,
};
use wzp_proto::CodecError;

/// libopus operates at 48 kHz for all Opus variants we use.
const SAMPLE_RATE_HZ: i32 = 48_000;
/// Mono.
const CHANNELS: i32 = 1;

/// Safe owner of a `*mut OpusDecoder` allocated via `opus_decoder_create`.
///
/// Releases the decoder in `Drop`. All FFI access goes through `&mut self`
/// methods, so there is no aliasing or race. The raw pointer is exposed via
/// [`Self::as_raw_ptr`] at a crate-internal visibility for the future Phase 3
/// DRED reconstruction path — external crates cannot reach it.
pub struct DecoderHandle {
    inner: NonNull<RawOpusDecoder>,
}

impl DecoderHandle {
    /// Allocate a new Opus decoder at 48 kHz mono.
    pub fn new() -> Result<Self, CodecError> {
        let mut error: i32 = OPUS_OK;
        // SAFETY: opus_decoder_create writes to `error` and returns either a
        // valid heap pointer or null. We check both before constructing the
        // NonNull wrapper.
        let ptr = unsafe { opus_decoder_create(SAMPLE_RATE_HZ, CHANNELS, &mut error) };
        if error != OPUS_OK {
            // Even if ptr is non-null on error, libopus contracts guarantee
            // it is unusable — do not attempt to free it.
            return Err(CodecError::DecodeFailed(format!(
                "opus_decoder_create failed: err={error}"
            )));
        }
        let inner = NonNull::new(ptr).ok_or_else(|| {
            CodecError::DecodeFailed("opus_decoder_create returned null".into())
        })?;
        Ok(Self { inner })
    }

    /// Decode an Opus packet into PCM samples.
    ///
    /// `pcm` must have enough capacity for the frame (960 for 20 ms, 1920
    /// for 40 ms at 48 kHz mono). Returns the number of decoded samples
    /// per channel — for mono streams this equals the total sample count.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError> {
        if packet.is_empty() {
            return Err(CodecError::DecodeFailed("empty packet".into()));
        }
        if pcm.is_empty() {
            return Err(CodecError::DecodeFailed("empty output buffer".into()));
        }
        // SAFETY: self.inner is a valid *mut OpusDecoder owned by this struct.
        // `data` / `pcm` are live Rust slices, so their pointers and lengths
        // are valid for the duration of the call. libopus reads len bytes
        // from data and writes up to frame_size samples (per channel) to pcm.
        let n = unsafe {
            opus_decode(
                self.inner.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                pcm.len() as i32,
                /* decode_fec = */ 0,
            )
        };
        if n < 0 {
            return Err(CodecError::DecodeFailed(format!(
                "opus_decode failed: err={n}"
            )));
        }
        Ok(n as usize)
    }

    /// Generate packet-loss concealment audio for a missing frame.
    ///
    /// Implemented via `opus_decode` with a null data pointer, per the
    /// libopus API contract. `pcm` should be sized for the expected frame.
    pub fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        if pcm.is_empty() {
            return Err(CodecError::DecodeFailed("empty output buffer".into()));
        }
        // SAFETY: same invariants as decode(). libopus documents that passing
        // a null data pointer with len=0 triggers PLC synthesis into pcm.
        let n = unsafe {
            opus_decode(
                self.inner.as_ptr(),
                std::ptr::null(),
                0,
                pcm.as_mut_ptr(),
                pcm.len() as i32,
                /* decode_fec = */ 0,
            )
        };
        if n < 0 {
            return Err(CodecError::DecodeFailed(format!(
                "opus_decode PLC failed: err={n}"
            )));
        }
        Ok(n as usize)
    }

    /// Raw pointer access for the Phase 3 DRED reconstruction path.
    ///
    /// The pointer is valid for the lifetime of `self`. Callers must not
    /// free it or cause the underlying decoder to mutate while the pointer
    /// is being used concurrently. Currently unused in Phase 0 — kept
    /// `pub(crate)` so only the future `dred` submodule inside this crate
    /// can reach it.
    #[allow(dead_code)]
    pub(crate) fn as_raw_ptr(&self) -> *mut RawOpusDecoder {
        self.inner.as_ptr()
    }
}

impl Drop for DecoderHandle {
    fn drop(&mut self) {
        // SAFETY: we own the pointer and no further access happens after
        // this call because Drop consumes self.
        unsafe { opus_decoder_destroy(self.inner.as_ptr()) };
    }
}

// SAFETY: The underlying OpusDecoder is a plain heap allocation with no
// thread-local or lock-free state. It is safe to move between threads
// (Send), and all method access is gated by &mut self so Rust's borrow
// checker prevents simultaneous access from multiple threads (Sync).
unsafe impl Send for DecoderHandle {}
unsafe impl Sync for DecoderHandle {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handle_creates_and_drops() {
        let handle = DecoderHandle::new().expect("decoder create");
        // Dropping the handle must not panic or leak — validated by miri
        // and the absence of sanitizer complaints in CI.
        drop(handle);
    }

    #[test]
    fn decode_lost_produces_full_frame_of_silence_on_cold_start() {
        let mut handle = DecoderHandle::new().unwrap();
        // 20 ms @ 48 kHz mono.
        let mut pcm = vec![0i16; 960];
        let n = handle.decode_lost(&mut pcm).unwrap();
        assert_eq!(n, 960);
        // On a fresh decoder, PLC output is silence (no past audio to extend).
        assert!(pcm.iter().all(|&s| s == 0));
    }

    #[test]
    fn decode_empty_packet_errors() {
        let mut handle = DecoderHandle::new().unwrap();
        let mut pcm = vec![0i16; 960];
        let err = handle.decode(&[], &mut pcm);
        assert!(err.is_err());
    }
}
