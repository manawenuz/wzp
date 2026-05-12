# Haiku-Ready Task Breakdown

> Companion to `docs/PRD/README.md`. Every task here is sized for an agent with limited context to pick up cold: it names exact files, exact symbols, and exact verification commands. Do tasks in order within a wave; waves are dependency-ordered.

---

## Agent operating instructions — read first

You are an implementing agent. The human is the reviewer. **Your job is not done when the code compiles; it is done when the reviewer has approved your report.** Read this section before touching any task.

### Workflow per task

1. **Claim the task.** Move its status in the [Status board](#status-board) at the bottom of this file from `Open` → `In Progress`. Add your handle / model name and a UTC timestamp.
2. **Implement.** Follow the steps in the task block exactly. If the steps don't fit reality (e.g. line numbers shifted, a referenced symbol doesn't exist, the API has evolved), **stop and surface the mismatch in your report** — do not improvise silently.
3. **Verify.** Run the exact commands in the task's `Verify` block. Capture their output verbatim — the reviewer will read it.
4. **Write the report.** Create `docs/PRD/reports/T<id>-report.md` using the template below. One report per task. No exceptions.
5. **Commit.** One commit per task. Message: `T<id>: <one-line summary>`. The report file is part of the same commit.
6. **Move to review.** Update the [Status board](#status-board): `In Progress` → `Pending Review`. Add a link to the report path.
7. **Stop.** Do NOT start the next task until the reviewer marks the previous one `Approved`. If they mark it `Changes Requested`, address the feedback in a follow-up commit, update the report, and move back to `Pending Review`.

### Follow-up tasks (`T<id>.<n>`)

When the reviewer approves a task but finds small non-blocking issues (missing docs, stale comments, minor cleanups), they **spawn new follow-up tasks** instead of carrying the work forward into an unrelated task. The parent task stays `Approved` and closed.

Follow-up IDs extend the parent: `T1.1.1`, `T1.1.2`, etc. They are first-class tasks — full block in this file with `Files`, `Steps`, `Verify`, `Done when` — and they show up in the status board between the parent and the next sibling (`T1.1.1` sits between `T1.1` and `T1.2`).

Agents pick up follow-ups in the same order they pick up wave tasks. A follow-up never blocks the next wave task: e.g. `T1.2` is claimable even if `T1.1.1` is still `Open`, unless the follow-up's body explicitly says otherwise (it usually doesn't).

Reviewers, when spawning a follow-up:

1. Add a numbered task block in the right section of this file (just below the parent).
2. Add a status-board row between the parent and the next sibling.
3. Reference the follow-up in the parent report's reviewer notes (e.g. "Spawned T1.1.1, T1.1.2 to track follow-ups.").

### Report template

Every report lives at `docs/PRD/reports/T<id>-report.md` and uses this template:

```markdown
# T<id> — <task title>

**Status:** Pending Review
**Agent:** <model/handle>
**Started:** <UTC ISO-8601>
**Completed:** <UTC ISO-8601>
**Commit:** <git sha>
**PRD:** ../<prd-filename>.md

## What I changed

- `<file path>:<line range>` — <one-line description of the change>
- `<file path>:<line range>` — <one-line description>
- (etc.)

## Why these choices

<2-6 sentences explaining any non-obvious decision: why this signature, why
this default, why this error type, why a deviation from the task steps if any.
If you followed the steps verbatim, say "Followed steps T<id>.1 through T<id>.N
without deviation." and that's enough.>

## Deviations from the task spec

<If you deviated from a numbered step, list each deviation with: which step,
what you did instead, why. If none, write "None.">

## Verification output

For each `Verify` command in the task block, paste the actual output. Trim
benign noise (warnings already present on main) but never trim test failure
output.

```
$ cargo test -p wzp-proto media_header_v2_roundtrip
running 1 test
test packet::tests::media_header_v2_roundtrip ... ok

test result: ok. 1 passed; 0 failed; ...
```

## Test summary

- Tests added: <count + names>
- Tests modified: <count + names>
- Workspace test count before: <N> / after: <M>
- `cargo clippy --workspace --all-targets -- -D warnings`: pass / fail (or N known-debt errors in <crate>; see PROTOCOL-AUDIT.md)
- `cargo fmt --all -- --check`: pass / fail

## Risks / follow-ups

<Anything the reviewer should know that isn't a bug: a TODO I left, a test I
couldn't write because of a missing fixture, a downstream task this enables
or blocks, an assumption I made that should be confirmed. If there are none,
write "None.">

## Reviewer checklist (filled in by reviewer)

- [ ] Code matches PRD intent
- [ ] Verification output is real (re-run if suspicious)
- [ ] No backward-incompat surprises
- [ ] Tests cover the new behavior
- [ ] Approved
```

### Coding standards — non-negotiable

These apply to every task. They are NOT repeated in each task block. Violating them is grounds for `Changes Requested` even if the code works.

1. **Rust edition 2024** (set in workspace root). No exceptions.
2. **`cargo fmt --all`** must produce a clean diff before commit. CI will reject otherwise.
3. **`cargo clippy --workspace --all-targets -- -D warnings`** must pass in crates you touch. Do not `#[allow(...)]` to silence — fix the root cause. If a lint is genuinely wrong, justify the allow in the report. Pre-existing debt in other crates (documented in `PROTOCOL-AUDIT.md`) is not your problem.
4. **No `unwrap()` / `expect()` in production code paths.** Tests are fine. Production: return a typed error.
5. **No `println!` / `eprintln!`.** Use `tracing::{debug,info,warn,error}!`. The crates are already wired for tracing.
6. **No new dependencies without justification.** If a task forces a new crate, list it under "Risks / follow-ups" in the report so the reviewer can sanity-check the supply chain.
7. **One commit per task** — see workflow. Don't squash multiple tasks. Don't split a task across commits unless the task itself instructs you to.
8. **Never modify `Cargo.lock` by hand.** Run a real build; commit the resulting lockfile delta.
9. **Public API changes need rustdoc.** Every new `pub fn`, `pub struct`, `pub enum`, or `pub trait` gets a `///` doc comment. Private items: doc only when non-obvious.
10. **Tests live with code.** `#[cfg(test)] mod tests { ... }` next to the code under test. Integration tests in `crates/<x>/tests/<name>.rs` only when they exercise multiple modules end-to-end.
11. **Async: tokio only.** Do not introduce `async-std` or `smol`. Spawn via `tokio::spawn`, not raw futures.
12. **Wire format types live in `wzp-proto`.** Do not redefine `MediaHeader`, `SignalMessage`, or codec/quality types in another crate. Re-export if needed.
13. **No emoji in code or commit messages** unless the surrounding context already uses them.
14. **No AI-attribution lines in commit messages.** Plain `T<id>: <summary>` body, that's it.
15. **Comments:** comment WHY, never WHAT. If the code needs a WHAT comment, rename the symbol instead. See repo-root CLAUDE.md (if present) for global guidance.
16. **Don't take destructive actions.** Specifically: never `git reset --hard`, `git push --force`, drop database tables, delete branches, or touch CI configs without the reviewer asking. If you think you need to, stop and ask in your report.
17. **Auto mode is not a license to skip these.** Even when the harness is set to autonomous execution, the workflow (report → Pending Review → wait for Approved) is mandatory.

### Environment-blocked tasks → file Blocked, do not ship stubs

You operate on a macOS host without an Android device or NDK build pipeline. For any task that requires Android-target compilation, an Android emulator/device, or Hetzner remote-builder access:

- **Do not "wrote it but couldn't test it" the deliverable.** A file with `#[cfg(target_os = "android")]` AMediaCodec code that has never been compiled for an Android target is not a completed task — it's an aspirational PR.
- **File a `Blocked` report** with whatever partial work made sense (e.g., trait surface, codec-agnostic helpers, cfg-gating fixes for non-Android builds). The reviewer will either pick up the Android validation themselves or close the task as `Deferred (reviewer-owned)`.
- Existing Deferred-but-reviewer-owned tasks today: **T4.3.1.1** (Android MediaCodec target-compile + device instrumentation). Skip past it.

### When to stop and ask

Stop and write a report with status `Blocked` (not `Pending Review`) if any of these happen:

- A task step references code that doesn't exist.
- A test fails for reasons unrelated to your change.
- The workspace doesn't build at HEAD before you started (the baseline is dirty).
- You need to make a meaningful design decision the task didn't anticipate.
- A "Verify" command produces output you don't understand.

A `Blocked` report is not a failure — it is the correct outcome when the task spec is wrong or incomplete.

---

## How to read a task

Each task block has:

- **ID & title** — `T<wave>.<n>` like `T1.1`.
- **PRD** — link to the parent PRD for the "why".
- **Effort** — rough hours for a junior dev with this doc + the repo.
- **Files** — exact paths you will edit.
- **Context** — 2-4 lines on what's there today.
- **Steps** — numbered, do them in order.
- **Verify** — exact commands; output must match.
- **Done when** — single-line acceptance.

---

## Environment setup (do this once)

```bash
# All commands assume CWD = /Users/manwe/CascadeProjects/warzonePhone
cargo build --workspace                 # baseline: must succeed
cargo test --workspace --no-fail-fast   # baseline: should be 571 pass / 0 fail (non-Android subset)
```

If either fails before you start a task, stop and report — the tree is dirty.

### Conventions

- Format on save: `cargo fmt --all` after any code change.
- Lints: `cargo clippy --workspace --all-targets -- -D warnings` must pass in crates you touch before commit. Pre-existing debt in other crates is documented in `PROTOCOL-AUDIT.md`.
- Tests live next to code under `#[cfg(test)]` modules, or in `crates/<x>/tests/`.
- Wire format types: `crates/wzp-proto/src/packet.rs` is authoritative. Do not duplicate field semantics elsewhere.
- Commit one task per commit. Reference task ID in commit message: `T1.1: widen MediaHeader to v2`.

### Useful greps

```bash
grep -rn "MediaHeader::" --include="*.rs"    # 6 files outside tests
grep -rn "MiniHeader::" --include="*.rs"
grep -rn "SignalMessage::" --include="*.rs"
grep -rn "CodecId::" --include="*.rs"
```

---

# Wave 1 — Foundation (target: 1 week)

Goal: v2 wire format lands cleanly. Audio works under v2. Old clients are politely rejected.

