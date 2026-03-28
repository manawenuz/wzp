# WZP Telemetry & Observability

## Overview

WarzonePhone exports Prometheus-compatible metrics from all services (relay, web bridge, client) for Grafana dashboards. Inter-relay health probes provide always-on monitoring with negligible bandwidth overhead via multiplexed test lines.

## Architecture

```
┌──────────┐    probe (1 pkt/s)    ┌──────────┐
│ Relay A  │◄─────────────────────►│ Relay B  │
│ :4433    │                       │ :4433    │
│ /metrics │                       │ /metrics │
└────┬─────┘                       └────┬─────┘
     │                                  │
     │ scrape                           │ scrape
     ▼                                  ▼
┌─────────────────────────────────────────────┐
│              Prometheus                      │
└─────────────────┬───────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────┐
│              Grafana                         │
│  ┌─────────┐ ┌──────────┐ ┌──────────────┐ │
│  │ Relay   │ │ Per-call │ │ Inter-relay  │ │
│  │ Health  │ │ Quality  │ │ Latency Map  │ │
│  └─────────┘ └──────────┘ └──────────────┘ │
└─────────────────────────────────────────────┘
```

## Metrics Exported

### Relay (`/metrics` on HTTP port, default :9090)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `wzp_relay_active_sessions` | Gauge | — | Current active sessions |
| `wzp_relay_active_rooms` | Gauge | — | Current active rooms |
| `wzp_relay_packets_forwarded_total` | Counter | `room` | Total packets forwarded |
| `wzp_relay_bytes_forwarded_total` | Counter | `room` | Total bytes forwarded |
| `wzp_relay_auth_attempts_total` | Counter | `result` (ok/fail) | Auth validation attempts |
| `wzp_relay_handshake_duration_seconds` | Histogram | — | Crypto handshake time |
| `wzp_relay_session_jitter_buffer_depth` | Gauge | `session_id` | Buffer depth per session |
| `wzp_relay_session_loss_pct` | Gauge | `session_id` | Packet loss percentage |
| `wzp_relay_session_rtt_ms` | Gauge | `session_id` | Round-trip time |
| `wzp_relay_session_underruns_total` | Counter | `session_id` | Jitter buffer underruns |
| `wzp_relay_session_overruns_total` | Counter | `session_id` | Jitter buffer overruns |

### Web Bridge (`/metrics` on same HTTP port)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `wzp_web_active_connections` | Gauge | — | Current WebSocket connections |
| `wzp_web_frames_bridged_total` | Counter | `direction` (up/down) | Audio frames bridged |
| `wzp_web_auth_failures_total` | Counter | — | Browser auth failures |
| `wzp_web_handshake_latency_seconds` | Histogram | — | Relay handshake time |

### Inter-Relay Probes

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `wzp_probe_rtt_ms` | Gauge | `target` | RTT to peer relay |
| `wzp_probe_loss_pct` | Gauge | `target` | Loss to peer relay |
| `wzp_probe_jitter_ms` | Gauge | `target` | Jitter to peer relay |
| `wzp_probe_up` | Gauge | `target` | 1 if reachable, 0 if not |

### Client (JSONL file)

When `--metrics-file <path>` is used, the client writes one JSON object per second:

```json
{
  "ts": "2026-03-28T06:30:00Z",
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

## Task Breakdown

### WZP-P2-T5: Telemetry & Observability

| ID | Task | Dependencies | Effort |
|----|------|-------------|--------|
| **S1** | Prometheus `/metrics` on relay | None | 2-3h |
| **S2** | Per-session metrics (jitter, loss, RTT) | S1 | 2-3h |
| **S3** | Prometheus `/metrics` on web bridge | None | 2h |
| **S4** | Client `--metrics-file` JSONL export | None | 2h |
| **S5** | Inter-relay health probe (`--probe`) | S1 | 4-6h |
| **S6** | Probe mesh mode (all relays probe each other) | S5 | 2-3h |
| **S7** | Grafana dashboard JSON | S1-S6 | 2h |

### Parallelization

- **Group A** (parallel): S1, S3, S4 — three different binaries, no file overlap
- **Group B** (sequential): S2 after S1, then S5 → S6
- **Last**: S7 after all metrics are defined

## Inter-Relay Health Probes

The probe is a multiplexed test line: one QUIC connection per peer relay, one silent media packet per second (~50 bytes/s). This provides:

- **Continuous RTT measurement**: Ping/Pong signals timed to <1ms precision
- **Loss detection**: Sequence gaps tracked over sliding 60s window
- **Jitter monitoring**: Variation in inter-packet arrival times
- **Outage detection**: `wzp_probe_up` drops to 0 within seconds

### Why multiplexed?

WZP already multiplexes media on a single QUIC connection. The probe session shares the same connection pool — no extra ports, no extra TLS handshakes. At 1 pkt/s of silence (~50 bytes after Opus encoding + headers), the overhead is negligible even on metered links.

### Probe mesh example

With 3 relays (A, B, C), each probes the other 2:

```
A → B: rtt=12ms loss=0.0% jitter=2ms
A → C: rtt=45ms loss=0.1% jitter=5ms
B → A: rtt=13ms loss=0.0% jitter=2ms
B → C: rtt=38ms loss=0.0% jitter=4ms
C → A: rtt=44ms loss=0.2% jitter=6ms
C → B: rtt=37ms loss=0.0% jitter=3ms
```

This matrix feeds the Grafana latency heatmap and triggers alerts on degradation.

## Usage

```bash
# Relay with metrics
wzp-relay --listen 0.0.0.0:4433 --metrics-port 9090

# Relay with metrics + probe peer
wzp-relay --listen 0.0.0.0:4433 --metrics-port 9090 --probe relay-b:4433

# Web bridge with metrics
wzp-web --port 8080 --relay 127.0.0.1:4433 --metrics-port 9091

# Client with JSONL telemetry
wzp-client --live --metrics-file /tmp/call-metrics.jsonl relay:4433
```

## Grafana Dashboard

The pre-built dashboard (`docs/grafana-dashboard.json`) includes:

1. **Relay Health** — active sessions, rooms, packets/s, bytes/s
2. **Call Quality** — per-session jitter depth, loss%, RTT, underruns over time
3. **Inter-Relay Mesh** — latency heatmap, probe status, loss trends
4. **Web Bridge** — active connections, frames bridged, auth failures
