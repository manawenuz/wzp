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
| `android_audio.rs` | `setCommunicationDevice()` (API 31+) with `startBluetoothSco()` fallback; `set_audio_mode_communication/normal()` for call lifecycle |
| `lib.rs` | `set_bluetooth_sco`, `is_bluetooth_available`, `get_audio_route` Tauri commands; SCO polling + 500ms route delay |
| `wzp_native.rs` | Added `audio_start_bt()` for BT-mode Oboe (skips 48kHz + VoiceCommunication preset) |
| `oboe_bridge.cpp` | `bt_active` flag: capture skips sample rate + input preset; playout uses `Usage::Media`; both use `Shared` mode + `SampleRateConversionQuality::Best` |
| `engine.rs` | `set_audio_mode_communication()` before `audio_start()`; `set_audio_mode_normal()` after `audio_stop()` |
| `MainActivity.kt` | Removed `MODE_IN_COMMUNICATION` from app launch — deferred to call start |
| `main.ts` | Replaced `speakerphoneOn` toggle with `currentAudioRoute` cycling logic |
| `style.css` | Added `.bt-on` CSS class (blue-400 highlight) |

## Audio Route Lifecycle

1. **App launch** → `MODE_NORMAL` (other apps' audio unaffected — BT A2DP music keeps playing)
2. **Call starts** → `MODE_IN_COMMUNICATION` set via JNI, Oboe opens with earpiece routing
3. **User taps route button** → cycles to next available route
4. **Route changes** → `setCommunicationDevice()` (API 31+) + Oboe restart in BT mode or normal mode
5. **BT device disconnects mid-call** → `AudioDeviceCallback.onAudioDevicesRemoved` fires → auto-fallback to Earpiece/Speaker
6. **Call ends** → route reset, `MODE_NORMAL` restored

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
- **API 31+ required for modern path** — `setCommunicationDevice()` is the primary BT routing API. Fallback to deprecated `startBluetoothSco()` on API < 31 (untested).
- **BT SCO capture at 8/16kHz** — Oboe resamples to 48kHz via `SampleRateConversionQuality::Best`. Quality is inherently limited by the SCO codec (CVSD at 8kHz or mSBC at 16kHz).
- **No auto-switch on BT connect** — when a BT device connects mid-call, user must tap the route button.
- **500ms route switch delay** — after `setCommunicationDevice()` returns, the audio policy needs time to apply the bt-sco route. We wait 500ms before restarting Oboe.

## Testing

1. Pair a Bluetooth SCO headset with Android device
2. Start call → verify Earpiece is default
3. Tap route → Speaker (audio moves to loudspeaker, button shows "Spk")
4. Tap route → BT (audio moves to headset, button shows "BT", blue highlight)
5. Tap route → Earpiece (audio back to earpiece, button shows "Ear")
6. Disconnect BT mid-call → verify auto-fallback
7. Verify both app variants work identically
8. Verify no audio glitches during route transitions
