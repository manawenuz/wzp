# PRD: Quality Upgrade Flow — UpgradeProposal / Response / Confirm

> **Status:** proposed
> **Resolves:** Four TODO comments in the signal task of `desktop/src-tauri/src/lib.rs` that leave quality upgrade messages unhandled. Audio quality never upgrades mid-call even when the network improves.
> **Depends on:** `wzp_proto::SignalMessage::{UpgradeProposal, UpgradeResponse, UpgradeConfirm, QualityCapability}` (already defined in `crates/wzp-proto/src/packet.rs`).

## Problem

The signal receive task in `lib.rs` matches `UpgradeProposal`, `UpgradeResponse`,
`UpgradeConfirm`, and `QualityCapability` messages from the peer, logs them,
then hits a `// TODO` comment and does nothing. The 4 TODOs are at lines
1930, 1949, 1966, and 1985 of `desktop/src-tauri/src/lib.rs`.

Consequence: audio quality is frozen at the profile negotiated at call start.
Even when the network improves, the encoder never upgrades.

## Goals

1. `UpgradeProposal` auto-accepts and sends `UpgradeResponse { accepted: true }`.
2. Accepted `UpgradeResponse` sends `UpgradeConfirm` and switches the local encoder.
3. Received `UpgradeConfirm` switches the local encoder.
4. Received `QualityCapability` caps the local encoder to the peer's max profile.
5. A unit test verifies the accept/confirm round-trip.
6. `cargo check --manifest-path desktop/src-tauri/Cargo.toml` passes.

## Non-goals

- UI for manual accept/reject of upgrade proposals (auto-accept only).
- Sending `UpgradeProposal` from our side (the outgoing path already exists in
  `lib.rs`; this PRD only handles receiving).
- Downgrade negotiation.
- Persisting quality profiles across calls.

## Design

### New shared state

Add the following to `AppState` (or as captured variables in the signal task
closure — whichever is cleaner given the existing structure):

```rust
/// Pending outgoing upgrade: (call_id, proposal_id, profile).
/// Set when we send an UpgradeProposal, consumed when we receive an accepted UpgradeResponse.
pending_upgrade: Arc<Mutex<Option<(String, String, QualityProfile)>>>,

/// Current quality profile for the encoder. The audio send task reads this
/// at the start of each encode cycle.
active_quality: Arc<Mutex<QualityProfile>>,

/// Peer's reported maximum quality cap. The audio send task clamps to min(active, peer_max).
peer_max_quality: Arc<Mutex<Option<QualityProfile>>>,
```

If `AppState` already holds these fields (check `lib.rs` for the struct
definition), reuse them instead of adding duplicates.

### Handler implementations

#### 1. `UpgradeProposal` (line ~1930)

```rust
// Replace the TODO comment with:
let response = SignalMessage::UpgradeResponse {
    version: wzp_proto::default_signal_version(),
    call_id: call_id.clone(),
    proposal_id: proposal_id.clone(),
    accepted: true,
    reason: None,
};
if let Err(e) = signal_transport.send_signal(&response).await {
    tracing::warn!("failed to send UpgradeResponse: {e}");
}
```

`signal_transport` is whatever variable holds the signal `Arc<dyn MediaTransport>`
in scope at that match arm. Inspect the enclosing task to find the right name.

#### 2. `UpgradeResponse` (line ~1949)

```rust
// Replace the TODO comment with:
if accepted {
    // Retrieve the pending proposal to get the confirmed_profile.
    let maybe_proposal = pending_upgrade.lock().unwrap().take();
    if let Some((_cid, pid, profile)) = maybe_proposal {
        if pid == proposal_id {
            // Send UpgradeConfirm.
            let confirm = SignalMessage::UpgradeConfirm {
                version: wzp_proto::default_signal_version(),
                call_id: call_id.clone(),
                proposal_id: proposal_id.clone(),
                confirmed_profile: profile.clone(),
            };
            if let Err(e) = signal_transport.send_signal(&confirm).await {
                tracing::warn!("failed to send UpgradeConfirm: {e}");
            }
            // Switch our encoder.
            *active_quality.lock().unwrap() = profile;
        }
    }
}
```

If `pending_upgrade` is a captured `Arc<Mutex<...>>` in the task closure, it
can be read/written without going through `AppState`.

#### 3. `UpgradeConfirm` (line ~1966)

```rust
// Replace the TODO comment with:
*active_quality.lock().unwrap() = confirmed_profile;
```

The audio send task (in `engine.rs`) reads `active_quality` at the start of
each encode cycle and reconfigures the Opus encoder bitrate accordingly.

