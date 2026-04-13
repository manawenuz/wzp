# Codebase Refactoring Audit (2026-04-13)

> Full analysis of the WarzonePhone codebase after the DashMap relay refactor, DRED continuous tuning, and adaptive quality wiring. The codebase is ~15K lines of Rust across 8 crates plus a 1.7K-line Tauri engine. This document identifies every refactoring opportunity ranked by impact.

## Critical: engine.rs is 1,705 Lines With ~35% Duplication

`desktop/src-tauri/src/engine.rs` has two nearly-identical `CallEngine::start()` implementations:
- **Android path:** 880 lines (lines 321–1200)
- **Desktop path:** 430 lines (lines 1203–1633)

### What's Duplicated (350+ lines)

| Block | Android Lines | Desktop Lines | Size | Identical? |
|-------|--------------|---------------|------|-----------|
| CallConfig initialization | 529–539 | 1353–1363 | 23 lines | Yes |
| DRED tuner + frame_samples setup | 541–555 | 1360–1375 | 15 lines | Yes |
| Adaptive quality profile switch | 651–665 | 1414–1428 | 15 lines | Yes |
| Codec-to-QualityProfile match | 852–864 | 1488–1500 | 19 lines | Yes |
| DRED ingest + gap fill | 886–902 | 1511–1528 | 17 lines | Yes |
| Quality report ingestion | 905–912 | 1531–1538 | 8 lines | Yes |
| Signal task (entire thing) | 1133–1180 | 1569–1616 | 48 lines | Yes |

### Suggested Fix: Extract Shared Helpers

```rust
// Top of engine.rs — shared between both platforms

fn build_call_config(quality: &str) -> CallConfig { ... }

fn codec_to_profile(codec: CodecId) -> QualityProfile { ... }

fn check_adaptive_switch(
    pending: &AtomicU8,
    encoder: &mut CallEncoder,
    tuner: &mut DredTuner,
    frame_samples: &mut usize,
    tx_codec: &Mutex<String>,
) { ... }

async fn run_signal_task(
    transport: Arc<QuinnTransport>,
    running: Arc<AtomicBool>,
    pending_profile: Arc<AtomicU8>,
    participants: Arc<Mutex<Vec<ParticipantInfo>>>,
) { ... }
```

This would reduce engine.rs by ~200 lines and make the Android/desktop paths only differ in their audio I/O (Oboe vs CPAL).

**Effort:** 2-3 hours. **Impact:** High — every future change to the send/recv pipeline currently requires editing two places.

---

## High: SignalMessage Enum Has 36 Variants

`crates/wzp-proto/src/packet.rs` (1,727 lines) has a `SignalMessage` enum with 36 variants mixing orthogonal concerns:

- Legacy call signaling (CallOffer, CallAnswer, IceCandidate, Rekey...)
- Direct calling (RegisterPresence, DirectCallOffer, DirectCallAnswer, CallSetup...)
- Federation (FederationHello, GlobalRoomActive/Inactive, FederatedSignalForward)
- Relay control (SessionForward, PresenceUpdate, RouteQuery, RoomUpdate)
- NAT traversal (Reflect, ReflectResponse, MediaPathReport)
- Quality (QualityUpdate, QualityDirective)
- Call control (Ping/Pong, Hold/Unhold, Mute/Unmute, Transfer)

Every new feature adds variants here, and every match on `SignalMessage` must handle all 36 arms (or use `_` wildcard).

### Suggested Fix: Sub-Enum Grouping

```rust
enum SignalMessage {
    Call(CallSignal),           // CallOffer, CallAnswer, IceCandidate, Rekey, Hangup...
    Direct(DirectCallSignal),   // RegisterPresence, DirectCallOffer, CallSetup, MediaPathReport...
    Federation(FedSignal),      // FederationHello, GlobalRoomActive, FederatedSignalForward...
    Control(ControlSignal),     // Ping/Pong, Hold/Unhold, Mute/Unmute, QualityDirective...
    Relay(RelaySignal),         // SessionForward, PresenceUpdate, RouteQuery, RoomUpdate...
}
```

**Caution:** This is a wire-format change. Serde serialization must remain backward-compatible with already-deployed relays. Use `#[serde(untagged)]` or versioned deserialization. Consider doing this as a v2 protocol bump.

**Effort:** 1 day. **Impact:** High for maintainability, but risky for wire compatibility.

---

## High: Federation Has Zero Tests

`crates/wzp-relay/src/federation.rs` (1,132 lines) has **no unit tests and no integration tests**. This is the most complex file in the relay crate, handling:

- Peer link management (connect, reconnect, stale sweep)
- Federation media egress (forward_to_peers)
- Federation media ingress (handle_datagram: dedup, rate limit, local delivery, multi-hop)
- Cross-relay signal forwarding
- Room event subscription and GlobalRoomActive/Inactive broadcasting

The relay crate has 91 tests, but none cover federation. Any refactoring of federation (like the DashMap migration or clone-before-send) is flying blind.

### Suggested Fix

Priority test cases:
1. `forward_to_peers` with 0, 1, 3 peers — verify datagram construction and label tracking
2. `handle_datagram` — dedup (same packet twice → second dropped), rate limit (exceed → dropped)
3. Stale presence sweeper — verify cleanup after timeout
4. `broadcast_signal` — verify signal reaches all peers
5. Multi-hop forward — verify source peer excluded from re-forward

**Effort:** 1 day. **Impact:** Critical for safe refactoring.

---

## Medium: Federation `peer_links` Lock-During-Send

`broadcast_signal()` (line 216) holds `peer_links` Mutex **across async `send_signal()` calls**. A slow peer blocks all signal delivery. `forward_to_peers()` (line 406) holds it during sync sends (less severe but still serializes).

### Fix (30 minutes)

