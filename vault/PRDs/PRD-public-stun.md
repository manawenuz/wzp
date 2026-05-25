---
tags: [prd, wzp]
type: prd
---

# PRD: Public STUN Client

> Phase: Implemented
> Status: Done (2026-04-14)
> Crate: wzp-client

## Problem

WarzonePhone's reflexive address discovery depends entirely on relay-based `Reflect` messages over an authenticated QUIC signal channel. If the relay is unreachable, overloaded, or not yet connected, the client cannot discover its public IP:port for P2P hole-punching. This single point of failure means call setup is delayed or falls back to relay-only unnecessarily.

Tailscale solves this by querying multiple public STUN servers in parallel, independent of its DERP relay infrastructure.

## Solution

Implement a minimal RFC 5389 STUN Binding client over raw UDP that queries public STUN servers (Google, Cloudflare) in parallel. This provides:

1. **Independent reflexive discovery** — works without any relay connection
2. **Redundancy** — STUN fallback when relay reflection fails
3. **Better NAT classification** — more probes = higher confidence in Cone vs Symmetric detection
4. **Faster call setup** — STUN can run before signal registration completes

## Implementation

### New Module: `crates/wzp-client/src/stun.rs`

**Wire format** (RFC 5389):
- 20-byte header: type (u16) + length (u16) + magic cookie (0x2112A442) + transaction ID (12 bytes)
- Binding Request (0x0001): no attributes, just the header
- Binding Response (0x0101): parses XOR-MAPPED-ADDRESS (0x0020, preferred) and MAPPED-ADDRESS (0x0001, fallback)
- XOR decoding: port XOR'd with top 16 bits of magic cookie, IPv4 XOR'd with cookie, IPv6 XOR'd with cookie || txn ID

**Public API**:
- `stun_reflect(socket, server, timeout)` — single-server probe with one retry on first-packet timeout
- `discover_reflexive(config)` — parallel probe of N servers, first success wins
- `probe_stun_servers(config)` — all-server probe returning `Vec<NatProbeResult>` for NAT classification
- `resolve_stun_server(host_port)` — DNS resolution preferring IPv4

**Default servers**: `stun.l.google.com:19302`, `stun1.l.google.com:19302`, `stun.cloudflare.com:3478`

**Error handling**: `StunError` enum — Io, Timeout, Malformed, TxnMismatch, ErrorResponse, NoMappedAddress, DnsError

### Integration Points

1. **`reflect.rs`**: New `detect_nat_type_with_stun()` runs relay probes and STUN probes concurrently via `tokio::join!`, merges results, re-classifies
2. **Desktop `lib.rs`**: `try_reflect_own_addr()` falls back to `try_stun_fallback()` when relay reflection fails or times out
3. **Desktop `detect_nat_type` command**: Uses `detect_nat_type_with_stun()` for combined relay + STUN classification

### Design Decisions

- **Separate UDP socket** per STUN probe — can't share the QUIC socket (quinn owns its I/O driver)
- **No external crate** — RFC 5389 Binding is ~200 lines of code, no need for `stun-rs` or `webrtc-rs`
- **Retry once** at half-timeout — handles the "first-packet problem" where some NATs drop the initial UDP packet to a new destination
- **IPv4 preferred** for DNS resolution — Phase 7 IPv6 is still flaky

## Files

| File | Change |
|------|--------|
| `crates/wzp-client/src/stun.rs` | New — STUN client |
| `crates/wzp-client/src/lib.rs` | Add `pub mod stun` |
| `crates/wzp-client/src/reflect.rs` | Add `detect_nat_type_with_stun()` |
| `crates/wzp-client/Cargo.toml` | Add `rand` dependency |
| `desktop/src-tauri/src/lib.rs` | STUN fallback in `try_reflect_own_addr()`, STUN in `detect_nat_type` |

## Testing

- 22 unit tests: encode/decode roundtrips, XOR-MAPPED-ADDRESS (IPv4, IPv6, high port), MAPPED-ADDRESS fallback (IPv4, IPv6), unknown family, attribute padding, unknown attributes skipped, truncated attributes, error response, bad cookie, txn mismatch, too short, no mapped address, XOR preferred over mapped, error Display, default config, empty servers
- 2 integration tests (`#[ignore]`): query `stun.l.google.com`, multi-server probe
