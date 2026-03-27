//! WarzonePhone Web Bridge
//!
//! Serves a web page for browser-based voice calls and bridges
//! WebSocket audio to the wzp relay protocol.
//!
//! Usage: wzp-web [--port 8080] [--relay 127.0.0.1:4433] [--tls]
//!
//! Rooms: clients connect to /ws/<room-name> and are paired by room.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, WebSocketUpgrade};
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
    rooms: Arc<Mutex<HashMap<String, RoomSlot>>>,
}

/// A waiting client in a room.
struct RoomSlot {
    /// Sender half — send audio TO this waiting client's browser.
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Receiver half — receive audio FROM this waiting client's browser.
    rx: Arc<Mutex<tokio::sync::mpsc::Receiver<Vec<i16>>>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let mut port: u16 = 8080;
    let mut relay_addr: SocketAddr = "127.0.0.1:4433".parse()?;
    let mut use_tls = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => { i += 1; port = args[i].parse().expect("invalid port"); }
            "--relay" => { i += 1; relay_addr = args[i].parse().expect("invalid relay address"); }
            "--tls" => { use_tls = true; }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-web [--port 8080] [--relay 127.0.0.1:4433] [--tls]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --port <port>     HTTP/WebSocket port (default: 8080)");
                eprintln!("  --relay <addr>    WZP relay address (default: 127.0.0.1:4433)");
                eprintln!("  --tls             Enable HTTPS (required for mic on Android)");
                eprintln!();
                eprintln!("Rooms: open https://host:port/<room-name> to join a room.");
                eprintln!("Two clients in the same room are connected for a call.");
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }

    let state = AppState {
        relay_addr,
        rooms: Arc::new(Mutex::new(HashMap::new())),
    };

    let static_dir = if std::path::Path::new("crates/wzp-web/static").exists() {
        "crates/wzp-web/static"
    } else if std::path::Path::new("static").exists() {
        "static"
    } else {
        "static"
    };

    let app = Router::new()
        .route("/ws/{room}", get(ws_handler))
        .fallback_service(ServeDir::new(static_dir))
        .with_state(state);

    let listen: SocketAddr = format!("0.0.0.0:{port}").parse()?;

    if use_tls {
        let cert_key = rcgen::generate_simple_self_signed(vec![
            "localhost".to_string(), "wzp".to_string(),
        ])?;
        let cert_der = rustls_pki_types::CertificateDer::from(cert_key.cert);
        let key_der = rustls_pki_types::PrivateKeyDer::try_from(cert_key.key_pair.serialize_der())
            .map_err(|e| anyhow::anyhow!("key error: {e}"))?;

        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        let tls_config = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls_config));

        info!(%listen, %relay_addr, "WarzonePhone web bridge (HTTPS)");
        info!("Open https://localhost:{port}/<room-name> in your browser");

        axum_server::bind_rustls(listen, tls_config)
            .serve(app.into_make_service())
            .await?;
    } else {
        info!(%listen, %relay_addr, "WarzonePhone web bridge (HTTP)");
        info!("Open http://localhost:{port}/<room-name> in your browser");
        info!("Use --tls for mic access on Android/remote browsers");

        let listener = tokio::net::TcpListener::bind(listen).await?;
        axum::serve(listener, app).await?;
    }

    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(room): Path<String>,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    info!(room = %room, "WebSocket upgrade request");
    ws.on_upgrade(move |socket| handle_ws(socket, room, state))
}

async fn handle_ws(socket: WebSocket, room: String, state: AppState) {
    info!(room = %room, "client joined room");

    // Connect to relay
    let relay_addr = state.relay_addr;
    let bind_addr: SocketAddr = if relay_addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };

    let client_config = wzp_transport::client_config();
    let endpoint = match wzp_transport::create_endpoint(bind_addr, None) {
        Ok(e) => e,
        Err(e) => { error!("create endpoint: {e}"); return; }
    };

    // Pass room name as QUIC SNI so the relay knows which room to join
    let sni = if room.is_empty() { "default" } else { &room };
    let connection =
        match wzp_transport::connect(&endpoint, relay_addr, sni, client_config).await {
            Ok(c) => c,
            Err(e) => { error!("connect to relay: {e}"); return; }
        };

    info!(room = %room, "connected to relay");

    let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));
    let config = CallConfig::default();

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let encoder = Arc::new(Mutex::new(CallEncoder::new(&config)));
    let decoder = Arc::new(Mutex::new(CallDecoder::new(&config)));

    // Browser → Relay
    let send_transport = transport.clone();
    let send_encoder = encoder.clone();
    let send_room = room.clone();
    let send_task = tokio::spawn(async move {
        let mut frames_sent = 0u64;
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Binary(data) => {
                    if data.len() < FRAME_SAMPLES * 2 { continue; }
                    let pcm: Vec<i16> = data.chunks_exact(2)
                        .take(FRAME_SAMPLES)
                        .map(|c| i16::from_le_bytes([c[0], c[1]]))
                        .collect();

                    let packets = {
                        let mut enc = send_encoder.lock().await;
                        match enc.encode_frame(&pcm) {
                            Ok(p) => p,
                            Err(e) => { warn!("encode: {e}"); continue; }
                        }
                    };

                    for pkt in &packets {
                        if let Err(e) = send_transport.send_media(pkt).await {
                            error!("relay send: {e}");
                            return;
                        }
                    }
                    frames_sent += 1;
                    if frames_sent % 500 == 0 {
                        info!(room = %send_room, frames_sent, "browser → relay");
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        info!(room = %send_room, frames_sent, "send ended");
    });

    // Relay → Browser
    let recv_transport = transport.clone();
    let recv_decoder = decoder.clone();
    let recv_room = room.clone();
    let recv_task = tokio::spawn(async move {
        let mut pcm_buf = vec![0i16; FRAME_SAMPLES];
        let mut frames_recv = 0u64;
        loop {
            match recv_transport.recv_media().await {
                Ok(Some(pkt)) => {
                    let is_repair = pkt.header.is_repair;
                    let mut dec = recv_decoder.lock().await;
                    dec.ingest(pkt);
                    if !is_repair {
                        if let Some(_n) = dec.decode_next(&mut pcm_buf) {
                            let bytes: Vec<u8> = pcm_buf.iter()
                                .flat_map(|s| s.to_le_bytes())
                                .collect();
                            if let Err(e) = ws_sender.send(Message::Binary(bytes.into())).await {
                                error!("ws send: {e}");
                                return;
                            }
                            frames_recv += 1;
                            if frames_recv % 500 == 0 {
                                info!(room = %recv_room, frames_recv, "relay → browser");
                            }
                        }
                    }
                }
                Ok(None) => { info!(room = %recv_room, "relay closed"); break; }
                Err(e) => { error!(room = %recv_room, "relay recv: {e}"); break; }
            }
        }
        info!(room = %recv_room, frames_recv, "recv ended");
    });

    tokio::select! {
        _ = send_task => {}
        _ = recv_task => {}
    }

    transport.close().await.ok();
    info!(room = %room, "session ended");
}
