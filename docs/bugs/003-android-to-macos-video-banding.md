# BUG-003: Android to macOS Video Banding / Horizontal Lines

**Severity:** P0/P1 - Android camera video is visibly corrupted on macOS at common resolutions.
**Status:** Root cause identified 2026-05-26; candidate fix in `crates/wzp-video/src/videotoolbox.rs`. Awaiting on-device verification.
**Branch:** `main`.
**Latest build observed:** `3ea25a0` (`fix(android): use MediaCodec input layout for video encode`).
**Direction affected:** Android camera -> macOS desktop display.
**Direction mostly OK:** macOS camera -> Android display.

---

## Root Cause (2026-05-26)

The Android H.264 bitstream is **valid**: the locally-encoded `.h264` files and
the macOS-reassembled `.h264` files both decode cleanly with software ffmpeg.
SPS reports the expected `960x540`, `coded_height=544`, `yuv420p`, High profile,
level 3.1.

The corruption appears purely on the macOS receive side. The shiguredo
`I420Frame` wrapper around `CVPixelBuffer` exposes each plane as
`bytes_per_row * height` bytes — i.e. the raw plane buffer including the
per-row stride padding that CoreVideo adds for alignment. `VideoToolboxDecoder`
was concatenating those slices verbatim, then handing the buffer downstream
tagged as tight I420 of `width x height`. The JPEG-encoding consumer
(`i420_to_jpeg_bytes` in `desktop/src-tauri/src/lib.rs`) indexes the buffer
with tight strides `width` and `width/2`, so any plane where
`bytes_per_row > tight_stride` produces per-row drift in the consumer's reads.

Numerical confirmation from the corrupted dump
`000002_desktop_remote_decoded_f000001_960x540.jpg`:

- Banding period along the diagonal: exactly **32 luma rows** = 16 chroma rows.
- Per-column-slice peak offsets shift by ~5 rows per 230-column step, i.e. the
  bands are a tilted diagonal, not horizontal — consistent with one chroma row
  of drift accumulating per 16 chroma rows of consumer read.
- Solving `u_stride / (u_stride - chroma_width) = 16` with `chroma_width = 480`
  yields `u_stride = 512`. That is exactly the 64-byte aligned chroma stride
  CoreVideo emits for a 480-wide plane.
- Luma at 960 wide is already 64-aligned, so `y_stride = 960` and the luma
  plane is unaffected. This matches the bug doc note that 640x360 looks fine
  (chroma_width 320 is also 64-aligned, no padding needed).

## Fix

`crates/wzp-video/src/videotoolbox.rs` now has an `i420_frame_to_tight` helper
that copies each plane row-by-row using its own `bytes_per_row`, producing a
genuine tight I420 buffer of `width * height + 2 * (width/2) * (height/2)`
bytes. All three decoders (H.264, HEVC, AV1) call the helper instead of
concatenating raw plane slices. On the first successful decode each decoder
logs the actual plane dimensions and strides (`tracing::info!` at target
`wzp_video::videotoolbox`) so future similar bugs are easier to diagnose
without re-deriving from band spacing.

---

## Symptom

When Android sends camera video to macOS, the macOS view shows repeated horizontal green/magenta line bands over the decoded picture. The lines cover the whole decoded frame, including black side bars added by the Android portrait-camera contain/crop fix.

The Android camera crop/zoom problem is fixed now: the Android front camera is no longer cover-cropped into an extreme zoom. The remaining bug is the line/banding corruption.

The issue is easy to see at H.264 960x540. At 640x360 it has been reported as visually good or much better. HEVC behaves differently: minimum resolution can look good, but 960x540 and 1280x720 tend to pause or deliver only bursts of frames.

---

## Current State

Recent commits relevant to this bug:

```text
3ea25a0 fix(android): use MediaCodec input layout for video encode
1124726 fix(video): add frame metadata and Android encode diagnostics
9a77459 feat(video): add codec and resolution controls
f85efb9 fix(video): improve android stream smoothness
31b2caa fix(video): request keyframes after packet loss
079e21e fix(video): resync decoder after packet gaps
e676641 fix(android): suppress debuggable lint for diagnostic builds
9713efc chore(android): add release debuggable build
```

Important behavior:

- Android source dumps are clean.
- Android I420 roundtrip dumps are clean.
- macOS decoded remote Android frames are corrupted.
- Android receiving macOS video is generally clean.
- Transport/reassembly is probably not the primary issue: early Android local encoded `.h264` files match the corresponding macOS remote reassembled `.h264` prefix/length.
- The bug is likely in Android MediaCodec encoder input layout/color handling, H.264 non-macroblock-aligned dimensions/cropping, or macOS VideoToolbox interpretation of Android-encoded H.264.

