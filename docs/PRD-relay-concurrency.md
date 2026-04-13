# PRD: Relay Concurrency — DashMap Room Sharding

## Problem

The relay's media forwarding hot path routes every packet through a single `Arc<Mutex<RoomManager>>`. In a room with N participants, all N per-participant tasks compete for this one lock on every packet. The lock hold time is short (~1ms, no I/O), but the serialization means a 100-participant room effectively runs single-threaded despite having a multi-core tokio runtime.

Separately, the federation manager holds `peer_links` locked across multiple network sends, meaning a slow federation peer blocks all others.

### Measured bottleneck (from code audit)

```
Per-packet hot path (room.rs:748-757, 968-976):
  lock(room_mgr)
    → observe_quality()    O(N) iterate qualities HashMap
    → others()             O(M) clone Vec<ParticipantSender>
  unlock
  → fan-out sends          sequential, no lock held
```

Lock contention = O(N) per room per packet, where N = participants in the room.

### Current lock inventory (hot path only)

| Lock | Location | Hold Duration | I/O While Locked | Frequency |
|------|----------|---------------|-------------------|-----------|
| `RoomManager` | room.rs:749, 968 | ~1ms | No | Every packet, every participant |
| `RoomManager` | room.rs:845, 1041 | <1ms | No | Every 5s per participant |
| `RoomManager` | room.rs:870 | ~1ms | No (explicit `drop` before broadcast) | On leave |
| `peer_links` | federation.rs:409 | N × send latency | **YES** — `send_raw_datagram` in loop | Every federation packet |
| `peer_links` | federation.rs:216 | N × send latency | **YES** — `send_signal` in loop | Every federation signal |
| `dedup` | federation.rs:1066 | <1ms | No | Every federation ingress packet |
| `rate_limiters` | federation.rs:1113 | <1ms | No | Every federation ingress packet |

### Scaling impact

| Room Size | Effective Core Usage | Bottleneck |
|-----------|---------------------|------------|
| 3 people × 100 rooms | All cores | None |
| 10 people × 10 rooms | Most cores | Mild contention per room |
| 100 people × 1 room | ~1 core | RoomManager lock |
| 1000 people × 1 room | ~1 core | Severely serialized |

## Goals

- Eliminate the global RoomManager Mutex as a serialization point for media forwarding
- Allow per-room parallelism: packets in room A don't block packets in room B
- Fix federation `peer_links` lock held across network sends
- Maintain correctness: no double-delivery, no stale participant lists
- Zero-copy or minimal-clone for fan-out participant lists
- Keep the refactor incremental — each phase independently shippable

## Non-Goals

- Lock-free data structures (overkill for our scale; DashMap or per-room Mutex is sufficient)
- Changing the SFU forwarding model (no mixing, no transcoding)
- Optimizing single-room beyond ~1000 participants (conferencing at that scale needs a different architecture)
- Changing the wire protocol or client behavior

## Design Options Evaluated

### Option A: Per-Room `Arc<Mutex<Room>>`

**Approach:** Replace `HashMap<String, Room>` inside RoomManager with `HashMap<String, Arc<Mutex<Room>>>`. The outer HashMap is protected by a short-lived lock for room lookup only; the per-room lock protects participant state.

```rust
struct RoomManager {
    rooms: Mutex<HashMap<String, Arc<Mutex<Room>>>>,  // outer: room lookup
    // ...
}

// Hot path becomes:
let room_arc = {
    let rooms = room_mgr.rooms.lock().await;
    rooms.get(&room_name).cloned()  // Arc clone, <1ns
};  // outer lock released

if let Some(room) = room_arc {
    let room = room.lock().await;  // per-room lock
    let others = room.others(participant_id);
    drop(room);
    // fan-out sends...
}
```

**Pros:**
- Rooms are fully independent — room A's lock doesn't block room B
- Minimal code change (~50 lines)
- Per-room lock contention = O(participants in that room), not O(total participants)
- Outer lock held for <1μs (just a HashMap get + Arc clone)

**Cons:**
- Two-level locking (room lookup + room lock) — slightly more complex
- Room creation/deletion still serialized through outer lock (acceptable, rare operation)
- Quality tracking needs to move into the Room struct

**Verdict: Best option. Biggest win for least effort.**

### Option B: `DashMap<String, Room>`

**Approach:** Replace `Mutex<HashMap<String, Room>>` with `dashmap::DashMap<String, Room>`. DashMap uses internal sharding (default 64 shards) with per-shard RwLocks.

