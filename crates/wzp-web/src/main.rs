//! WarzonePhone Web Bridge
//!
//! Serves a web page for browser-based voice calls and bridges
//! WebSocket audio to the wzp relay protocol.
//!
//! Usage: wzp-web [--port 8080] [--relay 127.0.0.1:4433]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::stream::StreamExt;
use futures::SinkExt;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

use wzp_client::call::{CallConfig, CallDecoder, CallEncoder};
use wzp_proto::MediaTransport;

const FRAME_SAMPLES: usize = 960;

#[derive(Clone)]
struct AppState {
    relay_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let mut port: u16 = 8080;
    let mut relay_addr: SocketAddr = "127.0.0.1:4433".parse()?;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args[i].parse().expect("invalid port");
            }
            "--relay" => {
                i += 1;
                relay_addr = args[i].parse().expect("invalid relay address");
            }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-web [--port 8080] [--relay 127.0.0.1:4433]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --port <port>     HTTP/WebSocket port (default: 8080)");
                eprintln!("  --relay <addr>    WZP relay address (default: 127.0.0.1:4433)");
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }

    let state = AppState { relay_addr };

    // Determine static file path (relative to binary or cargo manifest)
    let static_dir = if std::path::Path::new("crates/wzp-web/static").exists() {
        "crates/wzp-web/static"
    } else if std::path::Path::new("static").exists() {
        "static"
    } else {
        // Fallback: look relative to executable
        "static"
    };

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(ServeDir::new(static_dir))
        .with_state(state);

    let listen: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    info!(%listen, %relay_addr, "WarzonePhone web bridge starting");
    info!("Open http://localhost:{port} in your browser");

    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    info!("WebSocket client connected");

    // Connect to wzp relay
    let relay_addr = state.relay_addr;
    let bind_addr: SocketAddr = if relay_addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };

    let client_config = wzp_transport::client_config();
    let endpoint = match wzp_transport::create_endpoint(bind_addr, None) {
        Ok(e) => e,
        Err(e) => {
            error!("create endpoint: {e}");
            return;
        }
    };

    let connection =
        match wzp_transport::connect(&endpoint, relay_addr, "localhost", client_config).await {
            Ok(c) => c,
            Err(e) => {
                error!("connect to relay {relay_addr}: {e}");
                return;
            }
        };

    info!(%relay_addr, "connected to relay");

    let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));
    let config = CallConfig::default();

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let encoder = Arc::new(Mutex::new(CallEncoder::new(&config)));
    let decoder = Arc::new(Mutex::new(CallDecoder::new(&config)));

    // --- Browser → Relay: receive PCM from WebSocket, encode, send to relay ---
    let send_transport = transport.clone();
    let send_encoder = encoder.clone();
    let send_task = tokio::spawn(async move {
        let mut frames_sent = 0u64;
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Binary(data) => {
                    // data is raw s16le PCM from browser
                    if data.len() < FRAME_SAMPLES * 2 {
                        continue; // incomplete frame
                    }
                    let pcm: Vec<i16> = data
                        .chunks_exact(2)
                        .take(FRAME_SAMPLES)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect();

                    let packets = {
                        let mut enc = send_encoder.lock().await;
                        match enc.encode_frame(&pcm) {
                            Ok(p) => p,
                            Err(e) => {
                                warn!("encode error: {e}");
                                continue;
                            }
                        }
                    };

                    for pkt in &packets {
                        if let Err(e) = send_transport.send_media(pkt).await {
                            error!("relay send error: {e}");
                            return;
                        }
                    }
                    frames_sent += 1;
                    if frames_sent % 250 == 0 {
                        info!(frames_sent, "browser → relay");
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        info!(frames_sent, "browser send loop ended");
    });

    // --- Relay → Browser: receive from relay, decode, send PCM to WebSocket ---
    let recv_transport = transport.clone();
    let recv_decoder = decoder.clone();
    let recv_task = tokio::spawn(async move {
        let mut pcm_buf = vec![0i16; FRAME_SAMPLES];
        let mut frames_recv = 0u64;
        loop {
            match recv_transport.recv_media().await {
                Ok(Some(pkt)) => {
                    let is_repair = pkt.header.is_repair;
                    {
                        let mut dec = recv_decoder.lock().await;
                        dec.ingest(pkt);
                        if !is_repair {
                            if let Some(_n) = dec.decode_next(&mut pcm_buf) {
                                // Convert i16 PCM to bytes and send to browser
                                let bytes: Vec<u8> = pcm_buf
                                    .iter()
                                    .flat_map(|s| s.to_le_bytes())
                                    .collect();
                                if let Err(e) = ws_sender.send(Message::Binary(bytes.into())).await
                                {
                                    error!("ws send error: {e}");
                                    return;
                                }
                                frames_recv += 1;
                                if frames_recv % 250 == 0 {
                                    info!(frames_recv, "relay → browser");
                                }
                            }
                        }
                    }
                }
                Ok(None) => {
                    info!("relay connection closed");
                    break;
                }
                Err(e) => {
                    error!("relay recv error: {e}");
                    break;
                }
            }
        }
        info!(frames_recv, "relay recv loop ended");
    });

    tokio::select! {
        _ = send_task => {}
        _ = recv_task => {}
    }

    transport.close().await.ok();
    info!("WebSocket session ended");
}
