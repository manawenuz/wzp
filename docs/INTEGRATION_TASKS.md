# WZP Integration Tasks

Based on featherChat commit 65f6390 — FUTURE_TASKS.md with WZP integration items.

## Status Key
- DONE = implemented and tested
- PARTIAL = code exists but not wired into live path
- TODO = not started

---

## WZP-Side Tasks (our responsibility)

### WZP-S-1. HKDF Salt/Info String Alignment — DONE
- Both use `None` salt, info strings `warzone-ed25519` / `warzone-x25519`
- 15 cross-project tests verify identical output

### WZP-S-2. Accept featherChat Bearer Token on Relay — DONE
- `--auth-url` flag on relay
- Clients send `SignalMessage::AuthToken` as first signal
- Relay calls `POST {auth_url}` to validate, rejects if invalid
- Commit: `ad16ddb`

### WZP-S-3. Signaling Bridge Mode — DONE
- `featherchat.rs` module: encode/decode WZP SignalMessage into FC CallSignal.payload
- `WzpCallPayload` wraps signal + relay_addr + room
- Commit: `ad16ddb`

### WZP-S-4. Room Access Control — DONE
- `hash_room_name()` in wzp-crypto: SHA-256("featherchat-group:" + name)[:16] → 32 hex chars
- CLI `--room <name>` hashes before using as SNI
- Web bridge hashes room name before connecting to relay
- RoomManager gains ACL: `with_acl()`, `allow()`, `is_authorized()`
- `join()` now returns `Result<ParticipantId, String>`, rejects unauthorized
- Relay passes authenticated fingerprint to room join

### WZP-S-5. Wire Crypto Handshake into Live Path — DONE
- CLI: `perform_handshake()` called after connect, before any media mode
- Relay: `accept_handshake()` called after auth, before room join
- Web bridge: `perform_handshake()` called after auth token, before audio loops
- Relay generates ephemeral identity seed at startup, logs fingerprint
- Quality profile negotiated during handshake

### WZP-S-6. Web Bridge + featherChat Web Client — DONE
- `--auth-url` flag on web bridge
- Browser sends `{ "type": "auth", "token": "..." }` as first WS message
- Web bridge validates token against featherChat, then passes to relay
- `--cert`/`--key` flags for production TLS certificates

### WZP-S-7. Publish wzp-proto for featherChat — DONE
- `wzp-proto/Cargo.toml` now standalone (no workspace inheritance)
- featherChat can use: `wzp-proto = { git = "ssh://...", path = "crates/wzp-proto" }`

### WZP-S-8. CLI Seed Input — DONE
- `--seed <hex>` and `--mnemonic <24 words>` flags
- featherChat-compatible identity: same seed → same keys
- Commit: `12cdfe6`

### WZP-S-9. Fix Hardcoded Assumptions — DONE
1. No auth on relay — ✅ fixed via S-2 (`--auth-url`)
2. Room names from SNI — ✅ fixed via S-4 (hashed room names)
3. No signaling before media — ✅ fixed via S-5 (mandatory handshake)
4. Self-signed TLS — ✅ fixed via S-6 (`--cert`/`--key` for production)
5. No codec negotiation in web bridge — ✅ profile negotiated in handshake
6. No connection to FC key registry — ✅ fixed via S-2 (token validation)

---

## featherChat-Side Tasks (their responsibility, we support)

### WZP-FC-1. Add CallSignal WireMessage variant — DONE (v0.0.21, 064a730)
### WZP-FC-2. Call state management + sled tree — TODO (1-2d)
### WZP-FC-3. WS handler for call signaling — TODO (0.5d)
### WZP-FC-4. Auth token validation endpoint — DONE (v0.0.21, 064a730)
### WZP-FC-5. Group-to-room mapping — TODO (1d)
### WZP-FC-6. Presence/online status API — TODO (0.5-2d)
### WZP-FC-7. Missed call notifications — TODO (0.5d)
### WZP-FC-8. Cross-project identity verification — DONE (15 tests, 26dc848)
### WZP-FC-9. HKDF salt investigation — DONE (no mismatch)
### WZP-FC-10. Web bridge shared auth — TODO (1-2d)
### FC-CRATE-1. Standalone warzone-protocol — DONE (v0.0.21, 4a4fa9f)

---

## All WZP-S Tasks Complete

The WZP side of integration is finished. featherChat needs:
1. **FC-2 + FC-3** — call state management + WS routing (makes real calls possible)
2. **FC-5** — group-to-room mapping (uses `hash_room_name` convention)
3. **FC-6/7** — presence + missed calls (UX polish)
4. **FC-10** — web bridge shared auth (browser token flow)
