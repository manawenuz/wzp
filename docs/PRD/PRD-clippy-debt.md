# PRD: Fix wzp-codec Clippy Lint Debt

> **Status:** proposed
> **Resolves:** 9 pre-existing clippy lints in `crates/wzp-codec/src/` that cause `cargo clippy --workspace -D warnings` to fail, breaking any strict-CI configuration.
> **Depends on:** Nothing — all changes are in `crates/wzp-codec/src/`.

## Problem

`cargo clippy -p wzp-codec -- -D warnings` fails with 9 lints across 5 files.
These are pre-existing code patterns that were never flagged during development
because the CI flag was not set. They have no runtime impact today but prevent
adding `-D warnings` to CI without first cleaning them up.

The 3 errors in `deps/featherchat` are in a submodule — do NOT touch them.
`warzone_protocol` clippy errors are accepted debt (not our code).

## Goals

- `cargo clippy -p wzp-codec -- -D warnings` exits 0.
- No behavior changes — every fix is a semantically equivalent rewrite.
- No changes outside `crates/wzp-codec/src/`.

## Non-goals

- Fixing clippy lints in any crate other than `wzp-codec`.
- Adding new functionality.
- Touching the `deps/featherchat` submodule.

## Design

### Lint inventory

| Lint | Count | File | Approx line | Fix |
|------|-------|------|-------------|-----|
| `implicit_saturating_sub` | 1 | `aec.rs` | 117–119 | `saturating_sub` |
| `needless_range_loop` | 2 | `aec.rs:164`, `resample.rs:51` | — | iterate with `iter().enumerate()` or direct iter |
| `manual_div_ceil` | 2 | `codec2_dec.rs:48`, `codec2_enc.rs:48` | — | `div_ceil` |
| `manual_clamp` | 2 | `denoise.rs:59`, `opus_enc.rs:250` | — | `.clamp(min, max)` |
| `manual_ascii_check` | 1 | `opus_enc.rs:104` | — | `.eq_ignore_ascii_case()` |
| `same_item_push` | 1 | `resample.rs:184` | — | `vec.resize` or `extend(repeat)` |

### Fix details

#### 1. `implicit_saturating_sub` — `aec.rs` line ~117

Current code:

```rust
fn delay_available(&self) -> usize {
    let buffered = self.delay_write - self.delay_read;
    if buffered > self.delay_samples {
        buffered - self.delay_samples
    } else {
        0
    }
}
```

Clippy wants `saturating_sub` because the subtraction can underflow if
`buffered < self.delay_samples`:

```rust
fn delay_available(&self) -> usize {
    let buffered = self.delay_write - self.delay_read;
    buffered.saturating_sub(self.delay_samples)
}
```

This is semantically identical (both return 0 when `buffered <= delay_samples`).

#### 2a. `needless_range_loop` — `aec.rs` line ~164

Current code:

```rust
for i in 0..n {
    let near_f = nearend[i] as f32;
    let base = (self.far_pos + fl * ((n / fl) + 2) + i - n) % fl;
    ...
}
```

`i` is used both to index `nearend[i]` and in arithmetic (`+ i - n`).
Clippy fires because `nearend[i]` could use `.iter().enumerate()`.
Convert to `enumerate`:

```rust
for (i, &sample) in nearend.iter().enumerate() {
    let near_f = sample as f32;
    let base = (self.far_pos + fl * ((n / fl) + 2) + i - n) % fl;
    ...
}
```

Make sure to update any references to `nearend[i]` inside the loop body
to use `sample` (or `near_f` directly). Also update the NLMS adaptation
sub-loop if it references `nearend[i]`.

#### 2b. `needless_range_loop` — `resample.rs` line ~51

Current code:

```rust
for i in 0..FIR_TAPS {
    let n = i as f64 - m / 2.0;
    let sinc = ...;
    let t = 2.0 * i as f64 / m - 1.0;
    let kaiser = ...;
    kernel[i] = sinc * kaiser;
}
```

`i` is used both as an index (`kernel[i]`) and in arithmetic. Use
`iter_mut().enumerate()`:

