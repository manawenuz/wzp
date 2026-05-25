# Design Exploration: Federated Reputation Gossip (T6.3)

> **Status:** Design exploration — no approach selected.
> **Blocked on:** Reviewer design call (needs operator-trust model decision).
> **Scope:** How should WZP relays share abuse verdicts across a federation mesh so that abusers cannot hop between relays?

## Background

WZP relays are E2E-encrypted SFUs. They cannot inspect payload content, but they **can** observe metadata: packet rates, payload sizes, timestamps, keyframe patterns, and BWE responsiveness. Tiers A–F of the conformance pipeline observe these signals and produce a `Verdict ∈ {Legitimate, Suspect, Abusive}`.

Tier G (`ResponsePolicy`) escalates:
- `Abusive` → typed `Hangup` + 1 h fingerprint cool-down
- Repeat `Abusive` within 24 h → relay-local `Block` for 24 h

**The gap:** Block state is in-memory only and per-relay. A blocked fingerprint on relay A can reconnect to relay B (same room, same mesh) and resume abuse immediately. T6.3 explores closing this gap.

**What is being gossiped?** A *reputation event*: "fingerprint `F` produced violation `V` with verdict `Abusive` at time `T` on relay `R`."

---

## Assumptions

1. Relays trust each other *connection-level* (TLS fingerprints in `PeerConfig` / `TrustedConfig`) but are **not** guaranteed to share the same abuse-detection thresholds or calibration.
2. The federation mesh is small (tens of relays, not thousands).
3. False positives happen — a legitimate user on a long lecture call can trigger `Suspect` or even `Abusive` on an aggressively-tuned relay.
4. A compromised relay is a realistic threat model (stolen credentials, operator coercion, buggy calibration).
5. Relays are operated by different entities — there is no single administrative root of trust.

---

## Approach 1: Push Gossip

### Summary
When a relay issues a `Block` action (repeat abusive), it immediately broadcasts a `ReputationEvent` to all peer relays via the existing federation QUIC channels. Peers incorporate the event into their local block lists.

### Wire format

```rust
// New SignalMessage variant
ReputationEvent {
    version: u8,
    /// Fingerprint being reported (the abused party, not the reporter).
    fingerprint: String,
    /// Which violation code triggered the block.
    violation: ViolationCode,
    /// When the block was issued (Unix epoch seconds, u64).
    issued_at: u64,
    /// TTL in seconds (default 86400 = 24 h).
    ttl_secs: u32,
    /// Relay that issued the block (TLS fingerprint hex).
    origin_relay_fp: String,
    /// Ed25519 signature over (fingerprint || violation || issued_at || ttl_secs || origin_relay_fp).
    /// The signing key is the relay's long-term identity key (reused from client handshake identity).
    signature: [u8; 64],
}
```

**What is signed?** The canonical serialization of the payload fields (excluding the signature itself). This prevents replay of old events and binds the event to a specific origin relay.

**Key distribution:** Each relay's Ed25519 public key is published in a well-known endpoint (e.g., `/.well-known/wzp-relay.pub`) or embedded in the `FederationHello` handshake. Verification happens on receipt.

### Sybil resistance

- **Signing requirement:** Only relays with a known Ed25519 pubkey can produce valid events. A rogue relay must first be in the `TrustedConfig` to even connect.
- **Origin attribution:** Every event is traceable to a specific relay. If relay R starts flooding false positives, peers can blacklist R's pubkey or reduce R's event weight to zero.
- **No aggregate thresholding:** This design trusts every signed event equally. A single malicious relay can block any fingerprint across the mesh.

**Mitigation option (not implemented):** Require *k-of-n* independent relay reports before applying a cross-relay block. This introduces complexity (tracking per-fingerprint report counts, decaying counters, handling churn).

### Convergence model

- **Eventual consistency:** Events propagate via multi-hop flood (same mechanism as `GlobalRoomActive`).
- **Bounded staleness:** Events carry TTL. Stale events (> TTL) are ignored.
- **No ordering guarantee:** Two relays may issue conflicting events (relay A blocks F, relay B clears F). Last-writer-wins based on `issued_at`.

