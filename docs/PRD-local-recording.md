# PRD: Local Recording + Cloud Mixer for Podcast-Quality Interviews

## Problem

WarzonePhone delivers real-time encrypted voice, but the audio quality is limited by network conditions (codec compression, packet loss, jitter). Podcasters and interviewers need pristine, studio-grade recordings of each participant — independent of what the network delivers.

## Solution

**Dual-path architecture**: each client simultaneously (1) participates in the live call at whatever codec quality the network supports, and (2) records their own microphone locally as lossless PCM. After the session, all local recordings are uploaded to a self-hosted mixer service that aligns, normalizes, and outputs a final multi-track or mixed file.

## Architecture

```
                          ┌──────────────────┐
  Mic ──┬── Opus/Codec2 ──► Network (live)   │  ← real-time call
        │                 └──────────────────┘
        │
        └── WAV 48kHz ────► Local File        │  ← pristine recording
                           (timestamped)
                              │
                              ▼ (after hangup)
                          ┌──────────────────┐
                          │  Mixer Service    │  ← self-hosted
                          │  (align + mix)    │
                          └──────────────────┘
                              │
                              ▼
                          Final MP3/WAV/FLAC
```

## Requirements

### Phase 1: Local Recording (MVP)

**All clients (Desktop, Android, Web):**

1. **Record toggle**: User can enable "Record this call" before or during a call
2. **Recording pipeline**: Tap raw PCM from the microphone capture path *before* it enters the codec encoder
3. **File format**: WAV (48kHz, 16-bit, mono) — simple, universally supported, lossless
4. **Sync markers**: Embed a monotonic timestamp (ms since call start) at the beginning of the recording, and periodically (every 10s) write a sync marker packet into a sidecar JSON file:
   ```json
   {"ts_ms": 30000, "seq": 1500, "wall_clock_utc": "2026-04-07T12:00:30Z"}
   ```
   This allows the mixer to align recordings from different participants even if they join at different times.
5. **Storage**:
   - Desktop: `~/.wzp/recordings/{room}_{timestamp}.wav`
   - Android: `Documents/WarzonePhone/{room}_{timestamp}.wav`
   - Web: IndexedDB blob or File System Access API
6. **File size estimate**: 48kHz * 16-bit * mono = 96 KB/s = ~5.6 MB/min = ~345 MB/hour
7. **UI indicator**: Red dot + timer showing recording is active and file size growing
8. **On hangup**: Close the WAV file, show "Recording saved" with file path/size

### Phase 2: Upload to Mixer

1. **Upload endpoint**: Self-hosted HTTP service (Rust or Go) that accepts WAV uploads with metadata
2. **Chunked/resumable upload**: Large files need resumable uploads (tus protocol or simple chunked POST)
3. **Upload metadata**:
   ```json
   {
     "session_id": "uuid",
     "participant_fingerprint": "xxxx:xxxx:...",
     "alias": "Alice",
     "room": "podcast-ep-42",
     "duration_secs": 3600,
     "sync_markers": [...],
     "sample_rate": 48000,
     "channels": 1,
     "bit_depth": 16
   }
   ```
4. **Upload UI**: Progress bar after hangup, option to upload now or later
5. **Retry on failure**: Queue uploads for retry if network is unavailable

### Phase 3: Mixer Service

1. **Alignment**: Use sync markers (wall clock + sequence numbers) to align recordings from all participants to a common timeline
2. **Silence trimming**: Detect and optionally trim leading/trailing silence
3. **Normalization**: Per-track loudness normalization (LUFS-based)
4. **Noise reduction**: Optional per-track noise gate or RNNoise pass
5. **Output formats**:
   - Multi-track: ZIP of individual WAVs (aligned, normalized)
   - Mixed: Single stereo or mono WAV/MP3/FLAC with all participants
   - Podcast-ready: Loudness-normalized to -16 LUFS (podcast standard)
6. **Web UI**: Simple dashboard to see sessions, download outputs, preview waveforms
7. **Self-hosted**: Docker image, single binary, SQLite for metadata

## Implementation Notes

### Recording tap point

The recording must tap *after* AGC (so levels are normalized) but *before* the codec encoder (to avoid compression artifacts). In the current architecture:

```
Mic → Ring Buffer → AGC → [TAP HERE for recording] → Opus/Codec2 → Network
```

**Desktop** (`engine.rs`): After `capture_agc.process_frame()`, before `encoder.encode()`
**Android** (`engine.rs`): Same location — after AGC, before encode
**CLI** (`call.rs`): After `self.agc.process_frame()` in `CallEncoder::encode_frame()`

### WAV writer

Use a simple streaming WAV writer that:
- Writes the WAV header with placeholder data length
- Appends PCM samples as they come
- On close, seeks back to update the data length in the header

### Sync mechanism

Wall-clock UTC alone is insufficient (clocks drift). The sync strategy:
1. Each participant records their local monotonic time + wall clock at call start
2. Periodically (every 10s), each participant writes: `{local_mono_ms, seq_number, utc_iso}`
3. The mixer uses sequence numbers (which are shared via the wire protocol) as ground truth for alignment, with wall clock as a fallback

### Privacy

- Local recordings never leave the device without explicit user action
- Upload is manual, not automatic
- The mixer service processes files and can delete originals after mixing
- No recording data flows through the relay — only the user's own mic

## Non-Goals (v1)

- Live transcription (future)
- Video recording (audio only)
- Automatic upload without user consent
- Recording other participants' audio (only your own mic)
- Real-time mixing (post-session only)

## Milestones

| Phase | Scope | Effort |
|-------|-------|--------|
| 1a | Local WAV recording on Desktop | 1-2 days |
| 1b | Local WAV recording on Android | 1-2 days |
| 1c | Sync markers + metadata sidecar | 1 day |
| 2a | Upload service (HTTP + storage) | 2-3 days |
| 2b | Upload UI in clients | 1-2 days |
| 3a | Mixer: alignment + normalization | 2-3 days |
| 3b | Mixer: web dashboard | 2-3 days |
| 3c | Docker packaging | 1 day |
