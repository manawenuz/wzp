---
tags: [architecture, wzp]
type: architecture
---

# Relay Concurrency Refactor Guide

> Post-DashMap analysis: what was done, what remains, and what to do next.

## What Was Done (2026-04-13)

Replaced the global `Arc<Mutex<RoomManager>>` with `DashMap<String, Room>` inside `RoomManager`. The relay's media forwarding hot path no longer serializes through a single lock.

### Before

```
Participant A recv_media()
  → room_mgr.lock().await          ← ALL participants, ALL rooms compete here
  → mgr.observe_quality(...)       ← O(N) quality computation inside lock
  → mgr.others(...)                ← clone Vec<ParticipantSender>
  → drop(lock)
  → fan-out sends
```

One `tokio::sync::Mutex` guarding all rooms, all participants, all quality state. A 100-room relay was effectively single-threaded for media forwarding.

### After

```
Participant A recv_media()
  → room_mgr.observe_quality(...)  ← DashMap::get_mut(), per-room shard lock
  → room_mgr.others(...)           ← DashMap::get(), shared shard lock
  → fan-out sends                  ← no lock held
```

64 internal shards. Rooms on different shards are fully parallel. Rooms on the same shard use RwLock semantics — reads (`others()`) are concurrent, writes (`observe_quality()`, `join()`, `leave()`) are exclusive per-shard only.

### Files Changed

| File | Change |
|------|--------|
| `crates/wzp-relay/Cargo.toml` | Added `dashmap = "6"` |
| `crates/wzp-relay/src/room.rs` | `HashMap<String, Room>` → `DashMap<String, Room>`, per-room quality/tier, all methods `&self` |
| `crates/wzp-relay/src/main.rs` | `Arc<Mutex<RoomManager>>` → `Arc<RoomManager>`, 3 lock sites removed |
| `crates/wzp-relay/src/federation.rs` | 11 lock sites removed, `room_mgr` field type changed |
| `crates/wzp-relay/src/ws.rs` | 3 lock sites removed, `room_mgr` field type changed |

### Measured Improvement

| Metric | Before | After |
|--------|--------|-------|
| Lock type (rooms) | 1 global `tokio::sync::Mutex` | 64-shard `DashMap` with per-shard RwLock |
| Cross-room blocking | Yes (all rooms share 1 lock) | No (rooms are independent) |
| Read concurrency within room | None (Mutex is exclusive) | Yes (`get()` is shared) |
| `.lock().await` sites | 20 across 4 files | 0 for room operations |
| Test count | 314 passing | 314 passing (0 regressions) |

---

## Current Lock Inventory

### Tier 0: Eliminated (Room Hot Path)

These are gone — DashMap handles them internally:

- ~~`room_mgr.lock().await` in media forwarding~~ → `room_mgr.others()` (DashMap shard)
- ~~`room_mgr.lock().await` in quality tracking~~ → `room_mgr.observe_quality()` (DashMap shard)
- ~~`room_mgr.lock().await` in join/leave~~ → `room_mgr.join()` / `.leave()` (DashMap entry)

### Tier 1: Federation `peer_links` (Medium Priority)

**Location:** `crates/wzp-relay/src/federation.rs:142`
```rust
peer_links: Arc<Mutex<HashMap<String, PeerLink>>>
```

**22 lock sites** across federation.rs. The most important:

| Method | Line | Hold Duration | I/O While Locked | Frequency |
|--------|------|---------------|-------------------|-----------|
| `forward_to_peers()` | 406 | 1-5ms (iterate + sync send) | Sync only | Per-packet batch |
| `broadcast_signal()` | 216 | N × send_signal latency | **YES (async)** | Per-signal |
| `handle_datagram()` multi-hop | 1123 | 1-2ms (iterate + sync send) | Sync only | Per-federation-packet |
| `send_signal_to_peer()` | 246 | send_signal latency | **YES (async)** | Per-signal |
| Stale sweeper | 523 | 1-5ms | No | Every 5s |