```rust
// Before:
let links = self.peer_links.lock().await;
for (fp, link) in links.iter() {
    link.transport.send_signal(msg).await;  // lock held across await!
}

// After:
let peers: Vec<_> = {
    let links = self.peer_links.lock().await;
    links.values().map(|l| (l.label.clone(), l.transport.clone())).collect()
};
for (label, transport) in &peers {
    transport.send_signal(msg).await;  // no lock held
}
```

Apply to `forward_to_peers()`, `broadcast_signal()`, and `send_signal_to_peer()`.

**Effort:** 30 minutes. **Impact:** Medium — eliminates last lock-during-I/O pattern.

---

## Medium: Magic Numbers Scattered Through engine.rs

```rust
// These appear as literals in multiple places:
tokio::time::sleep(Duration::from_millis(5))    // 6 occurrences
tokio::time::sleep(Duration::from_millis(100))   // 2 occurrences
Duration::from_millis(200)                        // 2 occurrences (signal timeout)
Duration::from_secs(10)                           // 1 occurrence (QUIC connect timeout)
Duration::from_secs(2)                            // 2 occurrences (heartbeat interval)
const DRED_POLL_INTERVAL: u32 = 25;              // defined twice (Android + desktop)
vec![0i16; 1920]                                  // 2 occurrences (should use FRAME_SAMPLES_40MS)
```

### Fix

```rust
// Top of engine.rs
const CAPTURE_POLL_MS: u64 = 5;
const RECV_TIMEOUT_MS: u64 = 100;
const SIGNAL_TIMEOUT_MS: u64 = 200;
const CONNECT_TIMEOUT_SECS: u64 = 10;
const HEARTBEAT_INTERVAL_SECS: u64 = 2;
const DRED_POLL_INTERVAL: u32 = 25;
// Already exists: const FRAME_SAMPLES_40MS: usize = 1920;
```

**Effort:** 15 minutes. **Impact:** Low but prevents bugs from inconsistent values.

---

## Medium: CLI Arg Parsing in Relay main.rs

`parse_args()` in main.rs is 154 lines of manual `while i < args.len()` parsing with `match args[i].as_str()`. Every new flag adds 5-10 lines of boilerplate.

### Suggested Fix

Replace with `clap` derive macro:

```rust
#[derive(clap::Parser)]
struct RelayArgs {
    #[arg(long, default_value = "0.0.0.0:4433")]
    listen: SocketAddr,
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    auth_url: Option<String>,
    // ...
}
```

**Effort:** 1 hour. **Impact:** Medium — cleaner, auto-generates `--help`, validates types at parse time.

---

## Medium: Error Handling Inconsistency

13 instances of `.ok()` silently swallowing errors on `transport.close()` across the relay. Federation signal forwarding has inconsistent error handling — some paths log, some don't.

### Fix

```rust
// Helper at top of main.rs/federation.rs:
async fn close_transport(t: &impl MediaTransport, context: &str) {
    if let Err(e) = t.close().await {
        tracing::debug!(context, error = %e, "transport close error (non-fatal)");
    }
}
```

**Effort:** 30 minutes. **Impact:** Better observability when debugging connection issues.

---

## Low: Unused Crypto Fields

`crates/wzp-crypto/src/handshake.rs` has `x25519_static_secret` and `x25519_static_public` fields marked `#[allow(dead_code)]`. These are derived from the identity seed but never used in any handshake flow.

**Decision needed:** Are these intended for a future feature (static key federation auth)? If not, remove. If yes, document the intended use.

**Effort:** 5 minutes to remove, or 10 minutes to document.

---

## Low: 20 Unsafe Functions Missing Safety Docs

`crates/wzp-native/src/lib.rs` has 20 `unsafe` functions (extern "C" FFI bridge to Oboe) without `/// # Safety` documentation. Clippy flags all of them.

**Effort:** 30 minutes. **Impact:** Clippy clean, better documentation for contributors.

---

## Low: quality.rs vs dred_tuner.rs Overlap

Both files deal with network quality → codec decisions, but they're complementary:
- `quality.rs`: discrete tier classification (Good/Degraded/Catastrophic) → codec profile
- `dred_tuner.rs`: continuous DRED frame mapping from loss/RTT/jitter

No consolidation needed, but add cross-references:

```rust
// In dred_tuner.rs:
//! See also: `quality.rs` for discrete tier classification that drives
//! codec switching. DredTuner operates within a tier, adjusting DRED
//! parameters continuously.

// In quality.rs:
//! See also: `dred_tuner.rs` for continuous DRED tuning within a tier.
```

**Effort:** 5 minutes.

---

## Summary: Priority Matrix

| # | Refactor | Effort | Impact | Risk |
|---|----------|--------|--------|------|
| 1 | Extract shared engine.rs helpers | 2-3h | High | Low |
| 2 | Federation tests | 1 day | Critical | None |
| 3 | Federation clone-before-send | 30 min | Medium | Low |
| 4 | Extract magic numbers to constants | 15 min | Low | None |
| 5 | Error handling helpers | 30 min | Medium | None |
| 6 | CLI parser → clap | 1h | Medium | Low |
| 7 | SignalMessage sub-enums | 1 day | High | High (wire compat) |
| 8 | Safety docs on unsafe fns | 30 min | Low | None |
| 9 | Remove/document dead crypto fields | 5 min | Low | None |
| 10 | Cross-reference quality.rs ↔ dred_tuner.rs | 5 min | Low | None |

**Recommended order:** 4 → 3 → 5 → 1 → 2 → 6 → 8 → 9 → 10 → 7

Items 4, 3, 5 are quick wins (under 1 hour total). Item 1 is the biggest maintainability win. Item 2 is the most important for safety. Item 7 should wait for a protocol version bump.
