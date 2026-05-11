# Relay Abuse: Attack Surface & Mitigations

> WZP is end-to-end encrypted. The relay forwards ciphertext and cannot inspect payload content. This document enumerates the abuse vectors that survive E2E and the mitigations available without breaking it.
>
> Motivating threat: a PoC on another project (LiveKit) showed that an E2E SFU with no conformance enforcement can be repurposed as a free arbitrary-data tunnel. WZP must not be that.

## Threat model

### In scope

- **Bulk data tunneling.** Attacker uses a legitimate handshake, then pushes arbitrary bytes (file transfer, piracy, scraped traffic) through media datagrams.
- **Bandwidth parasitism.** Attacker uses the relay as a cheap forwarder for unrelated traffic at scale.
- **Quota / billing evasion.** Attacker disguises high-bandwidth use as low-bandwidth audio.
- **DoS via amplification.** Attacker sends one packet → SFU fans out to N peers, multiplying egress cost N×.

### Out of scope (cannot be solved without breaking E2E)

- **Steganography inside real audio.** Modulating Opus-encoded waveforms to encode a covert channel. Information-theoretic limit; ~tens to hundreds of bps achievable; economically uninteresting.
- **Modem-over-call.** Real audio whose semantic content is data. Same limit.
- **Slow exfiltration under all rate caps.** Attacker who stays within audio's natural bandwidth envelope, indefinitely.

### Threat actor profile

We are defending against **economically motivated abuse at scale**, not against a determined nation-state covert channel. The former needs bandwidth and is loud; the latter is impossible to stop and not worth the engineering cost.

## What the relay can observe

Despite E2E, the relay sees a lot. None of this is encrypted to the relay:

| Observable | Source | Bits available |
|---|---|---|
| `CodecID` (declared codec) | `MediaHeader`, AAD | 4 (today) / 6 (v2) |
| `MediaType` (audio / video / data / control) | `MediaHeader` v2 | 2 |
| `sequence`, `timestamp_ms` | `MediaHeader` | 32 + 32 |
| `fec_block_id`, `fec_symbol_idx`, `FecRatio`, `T` (repair) | `MediaHeader` | varies |
| `KeyFrame` bit | `MediaHeader` v2 | 1 |
| `Q` flag (QualityReport trailer present) | `MediaHeader` | 1 |
| Packet size | QUIC layer | — |
| Packet inter-arrival timing | QUIC layer | — |
| Aggregate bytes/sec per session | RelayMetrics | — |
| Source fingerprint, src IP | Session state | — |

This is enough surface for strong conformance enforcement without ever touching encrypted payload.

## Mitigation tiers

Listed in order of cost-to-implement vs. decisiveness. Tier A alone kills the gross-abuse threat. Higher tiers add defense in depth.

### Tier A — Codec-conformance bitrate caps

For each declared `CodecID`, the wire bitrate has a math-derivable hard ceiling:

```
ceiling_bps[CodecID] = nominal_bitrate * (1 + max_FEC_ratio) * (1 + overhead_pct)
                     = nominal * 3.0 * 1.15        // FEC max 2.0 → factor 3.0
```

| Codec | Nominal | Hard ceiling |
|---|---|---|
| Opus 64k | 64 kbps | ~221 kbps |
| Opus 24k | 24 kbps | ~83 kbps |
| Opus 6k | 6 kbps | ~21 kbps |
| Codec2 1200 | 1.2 kbps | ~4 kbps |
| ComfortNoise | 0 | ~2 kbps |

Sliding 1 s window per session. Sustained excess → hard violation, close session.

Decisive against bulk tunneling. False-positive rate negligible if ceilings set at math-derived max × 1.5.

### Tier B — Packet-rate conformance

Each codec has a fixed frame interval (20 ms or 40 ms), so legal `pps` is 25 or 50, plus FEC repair packets (max ~150 pps total at FEC ratio 2.0). Anything sustaining > 200 pps for an audio codec is not audio.

### Tier C — Timestamp-rate consistency

`timestamp_ms` advances at the declared frame interval. `Δtimestamp / Δseq` over a rolling window should match the codec's frame duration ±2×. Divergence catches abusers who send audio-rate small packets but burn fields for payload.

