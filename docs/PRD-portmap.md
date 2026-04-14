# PRD: NAT Port Mapping (PCP/PMP/UPnP)

> Phase: Implemented
> Status: Done (2026-04-14)
> Crate: wzp-client, wzp-proto, wzp-relay

## Problem

WarzonePhone falls back to relay-only when the client is behind a symmetric NAT (different external port per destination). The STUN-discovered reflexive address won't match what a peer sees, so direct hole-punching fails. Tailscale reports ~70% of consumer routers support NAT-PMP, PCP, or UPnP — protocols that let clients request explicit port mappings, making symmetric NATs traversable.

## Solution

Implement all three port mapping protocols, tried in sequence (NAT-PMP -> PCP -> UPnP). When a mapping is acquired, advertise the mapped address as a new candidate type alongside reflexive and host candidates. The relay cross-wires it into `CallSetup.peer_mapped_addr` so the peer can dial it.

## Implementation

### New Module: `crates/wzp-client/src/portmap.rs`

**NAT-PMP (RFC 6886)**:
- UDP to gateway:5351
- External address request (opcode 0) -> returns router's public IP
- Map UDP request (opcode 1) -> returns mapped external port + lifetime
- 12-byte request, 16-byte response

**PCP (RFC 6887)**:
- Same gateway:5351, version 2
- MAP opcode with client IP as IPv4-mapped IPv6
- 60-byte request/response with 12-byte nonce for anti-spoofing
- Superset of NAT-PMP, supports IPv6

**UPnP IGD**:
- SSDP M-SEARCH to 239.255.255.250:1900 for InternetGatewayDevice discovery
- Parse LOCATION header -> fetch device description XML -> find WANIPConnection controlURL
- SOAP `GetExternalIPAddress` -> router's public IP
- SOAP `AddPortMapping` -> maps the QUIC port

**Gateway discovery**:
- macOS: `route -n get default` (parse `gateway:` line)
- Linux/Android: `/proc/net/route` (parse hex gateway for 00000000 destination)

**Public API**:
- `acquire_port_mapping(internal_port, local_ip)` -> tries all 3, first success wins
- `release_port_mapping(mapping)` -> best-effort cleanup (lifetime=0 for NAT-PMP)
- `spawn_refresh(mapping)` -> background task renewing at half-lifetime
- `default_gateway()` -> cross-platform gateway discovery

### Signal Protocol Extensions

| Message | New Field | Purpose |
|---------|-----------|---------|
| `DirectCallOffer` | `caller_mapped_addr: Option<String>` | Caller's port-mapped address |
| `DirectCallAnswer` | `callee_mapped_addr: Option<String>` | Callee's port-mapped address |
| `CallSetup` | `peer_mapped_addr: Option<String>` | Relay cross-wires peer's mapped addr |

All fields use `#[serde(default, skip_serializing_if)]` for backward compatibility.

### Relay Cross-Wiring

`CallRegistry` extended with `caller_mapped_addr` / `callee_mapped_addr` fields + setter methods. The relay:
1. Extracts `caller_mapped_addr` from `DirectCallOffer`, stores in registry
2. Extracts `callee_mapped_addr` from `DirectCallAnswer`, stores in registry
3. Cross-wires into `CallSetup`: caller gets callee's mapped addr as `peer_mapped_addr`, and vice versa

### Candidate Priority

`PeerCandidates.mapped` added to `dual_path.rs`. Dial order:
1. Host (LAN) candidates — fastest on same-LAN
2. **Port-mapped** — stable even behind symmetric NATs
3. Server-reflexive (STUN) — standard hole-punching
4. Relay — always-available fallback

### Desktop Integration

Both `place_call()` and `answer_call()` call `acquire_port_mapping()` using the signal endpoint's local port. Privacy-mode answers (`AcceptGeneric`) skip portmap to keep the address hidden.

## Files

| File | Change |
|------|--------|
| `crates/wzp-client/src/portmap.rs` | New — NAT-PMP/PCP/UPnP client |
| `crates/wzp-client/src/dual_path.rs` | `PeerCandidates.mapped` field + dial_order update |
| `crates/wzp-proto/src/packet.rs` | `caller/callee_mapped_addr` + `peer_mapped_addr` fields |
| `crates/wzp-relay/src/call_registry.rs` | `caller/callee_mapped_addr` fields + setters |
| `crates/wzp-relay/src/main.rs` | Extract, store, cross-wire mapped addrs |
| `desktop/src-tauri/src/lib.rs` | Call portmap in place_call/answer_call |

## Testing

- 18 unit tests: NAT-PMP encoding, UPnP XML parsing (5 variants including real-world router XML), URL host extraction, error Display, protocol serde, PortMapping serialization, gateway detection, constants verification
- 2 integration tests (`#[ignore]`): gateway discovery, acquire_mapping
- 9 PeerCandidates tests: dial_order with all types, dedup, is_empty edge cases
- 12 protocol roundtrip tests: offer/answer/setup with mapped addr, backward compat without
