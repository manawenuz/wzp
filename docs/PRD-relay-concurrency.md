# PRD: Relay Concurrency — Per-Room Lock Sharding

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

**Verdict: Best theoretical performance, but adds complexity. Worth it if Option A proves insufficient.**

## Recommended Implementation: Option A + Federation Fix

### Phase 1: Per-Room Locks (Biggest Win)

1. Move `qualities` and `room_tiers` into the `Room` struct (they're per-room anyway)
2. Wrap each Room in `Arc<Mutex<Room>>`
3. RoomManager outer lock becomes a thin room-lookup layer
4. Per-packet hot path acquires only the per-room lock

**Files to modify:**
- `crates/wzp-relay/src/room.rs` — Room struct, RoomManager refactor
- `crates/wzp-relay/src/lib.rs` — re-exports if needed

**Expected change:** ~100 lines modified, ~20 new

**Concurrency improvement:**
- Before: 100 rooms × 10 people = all 1000 tasks compete for 1 lock
- After: 100 rooms × 10 people = 10 tasks compete for 1 lock per room (100× improvement)

### Phase 2: Federation Lock Fix

Fix `forward_to_peers()` and `broadcast_signal()` to clone the peer list, release the lock, then send:

```rust
pub async fn forward_to_peers(&self, room_hash: &[u8; 8], media_data: &Bytes) {
    let peers: Vec<_> = {
        let links = self.peer_links.lock().await;
        links.values().map(|l| (l.label.clone(), l.transport.clone())).collect()
    };  // lock released
    
    for (label, transport) in &peers {
        // send without holding lock
    }
}
```

**Files to modify:**
- `crates/wzp-relay/src/federation.rs` — `forward_to_peers()`, `broadcast_signal()`, `send_signal_to_peer()`

**Expected change:** ~30 lines modified

**Concurrency improvement:** Federation sends no longer block each other or room operations.

### Phase 3: Quality Tracking Optimization (Optional)

Move `observe_quality()` out of the per-packet critical path:

1. Accumulate quality reports in a lock-free counter per participant
2. A background task (every 1s) reads counters, computes tiers, broadcasts directives
3. Per-packet path becomes: `lock → others() → unlock` (no quality computation)

**Reduces per-packet lock hold time from ~1ms to ~0.1ms.**

## Verification

1. **Correctness:** Run existing relay tests (`cargo test -p wzp-relay`) — must pass
2. **Load test:** 10 rooms × 10 participants, verify all 10 rooms forward concurrently
3. **Large room test:** 1 room × 50 participants, verify no deadlocks
4. **Federation test:** 3 relays, verify media still bridges with new lock pattern
5. **Benchmark:** Before/after packets-per-second on a multi-core machine with `wzp-bench`

## Effort

- Phase 1: 1 day
- Phase 2: 0.5 day
- Phase 3: 1 day (optional)
- Total: 1.5–2.5 days
