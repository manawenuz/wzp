# PRD: E2E Media Encryption — Wire EncryptingTransport on Relay Path

> **Status:** proposed
> **Resolves:** Security gap — relay-path media travels in QUIC TLS only; WZP application-layer ChaCha20-Poly1305 is negotiated but never applied.
> **Depends on:** `wzp_client::encrypted_transport::EncryptingTransport` (already implemented).

## Problem

`CallEngine::start` (both the Android path and the desktop path) calls
`wzp_client::handshake::perform_handshake`, which returns a `HandshakeResult`
containing a `session: Box<dyn CryptoSession>` (a keyed `ChaChaSession`).
Both call sites discard the session — only `hs.video_codec` is retained.

All subsequent `send_media` / `recv_media` calls go directly through
`Arc<wzp_transport::QuinnTransport>`, which provides QUIC TLS (relay sees
plaintext application data after TLS termination at the relay). The WZP
application-level AEAD — ChaCha20-Poly1305, keyed per-call, relay-never-sees
— is never applied.

`wzp_client::encrypted_transport::EncryptingTransport` exists
(`crates/wzp-client/src/encrypted_transport.rs`) and is fully tested.
It wraps any `Arc<dyn MediaTransport>` and intercepts every `send_media` /
`recv_media` call with `session.encrypt()` / `session.decrypt()`.

## Goals

- The relay-path `HandshakeResult::session` is used to construct an
  `EncryptingTransport` that wraps the raw `QuinnTransport`.
- All `send_media` and `recv_media` calls in the relay path go through the
  wrapper, not the raw transport.
- The direct P2P path (`is_direct_p2p == true`) is left unchanged — QUIC TLS
  is the encryption layer there.
- `cargo check --manifest-path desktop/src-tauri/Cargo.toml` passes.
- A `#[cfg(test)]` test verifies that the relay path uses `EncryptingTransport`.

## Non-goals

- Rekeying (`SignalMessage::Rekey`) — tracked separately.
- Video transport encryption (same mechanism; apply after audio is confirmed working).
- Changes to the P2P path, the relay binary, or any crate outside `desktop/src-tauri`.

## Design

### `EncryptingTransport` API (read `crates/wzp-client/src/encrypted_transport.rs`)

```rust
pub struct EncryptingTransport { ... }

impl EncryptingTransport {
    pub fn new(inner: Arc<dyn MediaTransport>, session: Box<dyn CryptoSession>) -> Self;
}

// Implements MediaTransport:
//   send_media  → session.encrypt(header_bytes, payload) → inner.send_media
//   recv_media  → inner.recv_media → session.decrypt(header_bytes, ciphertext)
//   send_signal / recv_signal / path_quality / close → forwarded unchanged
```

`EncryptingTransport` is NOT `Arc`-wrapped by the constructor; wrap it in
`Arc::new(...)` when storing as `Arc<dyn MediaTransport>`.

### Two call sites in `desktop/src-tauri/src/engine.rs`

**Call site 1 — Android path** (`CallEngine::start` around line 575):

```rust
if !is_direct_p2p {
    let _hs = match wzp_client::handshake::perform_handshake(...).await { Ok(hs) => hs, ... };
    // hs.session is discarded here — fix this
}
```

Change: capture `hs`, then build a wrapped transport:

```rust
if !is_direct_p2p {
    let hs = match wzp_client::handshake::perform_handshake(...).await { Ok(hs) => hs, ... };
    info!(video_codec = ?hs.video_codec, "handshake complete");
    let transport: Arc<dyn wzp_proto::MediaTransport> =
        Arc::new(wzp_client::encrypted_transport::EncryptingTransport::new(
            transport.clone(),
            hs.session,
        ));
    // use `transport` (the wrapped version) for all subsequent send_t / recv_t clones
}
```

The variable `transport` must shadow the raw `Arc<QuinnTransport>` so that
every subsequent clone of `transport` picks up the encrypted wrapper.

**Call site 2 — Desktop path** (`CallEngine::start` around line 1551):

```rust
let _negotiated_video_codec = if !is_direct_p2p {
    let hs = wzp_client::handshake::perform_handshake(...).await?;
    info!(video_codec = ?hs.video_codec, "handshake complete");
    hs.video_codec   // session dropped here — fix this
} else { None };
```