---

## T1.1 — Add v2 `MediaHeader` type

- **PRD:** `PRD-wire-format-v2.md`
- **Effort:** 3 h
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Context
Today `MediaHeader` is defined at line 20 of `packet.rs` with `WIRE_SIZE = 12` (line 47). Fields are bit-packed across the first two bytes. It is constructed in tests starting around line 1229.

### Steps

1. Open `crates/wzp-proto/src/packet.rs`.
2. **Do not delete** the existing `MediaHeader`. Rename it in-place to `MediaHeaderV1` (also rename `WIRE_SIZE` consts only on that struct). Keep all impls.
3. Below the `MediaHeaderV1` block, add a new `MediaHeader` struct (16 bytes, byte-aligned):

   ```rust
   /// 16-byte v2 media header. See docs/PRD/PRD-wire-format-v2.md.
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub struct MediaHeader {
       pub version: u8,           // always 2
       pub flags: u8,             // bit 7 T, bit 6 Q, bit 5 KeyFrame, bit 4 FrameEnd
       pub media_type: MediaType, // u8 wire repr
       pub codec_id: CodecId,
       pub stream_id: u8,
       pub fec_ratio: u8,         // 0..200 → 0.0..2.0
       pub seq: u32,
       pub timestamp: u32,
       pub fec_block: u16,
   }

   impl MediaHeader {
       pub const WIRE_SIZE: usize = 16;
       pub const VERSION: u8 = 2;

       pub fn write_to(&self, buf: &mut impl BufMut) {
           buf.put_u8(self.version);
           buf.put_u8(self.flags);
           buf.put_u8(self.media_type.to_wire());
           buf.put_u8(self.codec_id.to_wire());
           buf.put_u8(self.stream_id);
           buf.put_u8(self.fec_ratio);
           buf.put_u32(self.seq);
           buf.put_u32(self.timestamp);
           buf.put_u16(self.fec_block);
       }

       pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
           if buf.remaining() < Self::WIRE_SIZE { return None; }
           let version = buf.get_u8();
           if version != Self::VERSION { return None; }
           let flags = buf.get_u8();
           let media_type = MediaType::from_wire(buf.get_u8())?;
           let codec_id = CodecId::from_wire(buf.get_u8())?;
           let stream_id = buf.get_u8();
           let fec_ratio = buf.get_u8();
           let seq = buf.get_u32();
           let timestamp = buf.get_u32();
           let fec_block = buf.get_u16();
           Some(Self { version, flags, media_type, codec_id, stream_id, fec_ratio, seq, timestamp, fec_block })
       }

       pub const FLAG_REPAIR: u8     = 0b1000_0000;
       pub const FLAG_QUALITY: u8    = 0b0100_0000;
       pub const FLAG_KEYFRAME: u8   = 0b0010_0000;
       pub const FLAG_FRAME_END: u8  = 0b0001_0000;

       pub fn is_repair(&self) -> bool  { self.flags & Self::FLAG_REPAIR != 0 }
       pub fn has_quality(&self) -> bool { self.flags & Self::FLAG_QUALITY != 0 }
       pub fn is_keyframe(&self) -> bool { self.flags & Self::FLAG_KEYFRAME != 0 }
       pub fn is_frame_end(&self) -> bool { self.flags & Self::FLAG_FRAME_END != 0 }
   }
   ```

4. `MediaType` and `CodecId::to_wire` (8-bit) come from T1.2 and T1.3 — add a `// TODO(T1.2)` placeholder if those aren't merged yet (use `u8` directly).
5. Add a round-trip test next to the existing tests:

   ```rust
   #[test]
   fn media_header_v2_roundtrip() {
       let h = MediaHeader {
           version: 2, flags: MediaHeader::FLAG_QUALITY,
           media_type: MediaType::Audio, codec_id: CodecId::Opus24k,
           stream_id: 0, fec_ratio: 50,
           seq: 0xDEAD_BEEF, timestamp: 0x1234_5678,
           fec_block: 0xABCD,
       };
       let mut buf = BytesMut::with_capacity(MediaHeader::WIRE_SIZE);
       h.write_to(&mut buf);
       assert_eq!(buf.len(), 16);
       let mut cursor = std::io::Cursor::new(&buf[..]);
       let parsed = MediaHeader::read_from(&mut cursor).unwrap();
       assert_eq!(h, parsed);
   }
   ```

### Verify

```bash
cargo test -p wzp-proto media_header_v2_roundtrip
cargo build --workspace
```

### Done when
- New test passes. Workspace still builds. `MediaHeaderV1` still exists (we delete it later in T1.5).

---

## T1.1.1 — Add rustdoc on `MediaHeaderV2` public fields

