//! WarzonePhone relay daemon entry point.
//!
//! Accepts client QUIC connections and bridges pairs of clients together.
//! When a --remote relay is configured, forwards traffic to it instead.

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
                eprintln!("  --remote <addr>  Remote relay address for forwarding");
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

/// Bridge two transports: A's packets go to B, B's go to A.
async fn run_bridge(
    a: Arc<wzp_transport::QuinnTransport>,
    b: Arc<wzp_transport::QuinnTransport>,
    a_addr: SocketAddr,
    b_addr: SocketAddr,
) {
    info!(%a_addr, %b_addr, "bridging two clients");

    let stats = Arc::new(RelayStats {
        upstream_packets: AtomicU64::new(0),
        downstream_packets: AtomicU64::new(0),
    });

    let stats_log = stats.clone();
    let stats_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let ab = stats_log.upstream_packets.load(Ordering::Relaxed);
            let ba = stats_log.downstream_packets.load(Ordering::Relaxed);
            info!(a_to_b = ab, b_to_a = ba, "bridge stats");
        }
    });

    let a1 = a.clone();
    let b1 = b.clone();
    let s1 = stats.clone();
    let a_to_b = tokio::spawn(async move {
        loop {
            match a1.recv_media().await {
                Ok(Some(pkt)) => {
                    if let Err(e) = b1.send_media(&pkt).await {
                        error!("A→B send error: {e}");
                        break;
                    }
                    s1.upstream_packets.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => { info!(%a_addr, "client A disconnected"); break; }
                Err(e) => { error!(%a_addr, "A recv error: {e}"); break; }
            }
        }
    });

    let a2 = a.clone();
    let b2 = b.clone();
    let s2 = stats.clone();
    let b_to_a = tokio::spawn(async move {
        loop {
            match b2.recv_media().await {
                Ok(Some(pkt)) => {
                    if let Err(e) = a2.send_media(&pkt).await {
                        error!("B→A send error: {e}");
                        break;
                    }
                    s2.downstream_packets.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None) => { info!(%b_addr, "client B disconnected"); break; }
                Err(e) => { error!(%b_addr, "B recv error: {e}"); break; }
            }
        }
    });

    tokio::select! {
        _ = a_to_b => {}
        _ = b_to_a => {}
    }
    stats_handle.abort();
    info!(%a_addr, %b_addr, "bridge ended");
}

/// Run upstream forwarding: client → pipeline → remote.
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

/// Run downstream forwarding: remote → pipeline → client.
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

/// Waiting client: address + transport.
struct WaitingClient {
    addr: SocketAddr,
    transport: Arc<wzp_transport::QuinnTransport>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = parse_args();
    tracing_subscriber::fmt().init();

    info!(addr = %config.listen_addr, "WarzonePhone relay starting");
    if let Some(remote) = config.remote_relay {
        info!(%remote, "forwarding mode → remote relay");
    } else {
        info!("bridge mode — pairs clients together (echo when alone)");
    }

    let (server_config, _cert) = wzp_transport::server_config();
    let endpoint = wzp_transport::create_endpoint(config.listen_addr, Some(server_config))?;

    let _sessions = Arc::new(Mutex::new(SessionManager::new(config.max_sessions)));

    // Remote relay transport (forwarding mode only)
    let remote_transport: Option<Arc<wzp_transport::QuinnTransport>> =
        if let Some(remote_addr) = config.remote_relay {
            let client_cfg = wzp_transport::client_config();
            let conn = wzp_transport::connect(&endpoint, remote_addr, "localhost", client_cfg).await?;
            Some(Arc::new(wzp_transport::QuinnTransport::new(conn)))
        } else {
            None
        };

    // Bridge mode: slot for waiting client
    let waiting: Arc<Mutex<Option<WaitingClient>>> = Arc::new(Mutex::new(None));

    info!("Listening for connections...");

    loop {
        let connection = match wzp_transport::accept(&endpoint).await {
            Ok(conn) => conn,
            Err(e) => { error!("accept: {e}"); continue; }
        };

        let remote_transport = remote_transport.clone();
        let waiting = waiting.clone();

        tokio::spawn(async move {
            let addr = connection.remote_address();
            let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));
            info!(%addr, "new client");

            if let Some(remote) = remote_transport {
                // Forwarding mode
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
                info!(%addr, "forwarding session ended");
            } else {
                // Bridge mode — try to pair with a waiting client
                let peer = {
                    let mut slot = waiting.lock().await;
                    slot.take()
                };

                if let Some(peer_client) = peer {
                    // Second client — bridge immediately
                    run_bridge(peer_client.transport.clone(), transport.clone(), peer_client.addr, addr).await;
                    peer_client.transport.close().await.ok();
                    transport.close().await.ok();

                    // After bridge ends, clean up so next pair can form
                    info!("bridge complete, ready for next pair");
                } else {
                    // First client — register and wait
                    {
                        let mut slot = waiting.lock().await;
                        *slot = Some(WaitingClient { addr, transport: transport.clone() });
                    }
                    info!(%addr, "waiting for peer (echo in meantime)");

                    // Echo loop — but check periodically if we've been claimed by a bridge
                    loop {
                        // Check if we've been taken from the waiting slot
                        // (meaning a second client connected and started the bridge)
                        {
                            let slot = waiting.lock().await;
                            if slot.is_none() {
                                // We were taken — a bridge is running with our transport.
                                // Just exit this task; the bridge task handles everything.
                                info!(%addr, "peer connected, exiting echo loop");
                                return;
                            }
                        }

                        // Echo with a short timeout so we can check the slot again
                        match tokio::time::timeout(
                            Duration::from_millis(100),
                            transport.recv_media()
                        ).await {
                            Ok(Ok(Some(pkt))) => {
                                let _ = transport.send_media(&pkt).await;
                            }
                            Ok(Ok(None)) => {
                                info!(%addr, "disconnected while waiting");
                                // Clean up our slot
                                let mut slot = waiting.lock().await;
                                *slot = None;
                                return;
                            }
                            Ok(Err(e)) => {
                                error!(%addr, "echo error: {e}");
                                let mut slot = waiting.lock().await;
                                *slot = None;
                                return;
                            }
                            Err(_) => {
                                // Timeout — loop back and check if we got paired
                            }
                        }
                    }
                }
            }
        });
    }
}