---

## Reproduction Build

Use the Tauri Android pipeline, not the legacy native Android Gradle app.

```bash
cd /Users/manwe/CascadeProjects/warzonePhone
git status --short
git log -1 --oneline
./scripts/android-build-async.sh --release-debuggable --wait
```

The APK lands here:

```bash
/Users/manwe/CascadeProjects/warzonePhone/target/tauri-android-apk/wzp-tauri-arm64.apk
```

Install it:

```bash
adb install -r /Users/manwe/CascadeProjects/warzonePhone/target/tauri-android-apk/wzp-tauri-arm64.apk
```

Use `--release-debuggable` for this bug. Plain debug builds can mask the issue because they run at much lower frame rate and look like a slideshow. Plain release builds are not usable for `run-as` frame-dump retrieval.

Critical build trap: `scripts/android-build-async.sh` runs `scripts/build-tauri-android.sh`, which SSHes to `SepehrHomeserverdk` and resets the remote source to `origin/$BRANCH`. Uncommitted local changes are ignored by the Android build. Commit and push before building, or the phone may run old code.

---

## macOS Build / Run

For local desktop repro:

```bash
cd /Users/manwe/CascadeProjects/warzonePhone/desktop
npm install
npm run tauri dev
```

Enable call debug logs in the app settings before starting the call. The in-app call log only keeps the last 200 entries; use the copy/share buttons if preserving textual logs matters.

---

## Repro Steps

1. Start the macOS desktop client.
2. Start the Android `--release-debuggable` APK.
3. Join the same room, usually `general`.
4. Use the same relay as the current manual tests, e.g. `172.16.81.135:4433`, unless testing relay-specific behavior.
5. Turn camera on for both clients.
6. Set both sides to H.264.
7. Set Android send resolution to 960x540. Mac can be 960x540 or higher.
8. Observe Android camera video on macOS.

Expected failure: macOS shows Android video with repeated horizontal green/magenta lines. Android camera source preview and Android frame dumps are clean.

Useful comparison tests:

| Codec / resolution | Observed result |
|---|---|
| H.264 960x540 | Lines/banding on macOS for Android video |
| H.264 640x360 | Reported good or much better; smoother |
| H.264 1280x720 | Lines/banding and/or worse smoothness |
| HEVC 1280x720 | Mac video smooth on Android; Android video on Mac pauses and can look zoomed/corrupt |
| HEVC 960x540 | Same pause pattern, shorter pauses |
| HEVC minimum resolution | Reported good on both devices |

---

## Artifact Collection

### Clear old dumps before a fresh run

macOS:

```bash
rm -rf "$HOME/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps"
```

Android:

```bash
adb shell run-as com.wzp.desktop rm -rf .wzp/frame-dumps
```

The Android clear command requires a debuggable build. If `run-as` fails, rebuild with `--release-debuggable`.

### Pull Android dumps

```bash
cd /Users/manwe/CascadeProjects/warzonePhone
./scripts/pull-android-frame-dumps.sh
```

Output directory:

```text
/Users/manwe/CascadeProjects/warzonePhone/android-frame-dumps/frame-dumps
```

The pull script packages files using:

```bash
adb exec-out "run-as com.wzp.desktop tar -C .wzp -cf - frame-dumps"
```

### macOS dump directory

```text
/Users/manwe/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps
```

### Important dump names

| Dump suffix | Meaning |
|---|---|
| `android_camera_jpeg_in_fXXXXXX_<WxH>.jpg` | Raw browser/camera JPEG entering Rust from Android WebView |
| `android_camera_i420_roundtrip_fXXXXXX_<WxH>.jpg` | Android camera frame after JS/canvas -> Rust I420 conversion, converted back to JPEG |
| `android_local_encoded_fXXXXXX.h264` / `.h265` | Encoded Android camera bitstream before packetization |
| `desktop_remote_encoded_reassembled_fXXXXXX.h264` / `.h265` | macOS reassembled encoded bitstream received from Android |
| `desktop_remote_decoded_fXXXXXX_<WxH>.jpg` | macOS decoded Android video frame, where the lines show |
| `android_remote_decoded_fXXXXXX_<WxH>.jpg` | Android decoded macOS video frame |

Known useful local examples from the latest sessions:

