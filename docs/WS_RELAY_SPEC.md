# WS Support in wzp-relay — Implementation Spec

## Goal

Add WebSocket listener to `wzp-relay` so browsers connect directly, eliminating `wzp-web` bridge.

```
Before:  Browser → WS → wzp-web → QUIC → wzp-relay
After:   Browser → WS → wzp-relay (handles both WS + QUIC)
```

## Architecture

```
wzp-relay
├── QUIC listener (:4433) — native clients, inter-relay
├── WS listener (:8080)   — browsers via Caddy
│   ├── GET /ws/{room}    — WebSocket upgrade
│   └── Auth: first msg = {"type":"auth","token":"..."}
└── Shared RoomManager    — both transports in same rooms
```

## Key Changes

### 1. Abstract `Participant` over transport type

**File: `room.rs`**

Currently:
```rust
struct Participant {
    id: ParticipantId,
    _addr: std::net::SocketAddr,
    transport: Arc<wzp_transport::QuinnTransport>,
}
```

Change to:
```rust
struct Participant {
    id: ParticipantId,
    _addr: std::net::SocketAddr,
    sender: ParticipantSender,
}

/// How to send a media packet to a participant.
enum ParticipantSender {
    Quic(Arc<wzp_transport::QuinnTransport>),
    WebSocket(tokio::sync::mpsc::Sender<bytes::Bytes>),
}
```

The `others()` method returns `Vec<ParticipantSender>` instead of `Vec<Arc<QuinnTransport>>`.

`ParticipantSender` implements a `send_pcm(&self, data: &[u8])` method:
- **Quic**: wraps in `MediaPacket`, calls `transport.send_media()`
- **WebSocket**: sends raw binary frame via the mpsc channel

### 2. Add `join_ws()` to RoomManager

```rust
pub fn join_ws(
    &mut self,
    room_name: &str,
    addr: std::net::SocketAddr,
    sender: tokio::sync::mpsc::Sender<bytes::Bytes>,
    fingerprint: Option<&str>,
) -> Result<ParticipantId, String>
```

### 3. Add WS listener in `main.rs`

New flag: `--ws-port 8080`

```rust
if let Some(ws_port) = config.ws_port {
    let room_mgr = room_mgr.clone();
    let auth_url = config.auth_url.clone();
    let metrics = metrics.clone();
    tokio::spawn(run_ws_server(ws_port, room_mgr, auth_url, metrics));
}
```

### 4. WebSocket handler (`ws.rs` — new file)

```rust
use axum::{
    extract::{ws::{Message, WebSocket}, Path, WebSocketUpgrade},
    routing::get,
    Router,
};

async fn ws_handler(
    Path(room): Path<String>,
    ws: WebSocketUpgrade,
    /* state */
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, room, state))
}

async fn handle_ws(mut socket: WebSocket, room: String, state: WsState) {
    let addr = /* peer addr */;

    // 1. Auth: first message must be {"type":"auth","token":"..."}
    let fingerprint = if let Some(ref auth_url) = state.auth_url {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                let parsed: serde_json::Value = serde_json::from_str(&text)?;
                if parsed["type"] == "auth" {
                    let token = parsed["token"].as_str().unwrap();
                    let client = auth::validate_token(auth_url, token).await?;
                    Some(client.fingerprint)
                } else { return; }
            }
            _ => return,
        }
    } else { None };

    // 2. Create mpsc channel for outbound frames
    let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(64);

    // 3. Join room
    let participant_id = {
        let mut mgr = state.room_mgr.lock().await;
        mgr.join_ws(&room, addr, tx, fingerprint.as_deref())?
    };

    // 4. Run send/recv loops
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Outbound: mpsc rx → WS send
    let send_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if ws_tx.send(Message::Binary(data.to_vec())).await.is_err() {
                break;
            }
        }
    });

    // Inbound: WS recv → fan-out to room
    loop {
        match ws_rx.next().await {
            Some(Ok(Message::Binary(data))) => {
                // Raw PCM Int16 from browser — fan-out to all others
                let others = {
                    let mgr = state.room_mgr.lock().await;
                    mgr.others(&room, participant_id)
                };
                for other in &others {
                    other.send_raw(&data);
                }
            }
            Some(Ok(Message::Close(_))) | None => break,
            _ => continue,
        }
    }

    // 5. Cleanup
    send_task.abort();
    let mut mgr = state.room_mgr.lock().await;
    mgr.leave(&room, participant_id);
}
```

### 5. Cross-transport fan-out

When a QUIC participant sends audio → WS participants receive raw PCM bytes.
When a WS participant sends audio → QUIC participants receive a `MediaPacket`.

The `ParticipantSender::send_raw()` method:
```rust
impl ParticipantSender {
    async fn send_raw(&self, pcm_bytes: &[u8]) {
        match self {
            ParticipantSender::WebSocket(tx) => {
                let _ = tx.try_send(bytes::Bytes::copy_from_slice(pcm_bytes));
            }
            ParticipantSender::Quic(transport) => {
                // Wrap raw PCM in a MediaPacket
                let pkt = MediaPacket {
                    header: MediaHeader::default_pcm(),
                    payload: bytes::Bytes::copy_from_slice(pcm_bytes),
                    quality_report: None,
                };
                let _ = transport.send_media(&pkt).await;
            }
        }
    }
}
```

For QUIC→WS direction, `run_participant` extracts `pkt.payload` bytes and sends to WS channels.

### 6. Dependencies to add

```toml
# wzp-relay/Cargo.toml
axum = { version = "0.8", features = ["ws"] }
tokio = { version = "1", features = ["full"] }  # already present
```

### 7. Config change

```rust
// config.rs
pub struct RelayConfig {
    // ... existing fields ...
    pub ws_port: Option<u16>,
}
```

### 8. Docker compose change (featherChat side)

Remove `wzp-web` service entirely. Update Caddy to proxy `/audio/*` to relay's WS port:

```yaml
# Before:
wzp-web:
  entrypoint: ["wzp-web"]
  command: ["--port", "8080", "--relay", "172.28.0.10:4433"]

# After: REMOVED. Relay handles WS directly.

wzp-relay:
  command:
    - "--listen"
    - "0.0.0.0:4433"
    - "--ws-port"
    - "8080"
    - "--auth-url"
    - "http://warzone-server:7700/v1/auth/validate"
```

## What Stays the Same

- Browser's `startAudio()` — unchanged, still connects WS to `/audio/ws/ROOM`
- Caddy proxies `/audio/*` → relay:8080 (same path, different backend)
- Auth flow — same JSON token as first message
- PCM format — same Int16 binary frames
- QUIC clients — unchanged, still connect to :4433
- Room naming, ACL, session management — all unchanged

## Testing

1. Start relay with `--ws-port 8080 --listen 0.0.0.0:4433`
2. Open browser, initiate call via featherChat
3. Verify audio flows (both directions)
4. Verify QUIC + WS clients can be in same room (mixed mode)
5. Verify auth works
6. Verify room cleanup on disconnect

## Migration Path

1. Implement WS in relay
2. Test with featherChat (no featherChat changes needed)
3. Remove wzp-web from Docker stack
4. Later: add WebTransport alongside WS
