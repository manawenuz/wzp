# PRD: Network Awareness

> Phase: Implemented (core path)  
> Status: Ready for testing  
> Platform: Android native Kotlin app (com.wzp)

## Problem

WarzonePhone's quality controller (`AdaptiveQualityController`) had a `signal_network_change()` API for proactive adaptation to WiFi↔cellular transitions, but nothing called it. Network handoffs during calls were only detected reactively via jitter spikes — by which time the user had already experienced degraded audio.

## Solution

Integrate Android's `ConnectivityManager.NetworkCallback` to detect network transport changes in real-time and feed them to the quality controller. This enables:

1. **Preemptive quality downgrade** when switching from WiFi to cellular
2. **FEC boost** (10-second window with +0.2 ratio) after any network change
3. **Faster downgrade thresholds** on cellular (2 consecutive reports vs 3 on WiFi)

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│ Android                                                      │
│                                                              │
│  ConnectivityManager                                         │
│       │ NetworkCallback                                      │
│       ▼                                                      │
│  NetworkMonitor.kt                                           │
│       │ onNetworkChanged(type, bandwidthKbps)                │
│       ▼                                                      │
│  CallViewModel.kt ──► WzpEngine.onNetworkChanged()           │
│                            │ JNI                             │
│                            ▼                                 │
│  jni_bridge.rs: nativeOnNetworkChanged(handle, type, bw)     │
│                            │                                 │
│                            ▼                                 │
│  engine.rs: state.pending_network_type.store(type)           │
│                            │ AtomicU8 (lock-free)            │
│                            ▼                                 │
│  recv task: quality_ctrl.signal_network_change(ctx)          │
│       │                                                      │
│       ├─ Preemptive downgrade (WiFi → cellular)              │
│       ├─ FEC boost 10s                                       │
│       └─ Faster cellular thresholds                          │
└──────────────────────────────────────────────────────────────┘
```

## Network Classification

`NetworkMonitor` classifies the active transport without requiring `READ_PHONE_STATE` permission by using bandwidth heuristics:

| Downstream Bandwidth | Classification | Rust `NetworkContext` |
|----------------------|---------------|----------------------|
| N/A (WiFi transport) | WiFi | `WiFi` |
| >= 100 Mbps | 5G NR | `Cellular5g` |
| >= 10 Mbps | LTE | `CellularLte` |
| < 10 Mbps | 3G or worse | `Cellular3g` |
| Ethernet | WiFi (equivalent) | `WiFi` |
| Network lost | None | `Unknown` |

## Cross-Task Signaling

The network type is communicated from the JNI thread to the recv task via `AtomicU8` — the same pattern used for `pending_profile` (adaptive quality profile switches):

```
JNI thread                     recv task (tokio)
    │                               │
    │ store(type, Release)          │
    │──────────────────────────────►│
    │                               │ swap(0xFF, Acquire)
    │                               │ if != 0xFF:
    │                               │   quality_ctrl.signal_network_change(ctx)
    │                               │
```

Sentinel value `0xFF` means "no change pending". The recv task polls on every received packet (~20-40ms), so latency is bounded by the inter-packet interval.

## Components

### New File

| File | Purpose |
|------|---------|
| `android/.../net/NetworkMonitor.kt` | ConnectivityManager callback, transport classification, deduplication |

### Modified Files

| File | Change |
|------|--------|
| `android/.../engine/WzpEngine.kt` | Added `onNetworkChanged()` method + `nativeOnNetworkChanged` external |
| `android/.../ui/call/CallViewModel.kt` | Instantiates NetworkMonitor, wires callback, register/unregister lifecycle |
| `crates/wzp-android/src/jni_bridge.rs` | Added `Java_com_wzp_engine_WzpEngine_nativeOnNetworkChanged` JNI entry |
| `crates/wzp-android/src/engine.rs` | Added `pending_network_type: AtomicU8` to EngineState, recv task polls it |

### Unchanged (already implemented)

| File | API |
|------|-----|
| `crates/wzp-proto/src/quality.rs` | `AdaptiveQualityController::signal_network_change(NetworkContext)` |
| `crates/wzp-transport/src/path_monitor.rs` | `PathMonitor::detect_handoff()` (available for future use) |

## Deferred Work

### Tauri Desktop App (com.wzp.desktop)

The Tauri engine doesn't use `AdaptiveQualityController` — quality is resolved once at call start. Adding network monitoring requires first adding adaptive quality to the Tauri call engine, which is a larger change.

### Mid-Call ICE Re-gathering

When the device's IP address changes, ideally we should:
1. Re-gather local host candidates (`local_host_candidates()`)
2. Re-probe STUN (`probe_reflect_addr()`)
3. Send updated candidates to the peer (`CandidateUpdate` signal message)
4. Attempt new dual-path race for path upgrade

`NetworkMonitor.onIpChanged` fires on `onLinkPropertiesChanged` — the hook is ready, but the signaling and re-racing logic is not yet implemented.

## Testing

1. Build native APK
2. Start a call on WiFi
3. Verify logcat: `quality controller: network context updated` with `ctx=WiFi`
4. Disable WiFi → device falls to cellular
5. Verify logcat: `ctx=CellularLte` (or `Cellular5g`/`Cellular3g`)
6. Verify FEC boost activates (check quality_ctrl logs)
7. Verify preemptive quality downgrade (tier drops one level on WiFi→cellular)
8. Re-enable WiFi → verify transition back
9. Rapid WiFi toggle (5x in 10s) → verify no crashes, deduplication works
10. Airplane mode → verify `onLost` fires with `TYPE_NONE`
