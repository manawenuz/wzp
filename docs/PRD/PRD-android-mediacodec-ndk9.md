# PRD: Android MediaCodec NDK 0.9 Compatibility

> **Status:** proposed
> **Resolves:** 31 compile errors in `crates/wzp-video/src/mediacodec.rs` blocking all Android video.
> **Depends on:** Remote build server `manwe@188.245.59.196` with Docker image `wzp-android-builder:latest`.

## Problem

`crates/wzp-video/src/mediacodec.rs` fails to compile for
`aarch64-linux-android` against the NDK 0.9 Rust crate. There are 31 errors
in 5 categories. Android video is completely blocked.

The file already compiles for non-Android targets (all Android code is behind
`#[cfg(target_os = "android")]`). Only the Android target path needs fixing.

## Goals

- `cargo build --target aarch64-linux-android -p wzp-video` produces 0 errors on the remote server.
- Each fix category lands in a separate commit so failures can be bisected.
- Non-Android compilation is not broken.

## Non-goals

- Upgrading the NDK Docker image or changing the NDK version.
- Fixing video functionality beyond compilation (runtime testing is a separate task).
- Any files outside `crates/wzp-video/`.

## Design

### Build command (run after each fix)

```bash
ssh manwe@188.245.59.196 'cd ~/wzp-builder/data/source && \
  git fetch github && git reset --hard github/experimental-ui && \
  docker run --rm \
    -v ~/wzp-builder/data/source:/build/source \
    -v ~/wzp-builder/data/cache/cargo-registry:/home/builder/.cargo/registry \
    -v ~/wzp-builder/data/cache/cargo-git:/home/builder/.cargo/git \
    -v ~/wzp-builder/data/cache/target:/build/source/target \
    wzp-android-builder:latest bash -c \
    "cd /build/source && cargo build --target aarch64-linux-android -p wzp-video 2>&1 | grep -E \"^error\" | head -30"'
```

### Fix order (commit one per category)

#### Fix 1 — `E0433`: `ndk_sys` not declared as a dependency

**Symptom**: `use of undeclared crate or module 'ndk_sys'`

**File**: `crates/wzp-video/Cargo.toml`

NDK 0.9 no longer re-exports raw `ndk_sys` symbols; they must be declared as
a direct dependency. Add to the `[target.'cfg(target_os = "android")'.dependencies]`
section (or create it if absent):

```toml
[target.'cfg(target_os = "android")'.dependencies]
ndk = { version = "0.9" }
ndk-sys = { version = "0.6" }   # ndk 0.9 depends on ndk-sys 0.6
```

If `mediacodec.rs` only uses safe wrappers from the `ndk` crate and the
`ndk_sys` imports are not strictly needed, remove the `use ndk_sys::*` lines
from `mediacodec.rs` instead — whichever approach results in fewer changes.

After this fix the `E0433` errors should drop from the build output.

#### Fix 2 — `E0425`: `BITRATE_MODE_CBR` constant missing

**Symptom**: `cannot find value 'BITRATE_MODE_CBR' in this scope`

**File**: `crates/wzp-video/src/mediacodec.rs`

`BITRATE_MODE_CBR` is already defined as a local constant at line 44:

```rust
#[cfg(target_os = "android")]
const BITRATE_MODE_CBR: i32 = 2;
```

If the error persists after Fix 1, the issue is that `ndk_sys` was providing
a conflicting symbol. Verify the constant is still at line 44 after Fix 1. If
NDK 0.9 moved `BITRATE_MODE_CBR` to an enum, update the usage at line 516
(`format.set_i32("bitrate-mode", BITRATE_MODE_CBR)`) to use the integer
value directly (`2`) or update the constant's value.

If `ndk 0.9` defines `MediaCodecBitrateMode::Cbr` as an enum, the call site
in `MediaCodecAv1Encoder::new` (line ~516) can be updated to:

```rust
format.set_i32(
    "bitrate-mode",
    ndk::media::media_codec::MediaCodecBitrateMode::Cbr as i32,
);
```

#### Fix 3 — `E0308`: `InputBuffer` returns `&mut [MaybeUninit<u8>]`

**Symptom**: `expected &mut [u8], found &mut [MaybeUninit<u8>]`

**File**: `crates/wzp-video/src/mediacodec.rs`

NDK 0.9 changed `InputBuffer::buffer_mut()` from `&mut [u8]` to
`&mut [MaybeUninit<u8>]`. There are multiple write sites in the file — all
follow the same pattern:

