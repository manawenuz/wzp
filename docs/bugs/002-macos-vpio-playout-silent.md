# BUG-002: macOS VPIO Playout Silent — Audio Decoded But Not Heard

**Severity:** P0 — outgoing audio (Mac mic → peer) works, but the user hears nothing on the Mac side
**Status:** Instrumented on 2026-05-25; awaiting next VPIO vs CPAL repro
**Branch:** `experimental-ui`
**Build observed:** `01f55ca` (Mac desktop), same-day Android `01f55ca`
**Last investigated:** 2026-05-25
**Platforms confirmed affected:** macOS desktop (VPIO path)

---

## Symptom

In a relay-forwarded group call between macOS and Android in the same room (`General`, `count:2`):

- The Mac user can be **heard** on Android (Mac→Android leg works).
- The Mac user **hears nothing** when the Android peer speaks (Android→Mac playout silent).
- Muting the Android peer's mic results in total silence on both ends — confirming the only audio the user perceived during the call was the Mac→Android leg playing through the Android speaker.

This was initially misreported as "I hear myself on Android" — the user was hearing their own Mac mic looped through Android playout, not an actual echo bug.

---

## Evidence

### Mac log excerpt (`01f55ca`, fingerprint `63ba…`, 10:31:22)

```
10:31:23 media:room_update {"count":2, participants:[Akbar fa06…, Manwe 63ba…]}
10:31:23 media:first_recv {"codec":"Opus24k","payload_bytes":27,"t_ms":933}
10:31:25 media:recv_heartbeat {"codec":"Opus24k","decode_errs":0,"decoded_frames":140,"last_written":960,"written_samples":134400}
10:31:29 media:recv_heartbeat {"codec":"Opus32k","decoded_frames":338,"last_written":960,"written_samples":324480}
10:31:35 media:recv_heartbeat {"codec":"Opus6k","decoded_frames":595,"last_written":1920,"written_samples":618240}
…
10:31:57 media:recv_heartbeat {"codec":"Opus6k","decoded_frames":1086,"last_written":1920,"written_samples":1560960}
```

Recv path is healthy:
- `decode_errs:0` throughout
- `decoded_frames` climbs monotonically 140 → 1086
- `written_samples` reaches 1.56 M (≈32 s of 48 kHz mono)
- `last_written` correctly flips 960 (Opus24k/32k, 20 ms) ↔ 1920 (Opus6k, 40 ms)

**Conclusion:** packets arrive, decode succeeds, samples are written into `playout_ring`. The breakage is **downstream of the ring write**, i.e. in the macOS playout consumer (the VPIO `set_render_callback`).

### Mac send path also works
`media:send_heartbeat` shows `last_rms` spiking to 168, 477, 867, 1458 in response to speech. Android's recv log for the same window decoded those frames successfully.

---

## Suspected Root Cause

`crates/wzp-client/src/audio_vpio.rs:128–147` — the render (output) callback reads from `playout_ring` in `FRAME_SAMPLES` (960) chunks. Three plausible failure modes:

### Hypothesis A: Codec-change frame-size mismatch
Mid-call codec switches (`Opus24k` → `Opus32k` → `Opus6k`) change the frame size written into the ring (960 ↔ 1920 samples per frame). The render callback reads in fixed 960-sample chunks. The ring is FIFO and should absorb this, but if `AudioRing` semantics drop partial frames or stall on alignment, the consumer side could starve while `written_samples` continues to climb on the producer side.

`engine.rs:1852` and `engine.rs:1895` write into `playout_ring` directly with the decoder's output length (variable). Worth confirming `AudioRing::read` handles arbitrary chunk sizes vs producer.

### Hypothesis B: VPIO output element never actually started
`audio_vpio.rs:151` calls `au.start()` once on the combined VPIO unit. VPIO is supposed to start both input and output elements together, but if AEC initialization fails silently, output rendering may be suppressed while input still produces callbacks. The `[vpio] capture callback: N f32 samples` log line proves input callbacks fire — but there is **no equivalent log line for the render callback**. We do not know whether the render callback is being invoked at all.

### Hypothesis C: Output device routing
VPIO may have grabbed an unexpected default output (e.g. the previous Bluetooth headset, an HDMI sink, or a virtual device). The render callback runs and pulls samples, but they're sent to a device the user can't hear.

### Hypothesis D: AEC over-suppression
VPIO's AEC uses the render callback as the far-end reference. If the unit decides the far-end and near-end are too correlated (it shouldn't here — different speakers in different rooms), it could attenuate playout. Unlikely to produce 100 % silence but listed for completeness.

---

## Instrumentation Added

As of the current workspace, the desktop client emits VPIO render/capture counters into the normal call debug log when OS AEC is enabled:

```
vpio:render_heartbeat {
  "capture_callbacks": ...,
  "capture_samples": ...,
  "render_callbacks": ...,
  "render_requested_samples": ...,
  "render_read_samples": ...,
  "render_underrun_callbacks": ...,
  "render_nonzero_callbacks": ...,
  "render_last_requested": ...,
  "render_last_read": ...,
  "render_last_rms": ...,
  "render_last_ring_available": ...
}
```

