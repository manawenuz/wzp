---
tags: [prd, wzp]
type: prd
---

# PRD: Relay Conformance Enforcement (Abuse Mitigation Tiers A–G)

> **Status:** proposed
> **Resolves:** All in-scope vectors from `docs/ATTACK-SURFACE-RELAY-ABUSE.md`.
> **Depends on:** PRD #1 (wire format v2 — for `MediaType` separation in Tiers D/F).

## Problem

WZP relays forward E2E-encrypted ciphertext and cannot inspect payload content. A trivial PoC on another E2E SFU (LiveKit) showed that without conformance enforcement, the relay becomes a free arbitrary-data tunnel. WZP must enforce media-shape conformance against observable header and timing metadata, without breaking E2E.

## Goals

- Make bulk data tunneling through WZP infeasible.
- Bound aggregate per-user abuse blast radius.
- Make covert tunneling expensive (Tier F) without false-positiving real calls.
- Audio and video evaluated by **separate scorers** (statistical signatures don't overlap).

## Non-goals

- Content inspection (would break E2E).
- Detecting steganographic covert channels inside legitimate audio (information-theoretic limit; not worth chasing).
- CSAM / copyright detection (would require E2E break; explicit non-goal).

## Design — tiered enforcement

### Tier A — Codec-conformance bitrate caps

For each `CodecID`, compute math-derived ceiling and enforce sliding 1 s window per session:

```
ceiling_bps[CodecID] = nominal * (1 + max_FEC_ratio) * (1 + overhead_pct)
                     = nominal * 3.0 * 1.15
```

Hard violation (sustained > ceiling for 1 s) → close session with `Hangup::PolicyViolation { code: BITRATE }`.

### Tier B — Packet-rate cap

Per `CodecID`, max `pps` known (25 or 50 base × up to 3× for FEC = ~150 pps for audio). Sustained > 200 pps audio → hard violation.

### Tier C — Timestamp-rate consistency

`Δtimestamp_ms / Δsequence` over rolling 200-packet window must match codec frame duration ± 2×. Violation → hard.

### Tier D — Per-codec packet-size sanity

EWMA(`payload_len`) per session; reject sustained mean > 2× codec typical. Per-codec table in spec.

### Tier E — Per-fingerprint / per-IP token bucket

```
For each (fingerprint, src_ip):
  monthly_bytes_quota   authed = 50 GB         (tunable)
                        anon   = 1 GB
  per-session bps cap   audio  = 256 kbps
                        video  = 5 Mbps
  burst                 = 30 s @ 2× cap
```

Anonymous quotas tight; authenticated (via featherChat) quotas generous. Soft enforcement: throttle, then close on persistent overage.

### Tier F — Behavioral entropy scoring (per `MediaType`)

Separate scorers for audio and video. Computed over 10–30 s windows.

**Audio scorer features:**

| Feature | Legitimate | Abusive |
|---|---|---|
| IAT coefficient of variation | 0.1–0.4 | > 1.0 |
| Payload-size bimodality | Bimodal (speech + silence) | Unimodal |
| Silence fraction | 10–40 % | < 2 % |
| 30 s bitrate vs. nominal | ± 20 % | Saturates ceiling |
| `Q` flag cadence | Periodic | Absent/random |

**Video scorer features (post-PRD #5):**

| Feature | Legitimate | Abusive |
|---|---|---|
| Keyframe periodicity | Regular (1–4 s or on PLI) | Absent / uniform KF=1 |
| I/P frame-size ratio | 5–20× | ~1× |
| Burst structure | I-frame in < 5 ms, then quiet | Uniform spacing |
| Bitrate response to BWE | Tracks `remb_bps` | Ignores |
| NACK/PLI responsiveness | Keyframe within 200 ms | No response |

Output: `legitimacy ∈ [0, 1]` per session per `MediaType`. < 0.3 for 60 s → Suspect; < 0.1 for 60 s → Abusive.

### Tier G — Reactive response

```
Verdict::Legitimate     → no action
Verdict::Suspect        → apply tighter Tier E quota; emit metric
Verdict::Abusive        → close session with typed Hangup; cool-down fingerprint 1 h
Verdict::RepeatAbusive  → relay-local block 24 h; (optional gossip)
```

Always typed close. No silent drops.

## Implementation outline

New module `wzp-relay/src/conformance.rs`:

```rust
pub struct ConformanceMeter {
    media_type: MediaType,
    declared_codec: AtomicU8,
    bytes_window: SlidingWindow<1000>,
    packet_window: SlidingWindow<1000>,
    iat_ewma: ExponentialMovingAverage,
    iat_variance: ExponentialMovingVariance,
    size_histogram: SizeBuckets<8>,
    silence_count: AtomicU32,
    speech_count: AtomicU32,
    quality_reports_seen: AtomicU32,
    last_timestamp_ms: AtomicU32,
    last_seq: AtomicU32,
    keyframe_intervals: RingBuffer<u32, 16>,
    violations: AtomicU32,
}

impl ConformanceMeter {
    pub fn observe(&self, h: &MediaHeader, payload_len: usize, now: Instant) -> Result<(), Violation>;
    pub fn legitimacy(&self) -> f32;
    pub fn verdict(&self) -> Verdict;
}
```

Hooked into per-participant forwarding loop in `RoomManager`. Tier A–D run synchronously (cheap). Tier F runs on a periodic task (every 1 s per session).

Prometheus exports:

```
wzp_relay_conformance_violations_total{tier,codec_id,media_type,verdict}
wzp_relay_conformance_legitimacy{media_type}     histogram
wzp_relay_conformance_iat_cov{media_type}        histogram
wzp_relay_conformance_silence_fraction           histogram
```

## Rollout

1. Deploy with all tiers in **observe-only** mode (Prometheus only, no enforcement).
2. Collect 1–2 weeks of baseline traffic.
3. Set thresholds at observed 99.9th percentile of legitimate traffic + headroom.
4. Flip Tier A enforcement first (highest confidence, lowest false-positive risk).
5. Flip B, C, D over 2 weeks.
6. Tune Tier F thresholds against the baseline; flip Suspect first, then Abusive.

## Acceptance criteria

- Synthetic abuse test (5 Mbps random bytes declared as Opus 24 k) closed within 1 s.
- Synthetic abuse test (audio-rate small packets with stuffed payload) closed within 5 s by Tier D.
- Synthetic abuse test (audio-rate, audio-sized, but no silence and CoV=2.0 IAT) flagged Suspect within 60 s.
- Real-call false-positive rate < 0.1 % over a week of production baseline.
- All verdict transitions emit Prometheus counters.

## Risks

- **False positives on edge cases** (long lectures with little silence, ambient-music calls). Mitigation: Tier F floor at Suspect for 30 s minimum; manual review channel for repeat-flagged authed users.
- **Threshold drift** as codecs evolve. Mitigation: ceilings are math-derived from codec table; updated when codec table updates.
- **Federated abuse moving between relays.** Mitigation: Tier G optional gossip (post-Wave 5).

## Effort

- Tier A + B + C: 1.5 d (T2.4 + T2.5)
- Tier D: 0.5 d (T3.6)
- Tier E: 1.5 d (T3.5)
- Tier F audio: 3 d (T5.7)
- Tier F video: 3 d (T6.2)
- Tier G: 1 d (T5.8)

Total: ~10 engineer-days, spread across Waves 2–6.
