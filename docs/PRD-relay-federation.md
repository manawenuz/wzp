# PRD: Relay Federation (Multi-Relay Mesh)

## Problem

Currently all participants in a call must connect to the same relay. This creates:
- **Single point of failure** — if the relay goes down, the entire call drops
- **Geographic latency** — users far from the relay get high RTT
- **Capacity limits** — one relay handles all traffic

Users should be able to connect to their nearest/preferred relay and still talk to users on other relays, as long as the relays are federated.

## Prerequisite: Fix Relay Identity Persistence

### Bug: TLS certificate regenerates on every restart

**Root cause:** `wzp-transport/src/config.rs:17` calls `rcgen::generate_simple_self_signed()` which creates a new keypair every time. The relay's Ed25519 identity seed IS persisted to `~/.wzp/relay-identity`, but the TLS certificate is not derived from it.

**Impact:** Clients see a different server fingerprint after every relay restart, triggering the "Server Key Changed" warning. This also breaks federation since relays identify each other by certificate fingerprint.

**Fix:** Derive the TLS certificate from the persisted relay seed:
1. Add `server_config_from_seed(seed: &[u8; 32])` to `wzp-transport`
2. Use the seed to create a deterministic keypair (e.g., derive an ECDSA key via HKDF from the Ed25519 seed)
3. Generate a self-signed cert with that keypair — same seed = same cert = same fingerprint
4. The relay passes its loaded seed to `server_config_from_seed()` instead of `server_config()`

**Effort:** 0.5 day

## Federation Design

### Core Concept

Two or more relays form a **federation mesh**. Each relay is an independent SFU. When relays are configured to trust each other, they bridge rooms with matching names — participants on relay A in room "podcast" hear participants on relay B in room "podcast" as if everyone were on the same relay.

### Configuration

Each relay reads a YAML config file (e.g., `~/.wzp/relay.yaml` or `--config relay.yaml`):

```yaml
# Relay identity (auto-generated if missing)
listen: 0.0.0.0:4433

# Federation peers — other relays we trust and bridge rooms with
# Both sides must configure each other for federation to work
peers:
  - url: "193.180.213.68:4433"
    fingerprint: "a5d6:e3c6:5ae7:185c:4eb1:af89:daed:4a43"
    label: "Pangolin EU"

  - url: "10.0.0.5:4433"
    fingerprint: "7f2a:b391:0c44:..."
    label: "Office LAN"
```

**Key rules:**
- Both relays must configure each other — **mutual trust** required
- A relay that receives a connection from an unknown peer logs: `"Relay a5d6:e3c6:... (193.180.213.68) wants to federate. To accept, add to peers config: url: 193.180.213.68:4433, fingerprint: a5d6:e3c6:..."`
- Fingerprints are verified via the TLS certificate (requires the identity fix above)

### Protocol

#### Peer Connection

1. On startup, each relay attempts QUIC connections to all configured peers
2. The connection uses SNI `"_federation"` (reserved room name prefix) to distinguish from client connections
3. After QUIC handshake, verify the peer's certificate fingerprint matches the configured fingerprint
4. If fingerprint mismatch → reject, log warning
5. If peer connects but isn't in our config → log the helpful "add to config" message, reject

#### Room Bridging

Once two relays are connected:

1. **Room discovery**: When a local participant joins room "T", the relay sends a `FederationRoomJoin { room: "T" }` signal to all connected peers
2. **Room leave**: When the last local participant leaves room "T", send `FederationRoomLeave { room: "T" }`
3. **Media forwarding**: For each room that exists on both relays:
   - Relay A forwards all media packets from its local participants to relay B
   - Relay B forwards all media packets from its local participants to relay A
   - Each relay then fans out received federated media to its local participants (same as local SFU forwarding)
4. **Participant presence**: `RoomUpdate` signals are merged — local participants + federated participants from all peers

```
Relay A (2 local users)          Relay B (1 local user)
┌─────────────────────┐          ┌─────────────────────┐
│ Room "T"            │          │ Room "T"            │
│  Alice (local)  ────┼──media──►│  Charlie (local)    │
│  Bob   (local)  ────┼──media──►│                     │
│                     │◄──media──┼── Charlie           │
│  Charlie (federated)│          │  Alice (federated)  │
│                     │          │  Bob   (federated)  │
└─────────────────────┘          └─────────────────────┘
```

#### Signal Messages (new)

```rust
enum FederationSignal {
    /// A room exists on this relay with active participants
    RoomJoin { room: String, participants: Vec<ParticipantInfo> },
    /// Room is empty on this relay
    RoomLeave { room: String },
    /// Participant update for a federated room
    ParticipantUpdate { room: String, participants: Vec<ParticipantInfo> },
}
```

#### Media Forwarding

Federated media is forwarded as raw QUIC datagrams — the relay doesn't decode/re-encode. Each packet is prefixed with a room identifier so the receiving relay knows which room to fan it out to:

```
[room_hash: 8 bytes][original_media_packet]
```

The 8-byte room hash is computed once when the federation room bridge is established.

### What Relays DON'T Do

- **No transcoding** — media passes through as-is. If Alice sends Opus 64k, Charlie receives Opus 64k
- **No re-encryption** — packets are already encrypted end-to-end between participants. Relays just forward opaque bytes
- **No central coordinator** — each relay independently connects to its configured peers. No master/slave, no consensus protocol
- **No automatic peer discovery** — peers must be explicitly configured in YAML

### Failure Handling

- If a peer relay goes down, the federation link drops. Local rooms continue to work. Federated participants disappear from presence.
- Reconnection: attempt every 30 seconds with exponential backoff up to 5 minutes
- If a peer relay restarts with a new identity (bug not fixed), the fingerprint check fails and federation is rejected with a clear error log

## Implementation Plan

### Phase 0: Fix Relay Identity (prerequisite)
- Derive TLS cert from persisted seed
- Same seed → same cert → same fingerprint across restarts

### Phase 1: YAML Config + Peer Connection
- Add `--config relay.yaml` CLI flag
- Parse peers config
- On startup, connect to all configured peers via QUIC
- Verify certificate fingerprints
- Log helpful message for unconfigured peers
- Reconnect on disconnect

### Phase 2: Room Bridging
- Track which rooms exist on each peer
- Forward media for shared rooms
- Merge participant presence across peers
- Handle room join/leave signals

### Phase 3: Resilience
- Graceful handling of peer disconnect/reconnect
- Don't duplicate packets if a participant is reachable via multiple paths
- Rate limiting on federation links (prevent amplification)
- Metrics: federated rooms, packets forwarded, peer latency

## Effort Estimates

| Phase | Scope | Effort |
|-------|-------|--------|
| 0 | Fix relay TLS identity from seed | 0.5 day |
| 1 | YAML config + peer QUIC connections | 2 days |
| 2 | Room bridging + media forwarding + presence merge | 3-4 days |
| 3 | Resilience + metrics | 2 days |

## Non-Goals (v1)

- Automatic peer discovery (mDNS, DHT, etc.)
- Cascading federation (relay A ↔ B ↔ C where A doesn't know C)
- Load balancing across relays
- Encryption between relays (QUIC provides transport encryption; e2e encryption between participants is orthogonal)
- Different rooms on different relays (all federated rooms are bridged by name)