Interpretation:

- `render_callbacks == 0`: VPIO output callback is not running. Focus on VPIO initialization / output element start.
- `render_callbacks > 0` and `render_read_samples == 0` while `media:recv_heartbeat.written_samples` climbs: VPIO callback runs but the ring it reads is not receiving the same samples the recv task writes, or the callback is draining before writes arrive.
- `render_read_samples > 0` and `render_last_rms > 0` while the user hears silence: VPIO is feeding non-zero speaker samples to CoreAudio; focus on output device routing or VoiceProcessingIO suppression.
- CPAL fallback test: disable OS AEC in settings. If CPAL playback is audible with the same call, the failure is VPIO-specific.

## Proposed Diagnostic Steps (Prioritized)

1. **Reproduce with current instrumentation** and compare `media:recv_heartbeat` to `vpio:render_heartbeat`.

2. **One-shot render callback stderr log is now present** (`audio_vpio.rs`) mirroring the existing capture-side `eprintln!`:
   ```rust
   let logged_render = Arc::new(AtomicBool::new(false));
   …
   if !logged_render.swap(true, Ordering::Relaxed) {
       eprintln!("[vpio] render callback: {} f32 samples, ring_read={}", ch.len(), read);
   }
   ```
   This will immediately distinguish Hypothesis B (callback never fires) from A/C/D (callback fires but output is silent or misrouted).

3. **Periodically log render-callback stats** — total samples pulled from ring, samples requested per callback, non-zero render callback count, and last render RMS. Compare against producer-side `written_samples` to confirm consumer is keeping up.

4. **Verify output device** via `AudioUnitGetProperty(kAudioOutputUnitProperty_CurrentDevice, Output)` immediately after `au.start()`. Log device name. If it doesn't match the user's intended speaker, force-set the default output device.

5. **Test with codec pinned** — set `WZP_FORCE_CODEC=Opus24k` (or wire a temporary CLI flag) so codec doesn't change mid-call. If audio works with a pinned codec, Hypothesis A is confirmed and `AudioRing` chunk handling needs review.

6. **Compare CPAL fallback path** — disable OS AEC in settings and reproduce. If CPAL playback works, the bug is VPIO-specific.

---

## Open Questions

- Does the macOS render callback have permission to write to the user's selected output device? Apple changed CoreAudio output-device permission semantics in macOS 14+.
- Is `_audio_unit: AudioUnit` being dropped early? It's stored in `VpioAudio` and that struct is boxed into `audio_handle: Box<dyn Any + Send>` in `engine.rs:1573`, which is held by `CallEngine`. Should be alive for the call duration — confirm no early-drop path.
- Are there any `os_log` / Console.app warnings from `AudioToolbox` / `CoreAudio` / `AVAudioSession` during the call?

---

## Reproduction Steps

1. Start macOS desktop client (build `01f55ca` or later), join relay `193.180.213.68:4433`, room `General`.
2. Start Android client (same build), join same relay + room.
3. Confirm `media:room_update count:2` on both ends.
4. Speak into Android mic.
5. Observe: Mac log shows `decoded_frames` climbing, `decode_errs:0`, `written_samples` increasing. User hears nothing on Mac speakers.
6. Speak into Mac mic — Android user hears Mac audio fine, confirming Mac→Android works.

---

## Related Files

- `crates/wzp-client/src/audio_vpio.rs:128–147` — render callback (primary suspect)
- `crates/wzp-client/src/audio_vpio.rs:35–161` — full VPIO start sequence
- `crates/wzp-client/src/audio_ring.rs` — ring buffer used by both producer and consumer
- `desktop/src-tauri/src/engine.rs:1562–1600` — VPIO vs CPAL selection
- `desktop/src-tauri/src/engine.rs:1760–1900` — recv task writing into `playout_ring`

---

## Fix Plan (Once Diagnosed)

| Diagnosis | Fix |
|-----------|-----|
| A — frame-size mismatch | Make `AudioRing` consumer drain variable chunks, or buffer to fixed 960 in recv task before ring write |
| B — render callback not firing | Investigate VPIO initialization order; consider separate input + output `AudioUnit` instances |
| C — wrong output device | Set `kAudioOutputUnitProperty_CurrentDevice` explicitly to `kAudioObjectSystemObject` default output at start |
| D — AEC suppression | Test with VPIO bypass mode (`kAUVoiceIOProperty_BypassVoiceProcessing`) on; if audio returns, file CoreAudio quirk and tune AEC config |

---

## Cross-References

- BUG-001 (Android join-voice hang) — separate issue, already mitigated; current Android build joins room successfully and recv works.
- Memory: `project_desktop_client.md` notes the desktop rewrite uses CPAL + VoiceProcessingIO with "direct playout, OS-level AEC" — this bug is the first failure of that path under real call conditions.
