# PRD: Delegated Trust for Relay Federation

## Problem

In the current federation model, when Relay 1 trusts Relay 2, and Relay 2 forwards media from Relay 3, Relay 1 has no way to know or control that Relay 3's traffic is reaching it. This is a trust gap — any relay in the chain can introduce untrusted traffic.

**Example:** Relay 1 (trusted zone) ←→ Relay 2 (hub) ←→ Relay 3 (unknown)

Relay 1 explicitly trusts Relay 2. But Relay 2 forwards Relay 3's media to Relay 1 without Relay 1's consent. Relay 1 receives media that originated from an entity it never approved.

## Solution

Add a `delegate` flag to `[[trusted]]` entries. When `delegate = true`, the relay accepts media forwarded through the trusted peer from relays that the trusted peer vouches for. When `delegate = false` (default), only media originating from explicitly trusted/peered relays is accepted.

## Trust Levels

| Config | Meaning |
|--------|---------|
| `[[peers]]` | "I connect to you and trust your identity" |
| `[[trusted]]` | "I accept connections from you" |
| `[[trusted]] delegate = true` | "I accept connections from you AND from relays you vouch for" |
| No entry | "I reject your connections and drop your forwarded media" |

## Configuration

```toml
# Relay 1: trusts Relay 2 and delegates trust
[[trusted]]
fingerprint = "relay-2-tls-fingerprint"
label = "Relay 2 (Hub)"
delegate = true    # Accept relays that Relay 2 forwards from

# Without delegate (default = false):
[[trusted]]
fingerprint = "relay-4-tls-fingerprint"
label = "Relay 4"
# delegate = false  (implicit default)
# Only direct media from Relay 4 is accepted
```

## Protocol Changes

### Relay-to-Relay Media Authorization

When Relay 2 forwards media from Relay 3 to Relay 1, the datagram needs to carry origin information so Relay 1 can decide whether to accept it.

**Option A: Origin tag in datagram** (recommended)

Extend the federation datagram format:
```
[room_hash: 8 bytes][origin_relay_fp: 8 bytes][media_packet]
```

The 8-byte origin fingerprint identifies which relay originally produced the media. The forwarding relay (Relay 2) sets this to the source relay's fingerprint. Relay 1 checks:
1. Is the origin relay directly trusted? → accept
2. Is the forwarding relay trusted with `delegate = true`? → accept
3. Otherwise → drop

**Option B: Trust announcement signal**

When Relay 2 connects to Relay 1, it sends a `FederationTrustChain` signal listing which relays it will forward from:
```rust
FederationTrustChain {
    /// Fingerprints of relays this peer may forward media from
    vouched_relays: Vec<String>,
}
```

Relay 1 checks each fingerprint against its policy:
- If Relay 2 has `delegate = true` in Relay 1's config → accept all listed relays
- If Relay 2 has `delegate = false` → reject, only accept direct media from Relay 2

Option B is simpler to implement (no datagram format change) but less granular.

### Recommended: Option B for v1, Option A for v2

Option B is simpler — the trust chain is established at connection time, not per-datagram. The forwarding relay announces what it will forward, and the receiving relay approves or rejects upfront.

## Implementation

### Config Changes

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedConfig {
    pub fingerprint: String,
    #[serde(default)]
    pub label: Option<String>,
    /// When true, also accept media forwarded through this relay from
    /// relays it vouches for. Default: false.
    #[serde(default)]
    pub delegate: bool,
}
```

### Federation Signal

```rust
/// Sent after FederationHello — lists relays this peer will forward from.
FederationTrustChain {
    /// TLS fingerprints of relays whose media may be forwarded through us.
    vouched_relays: Vec<String>,
}
```

### Forwarding Authorization

In `handle_datagram`, before forwarding media to local participants:

```rust
// Check if we should accept this forwarded media
let is_authorized = if source_is_direct_peer {
    true  // Direct peer, always accepted
} else {
    // Check if the forwarding peer has delegate=true
    let forwarding_peer = fm.find_trusted_by_fingerprint(forwarding_peer_fp);
    forwarding_peer.map(|t| t.delegate).unwrap_or(false)
};

if !is_authorized {
    warn!("dropping forwarded media from unauthorized relay chain");
    return;
}
```

### Relay 2 (Hub) Behavior

When Relay 2 receives `FederationTrustChain` queries from peers:
1. Collect all directly connected peer fingerprints
2. Send `FederationTrustChain { vouched_relays }` to each peer
3. When a new relay connects, update all peers' trust chains

### Anti-Spam Properties

| Attack | Mitigation |
|--------|-----------|
| Unknown relay connects to hub | Hub rejects (not in `[[trusted]]`) |
| Hub forwards spam relay's media | Receiving relay checks delegate flag, drops if false |
| Relay spoofs origin fingerprint | Origin tag is set by the forwarding relay, not the source. The forwarding relay is trusted, so if it lies about origin, the trust is misplaced at the config level. |
| Chain amplification (A→B→C→D→...) | TTL on forwarded datagrams (decrement at each hop, drop at 0). Default TTL=2 (one intermediate relay). |

## TTL for Chain Length

Add a TTL byte to the federation datagram to limit chain depth:

```
[room_hash: 8 bytes][ttl: 1 byte][media_packet]
```

- Default TTL = 2 (allows one intermediate relay: A→B→C)
- Each forwarding relay decrements TTL
- When TTL = 0, don't forward further (only deliver to local participants)
- Configurable per-relay: `max_federation_hops = 2`

## Milestones

| Phase | Scope | Effort |
|-------|-------|--------|
| 1 | Add `delegate` field to `TrustedConfig` | 0.5 day |
| 2 | `FederationTrustChain` signal + announcement | 1 day |
| 3 | Authorization check in `handle_datagram` | 0.5 day |
| 4 | TTL in federation datagrams | 0.5 day |
| 5 | Testing: authorized vs unauthorized forwarding | 0.5 day |

## Non-Goals (v1)

- Per-room trust policies (trust Relay X only for room "android")
- Dynamic trust negotiation (relays negotiate trust level at runtime)
- Revocation (removing a relay from trust chain requires config edit + restart)
- Cryptographic proof of origin (signed datagrams from source relay)
