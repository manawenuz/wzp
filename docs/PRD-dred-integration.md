# PRD: DRED Integration & Opus-Tier FEC Simplification

## Problem

WarzonePhone's audio loss-recovery stack is built around classical Opus + application-level RaptorQ FEC. It was the right answer when WZP was designed, but libopus 1.5 (December 2023) introduced **Deep REDundancy (DRED)** — a neural speech-recovery feature that is strictly better than classical FEC for the loss patterns VoIP calls actually experience. We are paying real latency, bitrate, and complexity costs for protection that DRED now does better and cheaper.

Concretely, on every Opus call today we pay:

- **~40–100 ms of receiver-side latency** waiting for RaptorQ block completion before decode
- **10–20% bitrate overhead** from RaptorQ repair symbols (more on studio profiles)
- **~20–40% codec-internal overhead** from Opus inband FEC (LBRR)
- Classical Opus PLC on loss bursts exceeding the RaptorQ block size — which sounds robotic and gap-ridden

…in exchange for bit-exact recovery of isolated single-frame losses, which is perceptually indistinguishable from classical Opus PLC for 20 ms of speech. The protection is misaligned with the failure modes.

DRED delivers:

- **Zero added receive latency** — reconstruction runs only on detected loss
- **~1 kbps flat bitrate overhead** regardless of base bitrate
- **Plausible reconstruction of bursts up to ~1 second** — DRED's headline capability, exactly the regime RaptorQ can't touch
- Neural PLC that sounds like continuous speech, not a gap

We also have a second, unrelated problem blocking adoption: our FFI crate `audiopus_sys 0.2.2` vendors **libopus 1.3**, predating DRED entirely. We cannot enable DRED without first swapping the FFI layer. The naïve choice (`opus` crate from SpaceManiac) is a trap — it depends on the same dead `audiopus_sys`. The real target is `opusic-c 1.5.5` by DoumanAsh, which vendors libopus 1.5.2 with full DRED support and documents Android NDK cross-compile.

This PRD covers the FFI swap, DRED enablement, the decision to **remove RaptorQ and Opus inband FEC from the Opus tiers entirely** (keeping RaptorQ only for Codec2 where DRED is N/A), and the jitter buffer refactor that the DRED lookahead/backfill pattern requires.

## Goals

- Replace `audiopus 0.3.0-rc.0` + `audiopus_sys 0.2.2` (dead upstream, libopus 1.3) with `opusic-c 1.5.5` + `opusic-sys 0.6.0` (active upstream, libopus 1.5.2)
- Enable DRED on every Opus profile with a tiered duration policy, lower at studio bitrates and higher at degraded bitrates
- Disable Opus inband FEC (LBRR) on all Opus profiles — opusic-c's own docs recommend this, and it overlaps DRED's job
- Remove `wzp-fec` (RaptorQ) from the Opus tiers entirely — the latency and bitrate savings are real, and DRED strictly dominates it on speech
- Keep RaptorQ + current FEC ratios on the Codec2 tiers unchanged — DRED is libopus-only, Codec2 has no neural equivalent
- Refactor `wzp-transport::jitter` to a lookahead/backfill pattern that lets DRED reconstruct loss windows when the next packet arrives, instead of the current "wait for block completion or fall through to classical PLC" policy
- Ship behind a runtime escape hatch (`AUDIO_USE_LEGACY_FEC`) for the first rollout window so we can revert to RaptorQ if DRED has surprises in real-world conditions

## Non-goals

- Changing Codec2 at all. Codec2 1200 / 3200 are outside the DRED lineage and keep their current RaptorQ protection, block sizes, and PLC path.
- Adding new Opus bitrate tiers or changing the quality adaptation thresholds. This PRD is about the protection layer, not the bitrate ladder.
- Enabling OSCE (Opus Speech Coding Enhancement — a separate libopus 1.5 neural post-processor that opusic-c exposes via an `osce` feature flag). Valuable, complementary, and free once opusic-c is in — but out of scope here to keep the PRD focused. Track as follow-up.
- Video, audio-over-MoQ, or any protocol-layer changes discussed in prior conversations.
- Touching the wzp-web / browser client. Browser Opus is a separate codepath via WebAudio / WASM libopus and is not affected by the native FFI swap.

## Background

### How the three protection mechanisms actually differ

| | Opus inband FEC (LBRR) | RaptorQ (wzp-fec) | DRED |
|---|---|---|---|
| Layer | codec-internal | application, across Opus packets | codec-internal |
| What it sends | low-bitrate copy of the *previous* frame, embedded in every packet | fountain-code repair symbols across a block | neural-coded history of the recent past |
| Protection horizon | 1 packet back | block duration (currently 100 ms, proposed 40 ms) | configurable, 0–1040 ms |
| Recovery granularity | 1 frame (lower quality) | 1 frame (bit-exact) | 10 ms frames (plausible reconstruction) |
| Latency cost | 0 ms | block duration on receive | 0 ms |
| Bitrate cost | ~20–40% of base | `fec_ratio × base` (currently +20% GOOD, +50% DEGRADED) | ~1 kbps flat |
| Effective loss tolerance | ~single-packet losses | up to `(repair symbols / block)` losses, cliff beyond | bursts up to the configured duration |
| Content assumption | any Opus audio | any | speech (DRED model is speech-trained) |

### Why DRED dominates on the Opus tiers

Loss-scenario walkthrough (verified against opusic-c and libopus 1.5 docs):

