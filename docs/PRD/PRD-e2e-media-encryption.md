# PRD: E2E Media Encryption (rewrite)

> **Status:** proposed (supersedes prior version)
> **Resolves:** Real end-to-end media encryption between call participants.
> **Replaces:** The prior version of this PRD described wrapping `QuinnTransport` in `EncryptingTransport` using the pairwise client↔relay session. That approach was implemented (commit `52a6f5e`) and **broke voice between any two clients** because the relay does not decrypt+re-encrypt — see "Why the prior fix failed" below. The wrapping was reverted in commit `e8cab25`.

---

## Why the prior fix failed

`wzp_client::handshake::perform_handshake` performs ECDH **between the client and the relay**. Each client in a room ends up with a **different** pairwise session key (key_A for client A, key_B for client B, etc.).

The relay is an SFU — it forwards `MediaPacket` bytes between participants in a room without inspecting their payloads. The relay does not run a decrypt-then-encrypt step keyed per-recipient.

Wrapping `QuinnTransport` in `EncryptingTransport` therefore produced:

```
Client A:  plaintext --[encrypt key_A]--> ciphertext --> Relay
Relay:     forwards ciphertext (bytes) --> Client B
Client B:  ciphertext --[decrypt key_B]--> garbage --> silent audio
```

Result: every recipient saw decryption failures, audio went silent.

This is **not a bug in `EncryptingTransport`** — the wrapper does exactly what it claims. The bug was thinking the pairwise client-relay session was usable for participant-to-participant media. It isn't.

## Goals

A future implementation must satisfy:

- Two clients in a room can exchange media that the **other client** can decrypt.
- The **relay cannot decrypt** any media payload (true E2E), OR alternatively, the relay can decrypt+re-encrypt per recipient (hop-by-hop, sometimes called SFU-trusted).
- Joining and leaving the room mid-call rotates keys so departed members can't decrypt subsequent traffic (forward secrecy on membership change).
- Compatible with the existing `MediaPacket` wire format (header in plaintext, payload encrypted).

## Two valid approaches

### Approach A — MLS group keys (true E2E)

Use the [MLS protocol](https://datatracker.ietf.org/doc/rfc9420/) (e.g. via the `openmls` crate) to derive a shared **group key** that all room members possess and the relay does not.

- Relay acts as a **delivery service** for MLS Handshake messages (`Welcome`, `Commit`, `Proposal`) but never sees the group secret.
- Every media packet is AEAD-sealed with the current group epoch key.
- Group rekey is triggered by:
  - Member join/leave (forward secrecy on membership)
  - Periodic (every N seconds or N packets) for post-compromise security
- Each room maintains its own MLS group; the relay just stores opaque `mls_blob` payloads in `SignalMessage::MlsHandshake`.

**Pros:** real E2E. Relay compromise does not leak media.
**Cons:** Significant complexity (MLS state machine per room, persistent ratchet trees, key schedule). Adds `openmls` dependency (~30 KLOC). Federation across relays is harder.

### Approach B — Hop-by-hop re-encryption at the relay

The relay holds a `CryptoSession` per connected client (which it already does — see `_crypto_session` discarded in `crates/wzp-relay/src/main.rs:1817`). On forward:

```
Relay.recv_media(from A):    decrypt with key_A → plaintext
Relay.send_media(to B, C, D): for each recipient X, encrypt with key_X
```

This is the same model as Matrix Megolm-without-Megolm — encrypted hop-by-hop but the relay sees plaintext briefly in between.

**Pros:** Reuses existing per-client `ChaChaSession`. Implementation is ~100 lines in the relay's room forwarding loop. Federation works the same way (each relay-relay hop has its own session).
**Cons:** Relay sees plaintext. A compromised relay can record and decrypt all media. This is **not E2E** — but it is strictly stronger than the current state (plaintext-over-QUIC-TLS exposes media to anyone with a TLS-terminating proxy on the relay).

## Recommendation

**Ship Approach B first.** It's a small, well-scoped change that closes the relay-operator-can-see-plaintext-in-RAM gap without requiring an MLS rewrite. Then layer Approach A on top when the threat model demands relay-untrusted operation.

## Out of scope for this PRD

- Federation gossip key exchange (separate PRD)
- SAS (Short Authentication String) verification UX (separate PRD)
- Rekey on session compromise (handled by the chosen approach's group/pairwise rekey)

## Acceptance criteria (Approach B, first iteration)

1. Relay's room forwarding loop (`crates/wzp-relay/src/room.rs:354` and `:1353`) calls `sender_session.decrypt()` then `recipient_session.encrypt()` per recipient before `send_media`.
2. Each `RoomMember` holds its `Box<dyn CryptoSession>` (currently discarded as `_crypto_session` in `main.rs:1817`).
3. Client-side: re-add the `EncryptingTransport` wrapping in `desktop/src-tauri/src/engine.rs` (the two sites reverted in `e8cab25`).
4. Integration test: two-client mock room exchanges media; verify each recipient gets the sender's plaintext back after the relay double-hop.
5. Existing 825 tests still pass.

## Verification

`cargo test -p wzp-relay --test multi_client_relay_path` should pass with two simulated clients sending audio in both directions and decrypting each other's frames.

## Files to touch

- `crates/wzp-relay/src/main.rs` — keep `crypto_session` per-client (drop the `_` prefix)
- `crates/wzp-relay/src/room.rs` — add decrypt/re-encrypt to forward path
- `crates/wzp-relay/src/session_mgr.rs` — store sessions keyed by peer
- `desktop/src-tauri/src/engine.rs` — restore `EncryptingTransport` wrapping (~2 sites)
- `crates/wzp-relay/tests/multi_client_relay_path.rs` — new integration test

## Risk / rollback

If multi-client tests fail in CI, the change is contained to the relay forwarding loop and one engine.rs edit — straightforward revert.