### Storage

- **In-memory only:** `HashMap<(fingerprint, origin_relay), ReputationEntry>` with TTL-based eviction.
- **No persistence:** Restarting a relay loses all gossiped state. Re-blocking requires the origin relay to re-gossip or the abuser to re-offend.
- **Memory bound:** ~100 bytes per entry × 10k entries = ~1 MB. Trivial.

### Partition tolerance

- **Partitioned relay A** blocks fingerprint F locally but cannot push to peers. When partition heals, A floods the backlog. Peers apply events with `issued_at` within TTL; expired backlog is ignored.
- **Partitioned relay B** never hears about F's block. F can abuse B freely during the partition. This is acceptable for a best-effort gossip system.

### Failure modes

| Scenario | Impact | Mitigation |
|---|---|---|
| Compromised relay floods false blocks | All fingerprints blocked mesh-wide | Manual pubkey blacklist; no automatic mitigation in this design |
| Buggy relay false-positives a popular fingerprint | Legitimate users blocked everywhere | Operator contact + pubkey downgrade |
| Network partition | Split-brain block lists | Acceptable; partition healing replays backlog |
| Clock skew | Events from future/past rejected or mis-ordered | Use `issued_at` with ±5 min tolerance; NTP assumed |
| Replay attack | Old event re-broadcast after TTL | Signature binds `issued_at`; verify TTL at receipt |

### Complexity

- **Low-medium:** Reuses existing federation broadcast infrastructure. Adds one `SignalMessage` variant, Ed25519 signing/verification, and an in-memory TTL map.

---

## Approach 2: Pull Gossip (Reputation Oracle)

### Summary
One relay in the mesh is designated the **reputation oracle** (or a small quorum of 3). Other relays query the oracle periodically (e.g., every 60 s) for the current block list. The oracle maintains the authoritative reputation state.

### Wire format

```rust
// Pull request
ReputationQuery {
    version: u8,
    /// Last checkpoint the requester has seen (opaque cursor).
    since_cursor: Option<String>,
}

// Pull response
ReputationSnapshot {
    version: u8,
    /// Opaque cursor for delta pagination.
    cursor: String,
    /// List of active blocks at the oracle.
    blocks: Vec<ReputationBlock>,
    /// Oracle's Ed25519 signature over the serialized snapshot.
    signature: [u8; 64],
}

struct ReputationBlock {
    fingerprint: String,
    violation: ViolationCode,
    issued_at: u64,
    ttl_secs: u32,
    /// Which relay originally reported this (for audit).
    reported_by: String,
}
```

**What is signed?** The entire `ReputationSnapshot` serialized canonically. The oracle is the sole signer.

**Oracle selection:** Config-based. Each relay's config names its oracle(s):
```toml
[reputation]
oracle = "https://relay-oracle.example.com"
oracle_pubkey = "AA:BB:CC:..."
```

### Sybil resistance

- **Centralized trust:** The oracle is a single point of trust. If the oracle is honest, no rogue relay can poison the mesh.
- **Oracle compromise:** A compromised oracle can block or unblock any fingerprint across all querying relays. This is a **catastrophic** failure mode.
- **Quorum variant:** 3 oracles with 2-of-3 signing. Queries return the intersection of block lists. More resilient but adds latency and complexity.

### Convergence model

- **Bounded staleness:** Worst-case = query interval (60 s) + network RTT.
- **Strong consistency within staleness bound:** All querying relays see the same oracle state (modulo query timing skew).
- **No multi-hop gossip:** Direct query/response only.

### Storage

- **Oracle side:** In-memory + optional persistence (SQLite or flat file). Oracle restarts reload from disk.
- **Querying relays:** In-memory cache of the last snapshot. No local state between restarts.
- **Memory bound:** Same as Approach 1 (~1 MB for 10k entries).

### Partition tolerance

