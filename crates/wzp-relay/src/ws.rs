//! WebSocket transport for browser clients.
//!
//! Browsers connect via `GET /ws/{room}` → WebSocket upgrade.
//! First message must be auth JSON (if auth is enabled).
//! Subsequent messages are binary PCM frames forwarded to/from the room.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

use crate::auth;
use crate::metrics::RelayMetrics;
use crate::presence::PresenceRegistry;
use crate::room::RoomManager;
use crate::session_mgr::SessionManager;

/// Shared state for WebSocket handlers.
#[derive(Clone)]
pub struct WsState {
    pub room_mgr: Arc<RoomManager>,
    pub session_mgr: Arc<Mutex<SessionManager>>,
    pub auth_url: Option<String>,
    pub metrics: Arc<RelayMetrics>,
    pub presence: Arc<Mutex<PresenceRegistry>>,
}

/// Start the WebSocket + static file server.
pub async fn run_ws_server(port: u16, state: WsState, static_dir: Option<String>) {
    let mut app = Router::new()
        .route("/ws/{room}", get(ws_upgrade_handler))
        .with_state(state);

    if let Some(dir) = static_dir {
        info!(dir = %dir, "serving static files");
        app = app.fallback_service(ServeDir::new(dir));
    }

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    info!(%addr, "WebSocket server listening");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind WS listener");
    axum::serve(listener, app).await.expect("WS server failed");
}

async fn ws_upgrade_handler(
    Path(room): Path<String>,
    State(state): State<WsState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, room, state))
}

async fn handle_ws_connection(socket: WebSocket, room: String, state: WsState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // 1. Auth: if auth_url is set, first message must be {"type":"auth","token":"..."}
    let fingerprint: Option<String> = if let Some(ref auth_url) = state.auth_url {
        match ws_rx.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(parsed) if parsed["type"] == "auth" => {
                        if let Some(token) = parsed["token"].as_str() {
                            match auth::validate_token(auth_url, token).await {
                                Ok(client) => {
                                    state.metrics.auth_attempts.with_label_values(&["ok"]).inc();
                                    info!(fingerprint = %client.fingerprint, "WS authenticated");
                                    let _ = ws_tx
                                        .send(Message::Text(r#"{"type":"auth_ok"}"#.into()))
                                        .await;
                                    Some(client.fingerprint)
                                }
                                Err(e) => {
                                    state
                                        .metrics
                                        .auth_attempts
                                        .with_label_values(&["fail"])
                                        .inc();
                                    let _ = ws_tx
                                        .send(Message::Text(
                                            format!(r#"{{"type":"auth_error","error":"{e}"}}"#)
                                                .into(),
                                        ))
                                        .await;
                                    warn!("WS auth failed: {e}");
                                    return;
                                }
                            }
                        } else {
                            warn!("WS auth: missing token field");
                            return;
                        }
                    }
                    _ => {
                        warn!("WS: expected auth message as first frame");
                        return;
                    }
                }
            }
            _ => {
                warn!("WS: connection closed before auth");
                return;
            }
        }
    } else {
        let _ = ws_tx
            .send(Message::Text(r#"{"type":"auth_ok"}"#.into()))
            .await;
        None
    };

    // 2. Create mpsc channel for outbound frames (room → browser)
    let (tx, mut rx) = mpsc::channel::<Bytes>(64);

    // 3. Create session
    let session_id = {
        let mut smgr = state.session_mgr.lock().await;
        match smgr.create_session(&room, fingerprint.clone()) {
            Ok(id) => id,
            Err(e) => {
                error!(room = %room, "WS session rejected: {e}");
                return;
            }
        }
    };
    state.metrics.active_sessions.inc();

    // 4. Join room with WS sender
    let addr: SocketAddr = ([0, 0, 0, 0], 0).into();
    let participant_id = {
        match state.room_mgr.join_ws(&room, addr, tx, fingerprint.as_deref()) {
            Ok(id) => {
                state.metrics.active_rooms.set(state.room_mgr.list().len() as i64);
                id
            }
            Err(e) => {
                error!(room = %room, "WS room join denied: {e}");
                state.metrics.active_sessions.dec();
                let mut smgr = state.session_mgr.lock().await;
                smgr.remove_session(session_id);
                return;
            }
        }
    };

    // 5. Register presence
    if let Some(ref fp) = fingerprint {
        let mut reg = state.presence.lock().await;
        reg.register_local(fp, None, Some(room.clone()));
    }

    info!(room = %room, participant = participant_id, "WS client joined");

    // 6. Outbound task: mpsc rx → WS binary frames
    let send_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if ws_tx
                .send(Message::Binary(data.to_vec().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 7. Inbound: WS recv → fan-out to room
    loop {
        match ws_rx.next().await {
            Some(Ok(Message::Binary(data))) => {
                let others = state.room_mgr.others(&room, participant_id);
                for other in &others {
                    let _ = other.send_raw(&data).await;
                }
                state
                    .metrics
                    .packets_forwarded
                    .inc_by(others.len() as u64);
                state
                    .metrics
                    .bytes_forwarded
                    .inc_by(data.len() as u64 * others.len() as u64);
            }
            Some(Ok(Message::Close(_))) | None => break,
            _ => continue,
        }
    }

    // 8. Cleanup
    send_task.abort();
    info!(room = %room, participant = participant_id, "WS client disconnected");

    if let Some(ref fp) = fingerprint {
        let mut reg = state.presence.lock().await;
        reg.unregister_local(fp);
    }

    state.room_mgr.leave(&room, participant_id);
    state.metrics.active_rooms.set(state.room_mgr.list().len() as i64);

    let session_id_str: String = session_id.iter().map(|b| format!("{b:02x}")).collect();
    state.metrics.remove_session_metrics(&session_id_str);
    state.metrics.active_sessions.dec();

    {
        let mut smgr = state.session_mgr.lock().await;
        smgr.remove_session(session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_state_is_clone() {
        // WsState must be Clone for axum's State extractor
        fn assert_clone<T: Clone>() {}
        assert_clone::<WsState>();
    }
}
