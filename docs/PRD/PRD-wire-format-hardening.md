# PRD: Wire Format Hardening — FEC block_id u16, SignalMessage version byte, FEC repair index wrap

> **Status:** proposed
> **Resolves:** Three small wire-format defects (H2, M1, M4) that compound over time into silent data corruption or protocol breakage.
> **Depends on:** Nothing — purely mechanical changes to `wzp-fec` and `wzp-proto`.

## Problem

Three independent issues:

**H2 — `fec_block_id` u8 wraps too fast.** The `block_id` field in
`RaptorQFecEncoder` (and `RaptorQFecDecoder`) is `u8`. At 5 audio frames
per block and 50 fps this wraps every ~51 seconds. A slow receiver or a
mid-session join can receive packets from two different blocks with the same
`block_id`, silently corrupting FEC recovery.

**M1 — Some `SignalMessage` variants lack a `version` byte.** Most variants
have `#[serde(default = "default_signal_version")] version: u8`. The unit
variant `Reflect` (and potentially others added recently) does not. Future
protocol changes that key on `version` will silently misparse old messages
from peers without the field.

**M4 — FEC repair index can silently wrap at 255.** In
`crates/wzp-fec/src/encoder.rs` line 140:

```rust
let idx = (num_source as u16).wrapping_add(i as u16);
```

(The line was already fixed to `u16` — verify it is `u16`, not `u8`. If it
is still `u8`, the fix is below.)

If the line currently reads `(num_source as u8).wrapping_add(i as u8)`, then
when `num_source + repair_count > 255` the repair symbol indices wrap silently,
producing incorrect ESI values that the decoder cannot correlate to source
blocks.

## Goals

- **H2**: Widen `block_id` in encoder and decoder from `u8` to `u16`.
  Update `finalize_block` return type and `current_block_id` return type in
  the trait (`wzp-proto`) and implementations (`wzp-fec`).
- **M1**: Audit every `SignalMessage` variant; add
  `#[serde(default = "default_signal_version")] version: u8` to any that
  are missing it.
- **M4**: Confirm the repair index uses `u16`; fix it if it is still `u8`.
  Update the decoder's `add_symbol` call site if the index type changes.
- `cargo test -p wzp-fec -p wzp-proto` passes; no existing tests broken.

## Non-goals

- Changing the wire encoding of `MediaHeaderV2::fec_block` — it is already
  `u16` on the wire. This PRD only widens the **internal counter** to match.
- Multi-block decode concurrency or block expiry policy.
- Any crate outside `wzp-fec` and `wzp-proto`.

## Design

### Item A — `fec_block_id` u8 → u16

**Files**:
- `crates/wzp-proto/src/traits.rs` — `FecEncoder` and `FecDecoder` traits
- `crates/wzp-fec/src/encoder.rs` — `RaptorQFecEncoder`
- `crates/wzp-fec/src/decoder.rs` — `RaptorQFecDecoder`

**Trait changes** (`traits.rs`):

```rust
// Before:
fn finalize_block(&mut self) -> Result<u8, FecError>;
fn current_block_id(&self) -> u8;
fn add_symbol(&mut self, block_id: u8, ...) -> Result<(), FecError>;
fn try_decode(&mut self, block_id: u8) -> Result<...>;
fn expire_before(&mut self, block_id: u8);
```

```rust
// After:
fn finalize_block(&mut self) -> Result<u16, FecError>;
fn current_block_id(&self) -> u16;
fn add_symbol(&mut self, block_id: u16, ...) -> Result<(), FecError>;
fn try_decode(&mut self, block_id: u16) -> Result<...>;
fn expire_before(&mut self, block_id: u16);
```

**Encoder changes** (`encoder.rs`):

- Change `block_id: u8` field to `block_id: u16`.
- Update `self.block_id.wrapping_add(1)` (already u16 semantics; keep as is).
- Update `finalize_block` to return `u16`.
- Update `current_block_id` to return `u16`.
- Update all tests that assert `block_id == 0u8` → `== 0u16`, and the
  wrap test (`block_id_wraps`) to iterate to `u16::MAX` (65535) — or reduce
  it to 300 iterations to keep it fast, asserting the wrap at 65536.

The wrap test at 256 iterations (`0..=255u8`) must be updated; a full
`u16` wrap test at 65536 iterations is too slow for CI. Change to:

```rust
#[test]
fn block_id_wraps_u16() {
    let mut enc = RaptorQFecEncoder::with_defaults(1);
    // Advance 300 blocks and verify no panic + monotonic increment.
    for expected in 0..300u16 {
        assert_eq!(enc.current_block_id(), expected);
        enc.add_source_symbol(&[0u8; 10]).unwrap();
        enc.finalize_block().unwrap();
    }
    // Explicitly test wrap at u16 boundary.
    let mut enc2 = RaptorQFecEncoder::with_defaults(1);
    enc2.block_id = u16::MAX;
    enc2.add_source_symbol(&[0u8; 10]).unwrap();
    let id = enc2.finalize_block().unwrap();
    assert_eq!(id, u16::MAX);
    assert_eq!(enc2.current_block_id(), 0);
}
```

