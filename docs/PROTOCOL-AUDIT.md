# WZP Protocol Audit

> Protocol-level review of WZP as of 2026-05-11. See `WZP-SPEC.md` for the spec being audited.

## Strengths

- **QUIC datagrams instead of raw UDP + SRTP** — buys TLS 1.3, PLPMTUD, path migration, and ACK-based loss/RTT estimation. Quinn's `PathSnapshot` feeding `DredTuner` is something WebRTC stacks build from scratch.
- **Continuous DRED tuning.** Mapping RTT / loss / jitter to a continuous Opus DRED lookback window is genuinely better than discrete tiers — most stacks treat DRED as on/off.
- **MiniHeader (49/50).** At 50 pps that is ~400 B/s saved per stream; meaningful at scale.
- **SFU never decodes.** Preserves E2E. Most SFUs (LiveKit, Janus) terminate SRTP at the SFU.
- **RaptorQ for low-bitrate Codec2 + DRED for Opus.** Correct split — DRED is cheaper than FEC at high bitrate; RaptorQ shines when you can afford many small symbols.

## Weaknesses

### W1. `u16` sequence wraps every ~21 minutes at 50 pps
Anti-replay window is 64 packets so wrap is safe for replay. **But** the jitter buffer's `BTreeMap<u16, _>` will misorder across the wrap boundary if a packet is delayed more than ~32 k frames. Widen to `u32` (or version the field).

### W2. `fec_block_id: u8` wraps every 256 blocks (~25 s at 5-frame blocks)
A late-joining peer or a slow reconstructor can collide block IDs. Widen to `u16` or carry an epoch counter.

### W3. `timestamp_ms` rebase behavior at rekey is unspecified
Rekey every 65,536 packets (~22 min). If `timestamp_ms` resets, downstream sync glitches. If it does not, document explicitly.

### W4. `MiniHeader` has no `seq`
Receiver infers absolute seq from the most recent full header + frame count. One missed full header (every 50 frames = 1 s) leaves 49 packets with unknown absolute seq. Acceptable for audio with short jitter buffers — **fatal for video** where one missed full header can desync an entire GOP. **Add `seq_delta: u8` to MiniHeader before video lands.**

### W5. `QualityReport` placement vs. AEAD
A 4-byte trailer on encrypted media is fine **iff it sits inside the AEAD payload**. If it is outside, anything stripping the last 4 bytes corrupts decryption and creates a downgrade vector. Verify in `packet.rs`; if outside, move it inside or AAD-bind it.

### W6. Adaptive controller is loss / RTT-only — no bandwidth estimator
Quinn exposes `cwnd` and `bytes_in_flight`, but `AdaptiveQualityController` does not consume them. Under low utilization you cannot detect that you *could* upgrade to Opus 64 k. **For video this is mandatory** — without BWE you will either oscillate or never use available capacity.

### W7. No NACK / explicit retransmit path
For audio with DRED + FEC this is fine. For video keyframes it is wasteful — an I-frame is 50–200 packets, protecting at 50 % FEC doubles bitrate. A NACK path is cheap and far cheaper than blanket FEC for I-frames.

### W8. TrunkFrame batching multiplies AEAD cost
Each inner payload is its own AEAD operation. At 10 entries that is 10× ChaCha calls per recv. Fine on x86 / ARM with AES-NI / NEON; profile on weak Android (Nothing A059 baseline).

### W9. `CodecID` is 4 bits → max 16 codecs; 9 already used
Adding H.264, H.265, AV1, VP9 takes you to 13. Land the widening **before** deployment — either steal from `reserved` / `csrc_count` to make CodecID 8-bit, or split into `MediaType:2 / CodecID:6`. Doing this post-deployment is painful.

### W10. No `MediaType` field
Audio vs. video vs. data is implicit in CodecID. A 2-bit `MediaType` lets the SFU apply per-type policy (drop video first under congestion, prioritize audio fan-out) without a codec lookup.

### W11. Anti-replay window 64 packets is tight for video
One keyframe burst can be 100+ packets; a single reordered earlier packet stalls the window. Bump to 256 or 1024 for video streams, or maintain a per-stream window.

### W12. `SignalMessage` has no version byte
Bincode + `#[serde(default, skip_serializing_if)]` covers field additions but not variant removal or semantic change. Lead every variant with `version: u8`.

### W13. RoomManager Mutex per-packet
Already flagged in `ARCHITECTURE.md`. At ~1500 pps/sender for video this becomes a real ceiling. `DashMap<RoomId, Arc<RwLock<Room>>>` is a Sunday afternoon.

### W14. No receiver → sender congestion feedback beyond inline QualityReport
For video you need REMB-style or transport-CC-style explicit BWE feedback at ~50 ms cadence, independent of media packets.

## Priorities

| Priority | Issue | Why |
|---|---|---|
| P0 | W9 (CodecID width), W10 (MediaType), W4 (MiniHeader seq_delta) | Wire-format changes — must land before video, painful to change post-deploy |
| P0 | W1 (seq u16 → u32) | Same window; audio benefits too |
| P1 | W6 (BWE), W14 (transport feedback) | Blocking for usable video; improves audio adaptation |
| P1 | W5 (QualityReport in AEAD) | Security correctness |
| P2 | W2 (fec_block_id width), W11 (anti-replay window), W12 (signal version byte) | Long-tail correctness |
| P2 | W7 (NACK path), W13 (RoomManager lock) | Video performance, not correctness |
| P3 | W3 (timestamp rebase doc), W8 (AEAD profiling) | Documentation / measurement |

## Resolution status (2026-05-11)

The v2 wire format specified in `ROAD-TO-VIDEO.md` Phase V1 addresses:

| Issue | Resolved by |
|---|---|
| W1 (seq u16 → u32) | `sequence: u32` in MediaHeader v2 |
| W4 (MiniHeader seq) | `seq_delta: u8` added; MiniHeader v2 is 5 B |
| W9 (CodecID width) | Widened to 8-bit (room for 256) |
| W10 (MediaType) | Explicit `media_type: u8` byte |

W6 / W14 (BWE + TransportFeedback) addressed in Phase V2. W7 (NACK) addressed in Phase V2 / V4. Others remain open.