- **1-frame loss (20 ms)**: RaptorQ recovers bit-exactly, DRED wouldn't run (classical Opus PLC is perceptually indistinguishable for single 20 ms frames). RaptorQ "wins" on paper but not on ears.
- **2–3 frame burst (40–60 ms)**: RaptorQ at current ratio 0.2 hits its tolerance cliff. DRED handles this trivially — well within a 200 ms window.
- **5–10 frame burst (100–200 ms)**: RaptorQ completely overwhelmed at any reasonable ratio. DRED's sweet spot.
- **10+ frame burst (>200 ms)**: RaptorQ useless. DRED at 500–1000 ms still recovers.

The only scenario where RaptorQ strictly beats DRED is bit-exact recovery of isolated single-frame losses — which is perceptually irrelevant for speech. In every other scenario DRED either ties or wins.

### Why Codec2 keeps RaptorQ

DRED lives inside libopus — it does not help Codec2 at all. Codec2's classical PLC is a parametric-vocoder interpolation that produces noticeably robotic artifacts on loss. On the Codec2 tiers, RaptorQ is the only protection we have, and it should stay at current ratios (1.0 on CATASTROPHIC, 0.5 on the Codec2 3200 tier).

### The opusic-c / opusic-sys situation

- `opusic-sys 0.6.0` — FFI crate, published 2026-03-17, vendors libopus 1.5.2 via its `bundled` feature (on by default), documents Android NDK cross-compile via `ANDROID_NDK_HOME` (which our `wzp-android/build.rs` already sets). Exposes raw bindings to `opus_dred_parse`, `opus_decoder_dred_decode`, and the `OpusDRED` state struct.
- `opusic-c 1.5.5` — high-level safe wrapper. Its **encoder** side is fine: exposes `Encoder::set_dred_duration(value: u8) -> Result<(), ErrorCode>` with range `0..=104` (each unit is 10 ms, so 0–1040 ms configurable). Also exposes `set_bitrate`, `set_inband_fec`, `set_dtx`, `set_packet_loss`, `set_signal`, `set_complexity`, `set_bandwidth`, `set_application` on the encoder.
- **opusic-c's decoder-side DRED wrapper is NOT sufficient for our architecture.** Confirmed by reading the source of `opusic-c/src/dred.rs`:
  1. `Dred::decode_to` ignores the `dred_end` output of `opus_dred_parse` (prefixed `_dred_end`), so the caller cannot know how much DRED history a given packet actually carried.
  2. In `opus_decoder_dred_decode(decoder, dred, dred_offset, pcm, frame_size)`, the wrapper passes `frame_size` to BOTH the `dred_offset` and `frame_size` arguments. This looks like a bug — it means reconstruction always starts at offset `frame_size` into the DRED window, not at an arbitrary caller-chosen offset. Arbitrary-gap reconstruction (which we need for the lookahead/backfill pattern) requires proper offset control.
  3. `DredPacket` is owned internally by a `Dred` instance; its internal buffer is overwritten on every `decode_to` call. We cannot hold a ring of parsed DredPackets from multiple recent arrivals — which is exactly what the lookahead/backfill jitter buffer pattern requires.
- **Decision**: use opusic-c for the encoder path (its wrapper is correct and saves work), and drop to `opusic-sys` raw FFI for the entire decoder path AND the DRED reconstruction path. Both use a single shared `DecoderHandle` so internal decoder state stays consistent. **Verified at pre-flight**: `opusic_c::Decoder.inner` is `pub(crate)`, so there is no way to reach the raw `*mut OpusDecoder` from outside opusic-c. Running two parallel decoders (one from opusic-c for audio, one from opusic-sys for DRED) would cause state drift because the DRED-only decoder wouldn't see the normal decode calls. Single unified decoder via opusic-sys is the only correct architecture.
- **Three FFI handles required** per decode session: `opusic_c::Encoder` (encoder side, unchanged), our own `DecoderHandle` wrapping `*mut OpusDecoder` from opusic-sys (for normal decode AND for the `OpusDecoder` pointer passed to `opus_decoder_dred_decode`), and a new `DredDecoderHandle` wrapping `*mut OpusDREDDecoder` from opusic-sys (passed to `opus_dred_parse`). Note: `OpusDREDDecoder` is a **separate struct** from `OpusDecoder` in libopus 1.5 — verified from opus.h. Allocation via `opus_dred_decoder_create()` (confirm exact symbol name at Phase 3a start).
- The `opus` crate from SpaceManiac (0.3.1, published 2026-01-03) is a trap: it depends on `audiopus_sys ^0.2.0` — the same dead FFI crate we're trying to get away from. Do not use.
- **Follow-up (out of scope for this PRD)**: upstream the fixes to `opusic-c/src/dred.rs` (preserve `dred_end`, fix the `dred_offset` double-pass, expose `DredPacket` externally). Worth a GitHub PR once our own implementation has proven correct. Would let us eventually delete our internal FFI wrapper.

### Critical note from opusic-c docs

From the `dred` module documentation: *"The documentation recommends disabling in-band FEC and using `Application::Voip` for optimal results."* This applies to the **codec-internal** Opus inband FEC (LBRR), not our application-level RaptorQ. The two are independent layers. This PRD disables both on Opus tiers, but for different reasons — inband FEC per upstream recommendation, RaptorQ per the analysis above.

### The libopus 1.5 loss-percentage gating quirk

In libopus 1.5, both inband FEC and DRED are gated on `OPUS_SET_PACKET_LOSS_PERC` being non-zero. If the encoder thinks loss is 0%, it will not emit DRED data even when `set_dred_duration` is configured. We must plumb a meaningful loss percentage into the encoder continuously, floored at a small non-zero value so DRED stays active even when the network is perfect. Planned floor: **5%**, overridden upward by the real `QualityReport` loss value when it exceeds the floor.