### Tier D — Per-codec packet-size sanity

EWMA of packet size per session, compared to per-codec typical:

| Codec | Typical | Reject above |
|---|---|---|
| Opus 24k 20 ms | 60–80 B | 160 B |
| Opus 6k 40 ms | 30–40 B | 90 B |
| Codec2 1200 40 ms | 6 B | 30 B |
| ComfortNoise | 0–4 B | 16 B |

### Tier E — Per-fingerprint / per-IP token bucket

Aggregate quota regardless of declared codec:

```
For each (fingerprint, src_ip):
  monthly_bytes_quota   authenticated  = 50 GB    (tune)
                        anonymous      = 1 GB
  per-session cap       audio          = 256 kbps
                        video          = 5 Mbps
  burst                                = 30 s at 2× cap
```

Won't stop a single rogue session under cap; bounds aggregate blast radius and makes relay economics predictable.

### Tier F — Behavioral entropy / statistical fingerprinting

The deeper layer. Computed continuously per session over 10–30 s windows. Combined score flags streams that pass declared-codec checks but do not statistically look like real media.

**Why this works:** real audio and real video have very specific statistical signatures that tunneled data does not naturally produce, and that an attacker would have to deliberately and expensively mimic. The signatures differ wildly between audio and video — which is exactly why we separate them (see next section).

#### Audio fingerprint features

| Feature | Real Opus speech | Tunneled data |
|---|---|---|
| **IAT coefficient of variation** | 0.1–0.4 (clocked) | > 1.0 (bursty) |
| **Payload-size distribution** | Bimodal: speech 60–80 B + silence/CN 0–10 B | Unimodal, large, MTU-skewed |
| **Silence fraction** | 10–40 % (real conversation pauses) | < 2 % |
| **Bitrate over 30 s** | Tracks nominal codec ±20 % | Often saturates ceiling |
| **`Q` flag cadence** | Periodic, regular | Absent or random |
| **DRED / FEC ratio response** | Tracks `QualityReport` trend | Static or noise |

Single derived score: `audio_legitimacy ∈ [0, 1]`. Below threshold (e.g. 0.3) for 60 s → flag.

#### Video fingerprint features (post-V1)

| Feature | Real H.264 / AV1 video | Tunneled data |
|---|---|---|
| **Keyframe periodicity** | Regular (every 1–4 s, or on PLI) | Absent or uniform `KeyFrame=1` |
| **Frame-size ratio (I / P)** | 5–20× | ≈ 1× |
| **Burst structure** | One I-frame = N packets in < 5 ms, then quiet | Uniform spacing |
| **Bitrate response to BWE feedback** | Tracks `TransportFeedback::remb_bps` | Ignores it |
| **Resolution / FPS implied by bitrate** | Coherent (240 p ≠ 8 Mbps) | Incoherent |
| **NACK / PLI responsiveness** | Sender produces keyframe within 200 ms | No response |

Single derived score: `video_legitimacy ∈ [0, 1]`.

#### Implementation shape

```rust
pub struct LegitimacyScorer {
    media_type: MediaType,
    iat_ewma: ExponentialMovingAverage,
    iat_variance: ExponentialMovingVariance,
    size_histogram: SizeBuckets<8>,
    silence_count: u32,
    speech_count: u32,
    quality_reports_seen: u32,
    keyframe_intervals: RingBuffer<u32, 16>,
    window_start: Instant,
}

impl LegitimacyScorer {
    pub fn observe(&mut self, header: &MediaHeader, payload_len: usize, now: Instant);
    pub fn score(&self) -> f32;             // [0, 1]
    pub fn verdict(&self) -> Verdict;       // Legitimate | Suspect | Abusive
}
```

Cheap: a few floats and counters per session. Update on every packet, score every 1 s, escalate over 30+ s.

### Tier G — Reactive response

A scoring system needs a response policy:

| Verdict | Action |
|---|---|
| Legitimate | None |
| Suspect | Apply tighter Tier-E quota; emit `relay_conformance_suspect_total` |
| Abusive | Close session with `Hangup::PolicyViolation`; log to audit; cool-down fingerprint |
| Repeat-abusive | Lower-tier quota across the federation (gossip via federation channel) |

