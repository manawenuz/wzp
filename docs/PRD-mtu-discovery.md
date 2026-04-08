# PRD: QUIC Path MTU Discovery

## Problem

WarzonePhone uses conservative 1200-byte QUIC datagrams. Some network paths support larger MTUs (1400+), wasting bandwidth. Some broken paths (VPNs, tunnels, double-NAT, cellular) have MTU < 1200, causing silent packet drops — this may explain why Opus 64k fails on some paths while 24k works (larger encoded frames + FEC repair packets).

## Solution

Enable Quinn's built-in Path MTU Discovery (PMTUD) and handle edge cases:
1. PMTUD probes larger packet sizes and discovers the actual path MTU
2. Graceful fallback when datagrams exceed discovered MTU
3. Expose MTU in metrics for debugging

## Implementation

### Phase 1: Enable PMTUD in Quinn

`crates/wzp-transport/src/config.rs` — update `transport_config()`:

```rust
// Enable PMTUD (Quinn default is enabled, but we should ensure it)
config.mtu_discovery_config(Some(quinn::MtuDiscoveryConfig::default()));

// Set minimum MTU for safety (some paths can't handle 1200)
// Quinn default min is 1200, which is the QUIC spec minimum
```

Quinn's `MtuDiscoveryConfig` has:
- `interval`: how often to probe (default: 600s)
- `upper_bound`: max MTU to probe (default: 1452 for IPv4)
- `minimum_change`: min MTU increase to be worth probing (default: 20)

### Phase 2: Handle MTU-related Failures

In federation forwarding (`send_raw_datagram`), if the datagram exceeds the connection's current MTU, Quinn returns an error. Handle gracefully:
- Log warning with packet size vs MTU
- Drop the packet (don't crash)
- Track in metrics: `wzp_relay_mtu_exceeded_total`

### Phase 3: Codec-Aware MTU

When the path MTU is small, the relay or client should:
- Prefer lower-bitrate codecs (smaller packets)
- Reduce FEC ratio (fewer repair packets)
- This feeds into the adaptive quality system

### Phase 4: Expose MTU in Stats

- Add `path_mtu` to relay metrics (per peer)
- Add `path_mtu` to client stats (visible in UI)
- Log MTU on connection establishment

## Non-Goals (v1)

- Datagram fragmentation (QUIC datagrams are atomic — either fit or don't)
- Manual MTU override per relay config
- MTU-based codec selection (future, needs adaptive quality)

## Effort: 1 day
