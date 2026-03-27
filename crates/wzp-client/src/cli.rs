//! WarzonePhone CLI test client.
//!
//! Usage: wzp-client [--live] [relay-addr]
//!
//! Without `--live`: sends silence frames for testing.
//! With `--live`: captures microphone audio and plays received audio through speakers.

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::{error, info};

use wzp_client::audio_io::{AudioCapture, AudioPlayback, FRAME_SAMPLES};
use wzp_client::call::{CallConfig, CallDecoder, CallEncoder};
use wzp_proto::MediaTransport;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let args: Vec<String> = std::env::args().collect();
    let live = args.iter().any(|a| a == "--live");
    let relay_addr: SocketAddr = args
        .iter()
        .skip(1)
        .find(|a| *a != "--live")
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:4433".to_string())
        .parse()?;

    info!(%relay_addr, live, "WarzonePhone client connecting");

    let client_config = wzp_transport::client_config();
    let endpoint = wzp_transport::create_endpoint("0.0.0.0:0".parse()?, None)?;
    let connection =
        wzp_transport::connect(&endpoint, relay_addr, "localhost", client_config).await?;

    info!("Connected to relay");

    let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));

    if live {
        run_live(transport).await
    } else {
        run_silence(transport).await
    }
}

/// Original test mode: send silence frames.
async fn run_silence(transport: Arc<wzp_transport::QuinnTransport>) -> anyhow::Result<()> {
    let config = CallConfig::default();
    let mut encoder = CallEncoder::new(&config);

    let frame_duration = tokio::time::Duration::from_millis(20);
    let pcm = vec![0i16; FRAME_SAMPLES]; // 20ms @ 48kHz silence

    let mut total_source = 0u64;
    let mut total_repair = 0u64;
    let mut total_bytes = 0u64;

    for i in 0..250u32 {
        let packets = encoder.encode_frame(&pcm)?;
        for pkt in &packets {
            if pkt.header.is_repair {
                total_repair += 1;
            } else {
                total_source += 1;
            }
            total_bytes += pkt.payload.len() as u64;
            if let Err(e) = transport.send_media(pkt).await {
                error!("send error: {e}");
                break;
            }
        }
        if (i + 1) % 50 == 0 {
            info!(
                frame = i + 1,
                source = total_source,
                repair = total_repair,
                bytes = total_bytes,
                "progress"
            );
        }
        tokio::time::sleep(frame_duration).await;
    }

    info!(
        total_source,
        total_repair,
        total_bytes,
        "done — closing"
    );
    transport.close().await?;
    Ok(())
}

/// Live mode: capture from mic, encode, send; receive, decode, play.
async fn run_live(transport: Arc<wzp_transport::QuinnTransport>) -> anyhow::Result<()> {
    let capture = AudioCapture::start()?;
    let playback = AudioPlayback::start()?;
    info!("Audio I/O started — press Ctrl+C to stop");

    // --- Send task: mic -> encode -> transport ---
    // AudioCapture::read_frame() is blocking, so we run this on a dedicated
    // OS thread. We use the tokio Handle to call the async send_media.
    let send_transport = transport.clone();
    let rt_handle = tokio::runtime::Handle::current();
    let send_handle = std::thread::Builder::new()
        .name("wzp-send-loop".into())
        .spawn(move || {
            let config = CallConfig::default();
            let mut encoder = CallEncoder::new(&config);
            loop {
                let frame = match capture.read_frame() {
                    Some(f) => f,
                    None => break, // channel closed / stopped
                };
                let packets = match encoder.encode_frame(&frame) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("encode error: {e}");
                        continue;
                    }
                };
                for pkt in &packets {
                    if let Err(e) = rt_handle.block_on(send_transport.send_media(pkt)) {
                        error!("send error: {e}");
                        return;
                    }
                }
            }
        })?;

    // --- Recv task: transport -> decode -> speaker ---
    let recv_transport = transport.clone();
    let recv_handle = tokio::spawn(async move {
        let config = CallConfig::default();
        let mut decoder = CallDecoder::new(&config);
        let mut pcm_buf = vec![0i16; FRAME_SAMPLES];
        loop {
            match recv_transport.recv_media().await {
                Ok(Some(pkt)) => {
                    decoder.ingest(pkt);
                    while let Some(_n) = decoder.decode_next(&mut pcm_buf) {
                        playback.write_frame(&pcm_buf);
                    }
                }
                Ok(None) => {
                    // No packet available right now, yield briefly.
                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                }
                Err(e) => {
                    error!("recv error: {e}");
                    break;
                }
            }
        }
    });

    // Wait for Ctrl+C
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for Ctrl+C");
    info!("Shutting down...");

    recv_handle.abort();
    // The send thread will exit once capture is dropped / stopped.
    drop(send_handle);
    transport.close().await?;
    info!("done");
    Ok(())
}
