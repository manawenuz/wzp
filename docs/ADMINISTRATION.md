# WarzonePhone Relay Administration Guide

This document covers deploying, configuring, and operating wzp-relay instances, including federation setup, monitoring, and troubleshooting.

## Relay Deployment

### Binary

Build and run the relay directly:

```bash
# Build release binary
cargo build --release --bin wzp-relay

# Run with defaults (listen on 0.0.0.0:4433, room mode, no auth)
./target/release/wzp-relay

# Run with config file
./target/release/wzp-relay --config /etc/wzp/relay.toml
```

### Remote Build (Linux)

The included build script provisions a temporary Hetzner Cloud VPS, builds all binaries, and downloads them:

```bash
# Requires: hcloud CLI authenticated, SSH key "wz" registered
./scripts/build-linux.sh
# Outputs to: target/linux-x86_64/
```

Produces: `wzp-relay`, `wzp-client`, `wzp-client-audio`, `wzp-web`, `wzp-bench`.

### Docker

```dockerfile
FROM rust:1.85 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --bin wzp-relay

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/wzp-relay /usr/local/bin/
EXPOSE 4433/udp
EXPOSE 9090/tcp
VOLUME /data
ENV HOME=/data
ENTRYPOINT ["wzp-relay"]
CMD ["--config", "/data/relay.toml", "--metrics-port", "9090"]
```

Build and run:

```bash
docker build -t wzp-relay .
docker run -d \
  --name wzp-relay \
  -p 4433:4433/udp \
  -p 9090:9090/tcp \
  -v /opt/wzp:/data \
  wzp-relay
```

### systemd

Create `/etc/systemd/system/wzp-relay.service`:

```ini
[Unit]
Description=WarzonePhone Relay
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=wzp
Group=wzp
ExecStart=/usr/local/bin/wzp-relay --config /etc/wzp/relay.toml
Restart=always
RestartSec=5
LimitNOFILE=65536

# Security hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/wzp
PrivateTmp=yes

Environment=HOME=/var/lib/wzp
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

Setup:

```bash
# Create service user
useradd --system --home-dir /var/lib/wzp --create-home wzp

# Install binary and config
cp target/release/wzp-relay /usr/local/bin/
mkdir -p /etc/wzp
cp relay.toml /etc/wzp/

# Enable and start
systemctl daemon-reload
systemctl enable --now wzp-relay
journalctl -u wzp-relay -f
```

## TOML Configuration Reference

All fields have defaults. A minimal config file only needs the fields you want to override.

### Core Settings

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `listen_addr` | string (socket addr) | `"0.0.0.0:4433"` | UDP address to listen on for incoming QUIC connections |
| `remote_relay` | string (socket addr) | none | Remote relay address for forward mode. Disables room mode when set |
| `max_sessions` | integer | `100` | Maximum concurrent client sessions |
| `log_level` | string | `"info"` | Logging level: trace, debug, info, warn, error |

### Jitter Buffer

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `jitter_target_depth` | integer | `50` | Target buffer depth in packets (50 = 1 second at 20ms frames) |
| `jitter_max_depth` | integer | `250` | Maximum buffer depth in packets (250 = 5 seconds) |

### Authentication

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `auth_url` | string | none | featherChat auth validation URL. When set, clients must send a bearer token as their first signal message. The relay validates it via `POST <auth_url>` |

### Metrics and Monitoring

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `metrics_port` | integer | none | Port for the Prometheus HTTP metrics endpoint. Disabled if not set |
| `probe_targets` | array of socket addrs | `[]` | Peer relay addresses to probe for health monitoring (1 Ping/s each) |
| `probe_mesh` | boolean | `false` | Enable mesh mode for probe targets |

### Media Processing

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `trunking_enabled` | boolean | `false` | Enable trunk batching for outgoing media. Packs multiple session packets into one QUIC datagram, reducing overhead |

### WebSocket / Browser Support

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `ws_port` | integer | none | Port for WebSocket listener (browser clients). Disabled if not set |
| `static_dir` | string | none | Directory to serve static files (HTML/JS/WASM) |

### Federation

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `peers` | array of PeerConfig | `[]` | Outbound federation peer relays |
| `trusted` | array of TrustedConfig | `[]` | Inbound federation trust list |
| `global_rooms` | array of GlobalRoomConfig | `[]` | Room names to bridge across federation |

### Debugging

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `debug_tap` | string | none | Log packet headers for matching rooms. Use `"*"` for all rooms, or a specific room name |

### PeerConfig Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `url` | string | yes | Address of the peer relay (e.g., `"193.180.213.68:4433"`) |
| `fingerprint` | string | yes | Expected TLS certificate fingerprint (hex with colons) |
| `label` | string | no | Human-readable label for logging |

### TrustedConfig Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `fingerprint` | string | yes | Expected TLS certificate fingerprint (hex with colons) |
| `label` | string | no | Human-readable label for logging |

### GlobalRoomConfig Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Room name to bridge across federation (e.g., `"android"`) |

## CLI Flags Reference

```
wzp-relay [--config <path>] [--listen <addr>] [--remote <addr>]
          [--auth-url <url>] [--metrics-port <port>]
          [--probe <addr>]... [--probe-mesh] [--mesh-status]
          [--trunking] [--global-room <name>]...
          [--debug-tap <room>]
          [--ws-port <port>] [--static-dir <dir>]
