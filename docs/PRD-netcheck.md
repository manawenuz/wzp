# PRD: Network Diagnostic (Netcheck)

> Phase: Implemented
> Status: Done (2026-04-14)
> Crate: wzp-client

## Problem

When P2P connections fail or call quality is poor, there is no diagnostic tool to understand why. Users and developers must manually probe STUN, check NAT type, test relay connectivity, and verify port mapping support — all separately. Tailscale's `netcheck` consolidates all of this into a single diagnostic report.

## Solution

A comprehensive `run_netcheck()` function that probes all network capabilities in parallel and produces a structured `NetcheckReport`. Exposed as a CLI subcommand (`wzp-client --netcheck`) and available for in-app diagnostics.

## Implementation

### New Module: `crates/wzp-client/src/netcheck.rs`

**NetcheckReport**:
```rust
pub struct NetcheckReport {
    pub nat_type: NatType,
    pub reflexive_addr: Option<String>,
    pub ipv4_reachable: bool,
    pub ipv6_reachable: bool,
    pub hairpin_works: Option<bool>,
    pub port_mapping: Option<PortMapProtocol>,
    pub relay_latencies: Vec<RelayLatency>,
    pub preferred_relay: Option<String>,
    pub stun_latency_ms: Option<u32>,
    pub upnp_available: bool,
    pub pcp_available: bool,
    pub nat_pmp_available: bool,
    pub gateway: Option<String>,
    pub duration_ms: u32,
    pub stun_probes: Vec<NatProbeResult>,
}
```

**Probes (all parallel via `tokio::join!`)**:
1. **STUN probes** — `probe_stun_servers()` to all configured STUN servers
2. **Relay latencies** — `probe_reflect_addr()` to each configured relay
3. **Port mapping** — `acquire_port_mapping()` to detect NAT-PMP/PCP/UPnP
4. **Gateway** — `default_gateway()` for the router address
5. **IPv6** — attempt to bind `[::]:0` and send to an IPv6 STUN server

**Derived fields**:
- `nat_type` / `reflexive_addr` — from `classify_nat()` on STUN probes
- `ipv4_reachable` — true if any STUN probe succeeded
- `preferred_relay` — relay with lowest RTT
- `port_mapping` / `nat_pmp_available` / `pcp_available` / `upnp_available` — from portmap result

**Human-readable output**: `format_report()` produces a formatted text report with sections for NAT info, port mapping, STUN probes, relay latencies.

### CLI Integration

`wzp-client --netcheck <relay-addr>` — runs the diagnostic using the specified relay plus default STUN servers, prints the report, and exits.

### Deferred

- **Hairpin test** — send packet from shared endpoint to own reflexive addr to test NAT hairpinning. Architecture is in place (`hairpin_works: Option<bool>`) but the actual probe is not yet implemented.
- **Android/Desktop in-app UI** — expose via JNI (Android) and Tauri command (desktop) for user-facing diagnostics.

## Files

| File | Change |
|------|--------|
| `crates/wzp-client/src/netcheck.rs` | New — NetcheckReport + run_netcheck + format_report |
| `crates/wzp-client/src/lib.rs` | Add `pub mod netcheck` |
| `crates/wzp-client/src/cli.rs` | `--netcheck` flag + handler |

## Testing

- 5 unit tests: default config, report JSON serialization + roundtrip, RelayLatency serialization, format_report with empty relays, format_report with full data (STUN probes, relay latencies, preferred relay, port mapping)
- 1 integration test (`#[ignore]`): full netcheck run
