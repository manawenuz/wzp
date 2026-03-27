//! WarzonePhone relay daemon entry point.
//!
//! Accepts client QUIC connections and optionally forwards media to a remote
//! relay. Each client connection spawns two tasks for bidirectional forwarding
//! through the relay pipeline (FEC decode -> jitter -> FEC encode).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use wzp_proto::MediaTransport;
use wzp_relay::config::RelayConfig;
use wzp_relay::pipeline::{PipelineConfig, RelayPipeline};
use wzp_relay::session_mgr::SessionManager;

/// Parse CLI arguments using std::env::args().
///
/// Usage: wzp-relay [--listen <addr>] [--remote <addr>]
fn parse_args() -> RelayConfig {
    let mut config = RelayConfig::default();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                if i < args.len() {
                    config.listen_addr = args[i]
                        .parse::<SocketAddr>()
                        .expect("invalid --listen address");
                } else {
                    eprintln!("--listen requires an address argument");
                    std::process::exit(1);
                }
            }
            "--remote" => {
                i += 1;
                if i < args.len() {
                    config.remote_relay = Some(
                        args[i]
                            .parse::<SocketAddr>()
                            .expect("invalid --remote address"),
                    );
                } else {
                    eprintln!("--remote requires an address argument");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-relay [--listen <addr>] [--remote <addr>]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --listen <addr>  Listen address (default: 0.0.0.0:4433)");
                eprintln!("  --remote <addr>  Remote relay address for forwarding");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                eprintln!("Usage: wzp-relay [--listen <addr>] [--remote <addr>]");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    config
}

/// Shared packet counters for periodic logging.
struct RelayStats {
    upstream_packets: AtomicU64,
    downstream_packets: AtomicU64,
}

/// Run the upstream forwarding task: client -> pipeline -> remote.
async fn run_upstream(
    client_transport: Arc<wzp_transport::QuinnTransport>,
    remote_transport: Arc<wzp_transport::QuinnTransport>,
    pipeline: Arc<Mutex<RelayPipeline>>,
    stats: Arc<RelayStats>,
) {
    loop {
        let packet = match client_transport.recv_media().await {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                info!("client connection closed (upstream)");
                break;
            }
            Err(e) => {
                error!("upstream recv error: {e}");
                break;
            }
        };

        // Process through pipeline
        let outbound = {
            let mut pipe = pipeline.lock().await;
            let decoded = pipe.ingest(packet);
            let mut out = Vec::new();
            for pkt in decoded {
                out.extend(pipe.prepare_outbound(pkt));
            }
            out
        };

        // Forward to remote
        for pkt in &outbound {
            if let Err(e) = remote_transport.send_media(pkt).await {
                error!("upstream send error: {e}");
                return;
            }
        }
        stats
            .upstream_packets
            .fetch_add(outbound.len() as u64, Ordering::Relaxed);
    }
}

