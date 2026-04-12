# WarzonePhone User Guide

This guide covers all WarzonePhone client applications: Desktop (Tauri), Android, CLI, and Web.

## Desktop Client (Tauri)

The desktop client is a Tauri application with a native Rust audio engine and a web-based UI. It runs on macOS, Windows, and Linux.

### Connect Screen

When you launch the desktop client, you see the connect screen with:

- **Relay selector** -- click the relay button to open the Manage Relays dialog. Shows relay name, address, connection status (verified/new/changed/offline), and RTT latency
- **Room** -- enter a room name. Clients in the same room hear each other. Room names are hashed before being sent to the relay for privacy
- **Alias** -- your display name shown to other participants
- **OS Echo Cancel** -- checkbox to enable macOS VoiceProcessingIO (Apple's FaceTime-grade AEC). Strongly recommended when using speakers
- **Connect button** -- connects to the selected relay and joins the room
- **Identity info** -- your identicon and fingerprint are shown at the bottom. Click to copy

Recent rooms are displayed below the form for quick reconnection. Click any recent room to select it and its associated relay.

### In-Call Screen

Once connected, the in-call screen shows:

- **Room name** and **call timer** at the top
- **Status indicator** -- green when connected, yellow when reconnecting
- **Audio level meter** -- real-time visualization of outgoing audio
- **Participant list** -- identicon, alias, and fingerprint for each participant. Your own entry is highlighted with a badge
- **Controls** -- Mic toggle, Hang Up, Speaker toggle
- **Stats bar** -- TX and RX frame rates

### Settings Panel

Open with the gear icon or **Cmd+,** (Ctrl+, on Windows/Linux). Contains:

#### Connection

- **Default Room** -- room name used on next connect
- **Alias** -- display name

#### Audio

- **Quality slider** -- 5 levels:

  | Position | Profile | Description |
  |----------|---------|-------------|
  | 0 | Auto | Adaptive quality based on network conditions |
  | 1 | Opus 24k | Good conditions (28.8 kbps with FEC) |
  | 2 | Opus 6k | Degraded conditions (9.0 kbps with FEC) |
  | 3 | Codec2 3.2k | Poor conditions (4.8 kbps with FEC) |
  | 4 | Codec2 1.2k | Catastrophic conditions (2.4 kbps with FEC) |

- **OS Echo Cancellation** -- macOS VoiceProcessingIO toggle
- **Automatic Gain Control** -- normalize mic volume

#### Identity

- **Fingerprint** -- your public identity fingerprint
- **Identity file** -- stored at `~/.wzp/identity`

#### Recent Rooms

- History of recently joined rooms with relay association
- Clear History button

### Manage Relays Dialog

Open by clicking the relay selector button on the connect screen:

- **Relay list** -- each entry shows name, address, identicon (from server fingerprint), lock status, and RTT
- **Select** -- click a relay to make it the default
- **Remove** -- click the X button to delete a relay
- **Add Relay** -- enter name and host:port to add a new relay
- **Ping** -- relays are automatically pinged when the dialog opens. RTT and server fingerprint are updated

### Key Change Warning Dialog

If a relay's TLS fingerprint has changed since your last connection, a warning dialog appears:

- Shows the previously known fingerprint and the new fingerprint
- **Accept New Key** -- trust the new fingerprint and proceed
- **Cancel** -- abort the connection

This is the TOFU (Trust on First Use) model. Fingerprint changes typically mean the relay was restarted with a new identity. However, they could also indicate a man-in-the-middle attack.

### Keyboard Shortcuts

| Shortcut | Action | Context |
|----------|--------|---------|
| **m** | Toggle microphone | In-call |
| **s** | Toggle speaker | In-call |
| **q** | Hang up | In-call |
| **Cmd+,** (Ctrl+,) | Open/close settings | Any |
| **Escape** | Close dialog/settings | Any |
| **Enter** | Connect | Connect screen (when room/alias field is focused) |

### Audio Engine

The desktop audio engine uses:

- **CPAL** for audio I/O (CoreAudio on macOS, WASAPI on Windows, ALSA on Linux)
- **VoiceProcessingIO** on macOS for OS-level echo cancellation (opt-in via checkbox)
- **Lock-free SPSC ring buffers** between audio threads and network threads
- **Direct playout** -- no jitter buffer on the client (the relay buffers instead)
- Audio callbacks deliver 512 f32 samples at 48 kHz on macOS (accumulated to 960-sample frames for codec)

#### Audio Quality Notes

- Always use **Release builds** for real-time audio. Debug builds are too slow for wzp-codec, nnnoiseless, audiopus, and raptorq
- VoiceProcessingIO is strongly recommended on macOS. Software AEC does not work well with the round-trip latency (~35-45ms)
- The quality slider only affects the **encode** side. Decoding always accepts all codecs

### Auto-Reconnect

If the connection drops, the client automatically attempts to reconnect with exponential backoff (1s, 2s, 4s, 8s, capped at 10s). After 5 failed attempts, the client returns to the connect screen. The status dot shows yellow during reconnection.

## Android Client

The Android client is built with Kotlin and Jetpack Compose, using JNI to call the Rust audio engine.

### Call Screen

The main call screen shows:

- **Server selector** -- tap to choose from configured servers
- **Room name** -- enter the room to join
- **Connect/Disconnect** button
- **Participant list** with identicons and aliases
- **Audio level visualization**
- **Mute/Unmute** button

### Settings Screen

The settings screen is organized into sections:

#### Identity

- **Display Name** -- your alias shown to other participants
- **Fingerprint** -- displayed with an identicon. Tap to copy
- **Copy Key** -- copy the 64-character hex seed to clipboard for backup
- **Restore Key** -- paste a previously backed-up hex seed to restore your identity

#### Audio Defaults

- **Voice Volume** -- playout gain slider (-20 dB to +20 dB)
- **Mic Gain** -- capture gain slider (-20 dB to +20 dB)
- **Echo Cancellation (AEC)** -- toggle Android's built-in AEC. Disable if audio sounds distorted
- **Quality slider** -- 8 levels from best to lowest:

  | Position | Profile | Bitrate | Color |
  |----------|---------|---------|-------|
  | 0 | Studio 64k | 70.4 kbps | Green |
  | 1 | Studio 48k | 52.8 kbps | Green |
  | 2 | Studio 32k | 35.2 kbps | Green |
  | 3 | Auto | Adaptive | Yellow-green |
  | 4 | Opus 24k | 28.8 kbps | Yellow-green |
  | 5 | Opus 6k | 9.0 kbps | Yellow |
  | 6 | Codec2 3.2k | 4.8 kbps | Orange |
  | 7 | Codec2 1.2k | 2.4 kbps | Red |

  Note: "Decode always accepts all codecs" -- the quality setting only affects encoding.

#### Servers

- **Server chips** -- tap to select, X to remove (built-in servers cannot be removed)
- **Add Server** -- enter host, port (default 4433), and optional label
- **Force Ping** -- servers are pinged on dialog open to measure RTT

#### Network

- **Prefer IPv6** -- toggle to prefer IPv6 connections when available

#### Room

- **Default Room** -- the room name pre-filled on the call screen

### Identity Backup and Restore

Your identity is a 32-byte seed stored as a 64-character hex string. To back up:

1. Go to Settings > Identity
2. Tap **Copy Key**
3. Store the hex string securely

To restore on a new device:

1. Go to Settings > Identity
2. Tap **Restore Key**
3. Paste the 64-character hex string
4. Tap **Restore** (key is staged)
5. Tap **Save** to apply

The same seed produces the same fingerprint on any device or platform.

## CLI Client (wzp-client)

The CLI client is a command-line tool for testing, recording, and live audio.

### Usage

```
wzp-client [options] [relay-addr]
```

Default relay address: `127.0.0.1:4433`

### Flags Reference

| Flag | Description |
|------|-------------|
| `--live` | Live mic/speaker mode. Requires `--features audio` at build time |
| `--send-tone <secs>` | Send a 440 Hz test tone for N seconds |
| `--send-file <file>` | Send a raw PCM file (48 kHz mono s16le) |
| `--record <file.raw>` | Record received audio to raw PCM file |
| `--echo-test <secs>` | Run automated echo quality test for N seconds. Produces a windowed analysis with loss%, SNR, correlation |
| `--drift-test <secs>` | Run automated clock-drift measurement for N seconds |
| `--sweep` | Run jitter buffer parameter sweep (local, no network). Tests different buffer configurations |
| `--seed <hex>` | Identity seed as 64 hex characters. Compatible with featherChat |
| `--mnemonic <words...>` | Identity seed as BIP39 mnemonic (24 words). All remaining non-flag words are consumed |
| `--room <name>` | Room name. Hashed before sending for privacy |
| `--token <token>` | featherChat bearer token for relay authentication |
| `--metrics-file <path>` | Write JSONL telemetry to file (1 line/sec) |
| `--help`, `-h` | Print help and exit |

### Common Usage Patterns

#### Connectivity Test (Silence)

```bash
# Send 250 silence frames (5 seconds) and exit
wzp-client 127.0.0.1:4433
```

#### Live Audio Call

```bash
# Terminal 1
wzp-relay

# Terminal 2: Alice
wzp-client --live --room myroom 127.0.0.1:4433

# Terminal 3: Bob
wzp-client --live --room myroom 127.0.0.1:4433
```

Both capture from mic and play received audio. Press Ctrl+C to stop.

#### Send Test Tone and Record

```bash
# Terminal 1
wzp-relay

# Terminal 2: Send 10 seconds of 440 Hz tone
wzp-client --send-tone 10 127.0.0.1:4433

# Terminal 3: Record what is received
wzp-client --record call.raw 127.0.0.1:4433
```

Play the recording:

```bash
ffplay -f s16le -ar 48000 -ac 1 call.raw
```

#### Send Audio File

```bash
# Convert to raw PCM first
ffmpeg -i song.mp3 -f s16le -ar 48000 -ac 1 song.raw

# Send through relay
wzp-client --send-file song.raw 127.0.0.1:4433
```

#### Echo Quality Test

```bash
wzp-relay &
wzp-client --echo-test 30 127.0.0.1:4433
```

Produces a windowed analysis showing loss percentage, SNR, correlation, and quality degradation trends.

#### Clock Drift Test

```bash
wzp-relay &
wzp-client --drift-test 60 127.0.0.1:4433
```

Measures clock drift between the send and receive paths over the specified duration.

#### Jitter Buffer Sweep

```bash
# Runs locally, no network needed
wzp-client --sweep
```

Tests different jitter buffer configurations and prints results.

#### With Identity and Auth

```bash
# Using hex seed
wzp-client --seed 0123456789abcdef...64chars --room secure-room --token my-bearer-token relay.example.com:4433

# Using BIP39 mnemonic
wzp-client --mnemonic abandon abandon abandon ... zoo --room secure-room relay.example.com:4433
```

#### With JSONL Telemetry

```bash
wzp-client --live --metrics-file /tmp/call.jsonl relay.example.com:4433
```

Writes one JSON object per second:

```json
{
  "ts": "2026-04-07T12:00:00Z",
  "buffer_depth": 45,
  "underruns": 0,
  "overruns": 0,
  "loss_pct": 1.2,
  "rtt_ms": 34,
  "jitter_ms": 8,
  "frames_sent": 50,
  "frames_received": 49,
  "quality_profile": "GOOD"
}
```

### Audio File Format

All raw PCM files use:

| Property | Value |
|----------|-------|
| Sample rate | 48 kHz |
| Channels | 1 (mono) |
| Sample format | signed 16-bit little-endian (s16le) |

Conversion commands:

```bash
# WAV to raw PCM
ffmpeg -i input.wav -f s16le -ar 48000 -ac 1 output.raw

# MP3 to raw PCM
ffmpeg -i input.mp3 -f s16le -ar 48000 -ac 1 output.raw

# Raw PCM to WAV
ffmpeg -f s16le -ar 48000 -ac 1 -i input.raw output.wav

# Play raw PCM
ffplay -f s16le -ar 48000 -ac 1 file.raw
```

## Web Client (Browser)

The web client runs in a browser via the wzp-web bridge server.

### Setup

```bash
# Start relay
wzp-relay

# Start web bridge
wzp-web --port 8080 --relay 127.0.0.1:4433

# For remote access (requires TLS for mic)
wzp-web --port 8443 --relay 127.0.0.1:4433 --tls
```

Open `http://localhost:8080/room-name` (or `https://...` with TLS).

### Features

- **Open mic** (default) and **push-to-talk** modes
- PTT via on-screen button, mouse hold, or spacebar
- Audio level meter
- Auto-reconnection on disconnect

### Audio Processing

The web client uses AudioWorklet (preferred) with a ScriptProcessorNode fallback:

- **Capture**: Accumulates Float32 samples into 960-sample (20ms) Int16 frames
- **Playback**: Ring buffer capped at 200ms (9600 samples at 48 kHz)

## Identity System

### Overview

Your identity is a 32-byte cryptographic seed that derives:

- **Ed25519 signing key** -- authenticates handshake messages
- **X25519 key agreement key** -- derives shared session encryption keys
- **Fingerprint** -- SHA-256 of the public key, truncated to 16 bytes, displayed as `xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx`
- **Identicon** -- deterministic visual avatar generated from the fingerprint

### Seed Sources

| Source | Description |
|--------|-------------|
| Auto-generated | Created on first run, stored in `~/.wzp/identity` (desktop/CLI) or app storage (Android) |
| `--seed <hex>` | 64-character hex string (CLI) |
| `--mnemonic <words>` | 24-word BIP39 mnemonic (CLI) |
| Copy Key / Restore Key | Hex backup/restore (Android settings) |

### BIP39 Mnemonic Backup

The 32-byte seed can be represented as a 24-word BIP39 mnemonic for human-readable backup. The same mnemonic produces the same identity on any platform or device.

### featherChat Compatibility

The identity derivation uses the same HKDF scheme as featherChat (Warzone messenger). The same seed produces the same fingerprint in both systems, allowing a unified identity across messaging and calling.

### Trust on First Use (TOFU)

Clients remember the fingerprints of relays and peers they connect to. On subsequent connections, if a fingerprint changes, the client warns the user. This protects against man-in-the-middle attacks but requires manual verification on first contact.

## Quality Profiles Explained

### When to Use Each Profile

| Profile | Total Bandwidth | Best For | Trade-offs |
|---------|----------------|----------|------------|
| **Studio 64k** | 70.4 kbps | LAN calls, music, podcasting | Highest quality, needs good network |
| **Studio 48k** | 52.8 kbps | Good WiFi, wired connections | Near-studio quality |
| **Studio 32k** | 35.2 kbps | Reliable WiFi, LTE | Very good quality with lower bandwidth |
| **Auto** | Adaptive | Most users | Automatically switches based on network conditions |
| **Opus 24k** | 28.8 kbps | General use, moderate networks | Good speech quality, reasonable bandwidth |
| **Opus 6k** | 9.0 kbps | 3G networks, congested WiFi | Intelligible speech, some artifacts |
| **Codec2 3.2k** | 4.8 kbps | Poor connections | Robotic but intelligible, narrowband |
| **Codec2 1.2k** | 2.4 kbps | Satellite links, extreme loss | Minimal intelligibility, last resort |

### Auto Mode

Auto mode starts at the **Good (Opus 24k)** profile and adapts based on observed network quality:

- **Downgrade** -- 3 consecutive bad quality reports (2 on cellular) trigger a step down
- **Upgrade** -- 10 consecutive good quality reports trigger a step up (one tier at a time)
- **Network handoff** -- switching from WiFi to cellular triggers a preemptive one-tier downgrade plus a 10-second FEC boost

Auto mode uses three tiers (Good, Degraded, Catastrophic). It does not use the Studio profiles, which must be selected manually.

### Manual Override

When you select a specific profile (not Auto), adaptive switching is disabled. The encoder stays at the selected profile regardless of network conditions. This is useful when you know your network quality and want consistent encoding, or when you want to force a specific bitrate.

Note: The decoder always accepts all codecs. A manual quality selection only affects what you send, not what you receive.

## Direct 1:1 Calling (Desktop + Android)

In addition to room-mode group calls, you can place direct calls to a specific peer by fingerprint. Direct calls bypass room state entirely — the relay is used purely as a signaling gateway and for media relay. There is no need for the callee to join a room beforehand; they just need to be registered with the same signal hub.

### UI elements in the direct-call panel

- **Place call field** — paste a fingerprint (the long hex string you see under your own identity) and click Call. The callee sees a ringing UI.
- **Recent contacts row** — a horizontal strip of chips showing your most recently called/receiving peers. Click a chip to re-dial. Aliases are shown if the peer has one, otherwise a short fingerprint prefix.
- **Call history list** — every direct call you've placed, received, or missed, with direction indicator (↗ Outgoing, ↙ Incoming, ✗ Missed), the peer's alias (if known) or fingerprint prefix, and a timestamp. Click an entry to re-dial.
- **Deregister button** — drops your signal-hub registration without quitting the app. Useful when switching identities (e.g. testing with two accounts on one machine) or when you want to explicitly appear offline to peers.
- **Clear history button** — wipes the call history store. Does not affect current calls.

### Live updates

The call history updates in real time across all views via Tauri events (`history-changed`). Placing, answering, or missing a call immediately refreshes the history list and the recent contacts row — no manual refresh needed.

### Default room

On first launch, the room name in the room-mode panel defaults to `general` (changed from the prior `android` default so the desktop and Android clients don't silently talk past each other). You can still change it to any room name, and the last-used room is remembered across launches.

### Random alias

New installations derive a human-friendly alias from your identity seed — something like `silent-forest-41` or `bold-river-07`. It's deterministic, so reinstalling without changing your seed gives you the same alias. The alias is shown alongside your fingerprint in the header and is what peers see in their call history when they receive your call.

You can override the alias in Settings → Identity if you want a specific name.

## Windows AEC Variants

The Windows desktop build ships in two variants for echo cancellation, depending on which backend you want to exercise. Both are `wzp-desktop.exe` binaries — only the internal audio backend differs.

| Build | File | Capture backend | AEC | When to use |
|---|---|---|---|---|
| **noAEC baseline** | `wzp-desktop-noAEC.exe` | CPAL (WASAPI shared mode) | None | Headphone-only use, or for A/B comparison against the AEC build |
| **Communications AEC** | `wzp-desktop.exe` | Direct WASAPI with `AudioCategory_Communications` | **Yes** — Windows routes the capture stream through the driver's communications APO chain (AEC + noise suppression + automatic gain control) | Any speaker-mode call, laptop built-in speakers, anywhere echo is audible |

**Quality caveat**: the communications AEC operates at the OS level and its algorithm depends on the audio driver's installed APO chain. On modern consumer laptops with Intel Smart Sound, Dolby, recent Realtek, or Windows 11 Voice Clarity, the quality is excellent (effectively matching what Teams/Zoom deliver). On generic class-compliant USB microphones or older drivers, the communications APO may not be present at all — in that case the build behaves identically to the noAEC baseline.

If you hear echo on the AEC build, try these in order before escalating:

1. **Check which capture device is selected as "Default Device - Communications"** in Windows Sound Settings → Recording tab. Right-click any device to set it. The AEC build opens the device marked as `eCommunications`, not `eConsole`, so changing the default-communications device changes what we capture from.
2. **Verify the driver exposes a communications APO**. Sound Settings → Recording → your mic → Properties → Advanced → look for an "Enhancements" or "Signal Enhancements" tab. If it's absent, the driver has no APOs and the AEC build effectively has no AEC.
3. **Try the classic Voice Capture DSP build** when it ships (tracked as task #26). That uses Microsoft's bundled software AEC (`CLSID_CWMAudioAEC`) which works on every Windows machine regardless of driver.

### Installing the Windows builds

1. Windows 10: install the [WebView2 Runtime Evergreen Bootstrapper](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) first. Windows 11 has it pre-installed.
2. Copy `wzp-desktop.exe` (or `wzp-desktop-noAEC.exe`) to any directory and double-click. No installer needed.
3. First launch creates the config + identity store at `%APPDATA%\com.wzp.phone\`.
