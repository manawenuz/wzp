# FeatherChat: Voice/Video Calling Integration with Warzone Messenger

## Overview

Voice/video calling system designed to integrate with the existing E2E encrypted Warzone messenger. Reuses the same identity, addressing, and key exchange infrastructure.

## Identity Model (reuse, not duplicate)

- **Identity**: 32-byte seed derives both keypairs via HKDF:
  - Ed25519 (signing)
  - X25519 (encryption)
- **Fingerprint**: `SHA-256(Ed25519 public key)[:16]`, displayed as `xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx`
- **Backup**: BIP39 mnemonic (24 words) for seed recovery
- **Storage**: Seed encrypted at rest with Argon2id + ChaCha20-Poly1305
- **Future**: Ethereum address as fingerprint (secp256k1 derived from same BIP39 seed)

## Addressing (reuse)

| Method | Format | Resolution |
|--------|--------|------------|
| Local alias | `@manwe` | Server resolves to fingerprint |
| Federated | `@manwe.b1.example.com` | DNS TXT record → fingerprint + server endpoint |
| ENS | `@manwe.eth` | Ethereum address → fingerprint (Phase 2-3) |
| Raw fingerprint | `xxxx:xxxx:...` | Direct lookup (always works as fallback) |

## Key Exchange (can extend)

- **X3DH** for session establishment:
  - Ed25519 identity key
  - X25519 ephemeral key
  - Signed pre-keys
- **Double Ratchet** for forward secrecy on data channels
- **Pre-key bundles** stored on server, fetched by callers

## Server Infrastructure

- **Stack**: Rust (axum), sled DB, WebSocket for real-time
- **Trust model**: Server is untrusted relay — never sees plaintext
- **Groups**: Named, auto-created, per-member encryption
- **Federation**: Via DNS TXT records (Phase 3)

## Calling System Requirements

1. **Signaling**: Reuse existing WebSocket connection and identity
2. **Key derivation**: SRTP/DTLS keys derived from existing X3DH shared secret (or new ephemeral exchange per call)
3. **Call initiation**: `WireMessage::CallOffer`, `CallAnswer`, `CallIceCandidate` variants
4. **NAT traversal**: STUN/TURN server integration
5. **Group calls**: SFU (Selective Forwarding Unit) vs mesh topology for up to 50 users
6. **Codecs**: Opus for audio, VP8/VP9/AV1 for video
7. **E2E media encryption**: Insertable streams API (WebRTC) or custom SRTP
8. **Unified addressing**: A user calls `@manwe` the same way they message `@manwe`

## Degradation Strategy

Calls should degrade gracefully under unreliable/warzone network conditions:

```
Video (full) → Video (low res) → Audio (high quality) → Audio (low bitrate)
```

- Support opportunistic cooperation
- Fall back to TURN/TCP through the existing WebSocket when UDP is blocked