#### 4. `QualityCapability` (line ~1985)

```rust
// Replace the TODO comment with:
*peer_max_quality.lock().unwrap() = Some(max_profile);
```

#### 5. Audio send task changes (`engine.rs`)

The audio send task already runs in a loop. Add a quality-check at the top of
each encode iteration:

```rust
// At the start of the encode loop body:
let effective_profile = {
    let active = active_quality.lock().unwrap().clone();
    let peer_cap = peer_max_quality.lock().unwrap().clone();
    match peer_cap {
        Some(cap) if cap.opus_bitrate_bps() < active.opus_bitrate_bps() => cap,
        _ => active,
    }
};
// Pass effective_profile to encoder if it changed since last iteration.
```

`QualityProfile::opus_bitrate_bps()` already exists (check
`crates/wzp-proto/src/codec_id.rs`). If `QualityProfile` does not have a
direct bitrate accessor, compare using the `PartialOrd` impl or a helper that
ranks profiles numerically.

To avoid calling `encoder.set_bitrate()` every single frame, cache the last
applied profile and only reconfigure on change:

```rust
let mut last_applied_profile: Option<QualityProfile> = None;

// Inside loop:
if Some(&effective_profile) != last_applied_profile.as_ref() {
    encoder.set_bitrate(effective_profile.opus_bitrate_bps());
    last_applied_profile = Some(effective_profile.clone());
}
```

`encoder.set_bitrate(bps: u32)` — add this method to `OpusEncoder` in
`crates/wzp-codec/src/opus_enc.rs` if it does not exist. It wraps
`opus_encoder_ctl(OPUS_SET_BITRATE_REQUEST, bps)`.

### Unit tests

Add a `#[cfg(test)]` module in `lib.rs` (or a dedicated test file) that:

1. Creates a `LoopbackSignalTransport` stub that records sent `SignalMessage`s.
2. Calls the `UpgradeProposal` handler logic directly, asserts that an
   `UpgradeResponse { accepted: true }` was sent.
3. Calls the `UpgradeResponse { accepted: true }` handler with a pre-populated
   `pending_upgrade`, asserts that `UpgradeConfirm` was sent and
   `active_quality` was updated.

These can be pure unit tests (no Tauri or audio), since the handlers are
pure async functions over captured state.

## Implementation steps

1. Read `desktop/src-tauri/src/lib.rs` lines 1910–1990 (the four TODO blocks)
   and the surrounding signal task structure to identify the variable names
   for `signal_transport`, `app_state`, and any existing quality-state fields.
2. Read `desktop/src-tauri/src/engine.rs` for `CallEngine` struct fields and
   the audio send task loop.
3. Read `crates/wzp-proto/src/codec_id.rs` for `QualityProfile` methods.
4. Add `pending_upgrade`, `active_quality`, `peer_max_quality` to the
   appropriate shared state (or as closure captures in the signal task).
5. Replace the 4 TODO comments with the handlers described above.
6. Add `set_bitrate` to `OpusEncoder` if missing.
7. Update the audio send task to read `active_quality` / `peer_max_quality`
   each iteration.
8. Add unit tests.
9. Run `cargo check --manifest-path desktop/src-tauri/Cargo.toml`.

## Files to read before implementing

- `desktop/src-tauri/src/lib.rs` — grep for `UpgradeProposal` to find the
  exact lines; also read the surrounding signal task for variable names.
- `crates/wzp-proto/src/packet.rs` lines 1130–1190 — `UpgradeProposal`,
  `UpgradeResponse`, `UpgradeConfirm`, `QualityCapability` struct layouts.
- `desktop/src-tauri/src/engine.rs` — `CallEngine` struct fields, audio
  send task loop.
- `crates/wzp-proto/src/codec_id.rs` — `QualityProfile` methods.
- `crates/wzp-codec/src/opus_enc.rs` — `OpusEncoder` API.

## Verify

```bash
cargo check --manifest-path desktop/src-tauri/Cargo.toml
cargo test -p wzp-desktop 2>/dev/null || cargo test --manifest-path desktop/src-tauri/Cargo.toml
```

Expected: 0 errors; unit tests pass.

## Done when

- All 4 TODO comments replaced with real logic.
- `cargo check --manifest-path desktop/src-tauri/Cargo.toml` exits 0.
- Unit test verifies: `UpgradeProposal` → `UpgradeResponse { accepted: true }` sent;
  `UpgradeResponse { accepted: true }` → `UpgradeConfirm` sent + `active_quality` updated.