- **Parent:** T1.1 (Approved)
- **PRD:** `PRD-wire-format-v2.md`
- **Effort:** 15 min
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Context
T1.1 added `MediaHeaderV2` with inline `//` comments on the public fields. The pre-existing `MediaHeaderV1` uses `///` rustdoc on every public field (coding standard #9 — public items need rustdoc). Match the existing pattern.

### Steps

1. Open `crates/wzp-proto/src/packet.rs`. Find `pub struct MediaHeaderV2`.
2. For each public field, replace the trailing `//` comment with a leading `///` doc comment. Example transformation:

   Before:
   ```rust
   pub struct MediaHeaderV2 {
       pub version: u8,    // always 2
       pub flags: u8,      // bit 7 T, bit 6 Q, bit 5 KeyFrame, bit 4 FrameEnd
       ...
   }
   ```

   After:
   ```rust
   pub struct MediaHeaderV2 {
       /// Protocol version. Always `2` on the wire; `read_from` rejects anything else.
       pub version: u8,
       /// Bit-packed flags. See `FLAG_REPAIR`, `FLAG_QUALITY`, `FLAG_KEYFRAME`, `FLAG_FRAME_END`.
       pub flags: u8,
       ...
   }
   ```

3. Document the four `FLAG_*` constants with `///` too. One line each is fine.
4. Document the four `is_*` / `has_*` accessor methods with `///`. One line each.
5. The `media_type: u8` field gets a doc comment that mentions the `TODO(T1.2)` — keep that TODO inline.

### Verify

```bash
cargo doc -p wzp-proto --no-deps 2>&1 | grep -i "missing"   # should be empty
cargo clippy -p wzp-proto --all-targets -- -D warnings -W missing_docs   # should pass
```

### Done when
- All public items on `MediaHeaderV2` carry `///` doc comments.
- `cargo doc -p wzp-proto --no-deps` emits no "missing documentation" warnings for `MediaHeaderV2`.

---

## T1.1.2 — Refresh stale test-count figures in docs

- **Parent:** T1.1 (Approved)
- **PRD:** `PRD-wire-format-v2.md` (housekeeping)
- **Effort:** 30 min
- **Files:**
  - `docs/ARCHITECTURE.md`
  - `docs/PRD/TASKS.md` (the Environment setup block)
  - Any other doc referencing "272 tests"

### Context
The original audit and the TASKS environment-setup block reference a workspace test count of **272**. The actual non-Android workspace baseline measured during T1.1 is **564** (with 1 added test → 565 after T1.1). The 272 figure is stale.

### Steps

1. Grep for the stale figure across the docs:
   ```bash
   grep -rn "272 tests\|272 pass\|272 total" docs/
   ```
2. For each hit, replace with the current count. **Re-measure before writing the number.**
   ```bash
   cargo test --workspace --no-fail-fast 2>&1 | grep "test result:" | awk '{s+=$4} END {print s}'
   # ... this gives a rough total; sanity-check against per-crate output
   ```
3. If `wzp-android` cannot build on the dev machine (no NDK), note that the count excludes `wzp-android` and is the "non-Android subset".
4. Update the per-crate Test Coverage table in `docs/ARCHITECTURE.md` (search for "## Test Coverage") with the new per-crate counts.

### Verify

```bash
grep -rn "272 tests\|272 pass" docs/   # should be empty
```

### Done when
- No doc references the stale 272 figure.
- ARCHITECTURE.md test coverage table reflects current per-crate counts.

---

## T1.2 — Add `MediaType` enum

- **PRD:** `PRD-wire-format-v2.md`
- **Effort:** 1 h
- **Files:**
  - `crates/wzp-proto/src/codec_id.rs` (or new sibling file `media_type.rs`)
  - `crates/wzp-proto/src/lib.rs` (re-export)

### Steps

1. Create `crates/wzp-proto/src/media_type.rs`:
   ```rust
   use serde::{Deserialize, Serialize};

   #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
   #[repr(u8)]
   pub enum MediaType {
       Audio = 0,
       Video = 1,
       Data = 2,
       Control = 3,
   }

   impl MediaType {
       pub const fn to_wire(self) -> u8 { self as u8 }
       pub const fn from_wire(v: u8) -> Option<Self> {
           match v {
               0 => Some(Self::Audio),
               1 => Some(Self::Video),
               2 => Some(Self::Data),
               3 => Some(Self::Control),
               _ => None,
           }
       }
   }
   ```
2. In `crates/wzp-proto/src/lib.rs`, add `pub mod media_type;` and `pub use media_type::MediaType;`.

### Verify
```bash
cargo build -p wzp-proto
cargo test -p wzp-proto
```

### Done when
`MediaType` is importable as `wzp_proto::MediaType`.

---

## T1.2.1 — Add rustdoc on `MediaType` variants and methods

- **Parent:** T1.2 (Approved)
- **PRD:** `PRD-wire-format-v2.md`
- **Effort:** 10 min
- **Files:**
  - `crates/wzp-proto/src/media_type.rs`

### Context
T1.2 created `MediaType` with a one-line top-level doc comment but no `///` rustdoc on the variants (`Audio`, `Video`, `Data`, `Control`) or methods (`to_wire`, `from_wire`). Coding standard #9 — public items need rustdoc. Same shape of follow-up as T1.1.1.

### Steps

1. Open `crates/wzp-proto/src/media_type.rs`.
2. Add a `///` doc comment to each variant. Examples (do not just copy — pick what's accurate):
   ```rust
   pub enum MediaType {
       /// Encoded speech / music (Opus, Codec2, ComfortNoise).
       Audio = 0,
       /// Encoded video access unit (H.264, H.265, AV1; PRD-video-multicodec).
       Video = 1,
       /// Opaque payload not interpreted by the relay (reserved).
       Data = 2,
       /// In-band control message carried on the media plane (reserved).
       Control = 3,
   }
   ```
3. Add a `///` doc on `to_wire` and `from_wire`. One line each is fine — explain the wire byte mapping and the `None` case.

### Verify
```bash
cargo doc -p wzp-proto --no-deps 2>&1 | grep -i "missing" || echo "no missing-doc warnings"
cargo clippy -p wzp-proto --all-targets -- -D warnings -W missing_docs
```

### Done when
- All variants and methods on `MediaType` carry `///` doc comments.
- `cargo doc -p wzp-proto --no-deps` emits no "missing documentation" warnings for `MediaType`.

---

## T1.3 — Widen `CodecId` wire representation to u8

- **PRD:** `PRD-wire-format-v2.md` (resolves audit W9)
- **Effort:** 1 h
- **Files:**
  - `crates/wzp-proto/src/codec_id.rs`

### Context
`CodecId::to_wire` returns `self as u8` (already u8 in memory). The "4 bits on wire" is enforced by how `MediaHeaderV1` packs it. With v2 the wire byte is full 8-bit — so reserve more IDs without touching `to_wire`/`from_wire` for the existing variants.

### Steps

1. In `codec_id.rs`, **reserve** (but do not implement) future codec IDs by adding doc comments after `Opus64k = 8`:
   ```rust
   // Reserved for video codecs; implementations land in PRD-video-multicodec.
   // 9  => H264 baseline
   // 10 => H264 main
   // 11 => H265 main
   // 12 => AV1
   // 13 => VP9
   ```
2. **Do not** add new variants yet — that happens in T4.x once `wzp-video` exists.
3. Add a regression test confirming `from_wire(9..=255)` returns `None`:
   ```rust
   #[test] fn codec_id_unknown_values_rejected() {
       for v in 9u8..=255 { assert!(CodecId::from_wire(v).is_none(), "v={v}"); }
   }
   ```

### Verify
```bash
cargo test -p wzp-proto codec_id_unknown_values_rejected
```

### Done when
Test passes. Existing audio tests still pass.

---

## T1.4 — Add v2 `MiniHeader` with `seq_delta`

- **PRD:** `PRD-wire-format-v2.md` (resolves audit W4)
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Context
Existing `MiniHeader` is 4 bytes at line 501. `MiniFrameContext::expand` infers `seq` by `wrapping_add(1)` (line ~553) — a missed full header desyncs. v2 carries explicit `seq_delta`.

### Steps

1. Rename existing `MiniHeader` → `MiniHeaderV1` and `MiniFrameContext` → `MiniFrameContextV1`. Keep impls intact.
2. Add new `MiniHeader` (5 bytes):
   ```rust
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub struct MiniHeader {
       pub seq_delta: u8,            // packets since baseline; 1 in steady state
       pub timestamp_delta_ms: u16,
       pub payload_len: u16,
   }

   impl MiniHeader {
       pub const WIRE_SIZE: usize = 5;

       pub fn write_to(&self, buf: &mut impl BufMut) {
           buf.put_u8(self.seq_delta);
           buf.put_u16(self.timestamp_delta_ms);
           buf.put_u16(self.payload_len);
       }

       pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
           if buf.remaining() < Self::WIRE_SIZE { return None; }
           Some(Self {
               seq_delta: buf.get_u8(),
               timestamp_delta_ms: buf.get_u16(),
               payload_len: buf.get_u16(),
           })
       }
   }
   ```
3. Add `MiniFrameContext` (no `V1` suffix) tracking v2 `MediaHeader`:
   ```rust
   #[derive(Clone, Debug, Default)]
   pub struct MiniFrameContext {
       last: Option<MediaHeader>,
   }
   impl MiniFrameContext {
       pub fn update(&mut self, h: &MediaHeader) { self.last = Some(*h); }
       pub fn expand(&mut self, m: &MiniHeader) -> Option<MediaHeader> {
           let base = self.last.as_ref()?;
           let mut e = *base;
           e.seq = base.seq.wrapping_add(m.seq_delta as u32);
           e.timestamp = base.timestamp.wrapping_add(m.timestamp_delta_ms as u32);
           self.last = Some(e);
           Some(e)
       }
   }
   ```
4. Add round-trip test mirroring `T1.1`.

### Verify
```bash
cargo test -p wzp-proto mini
```

### Done when
v2 mini header round-trips. v1 type still compiles.

---

## T1.4.1 — Add rustdoc on `MiniHeaderV2` / `MiniFrameContextV2` public items

- **Parent:** T1.4 (Approved)
- **PRD:** `PRD-wire-format-v2.md`
- **Effort:** 15 min
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Context
T1.4 added v2 types with top-level `///` docs on the structs themselves but no `///` rustdoc on the fields or methods. Same shape of follow-up as T1.1.1 and T1.2.1 — coding standard #9.

### Steps

1. Open `crates/wzp-proto/src/packet.rs`. Find `pub struct MiniHeaderV2`.
2. Add `///` doc comments to each public field:
   - `seq_delta` — explain it's the count of packets since the baseline (typically 1), and that explicit deltas resolve audit W4 (one missed full header no longer desyncs).
   - `timestamp_delta_ms` — milliseconds since baseline's `timestamp`.
   - `payload_len` — bytes of payload following the mini header.
3. Document `WIRE_SIZE`, `write_to`, `read_from`. One line each. Mention that `read_from` returns `None` on short buffer.
4. Same treatment for `MiniFrameContextV2`: doc the `update` and `expand` methods. `expand` should note that it returns `None` if no baseline has been set.

### Verify
```bash
cargo doc -p wzp-proto --no-deps 2>&1 | grep -i "missing" || echo "no missing-doc warnings"
cargo clippy -p wzp-proto --all-targets -- -D warnings -W missing_docs
```

### Done when
- All public items on `MiniHeaderV2` and `MiniFrameContextV2` carry `///` doc comments.
- `cargo doc -p wzp-proto --no-deps` emits no "missing documentation" warnings for these types.

---

## T1.5 — Migrate emit/parse sites to v2

- **PRD:** `PRD-wire-format-v2.md`
- **Effort:** 4 h
- **Files (touch all that use `MediaHeader::`):**
  - `crates/wzp-proto/src/packet.rs`
  - `crates/wzp-client/src/call.rs`
  - `crates/wzp-relay/src/room.rs`
  - `crates/wzp-relay/src/pipeline.rs`
  - `crates/wzp-android/src/engine.rs`

### Context
Only 6 production files outside `packet.rs` reference `MediaHeader::`. Confirm with:
```bash
grep -rln "MediaHeader::" crates/ | grep -v target
```

### Steps

1. For each file in the list above, replace v1 construction patterns with v2. The audio fields are unchanged in semantics; new fields default as follows:
   - `version: 2`
   - `flags: 0` (set `FLAG_QUALITY` where the v1 code set `has_quality_report = true`, etc.)
   - `media_type: MediaType::Audio`
   - `stream_id: 0`
   - `fec_ratio: <old fec_ratio_encoded * 200 / 127>` (convert range)
   - `seq: old_seq as u32`
   - `timestamp` unchanged
   - `fec_block: u16::from(old_fec_block) | (u16::from(old_fec_symbol) << 8)` for audio (low byte block_id, high byte symbol_idx)
2. Update `MediaHeaderV1`-using parse code identically — convert from u16 seq/u8 block_id to v2 layout at parse boundary.
3. Search for `WIRE_SIZE` arithmetic and update buffer sizes (12 → 16, 4 → 5).
4. Delete `MediaHeaderV1`, `MiniHeaderV1`, `MiniFrameContextV1` once everything builds.

### Verify
```bash
cargo build --workspace
cargo test --workspace --no-fail-fast
# Expected: all 571 tests still pass
```

### Done when
- Workspace builds clean.
- All audio tests pass.
- No reference to `MediaHeaderV1` / `MiniHeaderV1` anywhere.

---

## T1.5.1 — Remove `unwrap()` from `encode_compact`

- **Parent:** T1.5 (Approved)
- **PRD:** `PRD-wire-format-v2.md` (cleanup)
- **Effort:** 20 min
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Context
`encode_compact` calls `ctx.last_header().unwrap()` at line ~262. The invariant ("a full header is forced on the first frame and every `MINI_FRAME_FULL_INTERVAL` frames") makes it logically safe, but standard #4 forbids `unwrap()` in production paths. Carried over from v1.

### Steps

1. Open `crates/wzp-proto/src/packet.rs`. Find `pub fn encode_compact`.
2. Replace the `unwrap()` with one of:
   - **Recommended:** when `ctx.last_header()` is `None`, fall back to emitting a full frame and force `frames_since_full = 0`. This makes the invariant explicit in the code rather than implicit.
   - Alternative: return `Result<Bytes, EncodeError>` with a typed `NoBaselineHeader` variant. More invasive (changes the public signature).
3. Add a test that constructs a fresh `MiniFrameContext` and calls `encode_compact` immediately — without the existing fix, this would panic; with the fix, it should emit a full frame.

### Verify
```bash
cargo test -p wzp-proto encode_compact
cargo clippy -p wzp-proto --all-targets -- -D warnings
grep -n "\.unwrap()" crates/wzp-proto/src/packet.rs | grep -v "#\[cfg(test)\]\|^[[:space:]]*//\|tests::"
# the unwrap on line ~262 should be gone; only test-code unwraps remain.
```

### Done when
- No `unwrap()` in `encode_compact` or anywhere else in non-test code in `packet.rs`.
- New test passes; existing `encode_compact` tests still pass.

---

## T1.5.2 — Workspace clippy hygiene + document pre-existing debt

- **Parent:** T1.5 (Approved)
- **PRD:** `PRD-wire-format-v2.md` (process)
- **Effort:** 30 min
- **Files:**
  - `docs/PROTOCOL-AUDIT.md` (add a "Known pre-existing clippy debt" section)
  - This file (TASKS.md) — update report template instruction to require workspace clippy

### Context
T1.5 review revealed two issues: (1) the agent ran only `-p wzp-proto` clippy, not workspace; (2) workspace clippy fails with 9 `wzp-codec` errors and 3 `warzone-protocol` errors. Both are pre-existing (verified against HEAD~1). Need to capture these as known debt so they don't stay invisible, and tighten the report template to require workspace clippy on every task.

### Steps

1. Run `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^error\[|could not compile" | head -50` and capture the output.
2. Add a section to `docs/PROTOCOL-AUDIT.md` named **"Known pre-existing clippy debt (as of T1.5.2)"** listing the failing crates and a brief description per error category (manual ASCII case-cmp, manual arithmetic check, loop index, etc.). Reference the commit SHA of HEAD at time of measurement.
3. In `docs/PRD/TASKS.md`, update the report template's "Test summary" section: change `cargo clippy --workspace --all-targets -- -D warnings: pass / fail` to `cargo clippy --workspace --all-targets -- -D warnings: pass / fail (or N known-debt errors in <crate>; see PROTOCOL-AUDIT.md)`. This makes the expectation explicit and gives agents a way to acknowledge known debt without re-discussing it every task.
4. Optional: add a `make clippy-baseline` or similar script to `tools/` that prints expected-error count so agents can detect regressions.

### Verify
```bash
grep -c "Known pre-existing clippy debt" docs/PROTOCOL-AUDIT.md   # >= 1
grep -c "or N known-debt errors" docs/PRD/TASKS.md                # >= 1
```

### Done when
- PROTOCOL-AUDIT.md has the known-debt section with current error counts and categories.
- TASKS.md report template reflects the new expectation.
- A follow-up cleanup task is created in the audit (separate from this one) to actually fix the pre-existing debt over time.

---

## T1.6 — Protocol version negotiation in handshake

- **PRD:** `PRD-wire-format-v2.md` + `PRD-protocol-hardening.md` (W12)
- **Effort:** 3 h
- **Files:**
  - `crates/wzp-proto/src/packet.rs` (extend `SignalMessage`)
  - `crates/wzp-client/src/handshake.rs`
  - `crates/wzp-relay/src/handshake.rs`

### Steps

1. In `packet.rs`, add to `CallOffer`:
   ```rust
   #[serde(default = "default_proto_version")]
   pub protocol_version: u8,
   #[serde(default = "default_supported_versions")]
   pub supported_versions: Vec<u8>,
   ```
   Helpers:
   ```rust
   fn default_proto_version() -> u8 { 2 }
   fn default_supported_versions() -> Vec<u8> { vec![2] }
   ```
2. Add a new `Hangup` reason variant. Find `SignalMessage::Hangup` (look for the `Hangup` variant in the enum near the bottom) and add to the reason enum / fields:
   ```rust
   ProtocolVersionMismatch { server_supported: Vec<u8> },
   ```
   If `reason` is a `String`, instead add a structured variant `SignalMessage::ProtocolVersionMismatch { server_supported: Vec<u8> }` and use that.
3. In `crates/wzp-relay/src/handshake.rs`, after parsing `CallOffer`, check `protocol_version == 2`. If not, send `ProtocolVersionMismatch` and close.
4. In `crates/wzp-client/src/handshake.rs`, set the field on outgoing `CallOffer`; on receiving the mismatch variant, return a typed error.

### Verify
```bash
cargo test -p wzp-relay handshake
cargo test -p wzp-client handshake
```

### Done when
A v1-style offer (missing `protocol_version` field — serde default makes it 2 in this codebase, so explicitly test with `protocol_version: 1`) is rejected with the typed signal.

---

## T1.7 — Move `QualityReport` trailer inside AEAD payload

- **PRD:** `PRD-protocol-hardening.md` (W5)
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-client/src/call.rs` (encode/decode paths)
  - `crates/wzp-crypto/src/session.rs` (verify AEAD boundary)

### Context
A `QualityReport` (4 bytes) is appended to media packets when the `Q` flag is set. The flag is in the (plaintext, AAD-bound) header; the trailer must sit **inside** the AEAD payload so stripping it corrupts decryption.

### Steps

1. Grep for the encode site:
   ```bash
   grep -rn "has_quality_report\|FLAG_QUALITY\|QualityReport" crates/wzp-client/src/call.rs
   ```
2. Find where `QualityReport::write_to` (or `put_*` calls) writes the 4 bytes. Confirm it writes into the buffer that is **then** passed to `encrypt_in_place` / `seal` — not after.
3. If currently appended *after* AEAD seal: refactor so the order is:
   - Write `MediaHeader` (becomes AAD).
   - Write payload.
   - Write `QualityReport` trailer if Q flag set.
   - AEAD-seal the (payload + trailer) bytes with header as AAD.
4. Mirror on decode side.
5. Add a test that tampers with the trailer post-encrypt and asserts decrypt fails.

### Verify
```bash
cargo test -p wzp-client quality_report_aead
cargo test -p wzp-crypto
```

### Done when
- Tamper test passes (decryption fails on trailer tamper).
- Round-trip with quality flag set still works.

---

## T1.8 — Per-stream anti-replay window with configurable size

- **PRD:** `PRD-protocol-hardening.md` (W11)
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-crypto/src/anti_replay.rs`
  - `crates/wzp-crypto/src/session.rs` (or wherever the window is owned)

### Steps

1. Today the window is fixed 64 packets. Make it constructible with size:
   ```rust
   impl AntiReplay { pub fn with_window(size: usize) -> Self { ... } }
   ```
2. The session owner (search `AntiReplay::new`) is updated to allocate per `(stream_id, MediaType)`. Use a `HashMap<(u8, MediaType), AntiReplay>` keyed on the v2 header fields.
3. Default sizes:
   - `Audio`: 64
   - `Video`: 1024
   - `Data`: 256
   - `Control`: 32

### Verify
```bash
cargo test -p wzp-crypto anti_replay
```

### Done when
- A new test confirms a 200-packet video burst with one reorder doesn't drop any.
- Existing audio anti-replay tests pass.

---

# Wave 2 — Feedback + abuse mitigation (target: 1 week)

Goal: BWE drives adaptation. Tier A/B/C conformance running in observe-only mode at the relay.

---

## T2.1 — Add `SignalMessage::TransportFeedback`

- **PRD:** `PRD-transport-feedback-bwe.md`
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Steps

1. Add to the `SignalMessage` enum:
   ```rust
   TransportFeedback {
       #[serde(default)] version: u8,        // = 1
       stream_id: u8,
       acked_seqs: Vec<u32>,
       nacked_seqs: Vec<u32>,
       remb_bps: u32,
       recv_time_us: u64,
   },
   ```
2. Add a unit test serializing/deserializing with `bincode` to ensure forward/backward compat.

### Verify
```bash
cargo test -p wzp-proto transport_feedback
```

### Done when
Variant round-trips. No other code consumes it yet — that's T2.2/T2.3.

---

## T2.2 — `BandwidthEstimator` in `wzp-proto::bandwidth`

- **PRD:** `PRD-transport-feedback-bwe.md`
- **Effort:** 4 h
- **Files:**
  - `crates/wzp-proto/src/bandwidth.rs` (already exists — extend, don't replace)
  - `crates/wzp-transport/src/path_monitor.rs` (read existing cwnd/RTT exposure)

### Context
`bandwidth.rs` already exists (14 KB). Read it first. The `QuinnPathSnapshot` type exposes `loss_pct`, `rtt_ms` today; add `cwnd_bps`, `bytes_in_flight` if missing.

### Steps

1. Read `crates/wzp-transport/src/path_monitor.rs` to find how Quinn `PathStats` are exposed.
2. Add to `QuinnPathSnapshot`:
   ```rust
   pub cwnd_bytes: u64,
   pub bytes_in_flight: u64,
   ```
   Populate from `quinn::Connection::stats().path`.
3. In `wzp-proto/src/bandwidth.rs`, add:
   ```rust
   pub struct BandwidthEstimator {
       cwnd_bps: AtomicU64,
       peer_remb_bps: AtomicU64,
       smoothed_bps: AtomicU64,
   }
   impl BandwidthEstimator {
       pub fn new() -> Self { ... default ... }
       pub fn update_from_quinn(&self, snap: &QuinnPathSnapshot) { /* compute cwnd_bps = cwnd_bytes * 8 / rtt_s */ }
       pub fn update_from_peer(&self, fb_remb_bps: u32) { ... }
       pub fn target_send_bps(&self) -> u64 {
           let m = self.cwnd_bps.load(Relaxed).min(self.peer_remb_bps.load(Relaxed));
           (m as f64 * 0.9) as u64
       }
   }
   ```
4. EWMA smoothing: half-life 2 s. Update `smoothed_bps` from input on each tick.

### Verify
```bash
cargo test -p wzp-proto bandwidth
cargo test -p wzp-transport
```

### Done when
- Unit test: feed scripted cwnd + remb values, assert `target_send_bps` smooths correctly.

---

## T2.3 — Plumb BWE into adaptive controller

- **PRD:** `PRD-transport-feedback-bwe.md`
- **Effort:** 3 h
- **Files:**
  - `crates/wzp-proto/src/quality.rs` (`AdaptiveQualityController`)
  - `crates/wzp-client/src/call.rs` (instantiate + feed)

### Steps

1. Add a setter to `AdaptiveQualityController`:
   ```rust
   pub fn set_bandwidth_estimator(&mut self, bwe: Arc<BandwidthEstimator>) { self.bwe = Some(bwe); }
   ```
2. In the controller's upgrade decision (search for "consecutive_good_reports" or similar threshold logic), add a guard:
   ```rust
   if let Some(bwe) = &self.bwe {
       if bwe.target_send_bps() < self.current_tier_ceiling_bps() * 130 / 100 { return; }
   }
   ```
3. In `call.rs`, instantiate one `Arc<BandwidthEstimator>` per session, feed it from both send loop (`update_from_quinn` from path snapshot) and recv loop (`update_from_peer` from incoming TransportFeedback), pass to the controller.

### Verify
```bash
cargo test -p wzp-proto quality
```

### Done when
Existing quality tests pass with BWE attached. New test: scripted "loss = 0, cwnd = 50 kbps" never upgrades past Opus 24k.

---

## T2.4 — Relay conformance: Tier A (bitrate ceiling)

- **PRD:** `PRD-relay-conformance.md`
- **Effort:** 3 h
- **Files:**
  - `crates/wzp-relay/src/conformance.rs` (new)
  - `crates/wzp-relay/src/room.rs` (call site)

### Steps

1. Create `crates/wzp-relay/src/conformance.rs`:
   ```rust
   use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
   use std::time::Instant;
   use wzp_proto::{CodecId, MediaHeader, MediaType};

   pub struct ConformanceMeter {
       window_start: parking_lot::Mutex<Instant>,
       bytes_in_window: AtomicU64,
       packets_in_window: AtomicU64,
       last_seq: AtomicU64,
       last_ts: AtomicU64,
   }

   #[derive(Debug)]
   pub enum Violation { BitrateExceeded, PacketRateExceeded, TimestampDrift }

   impl ConformanceMeter {
       pub fn new() -> Self { ... }
       pub fn observe(&self, h: &MediaHeader, payload_len: usize, now: Instant) -> Result<(), Violation> {
           // Tier A
           let window_bytes = self.bytes_in_window.fetch_add((MediaHeader::WIRE_SIZE + payload_len) as u64, Relaxed);
           // ... compare against ceiling_bps_for(h.codec_id, h.media_type)
       }
   }

   pub fn ceiling_bps(codec: CodecId) -> u64 {
       let nominal = codec.bitrate_bps() as u64;
       (nominal * 3 * 115 / 100).max(2_000)   // FEC 2.0 + 15% overhead, floor 2 kbps
   }
   ```
2. In `room.rs`, attach one `ConformanceMeter` per participant. Call `observe` on each incoming media packet.
3. **Observe-only mode for now.** Log violations to `tracing::warn!` and bump a Prometheus counter. Do not close session.

### Verify
```bash
cargo test -p wzp-relay conformance
```

### Done when
Unit test: synthetic 1 MB/s declared as Opus 24k logs `Violation::BitrateExceeded`.

---

## T2.5 — Tier B (packet-rate) + Tier C (timestamp drift)

- **PRD:** `PRD-relay-conformance.md`
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-relay/src/conformance.rs`

### Steps

1. Add packet-rate enforcement: `packets_in_window > max_pps(codec) * 1.5` over a 1 s window → `PacketRateExceeded`.
2. `max_pps(codec) = 1000 / codec.frame_duration_ms() * 3` (×3 for FEC).
3. Timestamp drift: track `Δtimestamp / Δseq` over rolling 200-packet window. If outside `frame_duration_ms × [0.5, 2.0]`, log `TimestampDrift`.

### Verify
```bash
cargo test -p wzp-relay conformance
```

### Done when
Both new tests pass alongside Tier A test.

---

## T2.6 — Prometheus metrics for conformance

- **PRD:** `PRD-relay-conformance.md`
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-relay/src/metrics.rs`

### Steps

1. Add counters / histograms:
   ```rust
   wzp_relay_conformance_violations_total{tier, codec_id, media_type, verdict}
   wzp_relay_conformance_bytes_per_session{media_type}      histogram
   wzp_relay_conformance_iat_ms{media_type}                  histogram
   ```
2. Wire `ConformanceMeter` to bump these on `observe`.

### Verify
```bash
curl localhost:9090/metrics | grep wzp_relay_conformance
```
(after `cargo run -p wzp-relay -- --listen 127.0.0.1:4433 --no-auth` with a synthetic client)

### Done when
Counters increment under abusive traffic; quiet on legitimate audio.

---

# Wave 3 — Protocol hardening (target: 3-4 days)

---

## T3.1 — Confirm `RoomManager` concurrency (W13)

- **PRD:** `PRD-protocol-hardening.md`
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-relay/src/room.rs`

### Context
`RoomManager` already uses `DashMap<String, Room>` (verified at line 352). The audit (W13) was based on the older ARCHITECTURE doc which mentioned a single Mutex. The actual remaining contention point is whatever's *inside* `Room` — confirm.

### Steps

1. Read the `Room` struct definition.
2. If `Room` itself uses fine-grained locks or is `Arc<RwLock<Room>>` already, document this in `PROTOCOL-AUDIT.md` and mark W13 resolved.
3. If `Room` has a single per-room `Mutex` held during fan-out, identify the hot path and either:
   - Split fan-out list into `RwLock<Vec<Participant>>` (read-mostly).
   - Use `ArcSwap<Vec<Participant>>` for lock-free reads.
4. Run the 40+4 relay integration tests.

### Verify
```bash
cargo test -p wzp-relay
cargo test -p wzp-relay --test federation
cargo test -p wzp-relay --test handshake_integration
```

### Done when
Tests pass + a one-line update in `PROTOCOL-AUDIT.md` noting actual state.

---

## T3.2 — Document `timestamp_ms` rebase across rekey (W3)

- **PRD:** `PRD-protocol-hardening.md`
- **Effort:** 1 h
- **Files:**
  - `crates/wzp-proto/src/packet.rs` (doc comment on `MediaHeader::timestamp`)
  - `crates/wzp-crypto/src/rekey.rs` (add comment)
  - `docs/WZP-SPEC.md`
  - Add test in `crates/wzp-client/tests/long_session.rs`

### Steps

1. Decision (already made): `timestamp_ms` is monotonic across rekeys. Document inline:
   ```rust
   /// Milliseconds since session start. Monotonic for the full session lifetime;
   /// NOT reset by rekey (rekey changes only key material, not framing state).
   pub timestamp: u32,
   ```
2. In `rekey.rs`, add a comment near the rekey handler confirming sequence + timestamp are untouched.
3. Add a test that performs 2 rekeys mid-session and asserts `timestamp` continues monotonically.

### Verify
```bash
cargo test -p wzp-client --test long_session rekey_timestamp_monotonic
```

### Done when
Test passes.

---

## T3.3 — `SignalMessage` version field (W12)

- **PRD:** `PRD-protocol-hardening.md`
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-proto/src/packet.rs`

### Steps

1. For each variant of `SignalMessage`, add `#[serde(default)] version: u8` as the first field, with helper `fn default_signal_version() -> u8 { 1 }`.
2. Add fallback variant for unknown future signals:
   ```rust
   #[serde(other)]
   Unknown,
   ```
   (Note: bincode + serde `other` may need a wrapper — research before implementing. If not feasible, document the limitation and skip the `Unknown` arm.)
3. Decode path: on `Unknown`, log `tracing::warn!("unknown signal variant")` and **do not** close session.

### Verify
```bash
cargo test -p wzp-proto signal_message
```

### Done when
Existing signal tests pass. Old payloads (without `version` field) still deserialize.

---

## T3.4 — Tier D (per-codec packet size sanity)

- **PRD:** `PRD-relay-conformance.md`
- **Effort:** 2 h
- **Files:**
  - `crates/wzp-relay/src/conformance.rs`

### Steps

1. Add per-codec typical / max payload table:
   ```rust
   pub fn payload_size_bound(codec: CodecId) -> usize {
       match codec {
           CodecId::Opus64k => 320, CodecId::Opus48k => 240,
           CodecId::Opus32k => 200, CodecId::Opus24k => 160,
           CodecId::Opus16k => 100, CodecId::Opus6k => 90,
           CodecId::Codec2_3200 => 30, CodecId::Codec2_1200 => 30,
           CodecId::ComfortNoise => 16,
       }
   }
   ```
2. Maintain EWMA of payload size per meter. Reject if EWMA exceeds 2× typical for declared codec.

### Verify
```bash
cargo test -p wzp-relay conformance_tier_d
```

### Done when
Synthetic stream of 1400-byte payloads declared as Codec2_1200 flagged within 5 s.

---

## T3.5 — Tier E (per-fingerprint token bucket)

- **PRD:** `PRD-relay-conformance.md`
- **Effort:** 4 h
- **Files:**
  - `crates/wzp-relay/src/conformance.rs` (or sibling `quota.rs`)
  - `crates/wzp-relay/src/auth.rs` (for authed/anon split)

### Steps

1. Implement a simple token bucket per `(fingerprint, src_ip)`:
   ```rust
   pub struct TokenBucket {
       capacity: u64,
       tokens: AtomicU64,
       refill_per_sec: u64,
       last_refill: AtomicU64,
   }
   ```
2. Wire into per-participant forward loop. Refill on each `observe`.
3. Authed/anon split: authenticated quota = 50 GB/month; anon = 1 GB/month. Per-session cap = 256 kbps audio (5 Mbps video reserved for later).
4. **Observe-only:** log + counter; do not throttle yet.

### Verify
```bash
cargo test -p wzp-relay token_bucket
```

### Done when
Unit test: 100 KB at 256 kbps cap consumes no tokens; 1 MB exceeds.

---

# Wave 4 — Video v1 (3 weeks)

See `PRD-video-v1.md` for design.

---

## T4.1 — `wzp-video` crate scaffold + H.264 NAL framer + depacketizer

- **PRD:** `PRD-video-v1.md`
- **Effort:** 3 d
- **Files:**
  - `crates/wzp-video/Cargo.toml`
  - `crates/wzp-video/src/lib.rs`
  - `crates/wzp-video/src/framer.rs`
  - `crates/wzp-video/src/depacketizer.rs`
  - `crates/wzp-proto/src/codec_id.rs`
  - `Cargo.toml` (workspace members)

### Context

WZP currently has no video path. Wave 4 adds H.264 baseline single-layer video. T4.1 is the foundation: a new `wzp-video` crate parallel to `wzp-codec`, containing the NAL framer and depacketizer. No platform encoder/decoder yet — that lands in T4.2/T4.3.

### Steps

1. Create `crates/wzp-video` and register it in the workspace `Cargo.toml`.
2. Add `H264Baseline = 9` to `CodecId` in `wzp-proto` (reserved slot).
3. Implement `H264Framer` in `framer.rs`:
   - Parses access units into NAL units (split by 0x000001 / 0x00000001 start codes).
   - Emits Single-NAL packets when the NAL fits in `max_payload_size`.
   - Fragments oversized NALs using H.264 FU-A (RFC 6184).
   - Returns a `Vec<FramedPacket>` where the last packet has `is_frame_end = true`.
4. Implement `H264Depacketizer` in `depacketizer.rs`:
   - Reassembles Single-NAL packets directly.
   - Accumulates FU-A fragments until the end marker is seen.
   - Emits a complete access unit (`Vec<u8>`) when `is_frame_end` arrives and no fragmentation is in progress.
5. Add roundtrip tests and edge-case tests (empty input, single NAL, multi-NAL access unit, FU-A fragmentation, FU-A reassembly).

### Verify

```bash
cargo test -p wzp-video
```

### Done when

Synthetic H.264 access units (single NAL, multi-NAL, and oversized NAL requiring FU-A fragmentation) roundtrip correctly through framer + depacketizer.

---

## T4.2 — VideoToolbox H.264 encoder + decoder (macOS)

- **PRD:** `PRD-video-v1.md`
- **Effort:** 3 d
- **Files:**
  - `crates/wzp-video/src/encoder.rs`
  - `crates/wzp-video/src/decoder.rs`
  - `crates/wzp-video/src/videotoolbox.rs`

### Context

T4.1 created the `wzp-video` crate with framer/depacketizer. T4.2 adds the macOS platform layer: `VideoEncoder` and `VideoDecoder` traits plus a VideoToolbox implementation. "Minimum viable" means the API compiles on macOS, can be instantiated, and has the correct shape for T4.4–T4.7 to call into.

### Steps

1. Add `video-toolbox` crate dependency (safe Rust bindings to Apple VideoToolbox).
2. Define `VideoEncoder` trait in `encoder.rs`:
   ```rust
   pub trait VideoEncoder: Send {
       fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError>;
       fn request_keyframe(&mut self);
       fn is_keyframe(&self, packet: &[u8]) -> bool;
   }
   ```
3. Define `VideoDecoder` trait in `decoder.rs`:
   ```rust
   pub trait VideoDecoder: Send {
       fn decode(&mut self, packet: &[u8]) -> Result<Option<VideoFrame>, VideoError>;
   }
   ```
4. Implement `VideoToolboxEncoder` and `VideoToolboxDecoder` in `videotoolbox.rs` (macOS only, gated by `#[cfg(target_os = "macos")]`).
5. Add compile-guarded stubs for non-macOS targets.

### Verify

```bash
cargo test -p wzp-video videotoolbox
cargo build -p wzp-video
```

### Done when

`wzp-video` compiles on macOS with `VideoToolboxEncoder`/`VideoToolboxDecoder` structs present and instantiable.

---

## T4.2.1 — Wire real VideoToolbox VTCompressionSession / VTDecompressionSession (macOS)

- **Parent:** T4.2 (Approved — scaffold only)
- **PRD:** `PRD-video-v1.md`
- **Effort:** 3–4 d
- **Files:**
  - `crates/wzp-video/src/videotoolbox.rs`
  - `crates/wzp-video/Cargo.toml` (will need `core-foundation`, `core-media`, `core-video`, `block` crates or equivalent — disclose under "Risks / follow-ups")
  - `crates/wzp-video/tests/encode_decode_macos.rs` (new — round-trip test, `#[cfg(target_os = "macos")]`)

### Context
T4.2 shipped the API surface (traits, structs, `is_keyframe`) but stubbed both `encode()` and `decode()`. This task fills in those stubs against the actual Apple frameworks. **This is the task that satisfies the original PRD-video-v1 T4.2 acceptance criterion.**

The current TODOs are at:
- `crates/wzp-video/src/videotoolbox.rs:34` — `VideoToolboxEncoder::encode` stub.
- `crates/wzp-video/src/videotoolbox.rs:72` — `VideoToolboxDecoder::decode` stub.

### Steps

1. **Encoder.** Replace the `encode()` stub with a real `VTCompressionSession`:
   - Create the session once at first `encode()` call (or in `new()`).
   - Configure: `kVTCompressionPropertyKey_RealTime = true`, `kVTProfileLevel_H264_Baseline_AutoLevel`, `kVTCompressionPropertyKey_AverageBitRate = bitrate_bps`, `kVTCompressionPropertyKey_MaxKeyFrameInterval = 30` (≈ 1 s at 30 fps), `kVTCompressionPropertyKey_AllowFrameReordering = false`.
   - Wrap the input `VideoFrame.data` (assume NV12 or I420 for now — disclose the format choice) into a `CVPixelBuffer`.
   - Encode via `VTCompressionSessionEncodeFrame`, collect the resulting `CMSampleBuffer` from the callback.
   - Extract NAL units from the sample buffer's `CMBlockBuffer` and convert to Annex-B (add `0x000001` start codes).
   - Return the assembled Annex-B byte vector.
   - On `force_keyframe` flag: pass `kVTEncodeFrameOptionKey_ForceKeyFrame = true` and clear the flag.

2. **Decoder.** Replace the `decode()` stub with a real `VTDecompressionSession`:
   - Parse incoming Annex-B access unit into NAL units.
   - On SPS/PPS NALs, build/refresh `CMFormatDescription`.
   - Wrap remaining NALs into `CMSampleBuffer`.
   - Call `VTDecompressionSessionDecodeFrame`; in the callback, convert the output `CVImageBuffer` back to `VideoFrame.data` (mirror the encoder's pixel format).

3. **Threading.** VideoToolbox callbacks run on internal queues. Use a `crossbeam_channel` (single-producer, single-consumer; already in workspace deps via Quinn) or `std::sync::mpsc` to bridge callback → caller. Keep the encode/decode API synchronous from the caller's perspective.

4. **Test.** Add `crates/wzp-video/tests/encode_decode_macos.rs` (`#[cfg(target_os = "macos")]`):
   - Generate a synthetic 640×360 NV12 frame (gradient pattern).
   - Encode 30 frames at 30 fps.
   - Assert at least one keyframe in the first 5 frames.
   - Pipe the encoded bytes through the depacketizer and decoder.
   - Assert the decoded frame dimensions match input dimensions (pixel-exact match not required given lossy compression).

5. **Acceptance measurement.**
   - Measure encode CPU: run 60 s of 1280×720 @ 30 fps NV12 input on M1, log wall-clock + `getrusage` CPU time.
   - Acceptance: CPU < 5 % of one core on M1 (PRD-video-v1 line).

### Verify

```bash
cargo test -p wzp-video --test encode_decode_macos
cargo test -p wzp-video
cargo clippy -p wzp-video --all-targets -- -D warnings
cargo fmt --all -- --check
# Optional manual measurement (record in report):
cargo run -p wzp-video --release --example bench_encode_720p
```

### Done when
- `cargo test -p wzp-video --test encode_decode_macos` passes on macOS.
- A round-trip (raw frame → encode → packetize → depacketize → decode → frame) produces a frame with matching dimensions.
- CPU measurement at 720p30 documented in the report. If > 5 %, document why (e.g., software fallback path) and propose mitigation.
- Non-macOS targets remain unaffected (the existing `target_os` gates already do this; just don't break them).

### Out of scope
- Android MediaCodec (T4.3).
- NACK (T4.4) / FEC boost (T4.5) / keyframe cache (T4.6) / PLI (T4.7).
- Multi-codec negotiation (T5.4 / T6.1).

---

## T4.3 — MediaCodec H.264 encoder + decoder via JNI (Android)

- **PRD:** `PRD-video-v1.md`
- **Effort:** 5 d
- **Files:**
  - `crates/wzp-video/src/mediacodec.rs`
  - `crates/wzp-android/src/...`

### Context

T4.2 created the `VideoEncoder` / `VideoDecoder` traits and a macOS VideoToolbox implementation. T4.3 adds the Android equivalent using `MediaCodec` via JNI. Because the agent runs on macOS, the MediaCodec implementation is a compile-gated stub; real hardware integration requires an Android device/emulator.

### Steps

1. Create `MediaCodecEncoder` and `MediaCodecDecoder` structs in `wzp-video/src/mediacodec.rs`.
2. Implement `VideoEncoder` / `VideoDecoder` traits for the structs.
3. Gate the module with `#[cfg(target_os = "android")]`; on non-Android targets the module exports placeholder types that return `NotInitialized` errors.
4. Leave JNI surface-texture wiring as a TODO for the Android build environment.

### Verify

```bash
cargo test -p wzp-video mediacodec
cargo build -p wzp-video
```

### Done when

`MediaCodecEncoder` / `MediaCodecDecoder` compile on Android targets and return `Err(NotInitialized)` on non-Android targets.

---

## T4.3.1 — Wire real MediaCodec JNI bridge (Android)

- **Parent:** T4.3 (Approved — scaffold only)
- **PRD:** `PRD-video-v1.md`
- **Effort:** 5 d (gated on Android build environment working)
- **Files:**
  - `crates/wzp-video/src/mediacodec.rs`
  - `crates/wzp-android/src/video/mod.rs` (new — Kotlin/JNI side may live here)
  - `android/app/src/main/java/com/wzp/video/` (new — MediaCodec Kotlin glue if needed)

### Prerequisite
**The `wzp-android` build environment must work first.** Current `liblog` link failure must be resolved. This task is **Blocked** until that prerequisite is fixed; agents should not claim this task until the build env is confirmed working with `build-tauri-android.sh --init`.

### Context
T4.3 shipped the API surface but stubbed both `encode()` and `decode()` even on Android. This task fills in the real JNI MediaCodec wiring. **This is the task that satisfies the original PRD-video-v1 T4.3 acceptance.**

Current TODOs at `crates/wzp-video/src/mediacodec.rs:39` (encoder) and `:91` (decoder).

### Steps

1. **Decide on JNI surface.** Two options — pick one and document:
   - **(A) Direct ndk-sys `AMediaCodec`** (NDK r24+, no Java↔native bouncing). Pure Rust with `ndk-sys` crate dep. Simpler, but requires NDK API ≥ 21.
   - **(B) Java MediaCodec via JNI bridge** (call into Kotlin/Java glue that owns MediaCodec lifecycle). Slower (JNI calls per buffer) but matches existing `wzp-android` pattern.
   - Recommended: **(A)** for the encode/decode hot path, **(B)** only if surface-texture path is required.

2. **Encoder configure.**
   - `AMediaCodec_createEncoderByType("video/avc")`.
   - `AMediaFormat` keys: `KEY_MIME="video/avc"`, `KEY_WIDTH`, `KEY_HEIGHT`, `KEY_BIT_RATE = bitrate_bps`, `KEY_FRAME_RATE = 30`, `KEY_I_FRAME_INTERVAL = 1` (1 s ≈ 30 frames at 30 fps), `KEY_COLOR_FORMAT = COLOR_FormatYUV420Flexible` (or NV12 / I420 — choose and document).
   - `AMediaCodec_configure` with surface=NULL for byte-buffer mode (or attach a surface for the surface-texture path).
   - `AMediaCodec_start`.

3. **Encoder per-frame loop.**
   - `AMediaCodec_dequeueInputBuffer(timeout_us=10_000)`.
   - Copy `VideoFrame.data` (NV12/I420) into input buffer.
   - `AMediaCodec_queueInputBuffer(presentation_us=timestamp_ms*1000, flags=0)`.
   - `AMediaCodec_dequeueOutputBuffer` in a loop — collect Annex-B output. Note: MediaCodec emits AVCC by default; you may need to convert AVCC → Annex-B (replace 4-byte length prefix with `0x000001`) or set `KEY_PREPEND_HEADER_TO_SYNC_FRAMES=1`.
   - Return assembled Annex-B `Vec<u8>`.

4. **Decoder mirror.** Same `AMediaCodec` pattern but `createDecoderByType("video/avc")`, parse SPS/PPS from incoming access unit on first frame to build CSD, feed input, drain output buffer → `VideoFrame`.

5. **Keyframe request.** `AMediaCodec_setParameters` with `PARAMETER_KEY_REQUEST_SYNC_FRAME = 0`.

6. **Test.** New `crates/wzp-video/tests/encode_decode_android.rs` gated `#[cfg(target_os = "android")]`:
   - Run only when invoked from the Android test runner (instrumented test) or via emulator.
   - Synthetic 640×360 NV12 frame; encode 30 frames; assert at least one IDR in first 5; round-trip through depacketizer + decoder.
   - Skip with `#[ignore]` if MediaCodec init fails (e.g., on non-MediaCodec-capable emulator).

7. **Manual Android↔macOS test.** Wire both T4.2.1 (macOS real encoder) and T4.3.1 (Android real encoder) into a CLI test harness. Record latency + CPU on a real Android device and on M1.

### Verify

```bash
# On the Android builder (Hetzner remote):
./scripts/build-tauri-android.sh --init
# Then on the device:
adb shell am instrument -w -e class com.wzp.video.MediaCodecTests com.wzp/com.wzp.video.TestRunner
```

### Done when
- `cargo build -p wzp-video --target aarch64-linux-android` (or via cargo-ndk) succeeds.
- Android↔macOS unidirectional H.264 call works manually (record measurement in report).
- Encode CPU on a mid-tier Android device < 15 % of one core at 720p30 (PRD-video-v1 line).

### Out of scope
- iOS (use T4.2.1's VideoToolbox path).
- Per-receiver simulcast layer selection (T5.5/T5.6).

---

## T4.3.1.1 — Validate Android-target compile + run MediaCodec on device

- **Parent:** T4.3.1 (Approved — Android code present but unverified)
- **PRD:** `PRD-video-v1.md`
- **Effort:** 1–2 d (mostly waiting on Android builder + device access)
- **Files:**
  - `crates/wzp-video/src/mediacodec.rs` (fix any compile errors that surface on the Android target)
  - `crates/wzp-video/tests/encode_decode_android.rs` (new — `#[cfg(target_os = "android")]` instrumented test)
  - `android/app/src/androidTest/java/com/wzp/video/MediaCodecTest.kt` (new — invokes the Rust JNI test entry point)

### Prerequisite
The Android build pipeline must be functional. Use `build-tauri-android.sh --init` per the project memory. Trigger from the Hetzner remote builder (188.245.59.196) if needed.

### Context
T4.3.1 shipped `AMediaCodec`-based Rust code behind `#[cfg(target_os = "android")]` but **the agent could not compile or test it on their macOS host**. The code is structurally similar to T4.2.1's working VideoToolbox code, but plausibility is not verification. This task is the actual verification step that should have been part of T4.3.1.

### Steps

1. **Verify the target build compiles.** From the Hetzner remote or local NDK-equipped machine:
   ```bash
   cargo build -p wzp-video --target aarch64-linux-android
   ```
   Capture full stderr. If anything errors, fix the smallest possible thing in `crates/wzp-video/src/mediacodec.rs` to make it compile (record the diff in the report). Common likely failures:
   - `ndk` crate API differences between version `0.9` and whatever's actually resolvable.
   - Missing imports if `#[cfg]` gates weren't comprehensive.
   - Pixel-format constants that don't exist on the current `ndk` version.

2. **Add the instrumented test.** Create `crates/wzp-video/tests/encode_decode_android.rs`:
   ```rust
   #![cfg(target_os = "android")]
   use wzp_video::{MediaCodecEncoder, MediaCodecDecoder, VideoFrame};

   #[test]
   fn encode_decode_roundtrip_android() {
       // 30 synthetic 640×360 I420 gradient frames → encode → decode → assert dimensions
       // mirror T4.2.1's encode_decode_macos.rs structure
   }
   ```
   Mirror the macOS test in `tests/encode_decode_macos.rs` closely so the two are comparable.

3. **Run the test on a real device.** Connect via `adb`, deploy the test APK (`cargo apk` or via Gradle if `android/` Gradle build is set up), and run:
   ```bash
   adb shell am instrument -w com.wzp.video.test/androidx.test.runner.AndroidJUnitRunner
   ```
   Capture the result.

4. **Measure CPU at 720p30.** Encode 60 s of 1280×720 frames; record `getrusage()` / `top -p <pid>` CPU%. PRD acceptance: < 15 % of one core on a mid-tier Android device.

5. **Manual Android↔macOS interop.** Run the macOS T4.2.1 encoder, send a bitstream over QUIC datagrams (mock if the relay isn't wired yet), decode on Android. Confirm visual round-trip. Record device model + Android version in the report.

### Verify

```bash
# On Android builder:
cargo build -p wzp-video --target aarch64-linux-android
# After APK is on device:
adb shell am instrument -w -e class com.wzp.video.MediaCodecTest com.wzp.video.test/androidx.test.runner.AndroidJUnitRunner
```

### Done when
- `cargo build -p wzp-video --target aarch64-linux-android` succeeds (record any fixes needed in the report).
- Instrumented `encode_decode_roundtrip_android` test passes on at least one real device.
- 720p30 CPU measurement documented. Target < 15 % of one core; if higher, document why and propose mitigation (e.g., surface-input path, format negotiation).
- Manual Android↔macOS interop: visual decode of the same stream on both ends.

### Out of scope
- Surface-texture zero-copy path (defer to a later UX/battery-focused task).
- Decoder pixel-format negotiation (NV12 / NV21 / vendor tiled) — call out in the report which format MediaCodec actually emits on the test device.

---

## T4.4 — `SignalMessage::Nack` variant + RTT-gated NACK loop

- **PRD:** `PRD-video-v1.md`
- **Effort:** 2 d
- **Files:**
  - `crates/wzp-proto/src/packet.rs`
  - `crates/wzp-video/src/nack.rs`

Skeleton — expand before claiming.

---

## T4.5 — I-frame FEC ratio boost

- **PRD:** `PRD-video-v1.md`
- **Effort:** 1 d
- **Files:**
  - `crates/wzp-fec/src/...`
  - `crates/wzp-video/src/...`

Skeleton — expand before claiming.

---

## T4.6 — SFU keyframe cache

- **PRD:** `PRD-video-v1.md`
- **Effort:** 2 d
- **Files:**
  - `crates/wzp-relay/src/room.rs`

Skeleton — expand before claiming.

---

## T4.7 — PLI suppression at SFU

- **PRD:** `PRD-video-v1.md`
- **Effort:** 1 d
- **Files:**
  - `crates/wzp-relay/src/room.rs`

Skeleton — expand before claiming.

---

# Wave 5 — Quality, codecs, simulcast (3 weeks)

Detailed task breakdown deferred. Skeleton:

| Task | Summary |
|---|---|
| T5.1 | `PriorityMode` enum + `SignalMessage::SetPriorityMode` |
| T5.2 | `VideoQualityController` with per-mode allocation gates |
| T5.3 | `EncoderMode::SlideFallback` for ScreenShare |
| T5.4 | H.265 encoder/decoder (reuse framer from T4.1) |
| T5.5 | 3-layer simulcast at sender |
| T5.6 | Per-receiver layer selection at SFU |
| T5.7 | Tier F audio scorer (entropy/IAT/silence-fraction) |
| T5.8 | Tier G response policy (typed Hangup + audit log) |

---

# Wave 6 — AV1 + Tier F video (2-3 weeks)

| Task | Summary |
|---|---|
| T6.1 | AV1 encoder/decoder with HW probe + SVT-AV1 SW fallback |
| T6.2 | Tier F video scorer (keyframe periodicity, I/P ratio, BWE responsiveness) |
| T6.3 | Federated reputation gossip (optional) |

---

# Working agreements

- **One commit per task.** Message: `T<id>: <one-line summary>`.
- **Update PRD on deviation.** If you implement something differently than the PRD specifies, edit the PRD in the same commit explaining why.
- **Don't merge waves out of order** — dependencies are real.
- **Ask before destroying.** Any task that would delete data, drop tables, or force-push: stop and report.
- **Auto-mode caveat.** Even in auto mode, if a task description doesn't fit what you find in the code, stop and surface the mismatch before guessing.

---

# Status board

Edit this table directly when you claim, complete, or get blocked on a task. Keep it sorted by task ID. The reviewer (human) is the only one who flips `Pending Review` → `Approved` or `Changes Requested`.

Statuses (in order of progression):
- `Open` — not yet picked up
- `In Progress` — an agent is working on it
- `Blocked` — agent has hit something it can't resolve; see report
- `Pending Review` — agent has finished, report filed, awaiting human
- `Changes Requested` — reviewer pushed back; back to agent
- `Approved` — reviewer signed off; task is closed
- `Skipped` — made redundant by another task or scoped out
- `Deferred (reviewer-owned)` — agent does not have the environment / access to complete this; the reviewer (human) will pick it up later. **Agents must not claim Deferred tasks.** Move on to the next claimable one.

| Task | Status | Agent | Started (UTC) | Completed (UTC) | Report | Reviewer notes |
|---|---|---|---|---|---|---|
| T1.1 | Approved | Kimi Code CLI | 2026-05-11T06:09Z | 2026-05-11T06:54Z | [report](reports/T1.1-report.md) | Approved 2026-05-11. Spawned T1.1.1 (field rustdoc) and T1.1.2 (refresh stale test-count). |
| T1.1.1 | Approved | Kimi Code CLI | 2026-05-11T07:17Z | 2026-05-11T07:22Z | [report](reports/T1.1.1-report.md) | Approved after rework. Both Verify commands clean. |
| T1.1.2 | Approved | Kimi Code CLI | 2026-05-11T07:19Z | 2026-05-11T07:25Z | [report](reports/T1.1.2-report.md) | Approved after rework. Broader grep clean; remaining matches are self-refs in task spec + frozen historical note. |
| T1.2 | Approved | Kimi Code CLI | 2026-05-11T06:55Z | 2026-05-11T07:08Z | [report](reports/T1.2-report.md) | Approved 2026-05-11. Spawned T1.2.1 (rustdoc on MediaType variants/methods). Agent also resolved the T1.2 TODO inside MediaHeaderV2 — good call. |
| T1.2.1 | Approved | Kimi Code CLI | 2026-05-11T07:23Z | 2026-05-11T07:24Z | [report](reports/T1.2.1-report.md) | Approved. Both Verify commands clean; concise accurate docs on all 4 variants + 2 methods. |
| T1.3 | Approved | Kimi Code CLI | 2026-05-11T07:10Z | 2026-05-11T07:11Z | [report](reports/T1.3-report.md) | Approved 2026-05-11. No follow-ups; docs-and-test-only change. |
| T1.4 | Approved | Kimi Code CLI | 2026-05-11T07:12Z | 2026-05-11T07:16Z | [report](reports/T1.4-report.md) | Approved 2026-05-11. Spawned T1.4.1 (rustdoc on v2 mini types). The two-step expand test catches the W4 desync scenario nicely. |
| T1.4.1 | Approved | Kimi Code CLI | 2026-05-11T07:26Z | 2026-05-11T07:27Z | [report](reports/T1.4.1-report.md) | Approved. Closes rustdoc trilogy (T1.1.1/T1.2.1/T1.4.1). |
| T1.5 | Approved | Kimi Code CLI | 2026-05-11T07:28Z | 2026-05-11T10:09Z | [report](reports/T1.5-report.md) | Approved with follow-ups. Migration correct; scope creep (120 files) and workspace clippy skipped — spawned T1.5.1 (encode_compact unwrap) and T1.5.2 (clippy hygiene). |
| T1.5.1 | Approved | Kimi Code CLI | 2026-05-11T10:09Z | 2026-05-11T10:15Z | [report](reports/T1.5.1-report.md) | Approved. unwrap replaced with `if let Some(base)`; fallback test passes. Cargo.lock churn is legit dep updates. |
| T1.5.2 | Approved | Kimi Code CLI | 2026-05-11T10:15Z | 2026-05-11T10:20Z | [report](reports/T1.5.2-report.md) | Approved. PROTOCOL-AUDIT.md known-debt section present; standard #3 amended; report template updated. |
| T1.6 | Approved | Kimi Code CLI | 2026-05-11T10:20Z | 2026-05-11T11:05Z | [report](reports/T1.6-report.md) | Approved. Clean impl, both sides tested, T1.5 gap-fixes folded in with explicit disclosure — good course-correction from the T1.5 scope-creep review. |
| T1.7 | Approved | Kimi Code CLI | 2026-05-11T11:05Z | 2026-05-11T16:29Z | [report](reports/T1.7-report.md) | Approved. W5 invariant already encoded in `to_bytes()` order; regression test pins it. Guards future encryption wiring. |
| T1.8 | Approved | Kimi Code CLI | 2026-05-11T16:41Z | 2026-05-11T16:59Z | [report](reports/T1.8-report.md) | Approved. Per-stream/per-MediaType windows; AEAD-first then anti-replay; plaintext rollback on detection. W11 resolved. |
| T2.1 | Approved | Kimi Code CLI | 2026-05-11T17:00Z | 2026-05-11T17:06Z | [report](reports/T2.1-report.md) | Approved retroactively. Commit fe1f948 landed; closed by reviewer. |
| T2.2 | Approved | Kimi Code CLI | 2026-05-11T17:05Z | 2026-05-11T17:16Z | [report](reports/T2.2-report.md) | Approved. Substance solid; rule #7 violated. Last lenient pass. |
| T2.3 | Approved | Kimi Code CLI | 2026-05-11T17:13Z | 2026-05-11T17:20Z | [report](reports/T2.3-report.md) | Substance good (BWE guard); 4 process violations bundled with T2.4-T2.6 in single commit 54c1a35 — see T2.6 report for consolidated notes. |
| T2.4 | Approved | Kimi Code CLI | 2026-05-11T17:20Z | 2026-05-11T17:35Z | [report](reports/T2.4-report.md) | Substance good (Tier A); bundled in 54c1a35 — see T2.6 report. |
| T2.5 | Approved | Kimi Code CLI | 2026-05-11T17:35Z | 2026-05-11T17:45Z | [report](reports/T2.5-report.md) | Substance good (Tier B+C); bundled in 54c1a35 — see T2.6 report. |
| T2.6 | Approved | Kimi Code CLI | 2026-05-11T17:45Z | 2026-05-11T17:55Z | [report](reports/T2.6-report.md) | Substance good (Prom metrics); bundled in 54c1a35. Consolidated reviewer notes here. |
| T3.1 | Approved | Kimi Code CLI | 2026-05-11T20:55Z | 2026-05-11T21:05Z | [report](reports/T3.1-report.md) | Approved. DashMap<String, Arc<RwLock<Room>>>; W13 resolved. One commit per task this time — good. Two minor process notes in report. |
| T3.2 | Approved | Kimi Code CLI | 2026-05-11T21:15Z | 2026-05-11T21:25Z | [report](reports/T3.2-report.md) | Approved. timestamp_ms monotonic across rekey, documented + tested. Commit `1b4f7b0`. |
| T3.3 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T06:08Z | [report](reports/T3.3-report.md) | Approved. W12 SignalMessage versioning. Commit `f7f413e`. |
| T3.4 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T06:24Z | [report](reports/T3.4-report.md) | Approved. Tier D payload-size EWMA + per-codec bound table. Commit `017c371`. Clean process. |
| T3.5 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T02:46Z | [report](reports/T3.5-report.md) | Approved. Tier E TokenBucket (256 kbps/1.92 MB burst), observe-only. Commit `f1b86e0`. Wave 3 complete. |
| T4.1 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T07:22Z | [report](reports/T4.1-report.md) | Approved. wzp-video crate + H.264 NAL framer/depacketizer (RFC 6184 FU-A). Commit `490d2d3`. Wave 4 opened. |
| T4.2 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T05:10Z | [report](reports/T4.2-report.md) | Approved as scaffold (API surface + `is_keyframe`). Original PRD acceptance moved to T4.2.1 — `encode`/`decode` are stubs. Process note in report. Commit `3356ba9`. |
| T4.2.1 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T05:52Z | [report](reports/T4.2.1-report.md) | Approved. First real H.264 encoder/decoder via `shiguredo_video_toolbox`. 30-frame round-trip test passes. MSRV bump to 1.88 on macOS. CPU bench TODO. Commit `410c2a4`. |
| T4.3 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T05:15Z | [report](reports/T4.3-report.md) | Approved as scaffold. JNI MediaCodec deferred to T4.3.1. Same stub-and-rename pattern as T4.2 — process note in report. Commit `e177e63`. |
| T4.3.1 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T06:04Z | [report](reports/T4.3.1-report.md) | Approved (partial). liblog fix real; AMediaCodec code present but unverified on Android target. Spawned T4.3.1.1 to do the actual validation. Commit `397f9d2`. |
| T4.3.1.1 | Deferred (reviewer-owned) | — | — | — | — | Requires Android build pipeline + physical device. Agent does not have access. Reviewer will run on the Hetzner Android builder once Wave 4/5 land. Do NOT claim. |
| T4.4 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T05:25Z | [report](reports/T4.4-report.md) | Approved. Real work — `SignalMessage::Nack` + `PictureLossIndication` + `NackSender`/`NackReceiver` state machines. 12 new tests. Commit `81042ac`. |
| T4.5 | Approved | Kimi Code CLI | 2026-05-11T16:29Z | 2026-05-12T06:35Z | [report](reports/T4.5-report.md) | Approved. Keyframe-aware FEC ratio boost (default 0.5) via trait default + `AdaptiveFec` wiring. 3 new tests. Commit `4e174fe`. |
| T4.6 | Pending Review | Kimi Code CLI | 2026-05-12T16:29Z | 2026-05-12T16:40Z | [report](reports/T4.6-report.md) | SFU keyframe cache per (room, sender, stream). Replayed to new joiners before live traffic. |
| T4.7 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.1 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.2 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.3 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.4 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.5 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.6 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.7 | Open | — | — | — | — | Skeleton — expand before claiming |
| T5.8 | Open | — | — | — | — | Skeleton — expand before claiming |
| T6.1 | Open | — | — | — | — | Skeleton — expand before claiming |
| T6.2 | Open | — | — | — | — | Skeleton — expand before claiming |
| T6.3 | Open | — | — | — | — | Skeleton — expand before claiming |

## Review queue (human)

Items currently waiting on the reviewer:

- T1.8 — Per-stream anti-replay window with configurable size — report: reports/T1.8-report.md
- T2.1 — Add `SignalMessage::TransportFeedback` — report: reports/T2.1-report.md
- T2.2 — `BandwidthEstimator` in `wzp-proto::bandwidth` — report: reports/T2.2-report.md
- T3.2 — Document timestamp_ms monotonic across rekey — report: reports/T3.2-report.md
- T3.3 — SignalMessage version field — report: reports/T3.3-report.md
- T3.4 — Tier D per-codec payload size sanity — report: reports/T3.4-report.md
- T3.5 — Tier E per-session token bucket — report: reports/T3.5-report.md
- T4.1 — wzp-video crate scaffold + H.264 NAL framer + depacketizer — report: reports/T4.1-report.md
- T4.2 — VideoToolbox H.264 encoder/decoder traits (macOS, MVP) — report: reports/T4.2-report.md

Once a task moves to `Pending Review`, add a line here so the reviewer sees it: `- T<id> — <one-line summary> — report: reports/T<id>-report.md`. The reviewer removes the line when they mark it `Approved` (or moves it back to the agent on `Changes Requested`).
