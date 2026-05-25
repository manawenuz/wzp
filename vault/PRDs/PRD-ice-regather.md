---
tags: [prd, wzp]
type: prd
---

# PRD: Mid-Call ICE Re-Gathering

> Phase: Implemented (signal plane); transport hot-swap deferred
> Status: Partial (2026-04-14)
> Crate: wzp-client, wzp-proto, wzp-relay

## Problem

When a mobile device transitions between networks (WiFi -> cellular, IP address change), the active QUIC connection dies. The call stays on a dead path until timeout, then the user experiences silence. There is no mechanism to re-discover candidates and re-establish a direct path mid-call.

Android's `NetworkMonitor.onIpChanged` already fires on `onLinkPropertiesChanged`, but nothing consumes it for candidate re-gathering or path migration.

## Solution

Implement an `IceAgent` that manages the full candidate lifecycle — initial gathering, mid-call re-gathering on network change, and peer candidate application. A new `CandidateUpdate` signal message carries refreshed candidates to the peer through the relay.

## Implementation

### New Module: `crates/wzp-client/src/ice_agent.rs`

**IceAgent struct**:
- Owns `IceAgentConfig` (STUN config, portmap toggle, gather timeout, local ports)
- Monotonic `generation: AtomicU32` — incremented on each re-gather, peers reject stale updates
- `peer_generation: AtomicU32` — tracks last-seen peer generation for ordering

**Public API**:
- `gather()` -> `CandidateSet` — runs STUN + portmap + host candidates in parallel with timeout
- `re_gather()` -> `(CandidateSet, SignalMessage)` — increments generation, returns update to send
- `apply_peer_update(signal)` -> `Option<PeerCandidates>` — parses `CandidateUpdate`, rejects if generation <= last-seen

**CandidateSet**:
```rust
pub struct CandidateSet {
    pub reflexive: Option<SocketAddr>,
    pub local: Vec<SocketAddr>,
    pub mapped: Option<SocketAddr>,
    pub generation: u32,
}
```

### New Signal: `CandidateUpdate`

```rust
CandidateUpdate {
    call_id: String,
    reflexive_addr: Option<String>,
    local_addrs: Vec<String>,
    mapped_addr: Option<String>,
    generation: u32,
}
```

- All address fields use `#[serde(default, skip_serializing_if)]` for backward compat
- Generation counter is mandatory — prevents stale updates from network reordering

### Relay Forwarding

`CandidateUpdate` is forwarded to the call peer using the same pattern as `MediaPathReport`:
1. Look up peer fingerprint + `peer_relay_fp` from `CallRegistry`
2. If cross-relay: wrap in `FederatedSignalForward` and forward via federation link
3. If local: send via `signal_hub.send_to()`

### Desktop Handling

Signal recv loop handles `CandidateUpdate`:
- Logs generation, reflexive, mapped, local count
- Emits `recv:CandidateUpdate` debug event
- Emits `signal-event` type `candidate_update` to JS frontend
- TODO: wire into `IceAgent.apply_peer_update()` + `race_upgrade()` for transport hot-swap

### Deferred: Transport Hot-Swap

The actual mid-call transport replacement is not yet wired. The designed approach:
- `Arc<RwLock<Arc<QuinnTransport>>>` — send/recv tasks clone inner Arc per frame
- On upgrade, swap inner Arc under write lock — next frame picks up new transport
- Android: `pending_ice_regather: AtomicBool` polled in recv task, triggers re-gather + swap
- Requires live testing to validate seamless audio continuity during swap

## Signal Flow

```
Network change (WiFi -> cellular)
    |
    v
IceAgent::re_gather()
    |-- stun::discover_reflexive()
    |-- portmap::acquire_port_mapping()
    |-- local_host_candidates()
    |
    v
SignalMessage::CandidateUpdate { generation: N+1 }
    |
    v (via relay)
Peer IceAgent::apply_peer_update()
    |
    v
PeerCandidates { reflexive, local, mapped }
    |
    v
dual_path::race() with new candidates [NOT YET WIRED]
```

## Files

| File | Change |
|------|--------|
| `crates/wzp-client/src/ice_agent.rs` | New — IceAgent + CandidateSet |
| `crates/wzp-proto/src/packet.rs` | `CandidateUpdate` variant |
| `crates/wzp-relay/src/main.rs` | Forward `CandidateUpdate` to peer |
| `crates/wzp-client/src/featherchat.rs` | Map `CandidateUpdate` to `IceCandidate` type |
| `desktop/src-tauri/src/lib.rs` | Handle `CandidateUpdate` in signal recv loop |

## Testing

- 10 unit tests: generation monotonicity, apply_peer_update (all fields, empty fields, unparseable addrs, stale rejection, wrong signal type), default config, gather with no STUN, re_gather produces signal with incrementing generation
- 2 protocol roundtrip tests: CandidateUpdate full + minimal