## Solution

### High-level architecture change

**Before** (per Opus frame encode path):
```
PCM → AdaptiveEncoder.encode (Opus)
       → inband FEC embedded in packet
    → wzp-fec FEC encoder (accumulate into block, generate repair symbols)
    → DATAGRAM out
```

**Before** (per Opus frame decode path):
```
DATAGRAM in → wzp-fec block assembly (wait for block, recover if possible)
            → AdaptiveDecoder.decode (Opus) / decode_lost (classical PLC)
            → PCM
```

**After** (Opus tiers):
```
PCM → OpusEncoder.encode (opusic-c, DRED enabled via set_dred_duration, inband FEC off)
    → DATAGRAM out directly (no RaptorQ block)
```

```
DATAGRAM in → jitter buffer (lookahead/backfill)
            → on frame arrival: OpusDecoder.decode
            → on detected gap: if next packet has DRED state → dred::Dred.reconstruct(gap)
                                else → OpusDecoder.decode_lost (classical PLC)
            → PCM
```

**After** (Codec2 tiers): unchanged. RaptorQ block encoding + classical Codec2 decode path stay exactly as they are today.

### New per-profile protection matrix

| Profile | Codec | Inband FEC | RaptorQ ratio | DRED duration | Total overhead |
|---|---|---|---|---|---|
| `STUDIO_64K` | Opus 64k | **off** | **none** | **10 frames (100 ms)** | +1 kbps |
| `STUDIO_48K` | Opus 48k | **off** | **none** | **10 frames (100 ms)** | +1 kbps |
| `STUDIO_32K` | Opus 32k | **off** | **none** | **10 frames (100 ms)** | +1 kbps |
| `GOOD` | Opus 24k | **off** | **none** | **20 frames (200 ms)** | +1 kbps |
| `NORMAL_16K` | Opus 16k | **off** | **none** | **20 frames (200 ms)** | +1 kbps |
| `DEGRADED` | Opus 6k | **off** | **none** | **50 frames (500 ms)** | +1 kbps |
| `CODEC2_3200` | Codec2 3200 | N/A | **0.5 (unchanged)** | N/A | +50% |
| `CATASTROPHIC` | Codec2 1200 | N/A | **1.0 (unchanged)** | N/A | +100% |
| `COMFORT_NOISE` | CN | — | — | — | — |

DRED duration rationale:

- **Studio tiers (100 ms)**: loss is rare on the networks where users pick studio quality. Short DRED window keeps decode-side CPU modest. Still covers multi-frame bursts that classical PLC can't touch.
- **Normal tiers (200 ms)**: balanced baseline. Handles the common VoIP loss pattern (20–150 ms bursts from wifi roam, transient congestion).
- **Degraded tier (500 ms)**: users on Opus 6k are by definition on a bad link. Long DRED window buys maximum burst resilience where it matters most. Still well under the 1040 ms cap.

### Runtime escape hatch

Ship with a single environment variable / settings flag: **`AUDIO_USE_LEGACY_FEC`**. When set, the entire Opus-tier path reverts to the pre-PRD behavior: RaptorQ re-enabled at the old ratios, Opus inband FEC re-enabled, DRED disabled (`set_dred_duration(0)`). This is the rollback safety valve for the first production window.

Escape hatch semantics:
- Read once at `CallEncoder::new` / `CallDecoder::new` time. Call-scoped, not re-read mid-call.
- Exposed via Android Settings UI as a hidden "Legacy FEC (debug)" toggle, and as a CLI flag `--legacy-fec` on the desktop client.
- Logged in `DebugReporter` so we can tell which mode a call was in when diagnosing.
- Removed entirely after 2 months of stable production with no regressions reported. Removal is a follow-up PR, not part of this PRD's scope.

## Detailed design

### Phase 0 — FFI crate swap (prerequisite, no behavior change)

**Files touched:**
- `Cargo.toml` (workspace root) — replace `audiopus = "0.3.0-rc.0"` with `opusic-c = { version = "1.5.5", features = ["bundled", "dred"] }` and `opusic-sys = { version = "0.6.0", features = ["bundled"] }`. The `opusic-sys` direct dep is for the DRED decoder path below.
- `crates/wzp-codec/Cargo.toml` — update `audiopus = { workspace = true }` to `opusic-c = { workspace = true }`, add `opusic-sys = { workspace = true }`, add `bytemuck = "1"` for the i16↔u16 slice cast.
- `crates/wzp-codec/src/opus_enc.rs` — rewrite against opusic-c. API mapping:
  - `audiopus::coder::Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)` → `opusic_c::Encoder::new(Channels::Mono, SampleRate::Hz48000, Application::Voip)` (argument order swapped)
  - `set_bitrate(Bitrate::BitsPerSecond(bps))` → `set_bitrate(Bitrate::Bits(bps))` or equivalent variant — verify at implementation time
  - `set_inband_fec(true/false)` → `set_inband_fec(InbandFec::On/Off)` (now an enum)
  - `set_packet_loss_perc(u8)` → `set_packet_loss(u8)` (method renamed)
  - `set_dtx(bool)`, `set_signal(Signal::Voice)`, `set_complexity(u8)` — names match
  - `encode(&[i16], &mut [u8])` → `encode_to_slice(&[u16], &mut [u8])` with `bytemuck::cast_slice::<i16, u16>(pcm)` at the call site