Change: extract `session` before returning `video_codec`, then shadow
`transport` with the wrapped version. Because `transport` is used after this
block (cloned into `send_t`, `recv_t`, etc.), the shadow must happen inside
the same scope or immediately after:

```rust
let (_negotiated_video_codec, transport): (_, Arc<dyn wzp_proto::MediaTransport>) =
    if !is_direct_p2p {
        let hs = wzp_client::handshake::perform_handshake(...).await?;
        info!(video_codec = ?hs.video_codec, "handshake complete");
        let enc = Arc::new(wzp_client::encrypted_transport::EncryptingTransport::new(
            transport.clone(),
            hs.session,
        ));
        (hs.video_codec, enc)
    } else {
        info!("direct P2P — skipping relay handshake");
        (None, transport.clone())
    };
```

All subsequent `transport.clone()` calls then operate on the encrypted wrapper.

### Import

Add to the top of `engine.rs` if not already present:

```rust
use wzp_client::encrypted_transport::EncryptingTransport;
```

Or use the fully-qualified path inline (already shown above).

### Type compatibility

- `EncryptingTransport` implements `wzp_proto::MediaTransport` (confirmed in the source).
- The existing `send_t` / `recv_t` variables are already typed as
  `Arc<dyn MediaTransport>` (or coerced on first use) — the shadow is a
  drop-in replacement.
- The `vid_transport` for the video path (`line ~2090`) is also cloned from
  `transport`; it will automatically use the encrypted wrapper if the shadow
  is placed before those clones.

## Implementation steps

1. Read `desktop/src-tauri/src/engine.rs` lines 570–620 (Android path) and
   1547–1570 (desktop path) to see the exact variable names in each branch.
2. **Android path fix** (line ~585): rename `_hs` to `hs`, extract
   `hs.session`, wrap `transport` with `EncryptingTransport::new`, re-bind
   `transport` as `Arc<dyn MediaTransport>`.
3. **Desktop path fix** (line ~1551): restructure the
   `if !is_direct_p2p` block to return `(video_codec, wrapped_transport)`
   and shadow `transport`.
4. Confirm that `vid_transport` (line ~2090) is cloned after the shadow — if
   it is, no further changes are needed for video.
5. Run `cargo check --manifest-path desktop/src-tauri/Cargo.toml`. Fix any
   type-mismatch errors (usually a missing `as Arc<dyn MediaTransport>` cast
   or a moved value).
6. Add a `#[cfg(test)]` module to `engine.rs` (or to a new
   `engine_tests.rs` included via `#[cfg(test)] mod engine_tests`) with a
   test that constructs a `LoopbackTransport`, calls `perform_handshake`
   against a mock relay fixture, and verifies that a received payload is
   decrypted before returning from `recv_media`. A simpler alternative that
   avoids a full handshake: assert `is::<EncryptingTransport>()` on the
   `transport` variable at the test call site using `std::any::Any`.

## Files to read before implementing

- `desktop/src-tauri/src/engine.rs` lines 475–625 (Android path) and
  1480–1570 (desktop path)
- `crates/wzp-client/src/encrypted_transport.rs` (full — for the exact
  constructor signature and trait impl)
- `crates/wzp-client/src/handshake.rs` (for `HandshakeResult` struct
  definition — confirm the `session` field name and type)

## Verify

```bash
cargo check --manifest-path desktop/src-tauri/Cargo.toml
```

Expected: 0 errors.

Manual smoke check: both `perform_handshake` call sites in `engine.rs` must
use `hs.session` (grep: `hs\.session` should appear twice, once per call site).
The string `_hs` must not remain on the relay path (only on the `_hs =` binding if the variable is intentionally unused before wrapping).

## Done when

- `cargo check --manifest-path desktop/src-tauri/Cargo.toml` exits 0.
- Both relay-path `perform_handshake` call sites build an `EncryptingTransport`
  from `hs.session`.
- The direct-P2P branch (`is_direct_p2p == true`) is unchanged.
- A `#[cfg(test)]` test in `engine.rs` verifies that `EncryptingTransport`
  is used on the relay path (construction proof or decrypt round-trip).
