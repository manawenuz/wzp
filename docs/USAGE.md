# WarzonePhone Usage Guide

## Prerequisites

- **Rust** 1.85+ (2024 edition)
- **System libraries** (Linux): `cmake`, `pkg-config`, `libasound2-dev` (for audio feature)
- **System libraries** (macOS): Xcode command line tools (CoreAudio is included)

## Building from Source

### All Binaries (Headless)

```bash
cargo build --release --bin wzp-relay --bin wzp-client --bin wzp-bench --bin wzp-web
```

### Client with Live Audio Support

```bash
cargo build --release --bin wzp-client --features audio
```

### Run All Tests

```bash
cargo test --workspace --lib
```

### Building for Linux (Remote Build Script)

The project includes `scripts/build-linux.sh` which provisions a temporary Hetzner Cloud VPS, builds all binaries, and downloads them:

```bash
# Requires: hcloud CLI authenticated, SSH key "wz" registered
./scripts/build-linux.sh
# Outputs to: target/linux-x86_64/
```

The build script produces:
- `wzp-relay` -- relay daemon
- `wzp-client` -- headless client
- `wzp-client-audio` -- client with mic/speaker support (needs libasound2)
- `wzp-web` -- web bridge server
- `wzp-bench` -- performance benchmarks

### CI Build

The `.gitea/workflows/build.yml` workflow builds release binaries for:
- Linux amd64
- Linux arm64 (cross-compiled)
- Linux armv7 (cross-compiled)

Triggered on version tags (`v*`) or manual dispatch.

---

## Binaries and CLI Flags

### wzp-relay

The relay daemon that forwards media between clients.

```
Usage: wzp-relay [--listen <addr>] [--remote <addr>]

Options:
  --listen <addr>  Listen address (default: 0.0.0.0:4433)
  --remote <addr>  Remote relay for forwarding (disables room mode)
```

**Room mode** (default): Clients join rooms by name. Packets are forwarded to all other participants in the same room (SFU model). Room name comes from QUIC SNI or defaults to "default".

**Forward mode** (`--remote`): All traffic is forwarded to a remote relay. Used for chaining relays across lossy/censored links.

### wzp-client

The CLI test client for sending and receiving audio.

```
Usage: wzp-client [options] [relay-addr]

Options:
  --live                 Live mic/speaker mode (requires --features audio)
  --send-tone <secs>     Send a 440Hz test tone for N seconds
  --send-file <file>     Send a raw PCM file (48kHz mono s16le)
  --record <file.raw>    Record received audio to raw PCM file
  --echo-test <secs>     Run automated echo quality test
```

Default relay address: `127.0.0.1:4433`

### wzp-bench

Performance benchmark tool.

```
Usage: wzp-bench [OPTIONS]

Options:
  --codec       Run codec roundtrip benchmark (Opus 24kbps, 1000 frames)
  --fec         Run FEC recovery benchmark (100 blocks)
  --crypto      Run encryption benchmark (30000 packets)
  --pipeline    Run full pipeline benchmark (50 frames E2E)
  --all         Run all benchmarks (default if no flag given)
  --loss <N>    FEC loss percentage for --fec (default: 20)
```

### wzp-web

Web bridge server that connects browser audio via WebSocket to the relay.

```
Usage: wzp-web [--port 8080] [--relay 127.0.0.1:4433] [--tls]

Options:
  --port <port>     HTTP/WebSocket port (default: 8080)
  --relay <addr>    WZP relay address (default: 127.0.0.1:4433)
  --tls             Enable HTTPS (self-signed cert, required for mic on Android/remote)
```

Room URLs: `http://host:port/<room-name>` or `https://host:port/<room-name>` with `--tls`.

---

## Deployment Examples

### 1. Single Relay Echo Test

Start a relay, send a tone, and record the echo:

```bash
# Terminal 1: Start relay
wzp-relay --listen 0.0.0.0:4433

# Terminal 2: Send 10s of 440Hz tone and record the response
wzp-client --send-tone 10 --record echo.raw 127.0.0.1:4433
```

Play the recording:
```bash
ffplay -f s16le -ar 48000 -ac 1 echo.raw
```

### 2. Two-Party Call Through Relay

Two clients connected to the same relay default room:

```bash
# Terminal 1: Relay
wzp-relay

# Terminal 2: Client A — send tone
wzp-client --send-tone 30 127.0.0.1:4433

# Terminal 3: Client B — record
wzp-client --record call.raw 127.0.0.1:4433
```

### 3. Multi-Party Room Call

Multiple clients join the same named room. The relay QUIC SNI determines the room. With the web bridge, room names come from the URL path:

```bash
# Relay
wzp-relay

# Web bridge
wzp-web --port 8080 --relay 127.0.0.1:4433

# Browser clients open:
#   http://localhost:8080/my-room
# All clients on /my-room hear each other.
```

### 4. Two-Relay Chain (Lossy Link)

Chain two relays for crossing a censored or lossy network boundary:

```bash
# Destination-side relay (receives from the forward relay)
wzp-relay --listen 0.0.0.0:4433

# Client-side relay (forwards to the destination relay)
wzp-relay --listen 0.0.0.0:5433 --remote <dest-relay-ip>:4433

# Client connects to the client-side relay
wzp-client --send-tone 10 127.0.0.1:5433
```

### 5. Web Browser Call with TLS

TLS is required for microphone access on non-localhost origins (Android, remote browsers):

```bash
# Relay
wzp-relay

# Web bridge with TLS (self-signed certificate)
wzp-web --port 8443 --relay 127.0.0.1:4433 --tls

# Open in browser (accept self-signed cert warning):
#   https://your-server:8443/room-name
```

The web UI supports:
- Open mic (default) and push-to-talk modes
- PTT via on-screen button, mouse hold, or spacebar
- Audio level meter
- Auto-reconnection on disconnect

### 6. Automated Echo Quality Test

```bash
wzp-relay &
wzp-client --echo-test 30 127.0.0.1:4433
```

Produces a windowed analysis report showing loss percentage, SNR, correlation, and detects quality degradation trends over time.

### 7. Live Audio Call (requires `--features audio`)

```bash
wzp-relay &

# Terminal 2
wzp-client --live 127.0.0.1:4433

# Terminal 3
wzp-client --live 127.0.0.1:4433
```

Both clients capture from the default microphone and play received audio through the default speaker. Press Ctrl+C to stop.

---

## Audio File Format

All raw PCM files use:
- Sample rate: **48 kHz**
- Channels: **1** (mono)
- Sample format: **signed 16-bit little-endian** (s16le)

### ffmpeg Conversion Commands

```bash
# WAV to raw PCM
ffmpeg -i input.wav -f s16le -ar 48000 -ac 1 output.raw

# MP3 to raw PCM
ffmpeg -i input.mp3 -f s16le -ar 48000 -ac 1 output.raw

# Raw PCM to WAV
ffmpeg -f s16le -ar 48000 -ac 1 -i input.raw output.wav

# Play raw PCM directly
ffplay -f s16le -ar 48000 -ac 1 file.raw
# or with the newer channel layout syntax:
ffplay -f s16le -ar 48000 -ch_layout mono file.raw
```

### Sending an Audio File

```bash
# Convert your audio to raw PCM first
ffmpeg -i song.mp3 -f s16le -ar 48000 -ac 1 song.raw

# Send through relay
wzp-client --send-file song.raw 127.0.0.1:4433
```