- `crates/wzp-codec/src/opus_dec.rs` — same-style rewrite for the `Decoder` path. Note that opusic-c's decoder methods take `decode_fec: bool` as a parameter directly (not a separate ctl).
- `vendor/audiopus_sys/` — delete the directory (only exists on `feat/desktop-audio-rewrite`, not on `android-rewrite`, so this is a no-op on the current branch but do remove the `[patch.crates-io]` block from Cargo.toml when merging back).

**Acceptance criteria:**
- `cargo check --workspace` passes on Linux x86_64, macOS, and Android NDK cross-compile.
- All existing codec unit tests in `crates/wzp-codec/src/adaptive.rs` pass unchanged. DRED is still disabled at this phase (default `set_dred_duration(0)`), so behavior is equivalent to pre-swap libopus 1.3 for call quality purposes.
- A short real-call smoke test produces audio identical to current behavior (no audible regression).
- `opusic_c::version()` at startup logs libopus version containing `1.5.2` — hard signal that the swap landed correctly.

### Phase 1 — DRED encoder enable on all Opus profiles

**Files touched:**
- `crates/wzp-codec/src/opus_enc.rs`:
  - Add `fn dred_duration_for(codec: CodecId) -> u8` returning the per-profile value from the matrix above (10 / 20 / 50 frames).
  - In `OpusEncoder::new`, after the existing `set_bitrate`/`set_signal`/`set_complexity` block: call `inner.set_inband_fec(InbandFec::Off)`, then `inner.set_dred_duration(dred_duration_for(profile.codec))`, then `inner.set_packet_loss(5)` as the default floor.
  - Add `pub fn set_dred_duration(&mut self, frames: u8)` to allow the adaptive ladder to update DRED duration on profile switch.
  - In the existing `set_profile` impl, call `set_dred_duration(dred_duration_for(profile.codec))` after `apply_bitrate`.
- `crates/wzp-codec/src/adaptive.rs`:
  - `AdaptiveEncoder::set_profile` already delegates to `self.opus.set_profile` — no changes needed. DRED update rides along.
- `crates/wzp-client/src/call.rs` (and equivalent on `wzp-android/src/pipeline.rs`):
  - In the `QualityReport` handler (wherever we currently call `set_expected_loss` / `set_packet_loss_perc`), also ensure the loss value is floored at 5% before passing to the Opus encoder. This is a 1-line change.

**Acceptance criteria:**
- Encoder produces DRED-enabled Opus packets. Verifiable via libopus's reference decoder in debug mode, or by wire capture + inspection — a DRED-bearing Opus packet has a larger `opus_packet_get_nb_frames` footprint than a non-DRED one of the same nominal bitrate.
- Total outgoing bitrate on Opus 24k is ~25 kbps (up from ~24 kbps) — confirms ~1 kbps DRED overhead.
- On a lossless path, decoder output is audibly identical to Phase 0.
- Escape hatch `AUDIO_USE_LEGACY_FEC=1` cleanly reverts the DRED enable (calls `set_dred_duration(0)` and `set_inband_fec(InbandFec::On)` instead).

### Phase 2 — RaptorQ removal on Opus tiers

**Files touched:**
- `crates/wzp-client/src/call.rs`:
  - In `CallEncoder::encode_frame` (or wherever `wzp_fec::Encoder::add_source_symbol` is called), gate the RaptorQ path on `!profile.codec.is_opus()` — Opus frames go straight to DATAGRAM emit, Codec2 frames continue through RaptorQ.
  - When a profile switch crosses the Opus↔Codec2 boundary, flush/reset the RaptorQ encoder state.
- `crates/wzp-android/src/pipeline.rs`:
  - Mirror the same gate in the Android encode path.
- `crates/wzp-proto/src/packet.rs`:
  - `MediaHeader.fec_block` and `fec_symbol` are still valid fields on the wire. For Opus packets we emit `fec_block = 0`, `fec_symbol = 0`, `fec_ratio_encoded = 0`. No wire format change; the receiver just sees all-zeros in the FEC fields for Opus packets and skips the FEC decoder path.
  - Bump protocol version to v1 → v2? **No** — the change is semantically backward compatible because existing RaptorQ decoders handle a zero ratio correctly (ratio 0.0 means "no repair symbols expected"). Old receivers can still decode new Opus packets; they just won't see any DRED benefit because their libopus is old. This is a property we want: the opposite (new receiver, old sender) is the more common mixed-version case during rollout and also Just Works.
- `crates/wzp-client/src/call.rs` — `CallDecoder`:
  - Symmetric change: Opus frames bypass the RaptorQ block assembly, go straight to the decoder. Only Codec2 frames (`codec_id.is_codec2()`) feed through `wzp-fec` block decoding.

**Acceptance criteria:**
- Outgoing Opus packets have `fec_ratio_encoded == 0` (verifiable with the existing wire capture tooling in `wzp-client/src/echo_test.rs`).
- On a clean network, receiver latency (measured as encode-to-playout one-way delay) drops by ~40 ms versus Phase 1. This is the primary win and should be directly measurable with the existing telemetry.
- Codec2 calls show no latency change and no packet-format change. Regression-test Codec2 3200 and Codec2 1200 specifically.
- Total outgoing bitrate on Opus 24k drops from ~28.8 kbps (24k base + 0.2 RaptorQ ratio) to ~25 kbps (24k base + ~1 kbps DRED). Direct savings observable in network telemetry.

### Phase 3 — DRED reconstruction wrapper + jitter buffer lookahead/backfill refactor

This phase is larger than originally estimated because opusic-c's decoder-side DRED wrapper is unusable for our architecture (see Background). We write our own safe wrapper over `opusic-sys` raw FFI first, then plumb it through the jitter buffer.

