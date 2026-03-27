//! WarzonePhone relay daemon entry point.

use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{error, info};

use wzp_proto::MediaTransport;
use wzp_relay::config::RelayConfig;
use wzp_relay::session_mgr::SessionManager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = RelayConfig::default();

    tracing_subscriber::fmt().init();

    info!(addr = %config.listen_addr, "WarzonePhone relay starting");

    let (server_config, _cert_der) = wzp_transport::server_config();
    let endpoint =
        wzp_transport::create_endpoint(config.listen_addr, Some(server_config))?;

    let sessions = Arc::new(Mutex::new(SessionManager::new(config.max_sessions)));

    info!("Listening for connections...");

    loop {
        let connection = match wzp_transport::accept(&endpoint).await {
            Ok(conn) => conn,
            Err(e) => {
                error!("accept error: {e}");
                continue;
            }
        };

        let _sessions = sessions.clone();

        tokio::spawn(async move {
            let remote = connection.remote_address();
            info!(%remote, "new connection");

            let transport = wzp_transport::QuinnTransport::new(connection);

            loop {
                match transport.recv_media().await {
                    Ok(Some(packet)) => {
                        tracing::trace!(
                            seq = packet.header.seq,
                            block = packet.header.fec_block,
                            "received media packet"
                        );
                    }
                    Ok(None) => {
                        info!(%remote, "connection closed");
                        break;
                    }
                    Err(e) => {
                        error!(%remote, "recv error: {e}");
                        break;
                    }
                }
            }
        });
    }
}
