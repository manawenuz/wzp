//! WarzonePhone CLI test client.
//!
//! Usage:
//!   wzp-client [relay-addr]                     Send silence frames (connectivity test)
//!   wzp-client --live [relay-addr]              Live mic/speaker mode
//!   wzp-client --send-tone 10 [relay-addr]      Send 10s of 440Hz test tone
//!   wzp-client --record out.raw [relay-addr]    Record received audio to raw PCM file
//!   wzp-client --send-tone 10 --record out.raw [relay-addr]   Both at once
//!
//! Raw PCM files are 48kHz mono 16-bit signed little-endian.
//! Play with: ffplay -f s16le -ar 48000 -ac 1 out.raw
//! Or convert: ffmpeg -f s16le -ar 48000 -ac 1 -i out.raw out.wav

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::{error, info};

use wzp_client::call::{CallConfig, CallDecoder, CallEncoder};
use wzp_proto::MediaTransport;

const FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz

/// Generate a sine wave tone.
fn generate_sine_frame(freq_hz: f32, sample_rate: u32, frame_offset: u64) -> Vec<i16> {
    let start_sample = frame_offset * FRAME_SAMPLES as u64;
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = (start_sample + i as u64) as f32 / sample_rate as f32;
            (f32::sin(2.0 * std::f32::consts::PI * freq_hz * t) * 16000.0) as i16
        })
        .collect()
}

#[derive(Debug)]
struct CliArgs {
    relay_addr: SocketAddr,
    live: bool,
    send_tone_secs: Option<u32>,
    record_file: Option<String>,
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut live = false;
    let mut send_tone_secs = None;
    let mut record_file = None;
    let mut relay_str = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--live" => live = true,
            "--send-tone" => {
                i += 1;
                send_tone_secs = Some(
                    args.get(i)
                        .expect("--send-tone requires seconds")
                        .parse()
                        .expect("--send-tone value must be a number"),
                );
            }
            "--record" => {
                i += 1;
                record_file = Some(
                    args.get(i)
                        .expect("--record requires a filename")
                        .to_string(),
                );
            }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-client [options] [relay-addr]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --live                 Live mic/speaker mode");
                eprintln!("  --send-tone <secs>     Send a 440Hz test tone for N seconds");
                eprintln!("  --record <file.raw>    Record received audio to raw PCM file");
                eprintln!("                         (48kHz mono s16le, play with ffplay -f s16le -ar 48000 -ac 1 file.raw)");
                eprintln!();
                eprintln!("Default relay: 127.0.0.1:4433");
                std::process::exit(0);
            }
            other => {
                if relay_str.is_none() && !other.starts_with('-') {
                    relay_str = Some(other.to_string());
                } else {
                    eprintln!("unknown argument: {other}");
                    std::process::exit(1);
                }
            }
        }
        i += 1;
    }

    let relay_addr: SocketAddr = relay_str
        .unwrap_or_else(|| "127.0.0.1:4433".to_string())
        .parse()
        .expect("invalid relay address");

    CliArgs {
        relay_addr,
        live,
        send_tone_secs,
        record_file,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let cli = parse_args();

    info!(
        relay = %cli.relay_addr,
        live = cli.live,
        send_tone = ?cli.send_tone_secs,
        record = ?cli.record_file,
        "WarzonePhone client"
    );

    let client_config = wzp_transport::client_config();
    let bind_addr = if cli.relay_addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
    let connection =
        wzp_transport::connect(&endpoint, cli.relay_addr, "localhost", client_config).await?;

    info!("Connected to relay");

    let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));

    if cli.live {
        run_live(transport).await
    } else if cli.send_tone_secs.is_some() || cli.record_file.is_some() {
        run_file_mode(transport, cli.send_tone_secs, cli.record_file).await
    } else {
        run_silence(transport).await
    }
}

/// Send silence frames (connectivity test).
async fn run_silence(transport: Arc<wzp_transport::QuinnTransport>) -> anyhow::Result<()> {
    let config = CallConfig::default();
    let mut encoder = CallEncoder::new(&config);

    let frame_duration = tokio::time::Duration::from_millis(20);
    let pcm = vec![0i16; FRAME_SAMPLES];

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

    info!(total_source, total_repair, total_bytes, "done — closing");
    transport.close().await?;
    Ok(())
}