```

| Flag | Description |
|------|-------------|
| `--config <path>` | Load configuration from TOML file. CLI flags override config file values |
| `--listen <addr>` | Listen address (default: `0.0.0.0:4433`) |
| `--remote <addr>` | Remote relay for forwarding mode. Disables room mode |
| `--auth-url <url>` | featherChat auth endpoint (e.g., `https://chat.example.com/v1/auth/validate`) |
| `--metrics-port <port>` | Prometheus metrics HTTP port (e.g., `9090`) |
| `--probe <addr>` | Peer relay to probe for health monitoring. Repeatable |
| `--probe-mesh` | Enable mesh mode for probes |
| `--mesh-status` | Print mesh health table and exit (diagnostic) |
| `--trunking` | Enable trunk batching for outgoing media |
| `--global-room <name>` | Declare a room as global (bridged across federation). Repeatable |
| `--debug-tap <room>` | Log packet headers for a room (`"*"` for all rooms) |
| `--event-log <path>` | Write JSONL protocol event log for federation debugging |
| `--version`, `-V` | Print build git hash and exit |
| `--ws-port <port>` | WebSocket listener port for browser clients |
| `--static-dir <dir>` | Directory to serve static files from |
| `--help`, `-h` | Print help and exit |

CLI flags always override config file values when both are specified.

## Federation Setup

### Concepts

- **`[[peers]]`** -- outbound: relays we connect TO. Requires address + fingerprint
- **`[[trusted]]`** -- inbound: relays we accept connections FROM. Requires fingerprint only (they connect to us)
- **`[[global_rooms]]`** -- rooms bridged across all federated peers. Participants on different relays in the same global room hear each other

### Getting Your Relay's Fingerprint

When a relay starts, it logs its TLS fingerprint:

```
INFO TLS certificate (deterministic from relay identity) tls_fingerprint="a5d6:e3c6:5ae7:185c:4eb1:af89:daed:4a43"
INFO federation: to peer with this relay, add to relay.toml:
INFO   [[peers]]
INFO   url = "193.180.213.68:4433"
INFO   fingerprint = "a5d6:e3c6:5ae7:185c:4eb1:af89:daed:4a43"
```

Share this information with the administrator of the peer relay.

### Unknown Peer Connections

When an unknown relay tries to federate, the log shows:

```
WARN unknown relay wants to federate addr=10.0.0.5:12345 fp="7f2a:b391:0c44:..."
INFO   to accept, add to relay.toml:
INFO   [[trusted]]
INFO   fingerprint = "7f2a:b391:0c44:..."
INFO   label = "Relay at 10.0.0.5:12345"
```

## Example Configurations

### Single Relay (Minimal)

```toml
# /etc/wzp/relay.toml
# Minimal config -- all defaults, just enable metrics
metrics_port = 9090
```

Run:

```bash
wzp-relay --config /etc/wzp/relay.toml
```

### Single Relay (Full Featured)

```toml
# /etc/wzp/relay.toml
listen_addr = "0.0.0.0:4433"
max_sessions = 200
log_level = "info"

# Metrics
metrics_port = 9090

# Authentication
auth_url = "https://chat.example.com/v1/auth/validate"

# Browser support
ws_port = 8080
static_dir = "/opt/wzp/web"

# Performance
trunking_enabled = true

# Jitter buffer tuning
jitter_target_depth = 50
jitter_max_depth = 250
```

### Two-Relay Federation

**Relay A** (`relay-a.toml` on 193.180.213.68):

```toml
listen_addr = "0.0.0.0:4433"
metrics_port = 9090

# Outbound: connect to Relay B
[[peers]]
url = "10.0.0.5:4433"
fingerprint = "7f2a:b391:0c44:9e1d:a8b2:c5d7:e3f0:1234"
label = "Relay B (US)"

# Accept inbound from Relay B
[[trusted]]
fingerprint = "7f2a:b391:0c44:9e1d:a8b2:c5d7:e3f0:1234"
label = "Relay B (US)"

# Bridge these rooms
[[global_rooms]]
name = "android"

[[global_rooms]]
name = "general"
```

**Relay B** (`relay-b.toml` on 10.0.0.5):

```toml
listen_addr = "0.0.0.0:4433"
metrics_port = 9090

# Outbound: connect to Relay A
[[peers]]
url = "193.180.213.68:4433"
fingerprint = "a5d6:e3c6:5ae7:185c:4eb1:af89:daed:4a43"
label = "Relay A (EU)"

# Accept inbound from Relay A
[[trusted]]
fingerprint = "a5d6:e3c6:5ae7:185c:4eb1:af89:daed:4a43"
label = "Relay A (EU)"

# Same global rooms
[[global_rooms]]
name = "android"

[[global_rooms]]
name = "general"
```

### Three-Relay Chain (Full Mesh)

For three relays (A, B, C) in full mesh federation, each relay needs peers and trusted entries for the other two:

**Relay A** (EU):

```toml
listen_addr = "0.0.0.0:4433"
metrics_port = 9090

# Probe all peers
probe_targets = ["10.0.0.5:4433", "10.0.0.9:4433"]
probe_mesh = true

# Peers
[[peers]]
url = "10.0.0.5:4433"
fingerprint = "7f2a:b391:0c44:9e1d:a8b2:c5d7:e3f0:1234"
label = "Relay B (US)"

[[peers]]
url = "10.0.0.9:4433"
fingerprint = "3c8e:d2a1:f7b5:6049:81c3:e9d4:a2f6:5678"
label = "Relay C (APAC)"

# Trust
[[trusted]]
fingerprint = "7f2a:b391:0c44:9e1d:a8b2:c5d7:e3f0:1234"
label = "Relay B (US)"

[[trusted]]
fingerprint = "3c8e:d2a1:f7b5:6049:81c3:e9d4:a2f6:5678"
label = "Relay C (APAC)"

# Global rooms
[[global_rooms]]
name = "android"

[[global_rooms]]
name = "general"
```

**Relay B** and **Relay C** follow the same pattern, listing the other two relays in their `[[peers]]` and `[[trusted]]` sections.

## Monitoring

### Prometheus Metrics

