---
tags: [prd, wzp]
type: prd
---

# PRD: Region-Based Relay Selection

> Phase: Implemented (data model)
> Status: Done (2026-04-14)
> Crate: wzp-client, wzp-proto, wzp-relay

## Problem

Clients are configured with a single relay address. With multiple relays in the federation mesh, the client should automatically discover all available relays and select the lowest-latency one. Currently there is no mechanism for the relay to advertise its mesh peers to clients, and no client-side data structure to track relay health over time.

## Solution

1. Relays advertise their region and mesh peers in `RegisterPresenceAck`
2. Clients maintain a `RelayMap` sorted by measured RTT
3. `preferred()` returns the best relay for call setup

## Implementation

### New Module: `crates/wzp-client/src/relay_map.rs`

**RelayEntry**:
```rust
pub struct RelayEntry {
    pub name: String,
    pub addr: SocketAddr,
    pub region: Option<String>,
    pub rtt_ms: Option<u32>,
    pub last_probed: Option<Instant>,
    pub reachable: bool,
}
```

**RelayMap API**:
- `upsert(name, addr, region)` — add or update a relay entry
- `update_rtt(addr, rtt_ms)` — record probe result, marks reachable, re-sorts
- `mark_unreachable(addr)` — sorts unreachable entries to end
- `preferred()` -> `Option<&RelayEntry>` — lowest RTT reachable relay
- `populate_from_ack(relays, region)` — parse `RegisterPresenceAck.available_relays` (format: `"name|addr"`)
- `needs_reprobe(max_age)` — true if any entry has stale or missing probe
- `stale_entries(max_age)` — list of entries needing fresh probes

### Signal Protocol Extension

`RegisterPresenceAck` extended:
```rust
RegisterPresenceAck {
    success: bool,
    error: Option<String>,
    relay_build: Option<String>,
    relay_region: Option<String>,           // NEW
    available_relays: Vec<String>,          // NEW — "name|addr" format
}
```

### Relay Config Extension

`RelayConfig` extended:
```rust
pub region: Option<String>,          // e.g., "us-east", "eu-west"
pub advertised_addr: Option<SocketAddr>,  // for available_relays population
```

### Relay Population

On `RegisterPresenceAck`, the relay populates:
- `relay_region` from `config.region`
- `available_relays` from `config.peers` (label|url format)

### Deferred

- **Automatic relay switching** — using `preferred()` to select relay during call setup instead of hardcoded config
- **Background reprobing** — periodic RTT measurements to keep the relay map fresh
- **Cross-relay RTT estimation** — using mesh probe data to estimate combined caller-RTT + callee-RTT for optimal relay placement

## Files

| File | Change |
|------|--------|
| `crates/wzp-client/src/relay_map.rs` | New — RelayMap + RelayEntry |
| `crates/wzp-client/src/lib.rs` | Add `pub mod relay_map` |
| `crates/wzp-proto/src/packet.rs` | `relay_region` + `available_relays` on RegisterPresenceAck |
| `crates/wzp-relay/src/config.rs` | `region` + `advertised_addr` fields |
| `crates/wzp-relay/src/main.rs` | Populate RegisterPresenceAck from config + peers |

## Testing

- 15 unit tests: preferred by RTT, unreachable not preferred, preferred empty/all-unreachable, populate_from_ack (valid + malformed entries), upsert updates/preserves region, needs_reprobe (empty/never/fresh), stale_entries, sort stability with equal RTT, mark_unreachable sorts to end, RelayEntry serialization
- 2 protocol tests: RegisterPresenceAck roundtrip with new fields, backward compat without new fields
