# PRD: Transport Feedback & Bandwidth Estimator

> **Status:** proposed
> **Resolves:** Audit W6 (no BWE), W14 (no receiver→sender feedback channel).
> **Depends on:** PRD #1 (wire format v2 — for u32 seq).

## Problem

`AdaptiveQualityController` decides tier transitions from loss% and RTT only. Quinn exposes congestion-window and bytes-in-flight, but we don't consume them. There is no receiver→sender feedback channel beyond the inline 4-byte `QualityReport`.

Consequences:
- On stable links with spare capacity, we never upgrade past the declared profile (audio stuck at Opus 24 k when 64 k is available).
- Oscillation between adjacent tiers on the boundary.
- **No bandwidth-aware adaptation = no usable video.** Video without BWE either oscillates wildly or never uses available capacity.

## Goals

- Continuous bandwidth estimate per session, surfaced to adaptation controllers.
- Receiver→sender feedback at ~50 ms cadence carrying ack/nack/remb.
- Audio benefits immediately (smarter upgrades, fewer oscillations).
- Video uses BWE as its primary input (PRD #7).

## Non-goals

- Replacing Quinn's congestion controller — we ride on top.
- Cross-stream BWE (each session estimates independently for v1).

## Design

### `SignalMessage::TransportFeedback`

New signal variant, sent on the existing signal stream every 50 ms or every N media packets, whichever first:

```rust
pub struct TransportFeedback {
    pub version: u8,            // PRD #4 W12: always present
    pub stream_id: u8,           // 0 for session-wide; >0 for per-stream
    pub acked_seqs: Vec<u32>,    // recent seqs received OK (RLE-compressed)
    pub nacked_seqs: Vec<u32>,   // recent seqs missing (RLE-compressed)
    pub remb_bps: u32,           // receiver's estimated max bandwidth
    pub recv_time_us: u64,       // arrival-time for sender-side jitter calc
}
```

RLE compression keeps the wire size bounded (typical payload ~50 B).

### `BandwidthEstimator` (in `wzp-proto`)

```rust
pub struct BandwidthEstimator {
    cwnd_bps: AtomicU64,         // from Quinn path stats
    bytes_in_flight: AtomicU64,  // from Quinn path stats
    peer_remb_bps: AtomicU64,    // from TransportFeedback
    smoothed_bps: AtomicU64,     // EWMA output
}

impl BandwidthEstimator {
    pub fn update_from_quinn(&self, stats: &QuinnPathStats);
    pub fn update_from_peer(&self, fb: &TransportFeedback);
    pub fn target_send_bps(&self) -> u64 {
        // 0.9 × min(cwnd_bps, peer_remb_bps), EWMA-smoothed
    }
}
```

Three signals fused:
1. **Quinn cwnd.** Conservative ceiling — sending faster than cwnd just drops or queues.
2. **Peer REMB.** Receiver's perspective on what they can actually consume (after their own jitter buffer, decode budget, etc.).
3. **EWMA smoothing.** Half-life ~2 s; avoids oscillation.

Target = 90 % of `min(cwnd, remb)`, leaving headroom for probing upward.

### Adaptation controller integration

`AdaptiveQualityController::tick()` already consumes loss/RTT/jitter. Add BWE input:

```rust
if self.bwe.target_send_bps() > self.current_tier_ceiling_bps() * 1.3
    && consecutive_upgrade_reports >= UPGRADE_THRESHOLD {
    self.upgrade_one_tier();
}
```

Upgrade gated on BWE *headroom*, not just clean reports. Eliminates the "always at Opus 24 k on a fiber link" pathology.

### Probing

To detect unused capacity, sender occasionally adds 5–10 % padding/FEC during otherwise-clean windows. If `cwnd` doesn't drop and `remb` doesn't fall, the headroom is real — upgrade. If signals degrade, back off. Cheap and standard.

## Implementation outline

1. New `wzp-proto::bwe::BandwidthEstimator`.
2. `wzp-transport` exposes `QuinnPathStats { cwnd_bps, bytes_in_flight, rtt_ms }`; already partially there via `QuinnPathSnapshot`.
3. `SignalMessage::TransportFeedback` variant + serde.
4. Receiver-side: track recent seqs in a ring buffer; emit feedback every 50 ms.
5. Sender-side: BWE consumes own Quinn stats + incoming feedback.
6. `AdaptiveQualityController::set_bwe(&BandwidthEstimator)`.
7. Prometheus: `wzp_session_bwe_bps`, `wzp_session_remb_bps`, `wzp_session_cwnd_bps`.
8. Probing logic behind a flag for first deployment.

## Acceptance criteria

- On a shaped 5 Mbps link with Opus 24 k, controller upgrades to Opus 64 k within 30 s.
- On a shaped 50 kbps link, controller stays at Opus 6 k and does not oscillate.
- Feedback wire size < 100 B per 50 ms (= < 2 kbps overhead).
- Probing finds headroom on a 10 Mbps link in < 60 s.

## Risks

- **Probing-induced loss on already-saturated links.** Mitigation: probe only when smoothed loss < 1 % over 10 s.
- **Feedback storm under heavy loss.** Mitigation: feedback rate capped at 20 Hz independent of media rate.
- **Quinn cwnd lies on QUIC-over-some-VPNs.** Mitigation: REMB serves as cross-check; take min of the two.

## Effort

~4 engineer-days (Wave 2 tasks T2.1–T2.3).