```rust
// Before (NDK 0.8):
let buf = buffer.buffer_mut();   // &mut [u8]
let n = frame.data.len().min(buf.len());
buf[..n].copy_from_slice(&frame.data[..n]);
```

```rust
// After (NDK 0.9):
let buf = buffer.buffer_mut();   // &mut [MaybeUninit<u8>]
let n = frame.data.len().min(buf.len());
for (d, &s) in buf[..n].iter_mut().zip(frame.data[..n].iter()) {
    d.write(s);
}
```

The file already uses the `d.write(s)` pattern in some places (lines 125–127,
297–299, etc.). Search for **every** occurrence of `buffer.buffer_mut()` and
`buffer_mut()` and apply the same pattern. Affected structs:
`MediaCodecEncoder::encode` (~line 123), `MediaCodecDecoder::decode`
(~line 294), `MediaCodecHevcEncoder::encode` (~line 439),
`MediaCodecHevcDecoder::decode` (~line 773), `MediaCodecAv1Encoder::encode`
(~line 560), `MediaCodecAv1Decoder::decode` (~line 907).

Do NOT use `unsafe { std::mem::transmute }` — the `d.write(s)` pattern is
already present and safe.

Note: if the file already uses `d.write(s)` everywhere, this category may
already be addressed by the existing code. Check the actual error count.

#### Fix 4 — `E0599`: `.index()` is private

**Symptom**: `method 'index' is private`

**File**: `crates/wzp-video/src/mediacodec.rs`

NDK 0.9 removed the public `.index()` method from `DequeuedInputBuffer` and
`DequeuedOutputBuffer`. The pattern that broke:

```rust
// Broken: buffer.index() is private in NDK 0.9
let idx = buffer.index();
codec.queue_input_buffer_index(idx, ...);
```

In NDK 0.9 the correct API is to pass the buffer object directly to
`queue_input_buffer`:

```rust
codec.queue_input_buffer(buffer, offset, size, pts_us, flags)?;
```

The file already uses `codec.queue_input_buffer(buffer, 0, to_copy, ...)` in
most places (lines 131, 303, 447, etc.). Search for any remaining `.index()`
calls on buffer objects and replace them with the direct-pass pattern shown
above.

#### Fix 5 — `E0277`: `NonNull<AMediaCodec>` is not `Send`

**Symptom**: `NonNull<AMediaCodec>` cannot be sent between threads safely

**File**: `crates/wzp-video/src/mediacodec.rs`

Each codec struct must have an `unsafe impl Send` declaration. Audit all six
codec structs:

| Struct | `unsafe impl Send` present? |
|--------|----------------------------|
| `MediaCodecEncoder` | Yes (line 51) |
| `MediaCodecDecoder` | Yes (line 228) |
| `MediaCodecHevcEncoder` | Yes (line 374) |
| `MediaCodecHevcDecoder` | Yes (line 705) |
| `MediaCodecAv1Encoder` | Yes (line 503) |
| `MediaCodecAv1Decoder` | Yes (line 844) |

If any are missing, add them with a safety comment:

```rust
// SAFETY: AMediaCodec is documented as thread-safe.
#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecXxxYyy {}
```

This category may already be clean. Confirm with the build output.

## Implementation steps

1. Push the current branch to `github/experimental-ui` before starting.
2. **Commit 1**: Fix `ndk_sys` dependency (`Cargo.toml`). Push. Run build.
   Confirm `E0433` errors drop.
3. **Commit 2**: Fix `BITRATE_MODE_CBR`. Push. Run build. Confirm `E0425` gone.
4. **Commit 3**: Fix `MaybeUninit` write sites. Push. Run build. Confirm
   `E0308` gone.
5. **Commit 4**: Remove any `.index()` calls. Push. Run build. Confirm
   `E0599` gone.
6. **Commit 5**: Add missing `unsafe impl Send` if any. Push. Run build.
   Confirm `E0277` gone and total error count is 0.

## Files to read before implementing

- `crates/wzp-video/src/mediacodec.rs` (full file — 45 KB; read in chunks)
- `crates/wzp-video/Cargo.toml` (check existing `[dependencies]` sections)

## Verify

Final build command (see Design section). Expected output: no lines matching
`^error`.

Also verify non-Android host still compiles:

```bash
cargo check -p wzp-video
```

## Done when

`cargo build --target aarch64-linux-android -p wzp-video` on the remote
server produces 0 `error[...]` lines. Non-Android `cargo check -p wzp-video`
also passes.