Enable with `--metrics-port <port>` or `metrics_port` in TOML. The relay exposes metrics at `GET /metrics` on the specified HTTP port.

#### Relay Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `wzp_relay_active_sessions` | Gauge | -- | Current active sessions |
| `wzp_relay_active_rooms` | Gauge | -- | Current active rooms |
| `wzp_relay_packets_forwarded_total` | Counter | `room` | Total packets forwarded |
| `wzp_relay_bytes_forwarded_total` | Counter | `room` | Total bytes forwarded |
| `wzp_relay_auth_attempts_total` | Counter | `result` (ok/fail) | Auth validation attempts |
| `wzp_relay_handshake_duration_seconds` | Histogram | -- | Crypto handshake time |

#### Per-Session Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `wzp_relay_session_jitter_buffer_depth` | Gauge | `session_id` | Buffer depth per session |
| `wzp_relay_session_loss_pct` | Gauge | `session_id` | Packet loss percentage |
| `wzp_relay_session_rtt_ms` | Gauge | `session_id` | Round-trip time |
| `wzp_relay_session_underruns_total` | Counter | `session_id` | Jitter buffer underruns |
| `wzp_relay_session_overruns_total` | Counter | `session_id` | Jitter buffer overruns |

#### Inter-Relay Probe Metrics

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `wzp_probe_rtt_ms` | Gauge | `target` | RTT to peer relay |
| `wzp_probe_loss_pct` | Gauge | `target` | Loss to peer relay |
| `wzp_probe_jitter_ms` | Gauge | `target` | Jitter to peer relay |
| `wzp_probe_up` | Gauge | `target` | 1 if reachable, 0 if not |

### Prometheus Scrape Config

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'wzp-relay'
    static_configs:
      - targets:
        - 'relay-a:9090'
        - 'relay-b:9090'
    scrape_interval: 10s
```

### Grafana Dashboard

A pre-built dashboard is available at `docs/grafana-dashboard.json`. Import it into Grafana for:

1. **Relay Health** -- active sessions, rooms, packets/s, bytes/s
2. **Call Quality** -- per-session jitter depth, loss%, RTT, underruns over time
3. **Inter-Relay Mesh** -- latency heatmap, probe status, loss trends
4. **Web Bridge** -- active connections, frames bridged, auth failures

### Event Log (Protocol Analyzer)

Use `--event-log` to write a JSONL event log that traces every federation media packet through the relay pipeline. Essential for debugging federation audio issues.

```bash
wzp-relay --config relay.toml --event-log /tmp/events.jsonl
```

Each media packet emits events at every decision point:
- `federation_ingress` — packet arrived from a peer relay
- `local_deliver` — packet delivered to local participants
- `dedup_drop` — packet dropped as duplicate
- `rate_limit_drop` — packet dropped by rate limiter
- `room_not_found` — packet for unknown room
- `local_deliver_error` — delivery to local client failed

Analyze with:
```bash
# Count events by type
cat events.jsonl | python3 -c "
import json, collections, sys
c = collections.Counter()
for l in sys.stdin: c[json.loads(l)['event']] += 1
for k,v in sorted(c.items(), key=lambda x:-x[1]): print(f'  {k}: {v}')
"
```

### Remote Version Check

Verify a deployed relay's version without SSH:

```bash
wzp-client --version-check <relay-addr:port>
```

### Debug Tap

Use `--debug-tap` to log packet headers for debugging:

```bash
# Log headers for room "android"
wzp-relay --debug-tap android