**Step 3a — Safe DRED reconstruction wrapper in `wzp-codec`:**

New file `crates/wzp-codec/src/dred_ffi.rs`. Wraps the raw libopus 1.5 DRED API:

- `pub struct DredState` — owns an `OpusDRED` buffer (allocated via `opusic_sys::opus_dred_alloc` or equivalent; size is fixed at 10,592 bytes per libopus 1.5). `Clone` is intentionally NOT implemented — the state is heap-owned and non-trivial to copy.
- `pub fn parse_from_packet(&mut self, decoder: &opusic_c::Decoder, packet: &[u8], max_dred_samples: i32) -> Result<DredParseResult, DredError>` — wraps `opus_dred_parse`, preserves the `dred_end` output (number of samples of history the packet carried), returns it in `DredParseResult { samples_available: i32, frames_available: u8 }`.
- `pub fn reconstruct_into(&self, decoder: &mut opusic_c::Decoder, dred_offset_samples: i32, output: &mut [i16]) -> Result<usize, DredError>` — wraps `opus_decoder_dred_decode`, takes the offset explicitly, decodes `output.len()` samples starting from that offset in the DRED window.
- All `unsafe` contained here, strict bounds checking on offsets, Rust-level panic safety. Unit tests use a reference encoder + known-good reference decoder to verify that reconstruction at specific offsets produces expected output.
- Depends on `opusic-sys` directly and on `opusic-c::Decoder` for the decoder handle. The Decoder handle must be reachable as a raw pointer; opusic-c exposes this via an unstable internal or we wrap the pointer ourselves. **Verify at implementation time** — if opusic-c doesn't expose the raw decoder pointer safely, we create our own thin Decoder wrapper in `dred_ffi.rs` using raw opusic-sys, losing the convenience of opusic-c's decoder but keeping its encoder. This is the smaller-risk fallback.

New `pub trait DredReconstructor` in `wzp-codec/src/lib.rs`:
```rust
pub trait DredReconstructor: Send {
    /// Parse DRED state from an arriving Opus packet into `state`.
    /// Returns number of 48 kHz samples of history available, or 0 if the packet has no DRED.
    fn parse(&mut self, state: &mut DredState, packet: &[u8]) -> Result<i32, DredError>;

    /// Reconstruct `output.len()` samples from `state`, starting at the given
    /// sample offset (measured from the end of the DRED window going backward).
    fn reconstruct(&mut self, state: &DredState, offset_samples: i32, output: &mut [i16]) -> Result<usize, DredError>;
}
```

Implement `DredReconstructor` over the `dred_ffi::DredState` + opusic-c Decoder combination. This is the clean boundary the jitter buffer will talk to.

**Step 3b — Jitter buffer refactor in `crates/wzp-transport/src/jitter.rs`:**

- Current behavior: buffer waits a fixed number of frames of jitter before emitting; on a missing slot, after a timeout it gives up and signals the decoder to run `decode_lost()` (classical Opus PLC or Codec2 PLC).
- New behavior on Opus tiers: when a frame arrives (in-order or late), first call `DredReconstructor::parse` on it to update a rolling ring of `DredState` instances tagged with their originating sequence number. When a gap is detected (missing sequence number between last-emitted and current arrival), and the ring contains a `DredState` from a nearby packet that covers the gap's sample offset, call `DredReconstructor::reconstruct` with the correct offset to synthesize the missing frames, splice them into playout, then continue normal decode.
- If no DRED state covers the gap (e.g., gap too far back, or every nearby packet was dropped), fall through to classical PLC exactly as today. The classical path stays intact as the ultimate fallback.
- Codec2 packets bypass the entire DRED ring. They are not inspected for DRED state and take the unchanged classical PLC path.
- Ring sizing: `max_dred_duration_frames` + `jitter_depth_frames` worth of `DredState` instances. At 500 ms DRED on degraded tier + 60 ms jitter depth, that's ~28 DredState instances × 10,592 bytes ≈ 300 KB. Acceptable. On studio tier with 100 ms DRED it's only ~80 KB.
- The jitter buffer takes a `Box<dyn DredReconstructor>` at construction, passed in by the call engine. `wzp-transport` does NOT take a direct dep on `opusic-c` or `opusic-sys` — it only knows about the trait defined in `wzp-codec`.

**Files touched:**
- `crates/wzp-codec/src/dred_ffi.rs` (new, ~150–300 lines)
- `crates/wzp-codec/src/lib.rs` — expose `DredReconstructor`, `DredState`, `DredError` types
- `crates/wzp-codec/Cargo.toml` — add `opusic-sys = { workspace = true }` as a direct dep (already done in Phase 0)
- `crates/wzp-transport/src/jitter.rs` — lookahead/backfill refactor, DRED ring
- `crates/wzp-transport/Cargo.toml` — add `wzp-codec = { workspace = true }` (likely already present) for the trait import
- `crates/wzp-client/src/call.rs` — construct a `DredReconstructor` and pass into `CallDecoder`'s jitter buffer
- `crates/wzp-android/src/pipeline.rs` — same on Android

