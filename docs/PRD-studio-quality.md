# PRD: Studio Quality Tiers (Opus 32k/48k/64k)

## Status: Implemented

Studio quality tiers have been added to the wire protocol and all clients.

## What Was Added

### Wire Protocol (codec_id.rs)

Three new `CodecId` variants using the 4-bit header space (values 6-8):

| CodecId | Wire Value | Bitrate | Frame | Use Case |
|---------|-----------|---------|-------|----------|
| Opus32k | 6 | 32 kbps | 20ms | Studio low — noticeable improvement over 24k for voice |
| Opus48k | 7 | 48 kbps | 20ms | Studio — excellent voice, captures nuance |
| Opus64k | 8 | 64 kbps | 20ms | Studio high — near-transparent quality |

### Quality Profiles

| Profile | Codec | FEC | Bandwidth (with FEC) |
|---------|-------|-----|---------------------|
| STUDIO_32K | Opus 32k | 10% | ~35 kbps |
| STUDIO_48K | Opus 48k | 10% | ~53 kbps |
| STUDIO_64K | Opus 64k | 10% | ~70 kbps |

FEC is set to 10% (vs 20% for GOOD) — studio assumes a good network.

### Client Support

| Client | Selection | Status |
|--------|-----------|--------|
| Desktop (Tauri) | Quality slider in Settings (8 levels) | Done |
| CLI | `--profile studio-64k` / `studio-48k` / `studio-32k` | Done |
| Android | Needs codec picker update in SettingsScreen.kt | TODO |
| Web | Needs UI | TODO |

### Cross-Codec Interop

All decoder auto-switch paths (call.rs, desktop engine.rs) handle the new codec IDs. A studio-64k client can talk to a codec2-1200 client — the receiver auto-switches.

## When to Use Studio Tiers

- **Podcast recording sessions**: Use studio-64k for best quality (combined with local WAV recording for pristine output)
- **Music collaboration**: Opus at 48-64k captures instrument harmonics much better than 24k
- **Good network conditions**: Only useful when bandwidth isn't constrained; the extra bits are wasted on lossy networks

## When NOT to Use

- **Mobile data**: Stick with Auto/GOOD — studio tiers use 2-3x the bandwidth
- **High packet loss**: Studio profiles use minimal FEC (10%); degraded networks need DEGRADED or CATASTROPHIC profiles with 50-100% FEC
- **Large group calls**: Each participant's stream multiplies bandwidth; 64k * 10 participants = 640 kbps incoming

## Backward Compatibility

Old clients (before this change) will receive packets with CodecId 6/7/8 which they don't recognize. The `from_wire()` returns `None` for unknown values, causing the packet to be dropped. Old clients can still *send* to new clients fine (they use CodecId 0-5). This is acceptable for a pre-release protocol.