- **Partitioned querying relay:** Falls back to local-only blocking (current behavior). When partition heals, next query re-syncs.
- **Partitioned oracle:** All querying relays lose cross-relay reputation and fall back to local-only. Oracle restart without persistence = same.
- **No split-brain:** Either you have the oracle snapshot or you don't. No conflicting states.

### Failure modes

| Scenario | Impact | Mitigation |
|---|---|---|
| Oracle compromise | Global block/unblock of any fingerprint | 2-of-3 quorum; operator out-of-band alert |
| Oracle downtime | All querying relays lose cross-relay reputation | Fallback to local-only; no amplification |
| Rogue relay reports to oracle | Oracle may incorporate false positives | Oracle applies local thresholding (e.g., require 2 independent reports) |
| Query amplification | N relays × 60 s = many queries | Oracle caches; responses are cheap |
| Man-in-the-middle on query | Attacker injects fake snapshot | TLS + signature verification on response |

### Complexity

- **Medium:** Adds an HTTP/QUIC query endpoint, snapshot pagination, oracle election/config, and a new failure mode (oracle as SPOF).
- **Operational burden:** Someone must run the oracle. Small federations may not want this.

---

## Approach 3: No Gossip — Explicit Ban-List Distribution

### Summary
Relays do **not** gossip reputation automatically. Instead, a human admin (or an automated audit pipeline) curates an authoritative ban list, signs it, and pushes it to all relays. Relays apply the list verbatim.

### Wire format

```rust
// Signed ban list (distributed out-of-band: HTTPS, SCP, S3, etc.)
BanList {
    version: u8,
    /// Issued at (Unix epoch seconds).
    issued_at: u64,
    /// Expires at (Unix epoch seconds). After this, the list is ignored.
    expires_at: u64,
    /// Entries.
    entries: Vec<BanEntry>,
    /// Admin Ed25519 signature over canonical serialization.
    signature: [u8; 64],
}

struct BanEntry {
    fingerprint: String,
    /// Human-readable reason (not machine-parsed).
    reason: String,
    /// Optional: which relay originally reported.
    source_relay: Option<String>,
}
```

**What is signed?** The entire `BanList`. The admin (not a relay) is the signer.

**Distribution:** Out-of-band from the federation mesh. Could be:
- Admin `scp`s JSON to each relay's config directory
- Relays poll an HTTPS URL every 5 min
- Shared object storage (S3, GCS)

**Key distribution:** Admin pubkey is baked into each relay's config at provisioning time:
```toml
[ban_list]
admin_pubkey = "AA:BB:CC:..."
url = "https://ops.example.com/banlist.json"
refresh_secs = 300
```

### Sybil resistance

- **Strong:** Only the admin can produce a valid ban list. No relay can poison another relay.
- **Admin compromise:** Catastrophic — attacker controls the global ban list. But this is a standard operational threat model (same as DNS root, certificate transparency log, etc.).
- **No relay-to-relay trust required:** Relays don't need to trust each other's calibration or behaviour.

### Convergence model

- **Poll-based bounded staleness:** Worst-case = `refresh_secs` (default 300 s = 5 min).
- **Strong consistency:** All relays that successfully fetch the list see identical state.
- **No event propagation:** No flood, no multi-hop, no deduplication needed.

### Storage

- **On-disk cache:** Each relay stores the latest fetched ban list to survive restart.
- **In-memory lookup:** `HashSet<fingerprint>` for O(1) block checks.
- **Memory bound:** Same as other approaches.

### Partition tolerance

- **Partitioned relay:** Continues using its last cached ban list until `expires_at`. After expiry, falls back to local-only blocking.
- **No split-brain:** Either you have the signed list or you don't.

### Failure modes

| Scenario | Impact | Mitigation |
|---|---|---|
| Admin key compromise | Global block/unblock of any fingerprint | Key rotation + out-of-band alert |
| Distribution channel down | Relays use stale cached list until expiry | Short expiry + monitoring |
| Admin false-positives a popular fingerprint | Global block of legitimate users | Human review process; short expiry allows quick recovery |
| Relay never fetches list | Local-only blocking only | Monitoring alert on relay ops dashboard |
| List too large | Fetch latency, memory bloat | Pagination; but 10k entries = ~500 KB JSON, trivial |