```rust
struct RoomManager {
    rooms: DashMap<String, Room>,
}

// Hot path:
if let Some(room) = room_mgr.rooms.get(&room_name) {
    let others = room.others(participant_id);  // read lock on shard
    drop(room);  // release shard lock
    // fan-out sends...
}
```

**Pros:**
- No explicit locking in user code
- Built-in sharding (64 shards by default)
- Read-heavy workload benefits from RwLock per shard

**Cons:**
- New dependency (`dashmap` crate)
- DashMap guards can't be held across `.await` points (not `Send`)
- Mutable operations (join/leave/quality update) need `get_mut()` which takes exclusive shard lock
- Less control over lock granularity than Option A
- Quality tracking across rooms becomes awkward (can't iterate all rooms while holding one shard)

**Verdict: Good but Option A is simpler and more explicit.**

### Option C: Channel-Based Fan-Out

**Approach:** Replace direct `send_media()` calls with per-participant `mpsc::Sender` channels. Room join registers a sender; the forwarding loop just does `tx.send(pkt)` which is lock-free.

```rust
struct Room {
    participants: Vec<(ParticipantId, mpsc::Sender<MediaPacket>)>,
}

// Each participant's task:
let (tx, mut rx) = mpsc::channel(64);
room_mgr.join(room, participant_id, tx);

// Forwarding in recv loop:
let senders = room.others(participant_id);  // Vec<mpsc::Sender> clone
for tx in &senders {
    let _ = tx.try_send(pkt.clone());  // non-blocking, no lock
}
```

**Pros:**
- Fan-out is completely lock-free (channel send is atomic)
- Backpressure per participant (full channel = drop packet, not block others)
- Natural decoupling: recv task → channel → send task

**Cons:**
- Requires cloning MediaPacket per participant (currently we clone ParticipantSender Arc, much cheaper)
- Additional memory: 64-packet channel buffer × N participants
- Still need a lock to get the sender list (unless we snapshot on join/leave)
- Adds latency: channel hop + wake adds ~1-5μs vs direct send

**Verdict: Over-engineered for current scale. Consider for 1000+ participant rooms.**

### Option D: Snapshot-on-Change (Optimistic Read)

**Approach:** Maintain a read-optimized `Arc<Vec<ParticipantSender>>` snapshot per room. Updated atomically on join/leave (rare). Readers just `Arc::clone()` — no lock at all.

```rust
struct Room {
    participants: Vec<Participant>,
    /// Atomically-updated snapshot of all senders (rebuilt on join/leave).
    sender_snapshot: Arc<ArcSwap<Vec<ParticipantSender>>>,
}

// Hot path (zero locking!):
let senders = room.sender_snapshot.load();  // atomic load, ~1ns
for sender in senders.iter() {
    if sender.id != participant_id { ... }
}
```

**Pros:**
- Zero lock contention on hot path — just an atomic pointer load
- Rebuild cost amortized over all packets between joins/leaves
- `arc-swap` crate is battle-tested and tiny

**Cons:**
- New dependency (`arc-swap`)
- Quality tracking still needs a mutable path (separate concern)
- Snapshot doesn't include mutable room state (quality tiers)
- More complex join/leave (must rebuild snapshot atomically)

**Verdict: Best theoretical performance, but adds complexity. Consider if DashMap proves insufficient.**

## Recommended Implementation: Option B (DashMap) + Federation Fix

DashMap is the right tool here. The original objections don't hold up:

- "Guards can't be held across `.await`" — we already drop locks before any async sends
- "Less control" — DashMap's 64 internal shards give finer granularity than manual per-room locks
- "New dependency" — one crate, battle-tested, widely used in the Rust ecosystem

DashMap's advantages over manual per-room `Arc<Mutex<Room>>`:
- **No two-level locking** — single `rooms.get()` vs outer-lock → Arc clone → drop → inner-lock
- **Read/write separation** — `get()` is a shared shard lock, multiple rooms on the same shard can read concurrently
- **Less code** — no manual Arc/Mutex wrapping, no explicit lock choreography
- **Iteration without global lock** — federation room announcements don't block media forwarding

### Phase 1: DashMap Room Storage (Biggest Win)

1. Add `dashmap` dependency to `wzp-relay`
2. Replace `rooms: HashMap<String, Room>` with `rooms: DashMap<String, Room>`
3. Move `qualities` and `room_tiers` into the `Room` struct (per-room state, not global)
4. RoomManager no longer needs a wrapping Mutex — it becomes `Arc<RoomManager>` directly
5. Per-packet hot path: `rooms.get(&name)` takes a shared shard lock, releases on drop

```rust
pub struct RoomManager {
    rooms: DashMap<String, Room>,
    acl: Option<HashMap<String, HashSet<String>>>,  // read-only after init
    event_tx: broadcast::Sender<RoomEvent>,
}

struct Room {
    participants: Vec<Participant>,
    qualities: HashMap<ParticipantId, ParticipantQuality>,
    current_tier: Tier,
}

// Hot path becomes:
let (others, directive) = if let Some(mut room) = room_mgr.rooms.get_mut(&room_name) {
    let directive = if let Some(ref qr) = pkt.quality_report {
        room.observe_quality(participant_id, qr)
    } else {
        None
    };
    let o = room.others(participant_id);
    (o, directive)
} else {
    (vec![], None)
};
// Shard lock released here — fan-out sends are lock-free
```

**Files to modify:**
- `crates/wzp-relay/Cargo.toml` — add `dashmap` dependency
- `crates/wzp-relay/src/room.rs` — RoomManager struct, Room struct, all methods
- `crates/wzp-relay/src/lib.rs` — change from `Arc<Mutex<RoomManager>>` to `Arc<RoomManager>`
- `crates/wzp-relay/src/main.rs` — update RoomManager construction and all `.lock().await` call sites
- `crates/wzp-relay/src/federation.rs` — update room_mgr usage (no more `.lock().await`)

**Key behavior change:** `Arc<Mutex<RoomManager>>` → `Arc<RoomManager>`. Every call site that does `room_mgr.lock().await.some_method()` becomes `room_mgr.some_method()` directly. The DashMap handles internal locking.

**Concurrency improvement:**
- Before: 100 rooms × 10 people = all 1000 tasks compete for 1 Mutex
- After: 100 rooms × 10 people = distributed across 64 shards, ~15 tasks per shard average
- Within a room: participants still serialize through the shard lock, but hold time is <0.1ms for `get()` and `others()` (just Vec clone of Arcs)

### Phase 2: Federation Lock Fix

Clone the peer list, release lock, then send:

```rust
pub async fn forward_to_peers(&self, room_hash: &[u8; 8], media_data: &Bytes) {
    let peers: Vec<_> = {
        let links = self.peer_links.lock().await;
        links.values().map(|l| (l.label.clone(), l.transport.clone())).collect()
    };  // lock released immediately
    
    for (label, transport) in &peers {
        // send without holding lock — slow peer doesn't block others
    }
}
```

Also apply to `broadcast_signal()` and `send_signal_to_peer()`.

**Files to modify:**
- `crates/wzp-relay/src/federation.rs` — 3 methods

**Concurrency improvement:** A slow federation peer no longer blocks all other peers' media delivery.

### Phase 3: Quality Tracking Optimization (Optional)

With DashMap, quality tracking uses `get_mut()` (exclusive shard lock) on every packet that carries a QualityReport. For rooms where quality reports are frequent, this creates write contention on the shard.

Option: Move quality observation to a background task:
1. Per-participant `AtomicU8` for latest loss/RTT (lock-free write from hot path)
2. Background task every 1s reads atomics, computes tiers, broadcasts directives
3. Hot path becomes read-only: `rooms.get()` (shared lock) → `others()` → done

**Reduces shard lock from exclusive (`get_mut`) to shared (`get`) on every packet.**

## Verification

1. **Correctness:** `cargo test -p wzp-relay` — all existing tests must pass
2. **Compile check:** `cargo check --workspace` — no regressions
3. **Load test:** 10 rooms × 10 participants, verify rooms forward concurrently
4. **Large room:** 1 room × 50 participants, no deadlocks
5. **Federation:** 3 relays, media bridges correctly with new lock pattern
6. **Benchmark:** Before/after packets-per-second on multi-core with `wzp-bench`

## Effort

- Phase 1: 1 day (DashMap migration + test updates)
- Phase 2: 0.5 day (federation clone-and-release)
- Phase 3: 0.5 day (optional, quality tracking with atomics)
- Total: 1.5–2 days

## Implementation Status (2026-04-13)

Phase 1 (DashMap): DONE — global Mutex → DashMap<String, Room> with 64 shards
Phase 2 (Federation clone-before-send): DONE — forward_to_peers, broadcast_signal, send_signal_to_peer
Phase 3 (Quality atomics): NOT DONE — optional optimization

See also: docs/REFACTOR-relay-concurrency.md for the full post-refactor analysis.