```text
Clean Android source:
/Users/manwe/CascadeProjects/warzonePhone/android-frame-dumps/frame-dumps/000407_android_camera_jpeg_in_f000150_960x540.jpg
/Users/manwe/CascadeProjects/warzonePhone/android-frame-dumps/frame-dumps/000408_android_camera_i420_roundtrip_f000150_960x540.jpg

Corrupt macOS decode:
/Users/manwe/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps/000236_desktop_remote_decoded_f000030_960x540.jpg
/Users/manwe/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps/000241_desktop_remote_decoded_f000060_960x540.jpg
/Users/manwe/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps/000244_desktop_remote_decoded_f000090_960x540.jpg

Encoded bitstream comparison:
/Users/manwe/CascadeProjects/warzonePhone/android-frame-dumps/frame-dumps/000005_android_local_encoded_f000001.h264
/Users/manwe/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps/000064_desktop_remote_encoded_reassembled_f000001.h264
```

These files are local artifacts, not committed test fixtures.

---

## Text Logs

### In-app call debug log

Enable `Call debug logs` in settings before joining. The UI buffer is limited to 200 entries. Use the in-app copy/share buttons immediately after the repro.

Useful events:

```text
camera:get_user_media_ok
camera:capture_clock
camera:capture_frame
video:first_camera_frame
video:camera_frame_sample
video:encoded_frame
video:first_send
video:first_recv
video:first_reassembled
video:reassembled_frame
video:decoder_init_start
video:first_decoded_frame
video:decoded_frame_sample
video:frame_dump
video:byte_dump
```

The crop fix is active when Android `camera:capture_frame` includes portrait source dimensions with a landscape send frame, for example:

```text
camera:capture_frame {"frame_no":150,"width":960,"height":540,"source_width":540,"source_height":960,...}
```

### Android logcat

Logcat can be noisy and may not always retain the in-app call debug entries. Still useful commands:

```bash
adb logcat -c
adb logcat -v time | rg 'camera:capture_frame|video:frame_dump|video:byte_dump|video:first_camera_frame|video:camera_frame_sample|video:encoded_frame|h264_encoder_input|hevc_encoder_input|MediaCodec input format|decoder_debug'
```

For post-run collection:

```bash
adb logcat -d -v time > /tmp/wzp-android-logcat.txt
rg 'camera:|video:|h264_encoder_input|hevc_encoder_input|MediaCodec|decoder_debug' /tmp/wzp-android-logcat.txt
```

If no `h264_encoder_input` / `hevc_encoder_input` entries appear, the current `tracing::info!` path in `crates/wzp-video/src/mediacodec.rs` may not be making it into Android logcat. Convert that diagnostic to `emit_call_debug` from the caller if the next step needs guaranteed visibility.

---

## What We Know

### The Android camera/canvas path is probably clean

The Android dumps for `android_camera_jpeg_in` and `android_camera_i420_roundtrip` at 960x540 are clean. They show the portrait front camera contained inside a landscape frame with black side bars. This means the former zoom/crop bug is fixed and the current bands are not introduced by CSS, canvas sizing, or the browser camera preview.

### The corruption appears after encode/decode

The corrupt lines are present in `desktop_remote_decoded_*`. They cover black bars as well as image content, which points to frame buffer / codec layout corruption rather than a real scene artifact.

### Transport is not the leading suspect

`android_local_encoded_f000001.h264` and `desktop_remote_encoded_reassembled_f000001.h264` have matching sizes/prefixes in the latest diagnostic run. That does not fully prove every later packet is perfect, but it makes relay/datagram/reassembly much less likely as the root cause.

Relays should not need changes for this bug unless the wire format changes. The relay forwards datagrams and does not inspect video frame internals.

### Resolution alignment is suspicious

960x540 has a height that is not divisible by 16. H.264 macroblock encoders commonly encode 960x544 and signal cropping to 960x540. The horizontal line bands may be a crop/padding/chroma-plane issue. Testing 960x544 and/or 960x528 is a high-value next step.

---

## Code Areas

Primary suspects:

- `crates/wzp-video/src/mediacodec.rs` - Android MediaCodec H.264/HEVC encoder and decoder, color format, stride, slice height handling.
- `desktop/src-tauri/src/engine.rs` - packet send/receive, decode lifecycle, frame/byte dump calls.
- `desktop/src-tauri/src/lib.rs` - `maybe_dump_video_jpeg`, `maybe_dump_video_bytes`, app-data paths, call-debug event plumbing.
- `desktop/src/main.ts` - browser camera capture, canvas scaling, codec/resolution settings, UI debug log buffer.
- `crates/wzp-video/src/transport.rs` - video packetization/reassembly and `WZV1` metadata header.

