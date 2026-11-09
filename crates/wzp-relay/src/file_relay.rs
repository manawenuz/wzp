//! Opaque stream splicing for two-party rooms (file transfer).
//!
//! wzp-files peers exchange everything — their E2E handshake, the
//! encrypted `File*` control messages and the encrypted chunk data — on
//! QUIC *unidirectional* streams. In a two-party room (`file-*`, `call-*`)
//! the relay accepts every uni stream a participant opens, opens a matching
//! uni stream towards the other participant and copies bytes across. The
//! relay never sees plaintext (see docs/FILE-TRANSFER.md).
//!
//! Abuse controls (docs/ATTACK-SURFACE-RELAY-ABUSE.md, Tier E):
//! - per-session byte-rate shaping (token bucket, shaping not dropping —
//!   the streams are reliable, so excess traffic is simply slowed down),
//! - a per-session total byte cap and a per-stream cap (max file size +
//!   framing overhead), after which streams are reset,
//! - a cap on concurrently spliced streams per session,
//! - streams opened while no peer is present wait briefly, then are refused.
//!
//! [`DataRateLimiter`] is the datagram-side counterpart used by the room
//! forwarding loops for `MediaType::Data` packets.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::room::{ParticipantId, ParticipantSender, RoomManager};

/// QUIC application error code used when the relay refuses / cuts a stream.
pub const STREAM_REFUSED: u32 = 0x5710;
/// Stream reset because a byte cap was hit.
pub const STREAM_CAP: u32 = 0x5711;

/// Limits applied to spliced streams, per participant session.
#[derive(Clone, Debug)]
pub struct StreamRelayLimits {
    /// Sustained bytes/second per session.
    pub rate_bytes_per_sec: u64,
    /// Burst allowance in bytes.
    pub burst_bytes: u64,
    /// Total bytes one session may push through the relay.
    pub session_byte_cap: u64,
    /// Bytes one stream may carry (max file size + framing overhead).
    pub stream_byte_cap: u64,
    /// Concurrently spliced streams per session.
    pub max_concurrent_streams: usize,
    /// How long a new stream waits for the other participant.
    pub peer_wait: Duration,
}

impl Default for StreamRelayLimits {
    fn default() -> Self {
        Self {
            rate_bytes_per_sec: 32 * 1024 * 1024,
            burst_bytes: 8 * 1024 * 1024,
            session_byte_cap: 64 * 1024 * 1024 * 1024,
            // 10 GiB + ~1% AEAD/framing overhead + manifest.
            stream_byte_cap: 10 * 1024 * 1024 * 1024 + 128 * 1024 * 1024,
            max_concurrent_streams: 32,
            peer_wait: Duration::from_secs(10),
        }
    }
}

/// True for rooms whose participants get uni-stream splicing: only
/// two-party rooms, so "the other participant" is unambiguous.
pub fn room_allows_streams(room: &str) -> bool {
    room.starts_with("file-") || room.starts_with("call-")
}

/// True for rooms the relay must cap at two participants.
pub fn is_two_party_room(room: &str) -> bool {
    room_allows_streams(room)
}

/// Token-bucket shaper: `reserve` returns how long to wait before sending.
struct Shaper {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Shaper {
    fn new(rate: u64, burst: u64) -> Self {
        Self {
            rate: rate.max(1) as f64,
            burst: burst.max(1) as f64,
            tokens: burst as f64,
            last: Instant::now(),
        }
    }

    fn reserve(&mut self, bytes: usize) -> Duration {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate).min(self.burst);
        self.last = now;
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }
}

struct SessionState {
    shaper: std::sync::Mutex<Shaper>,
    total: AtomicU64,
    slots: Arc<Semaphore>,
}

