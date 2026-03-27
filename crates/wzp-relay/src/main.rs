//! WarzonePhone relay daemon entry point.
//!
//! Supports two modes:
//! - **Room mode** (default): clients join named rooms, packets forwarded to all others (SFU)
//! - **Forward mode** (--remote): all traffic forwarded to a remote relay
//!
//! Room names are passed via the QUIC SNI (server_name) field.
//! The web bridge connects with room name as SNI.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info};

use wzp_proto::MediaTransport;
use wzp_relay::config::RelayConfig;
use wzp_relay::pipeline::{PipelineConfig, RelayPipeline};
use wzp_relay::room::{self, RoomManager};

fn parse_args() -> RelayConfig {
    let mut config = RelayConfig::default();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                config.listen_addr = args.get(i).expect("--listen requires an address")
                    .parse().expect("invalid --listen address");
            }
            "--remote" => {
                i += 1;
                config.remote_relay = Some(
                    args.get(i).expect("--remote requires an address")
                        .parse().expect("invalid --remote address"),
                );
            }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-relay [--listen <addr>] [--remote <addr>]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --listen <addr>  Listen address (default: 0.0.0.0:4433)");
                eprintln!("  --remote <addr>  Remote relay for forwarding (disables room mode)");
                eprintln!();
                eprintln!("Room mode (default):");
                eprintln!("  Clients join rooms by name. Packets are forwarded to all");
                eprintln!("  other participants in the same room (SFU model).");
                eprintln!("  Room name comes from QUIC SNI or defaults to 'default'.");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    config
}

struct RelayStats {
    upstream_packets: AtomicU64,
    downstream_packets: AtomicU64,
}

async fn run_upstream(
    client: Arc<wzp_transport::QuinnTransport>,
    remote: Arc<wzp_transport::QuinnTransport>,
    pipeline: Arc<Mutex<RelayPipeline>>,
    stats: Arc<RelayStats>,
) {
    loop {
        match client.recv_media().await {
            Ok(Some(pkt)) => {
                let outbound = {
                    let mut pipe = pipeline.lock().await;
                    let decoded = pipe.ingest(pkt);
                    let mut out = Vec::new();
                    for p in decoded { out.extend(pipe.prepare_outbound(p)); }
                    out
                };
                for p in &outbound {
                    if let Err(e) = remote.send_media(p).await {
                        error!("upstream send: {e}");
                        return;
                    }
                }
                stats.upstream_packets.fetch_add(outbound.len() as u64, Ordering::Relaxed);
            }
            Ok(None) => { info!("client disconnected (upstream)"); break; }
            Err(e) => { error!("upstream recv: {e}"); break; }
        }
    }
}

async fn run_downstream(
    client: Arc<wzp_transport::QuinnTransport>,
    remote: Arc<wzp_transport::QuinnTransport>,
    pipeline: Arc<Mutex<RelayPipeline>>,
    stats: Arc<RelayStats>,
) {
    loop {
        match remote.recv_media().await {
            Ok(Some(pkt)) => {
                let outbound = {
                    let mut pipe = pipeline.lock().await;
                    let decoded = pipe.ingest(pkt);
                    let mut out = Vec::new();
                    for p in decoded { out.extend(pipe.prepare_outbound(p)); }
                    out
                };
                for p in &outbound {
                    if let Err(e) = client.send_media(p).await {
                        error!("downstream send: {e}");
                        return;
                    }
                }
                stats.downstream_packets.fetch_add(outbound.len() as u64, Ordering::Relaxed);
            }
            Ok(None) => { info!("remote disconnected (downstream)"); break; }
            Err(e) => { error!("downstream recv: {e}"); break; }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = parse_args();
    tracing_subscriber::fmt().init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    info!(addr = %config.listen_addr, "WarzonePhone relay starting");

    let (server_config, _cert) = wzp_transport::server_config();
    let endpoint = wzp_transport::create_endpoint(config.listen_addr, Some(server_config))?;

    // Forward mode
    let remote_transport: Option<Arc<wzp_transport::QuinnTransport>> =
        if let Some(remote_addr) = config.remote_relay {
            info!(%remote_addr, "forward mode → remote relay");
            let client_cfg = wzp_transport::client_config();
            let conn = wzp_transport::connect(&endpoint, remote_addr, "localhost", client_cfg).await?;
            Some(Arc::new(wzp_transport::QuinnTransport::new(conn)))
        } else {
            info!("room mode — clients join named rooms (SFU)");
            None
        };

    // Room manager (room mode only)
    let room_mgr = Arc::new(Mutex::new(RoomManager::new()));

    info!("Listening for connections...");

    loop {
        let connection = match wzp_transport::accept(&endpoint).await {
            Ok(conn) => conn,
            Err(e) => { error!("accept: {e}"); continue; }
        };

        let remote_transport = remote_transport.clone();
        let room_mgr = room_mgr.clone();

        tokio::spawn(async move {
            let addr = connection.remote_address();

            // Extract room name from QUIC handshake data (SNI).
            // The web bridge connects with the room name as server_name.
            let room_name = connection
                .handshake_data()
                .and_then(|hd| {
                    hd.downcast::<quinn::crypto::rustls::HandshakeData>().ok()
                })
                .and_then(|hd| hd.server_name.clone())
                .unwrap_or_else(|| "default".to_string());

            let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));

            info!(%addr, room = %room_name, "new client");

            if let Some(remote) = remote_transport {
                // Forward mode — same as before
                let stats = Arc::new(RelayStats {
                    upstream_packets: AtomicU64::new(0),
                    downstream_packets: AtomicU64::new(0),
                });
                let up_pipe = Arc::new(Mutex::new(RelayPipeline::new(PipelineConfig::default())));
                let dn_pipe = Arc::new(Mutex::new(RelayPipeline::new(PipelineConfig::default())));

                let stats_log = stats.clone();
                let stats_handle = tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(5));
                    loop {
                        interval.tick().await;
                        info!(
                            up = stats_log.upstream_packets.load(Ordering::Relaxed),
                            down = stats_log.downstream_packets.load(Ordering::Relaxed),
                            "forward stats"
                        );
                    }
                });

                let up = tokio::spawn(run_upstream(transport.clone(), remote.clone(), up_pipe, stats.clone()));
                let dn = tokio::spawn(run_downstream(transport.clone(), remote.clone(), dn_pipe, stats));

                tokio::select! { _ = up => {} _ = dn => {} }
                stats_handle.abort();
                transport.close().await.ok();
            } else {
                // Room mode — join room and forward to all others
                let participant_id = {
                    let mut mgr = room_mgr.lock().await;
                    mgr.join(&room_name, addr, transport.clone())
                };

                room::run_participant(
                    room_mgr.clone(),
                    room_name,
                    participant_id,
                    transport.clone(),
                ).await;

                transport.close().await.ok();
            }
        });
    }
}