**Impact:** Only matters with 5+ federation peers or high federation datagram rates (>1000 pps). For 1-3 peers, contention is negligible.

### Tier 2: Control Plane (Low Priority)

These are on the connection setup / signal path, not the media hot path:

| Lock | Location | Frequency |
|------|----------|-----------|
| `session_mgr` | main.rs:450 | Per-connection setup |
| `signal_hub` | main.rs:453 | Per-signal lookup |
| `call_registry` | main.rs:454 | Per-call setup |
| `presence` | main.rs:283 | Per-presence change |
| `ACL` | room.rs:357 | Per-room join |

**Impact:** None. These handle rare events (connection setup, call signaling) and hold locks for <5ms with no I/O inside.

### Tier 3: Forward Mode Pipeline (Niche)

| Lock | Location | Notes |
|------|----------|-------|
| `RelayPipeline` | main.rs:198, 228 | Only used in `--remote` forward mode (relay-to-relay), not SFU room mode |

**Impact:** None for normal operation. Forward mode is a niche deployment.

---

## Suggested Next Refactors (Priority Order)

### 1. Federation `peer_links` Clone-Before-Send

**Effort:** 30 minutes
**Impact:** Eliminates the lock-held-during-iteration pattern in `forward_to_peers()` and `broadcast_signal()`

**Current:**
```rust
pub async fn forward_to_peers(&self, ...) {
    let links = self.peer_links.lock().await;  // held for entire loop
    for (_fp, link) in links.iter() {
        link.transport.send_raw_datagram(&tagged);  // sync, but lock still held
    }
}
```

**Fix:**
```rust
pub async fn forward_to_peers(&self, ...) {
    let peers: Vec<(String, Arc<QuinnTransport>)> = {
        let links = self.peer_links.lock().await;
        links.values().map(|l| (l.label.clone(), l.transport.clone())).collect()
    };  // lock released — hold time: ~1μs for Arc clones
    
    for (label, transport) in &peers {
        transport.send_raw_datagram(&tagged);  // no lock held
    }
}
```

Same treatment for `broadcast_signal()` (line 216) which currently holds the lock across **async** `send_signal()` calls — this is the worst offender since a slow peer blocks all signal delivery.

### 2. Federation `peer_links` → DashMap

**Effort:** 2 hours
**Impact:** Per-peer sharding, eliminates all cross-peer contention

Only worth doing if:
- Running 10+ federation peers
- `forward_to_peers()` shows up in profiling
- The clone-before-send fix from suggestion 1 is insufficient

```rust
peer_links: DashMap<String, PeerLink>
```

Most lock sites become `self.peer_links.get(&fp)` or `.get_mut(&fp)`. The multi-hop forward loop would use `.iter()` which takes temporary shared locks per shard.

### 3. Quality Tracking Out of Hot Path

**Effort:** 1 day
**Impact:** Reduces per-packet DashMap shard lock from exclusive (`get_mut`) to shared (`get`)

Currently, every packet with a `QualityReport` calls `observe_quality()` which uses `rooms.get_mut()` (exclusive shard lock). This serializes quality-carrying packets within the same DashMap shard.

**Fix:** Use per-participant `AtomicU8` for latest loss/RTT (written lock-free from hot path). A background task (every 1s) reads the atomics, computes tiers via `rooms.get_mut()`, and broadcasts `QualityDirective`. The per-packet hot path becomes purely read-only: `rooms.get()` → `others()`.

