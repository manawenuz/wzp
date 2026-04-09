# PRD: Desktop Direct Calling — Backport SignalManager

## Problem

The desktop Tauri app has the direct calling UI (Room/Direct Call toggle, Register, Call buttons) but the backend uses inline async code in `main.rs` instead of a proper `SignalManager`. This needs to be backported from the Android refactor.

## Tasks

1. **Create `signal_mgr.rs` for desktop** — same pattern as Android, or reuse the crate directly
2. **Wire into Tauri commands** — `register_signal` should use `SignalManager::connect()` + `run_recv_loop()` on a dedicated thread
3. **State polling** — `get_signal_status` should call `SignalManager::get_state_json()`
4. **place_call / answer_call** — delegate to SignalManager methods
5. **Merge android branch into desktop branch** — resolve the 37 desktop-only + 90 android-only commit divergence
6. **Test** — Android calls Desktop, Desktop calls Android

## UI Fixes

1. **Default alias** — generate random name on first start (like Android does)
2. **Default room** — change from "android" to "general"
3. **Fingerprint display** — ensure full `xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx` format (not truncated)
4. **Deregister button** — ability to disconnect signal channel
5. **Call state reset** — after hangup, return to "Registered" state, not stuck on "Ringing"