### Complexity

- **Low:** No changes to federation mesh at all. Adds an HTTP polling loop, a file watcher, or an S3 sync. The signing/verification is trivial (one Ed25519 verify on fetch).
- **Operational burden:** Requires a human or automated pipeline to maintain the ban list. This is acceptable for small federations (the WZP target audience) but does not scale to open meshes.

---

## Comparative Summary

| Dimension | Approach 1: Push Gossip | Approach 2: Pull Oracle | Approach 3: Ban-List Distribution |
|---|---|---|---|
| **Trust model** | Every relay trusts every other relay's verdict equally | Trusts a designated oracle (or 2-of-3 quorum) | Trusts a single admin key |
| **Sybil resistance** | Weak — one rogue relay can poison the mesh | Medium-strong — oracle is gatekeeper | Strong — only admin can sign |
| **Convergence** | Eventual; multi-hop flood | Bounded (query interval); direct pull | Bounded (poll interval); out-of-band |
| **Partition tolerance** | Acceptable (backlog replay on heal) | Acceptable (fallback to local) | Good (cached list + expiry) |
| **False-positive blast radius** | Mesh-wide from one relay | Mesh-wide from oracle | Mesh-wide from admin |
| **Operational burden** | Low — fully automatic | Medium — must run oracle | Medium — must curate list |
| **Federation code changes** | Medium — broadcast loop, dedup, signatures | Medium — query endpoint, snapshot pagination | Low — out-of-band, no mesh changes |
| **Scaling** | Poor — flood doesn't scale past ~50 relays | Good — O(N) queries, oracle is O(1) | Good — O(N) fetches, no mesh load |
| **Audit trail** | Good — every event attributed to origin relay | Good — oracle logs all reports | Good — list is a snapshot |
| **Rollback / correction** | Hard — events spread everywhere; need counter-events | Easy — oracle updates snapshot | Easy — admin publishes new list |

## Open Questions (Blockers for Implementation)

1. **Trust model:** Do we trust all peer relays equally (Approach 1), trust a designated oracle (Approach 2), or trust a human admin (Approach 3)? This is a product/operations decision, not a technical one.
2. **Key infrastructure:** The federation layer currently has **no message-level signing**. All three approaches require adding Ed25519 signing/verification to relay-relay messages (or out-of-band fetches). The `wzp-crypto` crate already has Ed25519 identity support (used in client handshake) — it can be reused.
3. **Fingerprint scope:** Is the block scoped to a fingerprint (Ed25519 pubkey), an IP address, or both? Current `ResponsePolicy` uses a string fingerprint. Gossip should probably scope to the cryptographic identity, not IP, to prevent NAT rebind evasion.
4. **Privacy leakage:** Gossiping "fingerprint F is abusive" leaks that F was on a call. Is this acceptable? WZP is not anonymous by design (participants know each other's fingerprints), but operators learning about calls they are not in is a privacy concern.
5. **TTL vs. persistent bans:** Should a gossiped block expire automatically (TTL), or should it require manual review for extension? Automatic expiry limits false-positive damage but allows persistent abusers to cycle.
6. **Rate limiting on gossip:** A compromised relay could flood the mesh with `ReputationEvent` messages. The existing per-room rate limiter (500 pps) doesn't apply to control messages. A per-peer gossip rate limit is needed regardless of approach.

## Recommendation

**Do not implement any approach yet.** The blockers above (especially #1 trust model and #4 privacy) need a reviewer/operator design call. T6.3 should remain **Blocked** until then.

If forced to pick a default for a small, closed federation (the current WZP target audience), **Approach 3 (Ban-List Distribution)** has the lowest complexity, the strongest Sybil resistance, and the easiest rollback. It sacrifices automation but small federations typically have an ops person who can review blocks anyway. Approach 1 (Push Gossip) is more elegant for larger meshes but requires solving the rogue-relay problem first (e.g., k-of-n thresholding).
