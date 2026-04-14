# PRD: Hard NAT Traversal (Port Prediction + Birthday Attack)

> Phase: Partial implementation
> Status: Phase A done, Phase B signal ready, C-D not started (2026-04-14)
> Crate: wzp-client, wzp-proto, wzp-relay

## Problem

When both peers are behind **symmetric NATs** (endpoint-dependent mapping), standard hole-punching fails because the external port changes per destination. Our Phase 8.2 port mapping (NAT-PMP/PCP/UPnP) solves this when the router supports it (~70% of consumer routers), but the remaining ~30% — plus corporate firewalls, cloud NATs (AWS/Azure), and carrier-grade NATs — fall back to relay.

Tailscale tackles this with two techniques:
1. **Port prediction** for NATs with sequential allocation patterns
2. **Birthday attack** for NATs with random allocation

Both are viable when **at least one peer has a predictable NAT** (easy+hard pair). When **both** peers have fully random symmetric NATs, even Tailscale falls back to relay.

## Background: How Symmetric NATs Allocate Ports

| Pattern | Behavior | Prevalence | Traversal |
|---------|----------|------------|-----------|
| **Sequential** | port N, N+1, N+2... per new flow | ~40% of symmetric NATs (home routers) | Port prediction viable |
| **Random** | truly random port per flow | ~50% (enterprise, cloud, CGNAT) | Birthday attack only |
| **Port-preserving** | same as source port when possible | ~10% (behaves like cone NAT) | Standard hole-punch works |

## Solution Overview

### Phase A: NAT Port Allocation Pattern Detection

Before attempting hard NAT traversal, detect whether the NAT allocates ports sequentially or randomly. This determines which strategy to use.

**Method**: Send 5 STUN Binding Requests from the same source socket to 5 different STUN servers. Collect the 5 observed external ports. Analyze:

```
Ports: [40001, 40002, 40003, 40004, 40005]  → Sequential (delta=1)
Ports: [40001, 40003, 40005, 40007, 40009]  → Sequential (delta=2)
Ports: [40001, 52847, 19432, 61203, 8847]   → Random
Ports: [4433,  4433,  4433,  4433,  4433]   → Port-preserving (cone-like)
```

Classification:
- All same port → `PortPreserving` (use standard hole-punch)
- Consistent delta between consecutive ports → `Sequential { delta: i16 }`
- No pattern → `Random`

**New struct**:
```rust
pub enum PortAllocation {
    PortPreserving,
    Sequential { delta: i16 },
    Random,
    Unknown,
}
```

Add to `NetcheckReport` and `NatDetection`.

### Phase B: Port Prediction (Sequential NATs)

When the NAT is sequential, we can **predict** the next external port:

1. Client sends a STUN probe → observes external port P
2. Client knows the NAT will assign P+delta for the next outbound flow
3. Client tells peer (via relay or chat): "dial me at `my_ip:(P + delta * N)`" where N is the number of flows the client will open before the peer's packet arrives
4. Client opens a QUIC connection to the peer's predicted port at the same time
5. If the prediction lands within a small window, the QUIC handshake succeeds

**Timing is critical**: both peers must probe, predict, and dial within a tight window (~500ms) so the port prediction doesn't drift.

**Coordination via relay** (or out-of-band chat):
```
SignalMessage::HardNatProbe {
    call_id: String,
    /// My observed port sequence (last 3 ports, most recent first)
    port_sequence: Vec<u16>,
    /// My detected allocation pattern
    allocation: PortAllocation,
    /// Timestamp (ms since epoch) — for synchronization
    probe_time_ms: u64,
    /// My external IP (from STUN)
    external_ip: String,
}
```

Both peers exchange `HardNatProbe`, then simultaneously:
1. Each predicts the other's next port: `peer_ip:(peer_last_port + peer_delta * offset)`
2. Each opens N parallel QUIC connections to predicted port range: `[predicted - 2, predicted + 2]`
3. First successful handshake wins

**Expected success rate**: ~80% for sequential NATs with consistent delta, within 2-3 seconds.

### Phase C: Birthday Attack (Random NATs)

When the NAT is random, port prediction is impossible. Instead, exploit the **birthday paradox**:

**Math**: With N ports open on side A and M probes from side B into a 65536-port space:
- N=256, M=256: P(collision) ≈ 1 - e^(-256*256/65536) ≈ 63%
- N=256, M=512: P(collision) ≈ 1 - e^(-256*512/65536) ≈ 87%
- N=256, M=1024: P(collision) ≈ 1 - e^(-256*1024/65536) ≈ 98%

**Implementation**:

1. **Acceptor side** (easy NAT or the side with more ports available):
   - Open 256 UDP sockets bound to random ports
   - For each socket, send one STUN probe to learn its external port
   - Report all 256 external ports to the peer

2. **Dialer side** (hard NAT):
   - Send 1024 QUIC Initial packets to random ports on the Acceptor's external IP
   - Rate: 100-200 packets/sec to avoid triggering rate limits
   - Duration: ~5-10 seconds

3. **Collision detection**:
   - When one of the Dialer's packets hits one of the Acceptor's open ports, the QUIC handshake begins
   - The Acceptor sees an incoming Initial on one of its 256 sockets

**Problem for VoIP**: This takes 5-10 seconds even at high probe rates. For a phone call, this means a long "connecting..." phase. Acceptable as a last resort before relay fallback.

### Phase D: Hybrid Strategy

