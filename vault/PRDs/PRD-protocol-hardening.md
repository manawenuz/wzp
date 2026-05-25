---
tags: [prd, wzp]
type: prd
---

# PRD: Protocol Hardening Batch

> **Status:** proposed
> **Resolves:** Audit W2 (fec_block_id width), W3 (timestamp rebase doc), W5 (QualityReport AEAD binding), W11 (per-stream anti-replay), W12 (signal version byte), W13 (RoomManager lock).
> **Depends on:** PRD #1 (wire format v2 already widens block_id field).

## Problem

A handful of medium-priority audit findings that don't individually justify a PRD but together represent the long tail of protocol correctness and concurrency. Batching them avoids version churn.

## Items

### H1 — W5: `QualityReport` trailer must be inside AEAD

**Current risk.** If the 4-byte trailer sits *outside* the encrypted payload, anything stripping the last 4 bytes corrupts AEAD verification on legitimate packets and creates a quality-feedback downgrade vector. Even if it's correctly inside today, the v2 wire format change is the right moment to assert this explicitly.

**Action.**
- Audit `crates/wzp-proto/src/packet.rs` for `QualityReport` placement.
- Move inside AEAD payload if currently outside.
- Document: "QualityReport, when Q-flag set, is appended to plaintext payload before encryption."
- Test: tamper with trailer → AEAD decrypt fails.

**Severity.** Security correctness. Do this in Wave 1.

### H2 — W2: `fec_block_id` width

Resolved by v2 wire format (`u16` instead of `u8`). PRD #1 carries the wire change; this PRD just confirms semantics:

- Wraps at 2^16. At 5-frame blocks and 50 pps → ~22 min between collisions, vs. ~25 s in v1.
- Late-joining peers must still discard FEC blocks older than 2 s; widening is defense in depth.

**Action.** Update `wzp-fec` to operate on u16 block_id end-to-end. Test reconstruction across a synthetic 22-min session.

### H3 — W11: Per-stream, per-`MediaType` anti-replay window

**Current.** 64-packet sliding window globally.

**Problem.** Video keyframe burst (100+ packets) can stall the window behind one reordered prior packet.

**Action.**
- Anti-replay state is per (stream_id, media_type).
- Window size: 64 for audio, 1024 for video, 256 for data.
- Window size selected at session setup based on declared profile; tunable via `QualityProfile`.

**Severity.** Required before video. Wave 1.

### H4 — W12: `SignalMessage` versioning

**Current.** Bincode-serialized enum. `#[serde(default, skip_serializing_if)]` handles field additions; variant removals or semantic changes are unsafe.

**Action.**
- Every variant gains `version: u8` as its first field.
- Add `SignalMessage::Unknown { version, raw: Bytes }` to absorb future unknown variants gracefully.
- Decode path: unknown variant → log + drop, do not close session.

**Severity.** Future-proofing. Wave 3.

### H5 — W3: `timestamp_ms` rebase documentation

**Current.** Behavior at rekey (every 65,536 packets, ~22 min) is not documented.

**Decision (this PRD).** `timestamp_ms` is **monotonic across rekeys** — it does not reset. Rekey changes only the cryptographic key material; sequence and timestamp are session-scoped, not key-scoped.

**Action.**
- Document in `WZP-SPEC.md` and inline in `packet.rs` doc comments.
- Add a test that performs a rekey mid-session and asserts `timestamp_ms` continuity.

**Severity.** Doc + test. Wave 3.

### H6 — W13: `RoomManager` lock concurrency

**Current.** Single `Mutex<RoomManager>` acquired per packet by every participant for fan-out peer list. Serializes packet processing within a room.

**Problem.** At 1500 pps/sender for video, this is the dominant bottleneck.

**Action.**
- Migrate to `DashMap<RoomId, Arc<RwLock<Room>>>`.
- Per-room `RwLock` allows concurrent reads (fan-out peer list) and exclusive writes (join/leave/quality changes).
- Fan-out path holds read lock; participant churn holds write lock.
- Federation manager updated to match.

**Severity.** Required for video scale. Wave 3.

**Migration safety.**
- Integration test suite (40 + 4 relay tests) must pass.
- Federation tests must pass.
- Trunking tests must pass.
- Property-test: 100-participant room, 500 join/leave events, 10k packets — no panics, no missed forwards.

## Implementation order

| Wave | Item | Task |
|---|---|---|
| 1 | H1 (W5 AEAD binding) | T1.4 |
| 1 | H3 (W11 anti-replay per-stream) | T1.5 |
| 1 | H2 (W2 block_id widening) | folded into PRD #1 |
| 3 | H4 (W12 signal versioning) | T3.3 |
| 3 | H5 (W3 timestamp doc) | T3.2 |
| 3 | H6 (W13 RoomManager lock) | T3.4 |

## Acceptance criteria

- All current tests pass post-hardening.
- New tests: AEAD trailer tampering, rekey timestamp continuity, 100-participant property test, signal forward-compat decode.
- No Prometheus regression in fan-out latency p99 after H6.

## Effort

~4.5 engineer-days total (1.5 in Wave 1, 3 in Wave 3).
