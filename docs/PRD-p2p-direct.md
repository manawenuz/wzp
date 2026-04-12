# PRD: Peer-to-Peer Direct Calls (No Relay)

## Problem

All calls currently route through a relay, even 1-on-1 calls between clients that could reach each other directly. This adds latency (2x hop), creates a single point of failure, and requires trusting the relay operator (even though media is encrypted, the relay sees metadata).

## Solution

For 1-on-1 calls, clients attempt a direct QUIC connection using STUN-discovered addresses. If NAT traversal succeeds, media flows directly between peers. If it fails, fall back to relay-assisted mode (current behavior).

## Architecture

```
Preferred (P2P):
  Client A ←──QUIC direct──→ Client B
  (no relay in media path, true E2E)

Fallback (Relay):
  Client A ──→ Relay ──→ Client B
  (current model)

Hybrid discovery:
  Client A → Relay (signaling only) → Client B
       ↓                                    ↓
    STUN server                        STUN server
       ↓                                    ↓
    Discover public IP:port          Discover public IP:port
       ↓                                    ↓
    Exchange candidates via relay signaling
       ↓                                    ↓
    Attempt direct QUIC connection ←──→
```

## Why P2P = True E2E

- QUIC TLS handshake establishes encrypted tunnel directly between A and B
- No third party sees the traffic
- Certificate pinning via identity fingerprints: each client derives their TLS cert from their Ed25519 seed (same as relay identity). During QUIC handshake, both sides verify the peer's cert fingerprint against the known identity
- MITM elimination: if A knows B's fingerprint (from prior call, QR code, or identity server), any interceptor presents a different cert → fingerprint mismatch → connection rejected
- Stronger guarantee than relay-assisted: user doesn't need to trust relay operator

## Requirements

### Phase 1: STUN Discovery

1. **STUN client**: lightweight UDP-based STUN client to discover public IP:port
   - Use existing public STUN servers (stun.l.google.com:19302, etc.)
   - Or run a STUN server alongside the relay
   - Discover: local addresses, server-reflexive addresses (STUN), relay candidates (TURN/relay fallback)

2. **Candidate gathering**: on call initiation, gather all candidates:
   - Host candidates: local network interfaces
   - Server-reflexive: STUN-discovered public IP:port
   - Relay candidate: the relay's address (fallback)

3. **Candidate exchange**: via relay signaling channel (existing `IceCandidate` signal message)
   - A sends candidates to relay → relay forwards to B
   - B sends candidates to relay → relay forwards to A

### Phase 2: Direct Connection

1. **QUIC hole punching**: both clients simultaneously attempt QUIC connections to each other's candidates
   - Quinn supports connecting to multiple addresses
   - First successful connection wins
   - Timeout after 3 seconds, fall back to relay

2. **Identity verification**: during QUIC handshake, verify peer's TLS cert fingerprint
   - `server_config_from_seed()` already exists — derive client cert from identity seed
   - Both sides present certs (mutual TLS)
   - Verify fingerprint matches expected identity

3. **Media flow**: once connected, use existing `QuinnTransport` for media + signals
   - Same `send_media()` / `recv_media()` API
   - Same codec pipeline, FEC, jitter buffer
   - No code changes needed in the call engine

### Phase 3: Adaptive Quality (P2P)

P2P connections have direct quality visibility — no relay middleman:

1. Both clients observe RTT, loss, jitter directly from QUIC stats
2. Adapt codec quality based on direct observations
3. Since only 2 participants, coordinated switching is simple: propose → ack → switch

This is the simplest case for adaptive quality. Once proven, backport the logic to relay-assisted mode.

### Phase 4: Hybrid Mode

1. **Call initiation**: always connect to relay for signaling
2. **Parallel attempt**: while relay call is active, attempt P2P in background
3. **Seamless migration**: if P2P succeeds, migrate media path from relay to direct
   - Both clients switch simultaneously
   - Relay connection kept alive for signaling (presence, room updates)
4. **Fallback**: if P2P connection drops, seamlessly fall back to relay

## Security Properties

| Property | Relay Mode | P2P Mode |
|----------|-----------|----------|
| Encryption | ChaCha20-Poly1305 (app layer) | QUIC TLS 1.3 + ChaCha20-Poly1305 |
| Key exchange | Via relay signaling | Direct QUIC handshake |
| Identity verification | TOFU (server fingerprint) | Mutual TLS cert pinning |
| Metadata privacy | Relay sees who talks to whom | No third party sees anything |
| MITM resistance | Depends on relay trust | Strong (cert pinning) |
| Forward secrecy | ECDH ephemeral keys | QUIC built-in + app-layer rekey |

## Implementation Notes

### STUN in Rust

Use `stun-rs` or `webrtc-rs` crate for STUN client. Minimal: just need Binding Request/Response to discover server-reflexive address.

### Quinn Hole Punching

Quinn's `Endpoint` can both listen and connect. For hole punching:
```rust
let endpoint = create_endpoint(bind_addr, Some(server_config))?;
// Send connect to peer's address (opens NAT pinhole)
let conn = connect(&endpoint, peer_addr, "peer", client_config).await?;
// Simultaneously, peer connects to our address
// First successful handshake wins
```

### Client TLS Certificate

Already have `server_config_from_seed()` for relays. Create `client_config_from_seed()` that presents a TLS client certificate derived from the identity seed. The peer verifies this cert's fingerprint.

### Signaling via Relay

The existing relay connection carries `IceCandidate` signals. No new infrastructure needed — just use the relay as a dumb signaling pipe for candidate exchange.

## Non-Goals (v1)

- SFU over P2P (P2P is 1-on-1 only; multi-party uses relay SFU)
- TURN server (relay acts as the fallback, no separate TURN)
- mDNS local discovery (future)
- Mesh P2P for multi-party (future, complex)

## Milestones

| Phase | Scope | Effort |
|-------|-------|--------|
| 1 | STUN client + candidate gathering | 2 days |
| 2 | QUIC hole punching + identity verification | 3 days |
| 3 | Adaptive quality on P2P connection | 2 days |
| 4 | Hybrid mode (relay + P2P, seamless migration) | 3 days |
