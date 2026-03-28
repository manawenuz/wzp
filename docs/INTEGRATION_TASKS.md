# WZP Integration Tasks

Based on featherChat commit 65f6390 — FUTURE_TASKS.md with WZP integration items.

## Status Key
- DONE = implemented and tested
- PARTIAL = code exists but not wired into live path
- TODO = not started

---

## WZP-Side Tasks (our responsibility)

### WZP-S-1. HKDF Salt/Info String Alignment — DONE
- HKDF info strings aligned: `warzone-ed25519` / `warzone-x25519`
- Salt: both use `None` (featherChat converts `b""` → `None`). No mismatch.
- Commit: `ac3b997`

### WZP-S-2. Accept featherChat Bearer Token on Relay — TODO (HIGH)
- Add `--auth-url` flag to wzp-relay (e.g., `--auth-url https://chat.example.com/v1/auth/validate`)
- On new QUIC connection: expect first signaling message to contain a bearer token
- Relay calls featherChat's `/v1/auth/validate` to verify
- Reject connection if token invalid
- Files: `wzp-relay/src/main.rs`, new `wzp-relay/src/auth.rs`

### WZP-S-3. Signaling Bridge Mode — TODO (HIGH)
- Client should be able to send/receive `SignalMessage` through featherChat's WebSocket
- New `WireMessage::CallSignal` variant wraps opaque JSON `SignalMessage`
- Client connects to featherChat WS, sends CallOffer, receives CallAnswer
- Then uses the relay address from the answer to connect QUIC for media
- Files: new `wzp-client/src/featherchat.rs`

### WZP-S-4. Room Access Control — TODO (MEDIUM)
- Relay should verify room membership before allowing join
- Room name should be opaque hash (not human-readable group name)
- `room_id = SHA-256("featherchat-group:" + group_name)[:16]`
- Files: `wzp-relay/src/room.rs`

### WZP-S-5. Wire Crypto Handshake into Live Path — PARTIAL
- `handshake.rs` exists in both client and relay
- Not used in CLI live mode, file mode, or web bridge
- Need to make handshake mandatory before media flows
- Files: `wzp-client/src/cli.rs`, `wzp-web/src/main.rs`

### WZP-S-6. Web Bridge + featherChat Web Client — TODO (MEDIUM)
- featherChat has a WASM web client (warzone-wasm crate)
- Web bridge should accept featherChat session tokens
- Share authentication with featherChat web login
- Files: `wzp-web/src/main.rs`

### WZP-S-7. Publish wzp-proto for featherChat — TODO (LOW)
- featherChat needs `wzp_proto::SignalMessage` type for `CallSignal` variant
- Option A: publish wzp-proto to private registry
- Option B: featherChat uses JSON schema, WZP serializes to JSON
- Option C: git submodule / path dependency

### WZP-S-8. CLI Seed Input — TODO (LOW)
- Add `--seed <hex>` or `--mnemonic <words>` flag to wzp-client
- Derive identity from seed, use for handshake
- Files: `wzp-client/src/cli.rs`

### WZP-S-9. Fix Hardcoded Assumptions — TODO
1. No auth on relay — fix via WZP-S-2
2. Room names from SNI visible to network — fix via WZP-S-4 (use hashed names)
3. No signaling before media — fix via WZP-S-5
4. Self-signed TLS — acceptable for relay-to-relay; need real certs for web
5. No codec negotiation in web bridge — fix: add profile exchange in WS
6. No connection to featherChat key registry — fix via WZP-S-2/S-3

---

## featherChat-Side Tasks (their responsibility, we support)

### WZP-FC-1. Add CallSignal WireMessage variant — DONE (v0.0.21, 064a730)
- `CallSignal { id, sender_fingerprint, signal_type, payload, target }`
- `CallSignalType`: Offer, Answer, IceCandidate, Hangup, Reject, Ringing, Busy
- payload field is String — WZP puts JSON-serialized SignalMessage here
- target field: peer fingerprint (1:1) or room name (group)
### WZP-FC-2. Call state management + sled tree — 1-2d
### WZP-FC-3. WS handler for call signaling — 0.5d
### WZP-FC-4. Auth token validation endpoint — DONE (v0.0.21, 064a730)
- `POST /v1/auth/validate { "token": "..." }`
- Returns: `{ "valid": true, "fingerprint": "...", "alias": "..." }`
### WZP-FC-5. Group-to-room mapping — 1d
### WZP-FC-6. Presence/online status API — 0.5-2d
### WZP-FC-7. Missed call notifications — 0.5d
### WZP-FC-8. Cross-project identity verification test — 2-4h (CRITICAL)
### WZP-FC-9. HKDF salt investigation — VERIFIED: no mismatch
### WZP-FC-10. Web bridge shared auth — 1-2d

---

## Integration Priority Order

1. **WZP-FC-8 + WZP-S-1** — Cross-project identity test (DONE on WZP side)
2. **WZP-S-8** — CLI seed input (enables identity testing)
3. **WZP-FC-1** — CallSignal WireMessage (featherChat side)
4. **WZP-S-3** — Signaling bridge in client
5. **WZP-FC-4 + WZP-S-2** — Auth tokens (both sides)
6. **WZP-S-5** — Wire handshake into live path
7. **WZP-FC-5 + WZP-S-4** — Group-to-room mapping + access control
8. **WZP-FC-2/3** — Call state management
9. **WZP-S-6 + WZP-FC-10** — Web integration