```rust
for (i, slot) in kernel.iter_mut().enumerate() {
    let n = i as f64 - m / 2.0;
    let sinc = ...;
    let t = 2.0 * i as f64 / m - 1.0;
    let kaiser = ...;
    *slot = sinc * kaiser;
}
```

#### 3a. `manual_div_ceil` — `codec2_dec.rs` line ~48

Current code:

```rust
fn bytes_per_frame(&self) -> usize {
    (self.inner.bits_per_frame() + 7) / 8
}
```

Replace with:

```rust
fn bytes_per_frame(&self) -> usize {
    self.inner.bits_per_frame().div_ceil(8)
}
```

`div_ceil` is stable as of Rust 1.73. The builder uses a recent enough
toolchain. If `bits_per_frame()` returns `usize`, the method is available.
If it returns a different integer type, cast accordingly.

#### 3b. `manual_div_ceil` — `codec2_enc.rs` line ~48

Same pattern, same fix:

```rust
fn bytes_per_frame(&self) -> usize {
    self.inner.bits_per_frame().div_ceil(8)
}
```

#### 4a. `manual_clamp` — `denoise.rs` line ~59

Current code:

```rust
let clamped = val.max(-32768.0).min(32767.0);
```

Replace with:

```rust
let clamped = val.clamp(-32768.0_f32, 32767.0_f32);
```

Note: `.clamp()` on `f32` requires both bounds to be the same type. If `val`
is already `f32`, no extra cast is needed. Verify the type of `val` in
context (it is `f32` per the output array type `[f32; 480]`).

#### 4b. `manual_clamp` — `opus_enc.rs` line ~252

Read the surrounding code for the exact pattern. It will be something like:

```rust
let v = if x < min_val { min_val } else if x > max_val { max_val } else { x };
```

or the `.max().min()` chain. Replace with `x.clamp(min_val, max_val)`.

#### 5. `manual_ascii_check` — `opus_enc.rs` line ~104

Current code:

```rust
Ok(v) => !v.is_empty() && v != "0" && v.to_ascii_lowercase() != "false",
```

Clippy wants `.eq_ignore_ascii_case()` instead of lowercasing the whole string:

```rust
Ok(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
```

#### 6. `same_item_push` — `resample.rs` line ~183

Current code:

```rust
for _ in 1..RATIO {
    work.push(0.0);
}
```

This pushes the same `0.0` value `(RATIO - 1)` times. Replace with:

```rust
work.resize(work.len() + (RATIO - 1), 0.0f64);
```

Or equivalently:

```rust
work.extend(std::iter::repeat(0.0f64).take(RATIO - 1));
```

Note: `RATIO` is a `const usize`. Verify `work` is `Vec<f64>` in context
(it is — `work.push(s as f64)` immediately before).

## Implementation steps

1. Read each file at the line numbers listed above to confirm the exact current
   code before editing (line numbers may shift slightly due to prior edits).
2. Apply all 9 fixes. They are independent — no ordering requirement.
3. Run `cargo clippy -p wzp-codec -- -D warnings` locally or via the CI
   command.
4. If any lint persists, re-read that file section and adjust.

## Files to read before implementing

- `crates/wzp-codec/src/aec.rs` lines 114–200
- `crates/wzp-codec/src/resample.rs` lines 45–70 and 178–190
- `crates/wzp-codec/src/codec2_dec.rs` lines 40–55
- `crates/wzp-codec/src/codec2_enc.rs` lines 40–55
- `crates/wzp-codec/src/denoise.rs` lines 45–65
- `crates/wzp-codec/src/opus_enc.rs` lines 96–110 and 244–260

## Verify

```bash
cargo clippy -p wzp-codec -- -D warnings
```

Expected: exits 0 with no warnings.

Also run to confirm no regressions:

```bash
cargo test -p wzp-codec
```

## Done when

`cargo clippy -p wzp-codec -- -D warnings` exits 0. All 9 lints are gone.
`cargo test -p wzp-codec` passes. No changes outside `crates/wzp-codec/src/`.