/// Splice every uni stream `participant_id` opens to the other participant
/// of `room_name`. Runs until the participant's connection closes.
pub async fn run_stream_splicer(
    room_mgr: Arc<RoomManager>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    limits: StreamRelayLimits,
) {
    let conn = transport.connection().clone();
    let addr = conn.remote_address();
    let state = Arc::new(SessionState {
        shaper: std::sync::Mutex::new(Shaper::new(limits.rate_bytes_per_sec, limits.burst_bytes)),
        total: AtomicU64::new(0),
        slots: Arc::new(Semaphore::new(limits.max_concurrent_streams)),
    });
    info!(room = %room_name, participant = participant_id, %addr, "stream splicer started");
    loop {
        let mut src = match conn.accept_uni().await {
            Ok(s) => s,
            Err(_) => break,
        };
        let permit = match state.slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(room = %room_name, participant = participant_id, "too many concurrent streams, refusing");
                let _ = src.stop(quinn::VarInt::from_u32(STREAM_REFUSED));
                continue;
            }
        };
        let room_mgr = room_mgr.clone();
        let room_name = room_name.clone();
        let state = state.clone();
        let limits = limits.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // Find the peer (wait briefly if it hasn't joined yet).
            let deadline = Instant::now() + limits.peer_wait;
            let peer = loop {
                let others = room_mgr.others(&room_name, participant_id);
                if let Some(t) = others.iter().find_map(|s| match s {
                    ParticipantSender::Quic(t) => Some(t.clone()),
                    _ => None,
                }) {
                    break Some(t);
                }
                if Instant::now() >= deadline {
                    break None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            };
            let Some(peer) = peer else {
                debug!(room = %room_name, "no peer for spliced stream, refusing");
                let _ = src.stop(quinn::VarInt::from_u32(STREAM_REFUSED));
                return;
            };
            let mut dst = match peer.connection().open_uni().await {
                Ok(d) => d,
                Err(e) => {
                    debug!(room = %room_name, "open_uni to peer failed: {e}");
                    let _ = src.stop(quinn::VarInt::from_u32(STREAM_REFUSED));
                    return;
                }
            };
            splice(&mut src, &mut dst, &state, &limits).await;
        });
    }
    info!(room = %room_name, participant = participant_id, %addr,
        bytes = state.total.load(Ordering::Relaxed), "stream splicer stopped");
}

async fn splice(
    src: &mut quinn::RecvStream,
    dst: &mut quinn::SendStream,
    state: &SessionState,
    limits: &StreamRelayLimits,
) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut stream_bytes = 0u64;
    loop {
        let n = match src.read(&mut buf).await {
            Ok(Some(n)) => n,
            Ok(None) => {
                let _ = dst.finish();
                return;
            }
            Err(quinn::ReadError::Reset(code)) => {
                let _ = dst.reset(code);
                return;
            }
            Err(_) => {
                let _ = dst.reset(quinn::VarInt::from_u32(STREAM_REFUSED));
                return;
            }
        };
        stream_bytes += n as u64;
        let total = state.total.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        if stream_bytes > limits.stream_byte_cap || total > limits.session_byte_cap {
            warn!(stream_bytes, total, "spliced stream exceeded byte cap, resetting");
            let _ = dst.reset(quinn::VarInt::from_u32(STREAM_CAP));
            let _ = src.stop(quinn::VarInt::from_u32(STREAM_CAP));
            return;
        }
        let wait = state.shaper.lock().unwrap().reserve(n);
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        match dst.write_all(&buf[..n]).await {
            Ok(()) => {}
            Err(quinn::WriteError::Stopped(code)) => {
                let _ = src.stop(code);
                return;
            }
            Err(_) => {
                let _ = src.stop(quinn::VarInt::from_u32(STREAM_REFUSED));
                return;
            }
        }
    }
}

/// Per-session limiter for `MediaType::Data` datagrams in the room
/// forwarding loops. Over-budget packets are dropped (datagrams are
/// unreliable anyway; a RaptorQ sender repairs).
pub struct DataRateLimiter {
    bucket: crate::conformance::TokenBucket,
}

impl DataRateLimiter {
    /// Default: 8 MiB/s sustained, 16 MiB burst.
    pub fn new() -> Self {
        Self::with_rate(8 * 1024 * 1024, 16 * 1024 * 1024)
    }

    pub fn with_rate(bytes_per_sec: u64, burst: u64) -> Self {
        Self {
            bucket: crate::conformance::TokenBucket::new(burst, bytes_per_sec),
        }
    }

    /// True if a packet of `bytes` may be forwarded now.
    pub fn allow(&mut self, bytes: usize) -> bool {
        self.bucket.try_consume(bytes as u64, Instant::now()).is_ok()
    }
}

impl Default for DataRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_prefixes() {
        assert!(room_allows_streams("file-abc"));
        assert!(room_allows_streams("call-1"));
        assert!(!room_allows_streams("android"));
    }

    #[test]
    fn shaper_paces() {
        let mut s = Shaper::new(1000, 1000);
        assert!(s.reserve(1000).is_zero());
        let w = s.reserve(500);
        assert!(w >= Duration::from_millis(450) && w <= Duration::from_millis(550));
    }

    #[test]
    fn data_limiter_drops_over_budget() {
        let mut l = DataRateLimiter::with_rate(1000, 2000);
        assert!(l.allow(1500));
        assert!(!l.allow(1000));
    }
}
