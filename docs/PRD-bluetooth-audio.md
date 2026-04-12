# PRD: Bluetooth Audio Routing

> Phase: Implemented  
> Status: Ready for testing  
> Platforms: Android (native Kotlin app + Tauri desktop app)

## Problem

WarzonePhone had `AudioRouteManager.kt` with complete Bluetooth SCO support, but it was disconnected from both UIs. Users with Bluetooth headsets had no way to route call audio to them.

## Solution

Wire Bluetooth SCO routing end-to-end through both app variants, replacing the binary speaker toggle with a 3-way audio route cycle: **Earpiece → Speaker → Bluetooth**.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│ Native Kotlin App (com.wzp)                         │
│                                                     │
│  InCallScreen ──► CallViewModel ──► AudioRouteManager
│  (Compose UI)     cycleAudioRoute()  setSpeaker()   │
│  "Ear/Spk/BT"    audioRoute Flow     setBluetoothSco()
│                                      isBluetoothAvailable()
└─────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────┐
│ Tauri Desktop App (com.wzp.desktop)                 │
│                                                     │
│  main.ts ──► Tauri Commands ──► android_audio.rs    │
│  cycleAudioRoute()  set_bluetooth_sco()  JNI calls  │
│  "Ear/Spk/BT"      is_bluetooth_available()         │
│                     get_audio_route()                │
│                                                     │
│  After each route change: Oboe stop + start         │
│  (spawn_blocking to avoid stalling tokio)           │
└─────────────────────────────────────────────────────┘
```

## Components Modified

### Native Kotlin App

| File | Change |
|------|--------|
| `CallViewModel.kt` | Added `audioRoute: StateFlow<AudioRoute>`, `cycleAudioRoute()`, wired `onRouteChanged` callback |
| `InCallScreen.kt` | `ControlRow` now takes `audioRoute: AudioRoute` + `onCycleRoute`, displays Ear/Spk/BT with distinct colors |

### Tauri App

| File | Change |
|------|--------|
| `android_audio.rs` | Added `start_bluetooth_sco()`, `stop_bluetooth_sco()`, `is_bluetooth_sco_on()`, `is_bluetooth_available()` |
| `lib.rs` | Added `set_bluetooth_sco`, `is_bluetooth_available`, `get_audio_route` Tauri commands |
| `main.ts` | Replaced `speakerphoneOn` toggle with `currentAudioRoute` cycling logic |
| `style.css` | Added `.bt-on` CSS class (blue-400 highlight) |

## Audio Route Lifecycle

1. **Call starts** → route defaults to Earpiece
2. **User taps route button** → cycles to next available route
3. **Route changes** → AudioManager JNI call + Oboe stream restart (~60-400ms)
4. **BT device disconnects mid-call** → `AudioDeviceCallback.onAudioDevicesRemoved` fires → auto-fallback to Earpiece/Speaker
5. **Call ends** → route reset to Earpiece, BT SCO stopped

## Route Cycling Logic

```
Available routes = [Earpiece, Speaker] + [Bluetooth] if SCO device connected

Tap cycle:
  Earpiece → Speaker → Bluetooth (if available) → Earpiece → ...

If BT not available:
  Earpiece → Speaker → Earpiece → ...
```

## Permissions

- `BLUETOOTH_CONNECT` (Android 12+) — already in `AndroidManifest.xml`
- `MODIFY_AUDIO_SETTINGS` — already in manifest

## Known Limitations

- **SCO only** — no A2DP (stereo music profile). SCO is correct for VoIP (bidirectional mono).
- **Deprecated APIs** — `startBluetoothSco()`, `isBluetoothScoOn` are deprecated in API 31+ but still functional. Modern replacement `setCommunicationDevice()` requires API 31 and more complex device enumeration. Since minSdk is 26, deprecated path is correct.
- **No auto-switch on BT connect** — when a BT device connects mid-call, `onRouteChanged` fires but we don't auto-switch. User must tap the button.

## Testing

1. Pair a Bluetooth SCO headset with Android device
2. Start call → verify Earpiece is default
3. Tap route → Speaker (audio moves to loudspeaker, button shows "Spk")
4. Tap route → BT (audio moves to headset, button shows "BT", blue highlight)
5. Tap route → Earpiece (audio back to earpiece, button shows "Ear")
6. Disconnect BT mid-call → verify auto-fallback
7. Verify both app variants work identically
8. Verify no audio glitches during route transitions