Note: `block_id` is a private field; expose a test helper or set it in a
`#[cfg(test)]` `impl` block.

**Decoder changes** (`decoder.rs`):

- Change `blocks: HashMap<u8, BlockState>` to `HashMap<u16, BlockState>`.
- Update `get_or_create_block(block_id: u8)` → `get_or_create_block(block_id: u16)`.
- Update `add_symbol`, `try_decode`, `expire_before` signatures to `u16`.
- The `SourceBlockEncoder::new(self.block_id, ...)` call in `encoder.rs` passes
  `block_id` to `raptorq`. RaptorQ uses `u8` for source block number internally.
  Cast it: `(block_id & 0xFF) as u8` or `(block_id % 256) as u8` — the `raptorq`
  crate's source block ID is a logical identifier within a single object
  transmission, not a global counter. The u16 is our session counter; truncate
  to u8 when calling into raptorq.

### Item B — `SignalMessage` version byte audit

**File**: `crates/wzp-proto/src/packet.rs`

Read every variant in the `SignalMessage` enum (lines 555–1241) and check
for the presence of:

```rust
#[serde(default = "default_signal_version")]
version: u8,
```

The `Reflect` variant at line 974 is a **unit variant** (no fields). Unit
variants cannot carry a `version` field without becoming struct variants.
Change it to a struct variant:

```rust
// Before:
Reflect,

// After:
Reflect {
    #[serde(default = "default_signal_version")]
    version: u8,
},
```

This is a wire-compatible change: serde JSON struct variants serialize as
`{"Reflect": {"version": 1}}` whereas unit variants serialize as
`"Reflect"`. These are **not** backward-compatible formats. Since `Reflect`
is sent client → relay only and the relay immediately responds, upgrading
both sides atomically is acceptable. Add a serde test to confirm round-trip.

For any other variants missing `version`, follow the same pattern as all
existing variants.

Verify by grepping the enum for variants that do NOT have `version`:

```bash
grep -A3 "^\s*[A-Z][A-Za-z]*\s*{" crates/wzp-proto/src/packet.rs | \
  grep -B1 -v "serde.*default_signal_version\|version:"
```

### Item C — FEC repair index wrap (M4)

**File**: `crates/wzp-fec/src/encoder.rs`, line ~140.

Current code:

```rust
let idx = (num_source as u16).wrapping_add(i as u16);
```

If this line already uses `u16` (as shown in the file at line 140), M4 is
already fixed. Verify by reading the current file. If it still reads
`u8`, apply:

```rust
let idx = (num_source as u16).wrapping_add(i as u16);
```

**Decoder** (`crates/wzp-fec/src/decoder.rs`): `add_symbol` already accepts
`symbol_index: u16` (per the trait). Confirm the parameter flows through to
`PayloadId::new(block_id_u8, symbol_index as u32)` without truncation.

## Implementation steps

1. Read `crates/wzp-proto/src/traits.rs` lines 60–116 (FecEncoder/FecDecoder
   trait definitions) to confirm current signatures.
2. Read `crates/wzp-fec/src/encoder.rs` and `decoder.rs` (full files).
3. Apply Item C fix first (smallest change, easiest to verify).
4. Apply Item A: widen `block_id` from u8 to u16 in traits, encoder, decoder.
   Update all callers by running `cargo check -p wzp-fec -p wzp-proto` and
   fixing each E0308/E0308 error.
5. Apply Item B: read every variant, add missing `version` fields.
   Change `Reflect` to a struct variant.
6. Run tests.

## Files to read before implementing

- `crates/wzp-proto/src/traits.rs` lines 60–116 (trait signatures)
- `crates/wzp-fec/src/encoder.rs` (full)
- `crates/wzp-fec/src/decoder.rs` (full)
- `crates/wzp-proto/src/packet.rs` lines 555–1241 (all `SignalMessage` variants)

## Verify

```bash
cargo test -p wzp-fec -p wzp-proto
```

Expected: all tests pass, 0 failures. Also run:

```bash
cargo check --workspace
```

to catch any call sites outside `wzp-fec` and `wzp-proto` that passed `u8`
block IDs to the trait methods.

## Done when

- `cargo test -p wzp-fec -p wzp-proto` exits 0.
- `block_id` is `u16` in `RaptorQFecEncoder`, `RaptorQFecDecoder`, and the
  `FecEncoder`/`FecDecoder` traits.
- Every non-unit `SignalMessage` variant has a `version: u8` field with
  `#[serde(default = "default_signal_version")]`.
- Repair index in `encoder.rs` is computed with `u16` arithmetic.
- No existing tests are broken.
