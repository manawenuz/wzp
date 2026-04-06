# Incident Report: Send Task Fatal Exit on QUIC Congestion

**Date:** 2026-04-06
**Severity:** High — causes complete audio loss mid-call
**Status:** Fixed in Android client, **pending fix in desktop client and web client**

## Summary

A QUIC congestion event causes `send_datagram()` to return `Err(Blocked)`. The send task treats this as a fatal error and exits, which kills the entire call via `tokio::select!`. Audio becomes one-way (recv still works briefly) then dies completely.

## Root Cause

In the engine's send loop (`run_call` function), `transport.send_media()` errors were handled with `break`:

```rust
// BEFORE (broken)
if let Err(e) = transport.send_media(&source_pkt).await {
    error!("send error: {e}");
    break;  // <-- kills send task, which kills everything
}
```

Quinn's `send_datagram()` is synchronous and returns `Err(SendDatagramError::Blocked)` when the QUIC congestion window is full. This is a **transient condition** — the window opens again once ACKs arrive. But the `break` kills the send task, and since all tasks run under `tokio::select!`, the recv task, stats task, and signal task all die too.

### Why it manifests as "intermittent disconnections"

- Mobile networks have brief congestion spikes (cell tower handoff, WiFi interference)
- A single spike fills the QUIC congestion window
- One `Blocked` error → send task exits → `select!` cancels recv → complete silence
- The QUIC connection stays open (no error logged), so stats polling continues showing stale data
- From the user's perspective: audio drops for 5-20 seconds then "maybe comes back" (it doesn't — they're hearing cached playout ring drain)

### Evidence from debug reports

**Relay logs** confirmed the relay was healthy:
- `max_forward_ms=0` — relay forwards instantly
- `send_errors=0` — no relay-side failures
- The relay saw `large recv gap` warnings on participant 1 (Nothing A059): 722ms → 814ms → 1778ms → 3500ms → 6091ms — the client progressively stopped sending

**Client stats** confirmed:
- `frames_encoded` kept incrementing (Opus encoder running)
- `frames_decoded` froze at a fixed value (recv task died)
- `fec_recovered` froze simultaneously
- RTT, loss, jitter all frozen (stats task died)

## Fix Applied

### Android client (`crates/wzp-android/src/engine.rs`)

```rust
// AFTER (fixed)
if let Err(e) = transport.send_media(&source_pkt).await {
    send_errors += 1;
    frames_dropped += 1;
    if send_errors <= 3 || last_send_error_log.elapsed().as_secs() >= 1 {
        warn!(seq = s, send_errors, frames_dropped,
              "send_media error (dropping packet): {e}");
        last_send_error_log = Instant::now();
    }
    continue;  // <-- drop packet, keep going
}
```

Same pattern applied to FEC repair packet sends.

Recv task also hardened: transient errors (non-closed/reset) are now logged and survived rather than causing exit.

Added periodic health logging to both tasks (5-second intervals):
- Send: `frames_sent`, `frames_dropped`, `send_errors`, `ring_avail`
- Recv: `frames_decoded`, `fec_recovered`, `recv_errors`, `max_recv_gap_ms`, `playout_avail`

### Relay (`crates/wzp-relay/src/room.rs`)

Added debug logging to both plain and trunked forwarding loops:
- Per-recv gap tracking (warns on >200ms gaps)
- Room manager lock contention tracking (warns on >10ms)
- Forward latency tracking (warns on >50ms)
- Send error counting with peer identification
- 5-second periodic stats with all above metrics

## Affected Clients — FIX REQUIRED

### Desktop client (`crates/wzp-client/src/cli.rs`)

**Lines 345-348:**
```rust
if let Err(e) = transport.send_media(pkt).await {
    error!("send error: {e}");
    break;  // <-- SAME BUG
}
```

**Lines 431-434:**
```rust
if let Err(e) = send_transport.send_media(pkt).await {
    error!("send error: {e}");
    return;  // <-- SAME BUG
}
```

Both need the same continue-on-error pattern.

### Web client (`crates/wzp-web/src/main.rs`)

Needs audit — WebSocket transport may have different error semantics but same pattern should be checked.

## Testing

After fix, a congestion event will:
1. Log warnings with packet counts: `send_media error (dropping packet): Blocked`
2. Drop affected packets (brief audio glitch — ~20-100ms)
3. Resume normal sending once congestion window opens
4. FEC on the receiver side will recover most dropped packets
5. Call continues uninterrupted

## Timeline

- 10:37 — First crash observed (LinearProgressIndicator compose bug masked investigation)
- 10:58 — Debug reports collected, decoded stall pattern identified
- 11:16 — Relay debug logging deployed, confirmed relay is clean
- 11:17 — Second debug reports collected, send gaps correlated with relay recv gaps
- 11:30 — Root cause identified: `break` on `send_media` error in send task
- 11:45 — Fix applied and deployed