/// Run the downstream forwarding task: remote -> pipeline -> client.
async fn run_downstream(
    client_transport: Arc<wzp_transport::QuinnTransport>,
    remote_transport: Arc<wzp_transport::QuinnTransport>,
    pipeline: Arc<Mutex<RelayPipeline>>,
    stats: Arc<RelayStats>,
) {
    loop {
        let packet = match remote_transport.recv_media().await {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                info!("remote connection closed (downstream)");
                break;
            }
            Err(e) => {
                error!("downstream recv error: {e}");
                break;
            }
        };

        // Process through pipeline
        let outbound = {
            let mut pipe = pipeline.lock().await;
            let decoded = pipe.ingest(packet);
            let mut out = Vec::new();
            for pkt in decoded {
                out.extend(pipe.prepare_outbound(pkt));
            }
            out
        };

        // Forward to client
        for pkt in &outbound {
            if let Err(e) = client_transport.send_media(pkt).await {
                error!("downstream send error: {e}");
                return;
            }
        }
        stats
            .downstream_packets
            .fetch_add(outbound.len() as u64, Ordering::Relaxed);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = parse_args();

    tracing_subscriber::fmt().init();

    info!(addr = %config.listen_addr, "WarzonePhone relay starting");
    if let Some(remote) = config.remote_relay {
        info!(%remote, "will connect to remote relay");
    }

    let (server_config, _cert_der) = wzp_transport::server_config();
    let endpoint = wzp_transport::create_endpoint(config.listen_addr, Some(server_config))?;

    let sessions = Arc::new(Mutex::new(SessionManager::new(config.max_sessions)));

    // If a remote relay is configured, connect to it on startup
    let remote_transport: Option<Arc<wzp_transport::QuinnTransport>> =
        if let Some(remote_addr) = config.remote_relay {
            info!(%remote_addr, "connecting to remote relay");
            let client_cfg = wzp_transport::client_config();
            let remote_conn =
                wzp_transport::connect(&endpoint, remote_addr, "localhost", client_cfg).await?;
            info!(%remote_addr, "connected to remote relay");
            Some(Arc::new(wzp_transport::QuinnTransport::new(remote_conn)))
        } else {
            None
        };

    info!("Listening for connections...");

    loop {
        let connection = match wzp_transport::accept(&endpoint).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("accept error: {e}");
                continue;
            }
        };

        let sessions = sessions.clone();
        let remote_transport = remote_transport.clone();

        tokio::spawn(async move {
            let remote_addr = connection.remote_address();
            info!(%remote_addr, "new client connection");

            let client_transport = Arc::new(wzp_transport::QuinnTransport::new(connection));

            match remote_transport {
                Some(remote_tx) => {
                    // Create pipelines for both directions
                    let upstream_pipeline =
                        Arc::new(Mutex::new(RelayPipeline::new(PipelineConfig::default())));
                    let downstream_pipeline =
                        Arc::new(Mutex::new(RelayPipeline::new(PipelineConfig::default())));

                    // Register session
                    {
                        let mut mgr = sessions.lock().await;
                        let session_id = {
                            let mut id = [0u8; 16];
                            let addr_bytes = remote_addr.to_string();
                            let bytes = addr_bytes.as_bytes();
                            let len = bytes.len().min(16);
                            id[..len].copy_from_slice(&bytes[..len]);
                            id
                        };
                        mgr.create_session(session_id, PipelineConfig::default());
                    }

                    let stats = Arc::new(RelayStats {
                        upstream_packets: AtomicU64::new(0),
                        downstream_packets: AtomicU64::new(0),
                    });

                    // Spawn periodic stats logger
                    let stats_log = stats.clone();
                    let log_remote = remote_addr;
                    let stats_handle = tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(5));
                        loop {
                            interval.tick().await;
                            let up = stats_log.upstream_packets.load(Ordering::Relaxed);
                            let down = stats_log.downstream_packets.load(Ordering::Relaxed);
                            info!(
                                client = %log_remote,
                                upstream = up,
                                downstream = down,
                                "relay stats"
                            );
                        }
                    });

                    // Spawn upstream and downstream tasks
                    let up_handle = tokio::spawn(run_upstream(
                        client_transport.clone(),
                        remote_tx.clone(),
                        upstream_pipeline,
                        stats.clone(),
                    ));

                    let down_handle = tokio::spawn(run_downstream(
                        client_transport.clone(),
                        remote_tx,
                        downstream_pipeline,
                        stats,
                    ));

                    // Wait for either direction to finish, then clean up
                    tokio::select! {
                        _ = up_handle => {
                            info!(%remote_addr, "upstream task ended");
                        }
                        _ = down_handle => {
                            info!(%remote_addr, "downstream task ended");
                        }
                    }

                    // Abort the stats logger and close transport
                    stats_handle.abort();
                    if let Err(e) = client_transport.close().await {
                        warn!(%remote_addr, "error closing client transport: {e}");
                    }
                    info!(%remote_addr, "session ended");
                }
                None => {
                    // No remote relay configured — just receive and log (sink mode)
                    warn!("no remote relay configured, running in sink mode");
                    loop {
                        match client_transport.recv_media().await {
                            Ok(Some(packet)) => {
                                tracing::trace!(
                                    seq = packet.header.seq,
                                    block = packet.header.fec_block,
                                    "received media packet (sink)"
                                );
                            }
                            Ok(None) => {
                                info!(%remote_addr, "connection closed");
                                break;
                            }
                            Err(e) => {
                                error!(%remote_addr, "recv error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        });
    }
}