# Log headers for all rooms
wzp-relay --debug-tap '*'
```

Or in TOML:

```toml
debug_tap = "android"
```

### Mesh Status

Print the current mesh health table (diagnostic):

```bash
wzp-relay --mesh-status
```

## Authentication

### featherChat Token Validation

When `--auth-url` is set, the relay requires clients to send an `AuthToken` signal message as their first message after QUIC connection. The relay validates the token by calling:

```
POST <auth_url>
Content-Type: application/json
Authorization: Bearer <token>
```

Expected response:

```json
{
  "valid": true,
  "fingerprint": "a5d6:e3c6:...",
  "alias": "username"
}
```

If validation fails, the client is disconnected.

### Without Authentication

When `--auth-url` is not set, any client can connect. The relay logs:

```
INFO auth disabled -- any client can connect (use --auth-url to enable)
```

## Identity Persistence

### Relay Identity File

The relay stores its identity seed at `~/.wzp/relay-identity` (a 64-character hex string). This seed:

- Is generated automatically on first run
- Persists across restarts
- Derives the relay's Ed25519 signing key and X25519 key agreement key
- Derives the TLS certificate deterministically (same seed = same cert = same fingerprint)

If the identity file is corrupted, the relay generates a new one and logs a warning. This will change the relay's TLS fingerprint, requiring federation peers to update their config.

### Backup

Back up the identity file to preserve the relay's fingerprint:

```bash
cp ~/.wzp/relay-identity /secure/backup/relay-identity
```

To restore, copy the file back before starting the relay.

## Troubleshooting

### Common Issues

| Problem | Cause | Solution |
|---------|-------|---------|
| "unknown argument" on startup | Unrecognized CLI flag | Check `wzp-relay --help` for valid flags |
| "failed to load config" | Invalid TOML syntax | Validate TOML file with `toml-cli` or similar |
| "auth failed" for all clients | Wrong `auth_url` or featherChat server down | Verify URL is reachable: `curl -X POST <auth_url>` |
| "session rejected" | Max sessions reached | Increase `max_sessions` in config |
| Clients cannot connect | Firewall blocking UDP 4433 | Open UDP port 4433 in firewall |
| Federation "unknown relay wants to federate" | Peer's fingerprint not in `[[trusted]]` | Add the logged fingerprint to `[[trusted]]` |
| Federation "fingerprint mismatch" | Peer relay restarted with new identity | Update the fingerprint in `[[peers]]` config |
| Federation audio silent on consecutive connects | Dedup filter or jitter buffer state | Verify relay is running latest build with time-based dedup |
| Federation participant shows wrong relay label | Hub relay not propagating original labels | Update relay to latest build (label preservation fix) |
| Federation disconnect takes >15 seconds | QUIC idle timeout + stale sweeper | Normal: sweeper runs every 5s with 15s TTL. Use latest client with SIGTERM handler for instant disconnect |
| High packet loss between relays | Network congestion or misconfiguration | Check `wzp_probe_loss_pct` metric; consider relay chaining |
| Jitter buffer overruns | Packets arriving faster than playout | Increase `jitter_max_depth` |
| Jitter buffer underruns | Packets arriving too slowly or lost | Check network quality; increase `jitter_target_depth` |
| "probe connection closed" | Peer relay unreachable or crashed | Check peer relay status; will auto-reconnect |
| WebSocket clients cannot connect | `ws_port` not set | Add `--ws-port <port>` or `ws_port` in TOML |
| Browser mic access denied | Not using HTTPS | Use TLS termination in front of the relay or serve via `wzp-web --tls` |

### Log Level Tuning

Set `RUST_LOG` environment variable for fine-grained control:

```bash
# All relay logs at debug level
RUST_LOG=debug wzp-relay

# Only federation at trace, everything else at info
RUST_LOG=info,wzp_relay::federation=trace wzp-relay

# Quiet mode -- only warnings and errors
RUST_LOG=warn wzp-relay
```

### Health Checks

```bash
# Check if relay is listening
nc -zu relay-host 4433

# Check metrics endpoint
curl -s http://relay-host:9090/metrics | head -20

# Check active sessions
curl -s http://relay-host:9090/metrics | grep wzp_relay_active_sessions

# Check federation probe health
curl -s http://relay-host:9090/metrics | grep wzp_probe_up
```
