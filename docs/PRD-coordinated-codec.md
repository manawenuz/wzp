# PRD: Coordinated Codec Switching (Relay-Judged Quality)

## Problem

The current adaptive quality system (`QualityAdapter` in call.rs) exists but isn't wired into either engine. Clients encode at a fixed quality chosen at call start. When network conditions change mid-call, audio degrades instead of gracefully stepping down. When conditions improve, clients stay on low quality unnecessarily.

Additionally, in SFU mode with multiple participants, uncoordinated codec switching creates asymmetry: if client A upgrades to 64k while B stays on 24k, bandwidth is wasted. Participants should switch together.

## Solution

The **relay acts as the quality judge** since it sees both sides of every connection. It monitors packet loss, jitter, and RTT per participant, then signals quality recommendations. Clients react to these signals with coordinated codec switches.

## Architecture

```
┌─────────┐        ┌─────────┐        ┌─────────┐
│ Client A │◄──────►│  Relay  │◄──────►│ Client B │
│          │        │ (judge) │        │          │
│ Encoder  │        │         │        │ Encoder  │
│ Decoder  │        │ Monitor │        │ Decoder  │
└─────────┘        │ per-peer│        └─────────┘
                    │ quality │
                    └────┬────┘
                         │
                    Quality Signals:
                    - StableSignal (conditions good)
                    - DegradeSignal (conditions bad)
                    - UpgradeProposal (try higher quality?)
                    - UpgradeConfirm (all agreed, switch at T)
```

## Quality Classification (Relay-Side)

The relay monitors each participant's connection quality:

| Condition | Classification | Action |
|-----------|---------------|--------|
| loss >= 15% OR RTT >= 200ms | Critical | Immediate downgrade signal |
| loss >= 5% OR RTT >= 100ms | Degraded | Downgrade signal after 3 reports |
| loss < 2% AND RTT < 80ms | Good | Stable signal |
| loss < 1% AND RTT < 50ms for 30s | Excellent | Upgrade proposal |
| loss < 0.5% AND RTT < 30ms for 60s | Studio | Studio upgrade proposal |

## Coordinated Switching Protocol

### Downgrade (fast, safety-first)

1. Relay detects degradation for ANY participant
2. Relay sends `QualityUpdate { recommended_profile: DEGRADED }` to ALL participants
3. ALL participants immediately switch encoder to the recommended profile
4. No negotiation — downgrade is mandatory and instant

### Upgrade (slow, consensual)

1. Relay detects sustained good conditions for ALL participants (threshold: 30s stable)
2. Relay sends `UpgradeProposal { target_profile, switch_timestamp }` to all
3. Each client responds: `UpgradeAccept` or `UpgradeReject`
4. If ALL accept within 5s → Relay sends `UpgradeConfirm { profile, switch_at_ms }`
5. All clients switch encoder at the agreed timestamp (relative to session clock)
6. If ANY rejects or times out → upgrade cancelled, stay on current profile

### Asymmetric Encoding (SFU optimization)

In SFU mode, each client encodes independently. The relay could allow:
- Client A (strong connection): encode at 64k
- Client B (weak connection): encode at 6k
- Relay forwards A's 64k to B's decoder (auto-switch handles it)
- B benefits from A's quality without needing to send at 64k

This requires NO protocol changes — just each client independently following the relay's recommendation for their own encoding quality. The decoder already handles any codec.

### Split Network Consideration

If participant A has great quality but participant C has terrible quality:
- Option 1: **Match weakest link** — everyone encodes at C's level (current approach, simple)
- Option 2: **Per-participant recommendations** — A encodes at 64k, C encodes at 6k. B (good connection) receives and decodes both. Works because decoders auto-switch per packet.
- Option 3: **Relay transcoding** — relay re-encodes A's 64k as 6k for C. Adds CPU on relay, but saves bandwidth for C. Future feature.

Recommended: start with Option 1 (match weakest), add Option 2 later.

## Signal Messages (New/Modified)