Never silent-drop. Always close with a typed reason so legitimate users hitting a bug get a clear error.

## Separating audio and video

**Yes — this is one of the strongest arguments for the v2 `MediaType` bit and should be a hard design rule.**

Audio and video have nothing in common statistically:

| Property | Audio | Video |
|---|---|---|
| Bitrate | 6–64 kbps | 100 kbps – 5 Mbps |
| Packet rate | 25–50 pps | 500–2000 pps |
| Packet size | 6–160 B | 200–1450 B |
| Burst structure | Clocked, near-CBR | Bursty (I-frames) |
| Silence | Common (10–40 %) | Meaningless |
| Loss tolerance | High (PLC, DRED) | Variable (keyframes critical) |
| Recovery primitive | FEC + DRED | NACK + PLI + keyframe cache |

A single scoring model trying to cover both would have to be so permissive at the union of envelopes that it would let tunnels through. **Separation is mandatory for Tier F to work.**

### What separation requires

1. **`MediaType:2` in `MediaHeader` v2** (already in `ROAD-TO-VIDEO.md` Phase V1). Without this, the relay must keep a `CodecID → MediaType` table and update it every time a codec is added — fragile.
2. **Per-`MediaType` conformance rules.** A and B and D have separate tables per type. Tier F has separate scorers.
3. **Per-`MediaType` quotas.** Tier E uses two buckets: `audio_bps_cap`, `video_bps_cap`. A session in audio-only mode never gets to spend the video budget. A video session has both, audio-priority.
4. **Per-`MediaType` keyframe/silence semantics.** `KeyFrame` bit is meaningless for audio; silence fraction is meaningless for video. The scorer needs to know which features apply.

### Bonus: separation also helps the SFU

Beyond abuse detection, the same separation makes graceful degradation cleaner: under congestion the relay can drop video packets first while preserving audio, because it knows which is which without parsing the codec table.

## Open questions for later decision

1. **Hard-close on first hard violation, or three-strikes?** Three-strikes is friendlier but lets twice the abuse through. Recommend hard-close + clear typed reason; legitimate users will reconnect, abusers won't try again at the same fingerprint.
2. **Where do verdicts persist?** In-memory per relay is simplest. Federated gossip is more powerful but a new attack surface (poisoning).
3. **Threshold tuning.** All thresholds in this doc are first-pass math. Real numbers come from a few weeks of Prometheus data on legitimate traffic before any enforcement turns on.
4. **Anonymous vs. authenticated split.** featherChat-authed users get generous quotas; anonymous users get tight ones. This makes the economics of mass abuse hostile (need many real identities) without locking out small legitimate use.
5. **What to log.** Conformance hits should be Prometheus counters + ringbuffer of recent violations; never log raw payload content (even encrypted) for privacy.

## Suggested implementation order (whenever this is picked up)

| Step | What | Why first |
|---|---|---|
| 1 | Land v2 wire format with `MediaType:2` | Prereq for separation; already on the road-to-video plan |
| 2 | Tier A + B + C as `wzp-relay/src/conformance.rs` | Kills bulk tunneling; cheap; no false positives if math is right |
| 3 | Prometheus metrics for violations + raw observables (IAT, size, silence frac) | Gather baseline of legitimate traffic before tightening |
| 4 | Tier D + E (size sanity + token bucket) | Defense in depth |
| 5 | Tier F scorer, audio-only first; tuned against the baseline from step 3 | Adds covert-tunnel pressure |
| 6 | Tier F video scorer once video is in production | Same shape, different features |
| 7 | Tier G response policy + audit log | Operationalize |

Steps 1–2 are decisive against the LiveKit-style PoC. The rest is steady tightening as real traffic accumulates.

## What this does NOT promise

- It does not stop a patient adversary running a slow covert channel inside real audio. Nothing E2E-preserving can.
- It does not detect content (no CSAM scan, no copyright fingerprint). Those would require breaking E2E and are out of scope by design.
- It does not eliminate abuse — it makes abuse loud, expensive, and detectable, which is the realistic goal for any E2E system.
