//! WarzonePhone CLI test client.
//!
//! Usage: wzp-client <relay-addr>
//!
//! Connects to a relay and sends silence frames for testing.

use std::net::SocketAddr;

use tracing::{error, info};

use wzp_client::call::{CallConfig, CallEncoder};
use wzp_proto::MediaTransport;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let relay_addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:4433".to_string())
        .parse()?;

    info!(%relay_addr, "WarzonePhone client connecting");

    let client_config = wzp_transport::client_config();
    let endpoint = wzp_transport::create_endpoint("0.0.0.0:0".parse()?, None)?;
    let connection =
        wzp_transport::connect(&endpoint, relay_addr, "localhost", client_config).await?;

    info!("Connected to relay");

    let transport = wzp_transport::QuinnTransport::new(connection);
    let config = CallConfig::default();
    let mut encoder = CallEncoder::new(&config);

    let frame_duration = tokio::time::Duration::from_millis(20);
    let pcm = vec![0i16; 960]; // 20ms @ 48kHz silence

    for i in 0..250u32 {
        let packets = encoder.encode_frame(&pcm)?;
        for pkt in &packets {
            if let Err(e) = transport.send_media(pkt).await {
                error!("send error: {e}");
                break;
            }
        }
        if i % 50 == 0 {
            info!(frame = i, packets = packets.len(), "sent");
        }
        tokio::time::sleep(frame_duration).await;
    }

    info!("Done, closing");
    transport.close().await?;
    Ok(())
}