```rust
/// Quality signal from relay to client
QualityDirective {
    /// Recommended profile to use for encoding
    recommended_profile: QualityProfile,
    /// Reason for the recommendation
    reason: QualityReason,
}

enum QualityReason {
    /// Network conditions require this quality level
    NetworkCondition,
    /// Coordinated upgrade — all participants agreed
    CoordinatedUpgrade,
    /// Coordinated downgrade — weakest link determines level
    CoordinatedDowngrade,
}

/// Upgrade proposal from relay
UpgradeProposal {
    target_profile: QualityProfile,
    /// Milliseconds from now when the switch would happen
    switch_delay_ms: u32,
}

/// Client response to upgrade proposal
UpgradeResponse {
    accepted: bool,
}

/// Confirmed upgrade — all clients switch at this time
UpgradeConfirm {
    profile: QualityProfile,
    /// Session-relative timestamp to switch (ms since call start)
    switch_at_session_ms: u64,
}
```

## Relay-Side Implementation

### Per-Participant Quality Tracking

```rust
struct ParticipantQuality {
    /// Sliding window of recent observations
    loss_samples: VecDeque<f32>,    // last 30 seconds
    rtt_samples: VecDeque<u32>,     // last 30 seconds
    jitter_samples: VecDeque<u32>,
    /// Current classification
    classification: QualityClass,
    /// How long current classification has been stable
    stable_since: Instant,
}
```

### Quality Monitor Task (on relay)

Runs alongside the SFU forwarding loop:
1. Every 1 second, compute per-participant quality from QUIC connection stats
2. Classify each participant
3. If ANY participant degrades → send downgrade to ALL
4. If ALL participants stable for threshold → propose upgrade
5. Track upgrade negotiation state

### Integration with Existing Code

The relay already has access to:
- `QuinnTransport::path_quality()` → loss, RTT, jitter, bandwidth estimates
- `QualityReport` embedded in media packet headers
- Per-session metrics in `RelayMetrics`

The quality monitor just needs to read these existing metrics and produce signals.

## Client-Side Implementation

### Handling Quality Signals

In the recv loop (both Android engine and desktop engine):
```rust
SignalMessage::QualityDirective { recommended_profile, .. } => {
    // Immediate: switch encoder to recommended profile
    encoder.set_profile(recommended_profile)?;
    fec_enc = create_encoder(&recommended_profile);
    frame_samples = frame_samples_for(&recommended_profile);
    info!(codec = ?recommended_profile.codec, "quality directive: switched");
}
```

### P2P Quality (simpler case)

For P2P calls (no relay), both clients directly observe quality:
1. Each client runs its own `QualityAdapter` on the direct connection
2. When quality changes, client proposes to peer via signal
3. Simpler negotiation: only 2 parties, no relay middleman
4. Same coordinated switching logic, just peer-to-peer signals

## Backporting P2P → Relay

The quality monitoring and codec switching logic is identical:
- **P2P**: client observes quality directly → proposes switch to peer
- **Relay**: relay observes quality → proposes switch to all clients

The only difference is WHO makes the decision (client vs relay) and HOW many participants need to agree (2 vs N).

Implementation strategy: build for P2P first (simpler, 2 parties), then wrap the same logic with relay-mediated signals for SFU mode.

## Milestones

| Phase | Scope | Effort |
|-------|-------|--------|
| 1 | Relay-side quality monitor (per-participant tracking) | 1 day |
| 2 | Downgrade signal (immediate, match weakest) | 1 day |
| 3 | Client handling of QualityDirective | 1 day (both engines) |
| 4 | Upgrade proposal + negotiation protocol | 2 days |
| 5 | P2P quality adaptation (direct observation) | 1 day |
| 6 | Per-participant asymmetric encoding (Option 2) | 1 day |

## Implementation Status (2026-04-12)

Phases 1-2 are now implemented:

### What was built

- **`QualityDirective` signal** (`crates/wzp-proto/src/packet.rs`): New `SignalMessage` variant with `recommended_profile` and optional `reason`
- **`ParticipantQuality`** (`crates/wzp-relay/src/room.rs`): Per-participant quality tracking using `AdaptiveQualityController`, created on join, removed on leave
- **Weakest-link broadcast**: `observe_quality()` method computes room-wide worst tier, broadcasts `QualityDirective` to all participants when tier changes
- **Desktop engine handling** (`desktop/src-tauri/src/engine.rs`): `AdaptiveQualityController` in recv task, `pending_profile` AtomicU8 bridge to send task, auto-mode profile switching

### Phases 3-4 remaining

- Phase 3: Client-side handling of `QualityDirective` (reacting to relay-pushed profile)
- Phase 4: Upgrade proposal/negotiation protocol for quality recovery