**Acceptance criteria:**
- Unit tests in `dred_ffi.rs`: round-trip a known speech waveform through an encoder with DRED enabled, parse the resulting packets, reconstruct at several different offsets, verify the reconstructed samples are within an energy/spectral threshold of the original. (Not bit-exact — DRED reconstruction is lossy by design.)
- Synthetic loss test on the full pipeline: inject 200 ms bursts at 10% rate into a looped call, verify the DRED reconstruction rate on receiver telemetry is ≥95% of all loss events whose gaps fall within the configured DRED duration window.
- Reconstructed audio is audibly continuous on 40–200 ms bursts — no gaps, no classical-PLC robot artifact. Verified on real voice samples (not just sine tones), and on at least two distinct speaker profiles (male, female) because DRED can have voice-dependent quality.
- End-to-end latency metric is unchanged versus Phase 2 (no regression from adding the lookahead path). The DRED ring insertion on packet arrival must be O(1) in practice.
- Existing `echo_test.rs` and `drift_test.rs` pass with the new jitter buffer.
- Codec2 path uses classical PLC exclusively (no DRED invocation) because Codec2 packets don't carry DRED state. Verify by injecting loss on a Codec2 call and confirming zero DRED reconstruction telemetry events during that call.
- `wzp-transport` has no direct dependency on `opusic-sys` or `opusic-c` in its `Cargo.toml` after the refactor — only on `wzp-codec`. Verify by grepping the Cargo.toml file.

### Phase 4 — Telemetry and tooling updates

**Files touched:**
- `crates/wzp-proto/src/packet.rs` — `QualityReport` or equivalent telemetry message gains `dred_reconstructions: u32` as a new counter (frames reconstructed via DRED this reporting window) and `classical_plc_invocations: u32` (frames filled by Opus/Codec2 classical PLC). These are separate counters because they're different recovery mechanisms.
- `crates/wzp-relay/src/*` — relay telemetry pipeline surfaces both counters in Prometheus metrics: `wzp_dred_reconstructions_total{call_id}`, `wzp_classical_plc_total{call_id}`.
- `docs/grafana-dashboard.json` — new panel: "Loss recovery breakdown" stacked bar, DRED vs classical PLC vs clean decode, per call.
- `android/app/src/main/java/com/wzp/debug/DebugReporter.kt` — surfaces `dredReconstructions` and `classicalPlc` counts in the debug report; also logs active DRED duration and whether legacy-FEC mode is engaged.

**Acceptance criteria:**
- Grafana dashboard shows a clear visual distinction between DRED-recovered and classical-PLC-recovered frames across a test fleet of calls.
- Debug report includes the active protection mode ("DRED 200 ms" / "Legacy RaptorQ") and reconstruction counts, so incidents can be classified unambiguously.

### Phase 5 — Escape hatch removal (follow-up, ~2 months post-ship)

After 2 months of stable production with no rollbacks triggered:
- Delete `AUDIO_USE_LEGACY_FEC` handling in `opus_enc.rs` / `call.rs` / `pipeline.rs`
- Delete the Opus-tier paths of `wzp-fec` (the crate stays for Codec2)
- Delete the Android settings toggle and desktop CLI flag
- Remove the `--legacy-fec` path from smoke tests

## Critical files to modify (summary)

- `Cargo.toml` (workspace) — dep swap (audiopus → opusic-c + opusic-sys)
- `crates/wzp-codec/Cargo.toml` — dep swap + `bytemuck` for slice cast
- `crates/wzp-codec/src/opus_enc.rs` — opusic-c rewrite + DRED enable + inband FEC off
- `crates/wzp-codec/src/opus_dec.rs` — opusic-c rewrite
- `crates/wzp-codec/src/dred_ffi.rs` — **new file**, safe wrapper over opusic-sys raw DRED FFI
- `crates/wzp-codec/src/lib.rs` — expose `DredReconstructor` trait, `DredState`, `DredError`
- `crates/wzp-codec/src/adaptive.rs` — verify profile switch carries DRED duration
- `crates/wzp-client/src/call.rs` — Opus/Codec2 gate on RaptorQ path, loss floor, wire DredReconstructor into CallDecoder
- `crates/wzp-android/src/pipeline.rs` — same gate, same loss floor, wire DredReconstructor
- `crates/wzp-transport/src/jitter.rs` — lookahead/backfill refactor, DRED ring, reconstruction dispatch
- `crates/wzp-transport/Cargo.toml` — verify it depends only on `wzp-codec`, not directly on opusic-*
- `crates/wzp-proto/src/packet.rs` — new telemetry counters
- `crates/wzp-relay/` — Prometheus metric exposure
- `android/app/src/main/java/com/wzp/debug/DebugReporter.kt` — debug output
- `docs/grafana-dashboard.json` — loss-recovery panel
- (delete) `vendor/audiopus_sys/` on `feat/desktop-audio-rewrite` when merging back

## Existing utilities to reuse

- `wzp_codec::resample::Downsampler48to8` / `Upsampler8to48` — unchanged, only Codec2 path uses them
- `wzp_codec::adaptive::AdaptiveEncoder` / `AdaptiveDecoder` — existing profile-switching machinery, DRED duration changes ride along
- `wzp_codec::silence::SilenceDetector` / `ComfortNoise` — unchanged
- `wzp_codec::agc::AutoGainControl` — unchanged, runs before encode as today
- `wzp_fec::RaptorQFecEncoder` / decoder — unchanged, still used for Codec2 tiers
- `wzp_client::call::QualityAdapter` — unchanged; drives profile switching, which now also reconfigures DRED duration via the existing `set_profile` path

## Verification

End-to-end testing, in order:

1. **Unit**: `cargo test -p wzp-codec` — Opus encode/decode round-trip at every profile, DRED enabled. Verify `version()` reports libopus 1.5.2.
2. **Unit**: `cargo test -p wzp-transport` — jitter buffer lookahead/backfill behavior with injected loss patterns (0%, 5%, 15%, 30%, 50% loss; isolated losses, 40 ms bursts, 200 ms bursts, 500 ms bursts).
3. **Integration**: `crates/wzp-client/src/echo_test.rs` — existing echo test must pass on all Opus profiles with <5% perceived quality regression (measure via the time-window analysis already built into `echo_test.rs`).
4. **Integration**: `crates/wzp-client/src/drift_test.rs` — latency measurement. Must show ~40 ms reduction on Opus profiles versus pre-PRD baseline. Codec2 profiles unchanged.
5. **Manual**: Android release build, real call over bad wifi (or a shaped network via `tc netem` on Linux). Burst losses of 200 ms should be perceptually continuous speech, not robotic gaps.
6. **Manual**: Same call with `AUDIO_USE_LEGACY_FEC=1` — verify behavior reverts to current production behavior. This is the pre-ship rollback rehearsal.
7. **Cross-compile**: full build matrix — Android arm64-v8a + armeabi-v7a (via `scripts/build-and-notify.sh`), macOS universal, Linux x86_64 (via `scripts/build-linux-docker.sh`). Windows cross-compile via cargo-xwin should also pass — libopus 1.5 upstream fixed the clang-cl SIMD issue that required the vendor patch on `feat/desktop-audio-rewrite`.
8. **Telemetry smoke**: deploy to staging relay, make 10 test calls, verify Grafana's new "Loss recovery breakdown" panel shows DRED reconstruction events firing on injected loss and classical-PLC on packet-loss beyond DRED's window.

## Risks and mitigations

- **Custom DRED FFI wrapper is WZP-maintained code with no second source.** opusic-c's decoder-side DRED wrapper is insufficient (see Background), so we carry our own `dred_ffi.rs` that calls `opus_dred_parse` and `opus_decoder_dred_decode` directly via opusic-sys. Bugs in this wrapper — offset arithmetic off-by-ones, lifetime errors on `OpusDRED` buffers, UB from misuse of the C API — could manifest as silent audio corruption on loss bursts, hard to diagnose. **Mitigation**: extensive unit tests in `dred_ffi.rs` using a reference encoder + reference decoder round-trip with known offsets; strict bounds checking on every `unsafe` boundary; Miri run in CI if feasible; the legacy-FEC escape hatch disables the entire DRED code path including our custom wrapper, giving us a single flag to revert any wrapper bug in production. Long-term: upstream the fixes to opusic-c (follow-up task, not blocking).
- **opusic-c's encoder-side API and internal Decoder pointer access**. Step 3a depends on being able to call opusic-sys raw functions that take an `*mut OpusDecoder` pointer while still using opusic-c's `Decoder` for normal decode. If opusic-c doesn't expose the raw pointer cleanly, we fall back to a thin opusic-sys-direct Decoder wrapper inside `dred_ffi.rs` and lose some of opusic-c's convenience. **Mitigation**: verify at the start of Phase 3 (one afternoon of reading opusic-c source). If the clean path doesn't work, the fallback is not difficult — it's what we'd have built anyway if opusic-c didn't exist.
- **DRED reconstruction quality varies by voice / content**. The neural model is trained on speech; edge cases (shouting, whispering, heavy accents, music-on-hold, cough, laughter) may reconstruct less cleanly than continuous speech. **Mitigation**: escape hatch ships from day one. If production telemetry shows perceptible quality regression on specific voice patterns, flip legacy mode for affected users while tuning. Also: classical Opus PLC remains as the third-tier fallback when DRED state is unavailable.
- **Removing RaptorQ removes bit-exact recovery**. Isolated single-packet losses are now reconstructed plausibly instead of bit-exactly. **Mitigation**: as argued in Background, bit-exactness on a single 20 ms speech frame is perceptually meaningless. The assumption is "speech is the workload" — if we ever add non-speech features (music bot, ringtones over the call path, DTMF-over-audio) we revisit.
- **libopus 1.5 DRED API stability**. **Verified at pre-flight**: opus.h in the upstream xiph/opus repository has no "experimental" marker on the DRED API declarations. The earlier characterization was incorrect. DRED shipped as a first-class feature in libopus 1.5.0 (Dec 2023) and has been iterated in 1.5.1 and 1.5.2. Google Meet and Duo ship it at scale. **Mitigation**: pin `opusic-sys` exactly (no `^` range) to ensure reproducible builds, follow upstream 1.5.x bugfixes as they land. No special stability concerns beyond normal dependency hygiene.
- **Jitter buffer refactor is the largest code change**. Jitter bugs are notoriously subtle (off-by-one on sequence wraparound, clock drift interactions, playout starvation corner cases). **Mitigation**: keep the classical-PLC path intact as the DRED fallback, so jitter bugs degrade to "current behavior" rather than "broken audio". Write targeted unit tests for the buffer at each loss-pattern scenario before touching production paths. Consider shipping Phase 3 behind a sub-flag separate from the main escape hatch, so we can independently toggle "DRED enabled but classical jitter buffer" for bisection.
- **Cross-compile surprises**. `opusic-sys` is actively maintained but our exact combination of Android NDK version / Docker builder environment / Windows cross-compile via cargo-xwin has not been tested by upstream. **Mitigation**: Phase 0 includes the full cross-compile matrix as an acceptance criterion. Any blockers surface before we touch loss-recovery behavior.
- **Wire-format compatibility during rollout**. Mixed-version calls (new sender + old receiver, or vice versa) need to keep working. **Verified at pre-flight**: traced both live receive paths (`wzp-client/src/call.rs::CallDecoder::ingest` and `wzp-android/src/engine.rs` the JNI-driven engine path), and both degrade gracefully: new-sender Opus packets with `fec_ratio_encoded=0` / `fec_block=0` / `fec_symbol=0` flow through to the jitter buffer and decode normally on old receivers. The RaptorQ decoder either ignores zero-FEC packets entirely (Android pipeline.rs gates on non-zero fec_block/fec_symbol) or accumulates them harmlessly until the 2-second staleness eviction (desktop call.rs). Old-sender packets with populated RaptorQ fields are handled by new receivers via the unchanged Codec2 path (new receivers keep wzp-fec for Codec2 tiers and simply ignore RaptorQ fields on Opus packets). **No wire format version bump required.**
- **Pre-existing desktop RaptorQ gap** (incidental finding, NOT caused by this PRD). The desktop `wzp-client/src/call.rs::CallDecoder` feeds packets into `fec_dec.add_symbol` but **never calls `fec_dec.try_decode`** — RaptorQ recovery is effectively dead code on the desktop path today. Main decode reads from the jitter buffer directly, falling through to classical Opus PLC on missing packets. The Android `engine.rs` path properly uses `try_decode` for recovery. This PRD does not fix the desktop gap — it's unrelated — but is noted here so nobody is surprised that removing RaptorQ from Opus tiers on the desktop client causes no measurable recovery regression (there was nothing to lose). Recommend filing a follow-up task to either fix or remove the vestigial desktop RaptorQ wiring independently of this work.
- **`AUDIO_USE_LEGACY_FEC` itself becoming permanent tech debt**. Escape hatches have a way of outliving their intended lifespan. **Mitigation**: put an explicit removal date in a `// TODO(2026-06-15): remove legacy FEC path` comment at the flag-handling site. Track in taskmaster.