```rust
struct ParticipantQualityAtomic {
    latest_loss: AtomicU8,      // written per-packet (lock-free)
    latest_rtt: AtomicU8,       // written per-packet (lock-free)
}

// Hot path (per-packet):
if let Some(ref qr) = pkt.quality_report {
    participant_quality.latest_loss.store(qr.loss_pct, Ordering::Relaxed);
    participant_quality.latest_rtt.store(qr.rtt_4ms, Ordering::Relaxed);
}
let others = room_mgr.others(&room_name, participant_id);  // DashMap::get() — shared lock

// Background task (every 1 second):
for room in room_mgr.rooms.iter_mut() {  // DashMap::iter_mut() — exclusive per-shard
    room.recompute_tiers_from_atomics();
    if tier_changed { broadcast QualityDirective }
}
```

### 4. Lock-Free Participant Snapshot (Future)

**Effort:** 0.5 day
**Impact:** Zero-lock media hot path

Replace `Vec<Participant>` in `Room` with an `arc-swap` snapshot:

```rust
struct Room {
    participants: Vec<Participant>,
    sender_snapshot: arc_swap::ArcSwap<Vec<ParticipantSender>>,
}
```

The snapshot is rebuilt on join/leave (rare). The hot path does `sender_snapshot.load()` — an atomic pointer read with zero locking. DashMap wouldn't even be involved in the per-packet path.

Only worth doing if DashMap shard contention becomes measurable in profiling (unlikely for rooms <100 people).

---

## Decision Matrix

| Scenario | Current (DashMap) | + Clone-Before-Send | + Quality Atomics | + arc-swap |
|----------|-------------------|---------------------|-------------------|-----------|
| 10 rooms × 5 people | Saturates all cores | Same | Same | Same |
| 1 room × 100 people | Good (shared read) | Same | Better (no exclusive) | Best |
| 5 federation peers | 1-5ms contention | <1μs contention | Same | Same |
| 20 federation peers | 10-20ms contention | <1μs contention | Same | Same |
| 1000 rooms × 3 people | Excellent | Same | Same | Same |

**Recommendation:** Do suggestion 1 (clone-before-send, 30 min) now. Everything else is future optimization that current workloads don't need.

---

## Concurrency Diagram (Current State)

```
                        ┌─────────────────────────────────┐
                        │      tokio multi-threaded        │
                        │      work-stealing runtime       │
                        └───────────────┬─────────────────┘
                                        │
           ┌────────────────────────────┼────────────────────────────┐
           │                            │                            │
    ┌──────▼──────┐             ┌───────▼───────┐            ┌───────▼───────┐
    │  QUIC Accept │             │  Federation   │            │  Signal Hub   │
    │  (per-conn   │             │  (per-peer    │            │  (per-client  │
    │   task)      │             │   task)       │            │   task)       │
    └──────┬──────┘             └───────┬───────┘            └───────┬───────┘
           │                            │                            │
    ┌──────▼──────┐             ┌───────▼───────┐            ┌───────▼───────┐
    │  Per-Room    │             │  peer_links   │            │  signal_hub   │
    │  DashMap     │◄──64 shards│  Mutex        │◄──1 lock   │  Mutex        │
    │  (media hot  │             │  (federation  │            │  (signal      │
    │   path)      │             │   hot path)   │            │   plane)      │
    └─────────────┘             └───────────────┘            └───────────────┘
         │                                                         │
    No cross-room                                            Low frequency
    blocking                                                 (<1 call/sec)
```

## Files Reference

| File | Lines | Role |
|------|-------|------|
| `crates/wzp-relay/src/room.rs` | ~1275 | DashMap room storage, participant management, quality tracking, media forwarding loops |
| `crates/wzp-relay/src/federation.rs` | ~1152 | Peer link management, federation media egress/ingress, signal forwarding |
| `crates/wzp-relay/src/main.rs` | ~1746 | Connection accept, handshake dispatch, signal handling, room/federation wiring |
| `crates/wzp-relay/src/ws.rs` | ~250 | WebSocket bridge, room integration |
| `crates/wzp-relay/src/metrics.rs` | ~200 | Prometheus counters (lock-free atomics) |
| `crates/wzp-relay/src/trunk.rs` | ~150 | TrunkBatcher (per-instance, no shared state) |