Combine all techniques in a waterfall:

```
1. Port mapping (NAT-PMP/PCP/UPnP)     → <100ms   [Phase 8.2, done]
   ↓ failed
2. Standard hole-punch (cone NAT)       → <500ms   [Phase 3-6, done]
   ↓ failed (symmetric NAT detected)
3. Port prediction (sequential NAT)     → <2s      [Phase A+B, new]
   ↓ failed (random NAT detected)
4. Birthday attack (one side random)    → <10s     [Phase C, new]
   ↓ failed (both sides random)
5. Relay fallback                       → always   [Phase 1, done]
```

The relay path starts **immediately in parallel** with all direct attempts (existing 500ms head-start architecture). The user hears audio via relay while the harder traversal techniques probe in the background. If a direct path is found, the call seamlessly upgrades (using the Phase 8.3 transport hot-swap mechanism).

## QUIC-Specific Challenges

### 1. Connection ID Mismatch
QUIC's Initial packet contains a random Destination Connection ID. When birthday-attack probes land on the Acceptor's socket, the CID won't match any expected value. Quinn handles this via its `Endpoint` which accepts any incoming Initial — but we need to ensure the Endpoint is in server mode on all 256 ports.

**Solution**: Use quinn's `Endpoint` with a server config on each socket. Quinn's accept logic handles unknown CIDs correctly.

### 2. Probe Packet Format
Birthday attack probes must be valid QUIC Initial packets (not raw UDP). Quinn's `Endpoint::connect()` sends a proper Initial, so each probe is a real connection attempt. Failed probes time out naturally.

### 3. Stateful Connections
Unlike WireGuard (stateless), each QUIC probe creates connection state. With 1024 probes, that's 1024 half-open connections. Must aggressively abort losers once one succeeds.

**Solution**: Use `JoinSet` (existing pattern in `dual_path.rs`) and `abort_all()` on first success.

### 4. NAT Pinhole Lifetime
QUIC Initial retransmission timer (1s default) may exceed the NAT pinhole lifetime on aggressive NATs. One probe per port may not be enough.

**Solution**: Send 2-3 Initials per predicted port, 200ms apart.

## Signal Protocol

New variants:

```rust
/// Hard NAT probe coordination — exchanged before birthday attack.
HardNatProbe {
    call_id: String,
    /// Last 5 observed external ports (most recent first).
    port_sequence: Vec<u16>,
    /// Detected allocation pattern.
    allocation: String,  // "sequential:1", "sequential:2", "random", "preserving"
    /// Probe timestamp for synchronization (ms since epoch).
    probe_time_ms: u64,
    /// External IP from STUN.
    external_ip: String,
}

/// Hard NAT birthday attack coordination.
HardNatBirthdayStart {
    call_id: String,
    /// Number of ports opened by the acceptor side.
    acceptor_port_count: u16,
    /// External ports the acceptor has open (for targeted probing).
    /// Only sent if port_count is small enough to enumerate.
    acceptor_ports: Vec<u16>,
    /// "start probing now" timestamp.
    start_at_ms: u64,
}
```

## Integration with Existing Architecture

- **Netcheck**: `NetcheckReport` gains `port_allocation: PortAllocation` field
- **IceAgent**: `gather()` includes port allocation detection; `re_gather()` re-probes on network change
- **dual_path**: `race()` extended with hard-NAT probe phase between standard hole-punch timeout and relay commitment
- **Desktop**: `place_call` / `answer_call` exchange `HardNatProbe` when both sides report `SymmetricPort` NAT type

## Effort Estimate

| Phase | Scope | Effort | Status |
|-------|-------|--------|--------|
| A | Port allocation pattern detection | 1 day | **Done** — `PortAllocation` enum, `detect_port_allocation()`, `classify_port_allocation()`, `predict_ports()`, 17 tests |
| B | Sequential port prediction + coordination | 2 days | **Signal ready** — `HardNatProbe` signal + relay forwarding done. `dual_path::race()` integration pending |
| C | Birthday attack (256 sockets + 1024 probes) | 3 days | Not started |
| D | Hybrid waterfall + background upgrade | 2 days | Not started |

**Total**: ~8 days. Phase A is done and feeds into netcheck. Phase B has signal plumbing complete — needs `dual_path::race()` integration to actually dial predicted ports. Phase C (birthday) is the most complex and lowest ROI.

## Success Criteria

- Port allocation detection correctly classifies sequential vs random on test routers
- Sequential port prediction achieves >70% direct connection rate on sequential-NAT routers
- Birthday attack achieves >90% within 10 seconds when one peer has cone NAT
- Relay-to-direct upgrade is seamless (no audio gap) via Phase 8.3 transport hot-swap
- No regression in call setup time for cone-NAT pairs (the common case)

## References

- [Tailscale: How NAT traversal works](https://tailscale.com/blog/how-nat-traversal-works)
- [Tailscale: NAT traversal improvements pt.1](https://tailscale.com/blog/nat-traversal-improvements-pt-1)
- [Tailscale: NAT traversal improvements pt.2 — cloud environments](https://tailscale.com/blog/nat-traversal-improvements-pt-2-cloud-environments)
- RFC 4787: NAT Behavioral Requirements for Unicast UDP
- RFC 5245: ICE (Interactive Connectivity Establishment)
- Birthday problem: P(collision) = 1 - e^(-n²/2m) where n=probes, m=port space