## Open questions

- ~~**Does opusic-c expose `opusic_c::Decoder`'s raw inner pointer?**~~ **Resolved at pre-flight**: no, it's `pub(crate)`. We build a unified `DecoderHandle` over raw opusic-sys in `dred_ffi.rs` and use it for both normal decode and DRED reconstruction. Opusic-c is used only for the encoder side.
- **Exact opusic-sys symbol name for DRED decoder allocation**. opus.h documents the `OpusDREDDecoder` type and `opus_dred_parse`/`opus_decoder_dred_decode` functions, but the allocation function name is not in the fetched snippet. Expected to be `opus_dred_decoder_create` / `opus_dred_decoder_destroy` per libopus naming convention, but confirm at the very start of Phase 3a by reading the actual opusic-sys bindings. If the function is not exported by opusic-sys, we file a PR upstream to opusic-sys (small fix, trivially mergeable) and temporarily vendor the function declaration locally.
- **Should the 5% loss floor be configurable per profile?** Currently specified as a constant. A future refinement might make it higher at degraded tiers and lower at studio tiers, but without real telemetry we don't know if the constant is wrong. Keep as a constant for now, revisit after 1 month of production data.
- **OSCE enable**: opusic-c has an `osce` feature flag for Opus Speech Coding Enhancement, a separate libopus 1.5 neural post-processor. Out of scope for this PRD but should be the next audio-quality follow-up. Probably one-line enable once opusic-c is in.
- **Upstream PR to opusic-c**: our own `dred_ffi.rs` wrapper should be proven in production first, then the fixes upstreamed to `opusic-c/src/dred.rs` (preserve `dred_end`, fix `dred_offset` double-pass, expose `DredPacket` externally). Follow-up task, not blocking this PRD.
- **`feat/desktop-audio-rewrite` merge**: the vendored `audiopus_sys` patch on that branch becomes obsolete under this PRD. Coordinate removal with whoever owns that branch.

## Phase A: Continuous DRED Tuning (Implemented 2026-04-12)

Phase A extends the discrete tier-locked DRED durations from Phases 1-3 with continuous, network-driven tuning.

### What was built

- **`DredTuner`** (`crates/wzp-proto/src/dred_tuner.rs`): Maps `(loss_pct, rtt_ms, jitter_ms)` → `(dred_frames, expected_loss_pct)` continuously
- **Quinn stats exposure** (`crates/wzp-transport/src/quic.rs`): `QuinnPathSnapshot` provides quinn's internal RTT, loss, congestion events — more accurate than sequence-gap heuristics
- **Jitter variance window** (`crates/wzp-transport/src/path_monitor.rs`): 10-sample sliding window for RTT standard deviation, used for spike detection
- **`AudioEncoder` trait extensions** (`crates/wzp-proto/src/traits.rs`): `set_expected_loss()` and `set_dred_duration()` with default no-op, overridden by `OpusEncoder` and `AdaptiveEncoder`
- **Engine integration** (`desktop/src-tauri/src/engine.rs`): Both Android and desktop send tasks poll every 25 frames and apply tuning

### Opus6k DRED extended

`dred_duration_for(Opus6k)` changed from 50 (500ms) to 104 (1040ms) — the maximum libopus 1.5 supports. The RDO-VAE's quality-vs-offset curve makes this nearly free in bitrate terms while doubling burst resilience on the worst links.

### Jitter spike detection ("Sawtooth" prediction)

When instantaneous jitter exceeds the EWMA × 1.3 (asymmetric: fast-up α=0.3, slow-down α=0.05), the tuner enters spike-boost mode:
- DRED immediately jumps to the codec tier's ceiling
- Cooldown: 10 cycles (~5 seconds at 25 packets/cycle)
- Designed for Starlink satellite handover sawtooth jitter pattern

### Test coverage

- 10 unit tests for tuner math (baseline, scaling, spike, cooldown, codec switch, Codec2 no-op)
- 4 integration tests (encoder adjustment, spike boost, Codec2 no-op, profile switch with encode verification)