/// File/tone mode: send a test tone and/or record received audio.
async fn run_file_mode(
    transport: Arc<wzp_transport::QuinnTransport>,
    send_tone_secs: Option<u32>,
    record_file: Option<String>,
) -> anyhow::Result<()> {
    let config = CallConfig::default();

    // --- Send task: generate tone and send ---
    let send_transport = transport.clone();
    let send_handle = tokio::spawn(async move {
        let secs = match send_tone_secs {
            Some(s) => s,
            None => {
                // No sending, just wait
                tokio::signal::ctrl_c().await.ok();
                return;
            }
        };

        let mut encoder = CallEncoder::new(&config);
        let total_frames = (secs as u64) * 50; // 50 frames/sec at 20ms
        let frame_duration = tokio::time::Duration::from_millis(20);

        let mut total_source = 0u64;
        let mut total_repair = 0u64;

        info!(seconds = secs, frames = total_frames, "sending 440Hz tone");

        for frame_idx in 0..total_frames {
            let pcm = generate_sine_frame(440.0, 48_000, frame_idx);
            let packets = match encoder.encode_frame(&pcm) {
                Ok(p) => p,
                Err(e) => {
                    error!("encode error: {e}");
                    continue;
                }
            };
            for pkt in &packets {
                if pkt.header.is_repair {
                    total_repair += 1;
                } else {
                    total_source += 1;
                }
                if let Err(e) = send_transport.send_media(pkt).await {
                    error!("send error: {e}");
                    return;
                }
            }
            if (frame_idx + 1) % 250 == 0 {
                info!(
                    frame = frame_idx + 1,
                    source = total_source,
                    repair = total_repair,
                    "send progress"
                );
            }
            tokio::time::sleep(frame_duration).await;
        }
        info!(total_source, total_repair, "tone send complete");
    });

    // --- Recv task: decode and write to file ---
    let recv_transport = transport.clone();
    let recv_handle = tokio::spawn(async move {
        let record_path = match record_file {
            Some(p) => p,
            None => {
                // No recording, just wait
                tokio::signal::ctrl_c().await.ok();
                return;
            }
        };

        let mut decoder = CallDecoder::new(&CallConfig::default());
        let mut pcm_buf = vec![0i16; FRAME_SAMPLES];
        let mut all_pcm: Vec<i16> = Vec::new();
        let mut frames_received = 0u64;

        info!(file = %record_path, "recording received audio");

        loop {
            match recv_transport.recv_media().await {
                Ok(Some(pkt)) => {
                    decoder.ingest(pkt);
                    while let Some(n) = decoder.decode_next(&mut pcm_buf) {
                        all_pcm.extend_from_slice(&pcm_buf[..n]);
                        frames_received += 1;
                        if frames_received % 250 == 0 {
                            info!(
                                frames = frames_received,
                                samples = all_pcm.len(),
                                "recv progress"
                            );
                        }
                    }
                }
                Ok(None) => {
                    info!("connection closed by remote");
                    break;
                }
                Err(e) => {
                    error!("recv error: {e}");
                    break;
                }
            }
        }

        // Write raw PCM to file
        if !all_pcm.is_empty() {
            let bytes: Vec<u8> = all_pcm
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            if let Err(e) = std::fs::write(&record_path, &bytes) {
                error!(file = %record_path, "write error: {e}");
            } else {
                let duration_secs = all_pcm.len() as f64 / 48_000.0;
                info!(
                    file = %record_path,
                    frames = frames_received,
                    samples = all_pcm.len(),
                    duration_secs = format!("{:.1}", duration_secs),
                    bytes = bytes.len(),
                    "recording saved"
                );
                info!("play with: ffplay -f s16le -ar 48000 -ac 1 {record_path}");
            }
        } else {
            info!("no audio received, nothing to write");
        }
    });

    // Wait for both tasks
    let _ = tokio::join!(send_handle, recv_handle);

    transport.close().await?;
    Ok(())
}

/// Live mode: capture from mic, encode, send; receive, decode, play.
async fn run_live(transport: Arc<wzp_transport::QuinnTransport>) -> anyhow::Result<()> {
    use wzp_client::audio_io::{AudioCapture, AudioPlayback};

    let capture = AudioCapture::start()?;
    let playback = AudioPlayback::start()?;
    info!("Audio I/O started — press Ctrl+C to stop");

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
                    None => break,
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
                    tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                }
                Err(e) => {
                    error!("recv error: {e}");
                    break;
                }
            }
        }
    });

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    recv_handle.abort();
    drop(send_handle);
    transport.close().await?;
    info!("done");
    Ok(())
}