The latest attempted fix in `mediacodec.rs` uses `codec.input_format()` on Android API 28+ to derive encoder input stride/slice/color layout. Since the lines persist, either those fields are not reliable for this encoder, the chosen color format conversion is wrong, or macOS decode/crop interpretation is involved.

---

## Recommended Next Debug Steps

1. Verify whether Android logs the encoder input format on the failing build.

   ```bash
   adb logcat -d -v time | rg 'h264_encoder_input|hevc_encoder_input|input_color_format|effective_stride|effective_slice'
   ```

   If absent, make this an app call-debug event instead of plain tracing so it appears in the copied call log.

2. Add Android loopback decode of `android_local_encoded_*` before network.

   Dump a new `android_local_decoded_fXXXXXX_<WxH>.jpg` immediately after encoding. If this local Android decode already has bands, the encoder output is bad. If Android local decode is clean but macOS decode is bad, focus on H.264 SPS cropping / VideoToolbox decode assumptions.

3. Test macroblock-aligned debug resolutions.

   Add or force:

   ```text
   960x544
   960x528
   640x368
   640x352
   ```

   If 960x544 fixes the lines, the bug is almost certainly H.264 crop/padding handling. If 960x528 fixes it but 960x544 does not, inspect bottom padding and crop signaling.

4. Offline-decode `android_local_encoded_*.h264` with a known-good decoder.

   Example on a machine with working ffmpeg:

   ```bash
   ffmpeg -f h264 -i android-frame-dumps/frame-dumps/000005_android_local_encoded_f000001.h264 -frames:v 1 /tmp/android-local-f1.png
   ffmpeg -f h264 -i "$HOME/Library/Application Support/com.wzp.desktop/.wzp/frame-dumps/000064_desktop_remote_encoded_reassembled_f000001.h264" -frames:v 1 /tmp/macos-remote-f1.png
   ```

   Note: Homebrew ffmpeg on this Mac was broken during debugging with a missing `libvpx.11.dylib`, so do not assume `/opt/homebrew/bin/ffmpeg` works until fixed.

5. Try explicit Android encoder input variants.

   Test one variable at a time:

   - Force planar color format `COLOR_FormatYUV420Planar` / value `19` and feed I420.
   - Force semiplanar and try NV12 vs NV21/VU order.
   - Use `COLOR_FormatYUV420Flexible` if accepted by this device.
   - Use `stride = width`, `slice_height = align_up(height, 16)` only.
   - Use `stride = align_up(width, 16)`, `slice_height = align_up(height, 16)`.

6. Parse SPS from Android H.264 output.

   Confirm encoded dimensions and frame cropping offsets for 960x540. Compare Android output against macOS output. If SPS says 960x544 with crop to 540, test whether VideoToolbox applies the crop correctly.

7. Keep relay out of the first debugging loop.

   The relay is unlikely to affect deterministic decoded line bands when local encoded and remote reassembled payloads match. Only redeploy relay if packet framing changes.

---

## Verification Criteria For A Fix

A candidate fix is good when:

- Android `android_camera_jpeg_in` and `android_camera_i420_roundtrip` remain clean.
- Android `android_local_decoded`, if added, is clean.
- macOS `desktop_remote_decoded` is clean at H.264 960x540.
- 960x540 is smooth enough for normal calls, not a debug-build slideshow.
- H.264 1280x720 either works or fails in an understood performance-only way.
- HEVC behavior is not regressed from current minimum-resolution success.

Run at least:

```bash
cargo check -p wzp-video --target aarch64-linux-android
cargo check -p wzp-video -p wzp-client -p wzp-desktop
```

Then build Android with:

```bash
./scripts/android-build-async.sh --release-debuggable --wait
```

---

## Open Questions

- Does the failing Android device actually report encoder input `stride`, `slice-height`, and `color-format` after `start()`? The code asks for this, but recent logcat sampling did not show the `h264_encoder_input` tracing lines.
- Does Android local decode of its own encoded H.264 reproduce the same lines?
- Is 960x540 failing because H.264 encodes a 544-high macroblock frame and macOS crops or interprets chroma padding incorrectly?
- Are the green/magenta bands chroma-plane corruption, luma padding leakage, or debug overlay from an encoder surface path? Current pipeline uses byte-buffer input, not surface input.
- Is HEVC's pause behavior a separate decoder buffering/keyframe issue or the same layout problem expressed differently?

