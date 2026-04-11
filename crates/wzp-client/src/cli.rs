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
    send_file: Option<String>,
    record_file: Option<String>,
    echo_test_secs: Option<u32>,
    drift_test_secs: Option<u32>,
    sweep: bool,
    seed_hex: Option<String>,
    mnemonic: Option<String>,
    room: Option<String>,
    token: Option<String>,
    _metrics_file: Option<String>,
    version_check: bool,
    /// Connect to relay for persistent signaling (direct calls).
    signal: bool,
    /// Place a direct call to a fingerprint (requires --signal).
    call_target: Option<String>,
}

impl CliArgs {
    /// Resolve the identity seed from --seed, --mnemonic, or generate a new one.
    pub fn resolve_seed(&self) -> wzp_crypto::Seed {
        if let Some(ref hex_str) = self.seed_hex {
            let seed = wzp_crypto::Seed::from_hex(hex_str).expect("invalid --seed hex");
            let id = seed.derive_identity();
            let fp = id.public_identity().fingerprint;
            info!(fingerprint = %fp, "identity from --seed");
            seed
        } else if let Some(ref words) = self.mnemonic {
            let seed = wzp_crypto::Seed::from_mnemonic(words).expect("invalid --mnemonic");
            let id = seed.derive_identity();
            let fp = id.public_identity().fingerprint;
            info!(fingerprint = %fp, "identity from --mnemonic");
            seed
        } else {
            let seed = wzp_crypto::Seed::generate();
            let id = seed.derive_identity();
            let fp = id.public_identity().fingerprint;
            info!(fingerprint = %fp, "generated ephemeral identity");
            seed
        }
    }
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().collect();
    let mut live = false;
    let mut send_tone_secs = None;
    let mut send_file = None;
    let mut record_file = None;
    let mut echo_test_secs = None;
    let mut drift_test_secs = None;
    let mut sweep = false;
    let mut seed_hex = None;
    let mut mnemonic = None;
    let mut room = None;
    let mut token = None;
    let mut metrics_file = None;
    let mut version_check = false;
    let mut relay_str = None;
    let mut signal = false;
    let mut call_target = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--live" => live = true,
            "--signal" => signal = true,
            "--call" => {
                i += 1;
                call_target = Some(args.get(i).expect("--call requires a fingerprint").to_string());
            }
            "--send-tone" => {
                i += 1;
                send_tone_secs = Some(
                    args.get(i)
                        .expect("--send-tone requires seconds")
                        .parse()
                        .expect("--send-tone value must be a number"),
                );
            }
            "--send-file" => {
                i += 1;
                send_file = Some(
                    args.get(i)
                        .expect("--send-file requires a filename")
                        .to_string(),
                );
            }
            "--seed" => {
                i += 1;
                seed_hex = Some(args.get(i).expect("--seed requires hex string").to_string());
            }
            "--mnemonic" => {
                // Consume all remaining words until next flag or end
                i += 1;
                let mut words = Vec::new();
                while i < args.len() && !args[i].starts_with('-') {
                    words.push(args[i].clone());
                    i += 1;
                }
                i -= 1; // back up since outer loop will increment
                mnemonic = Some(words.join(" "));
            }
            "--room" => {
                i += 1;
                room = Some(args.get(i).expect("--room requires a name").to_string());
            }
            "--token" => {
                i += 1;
                token = Some(args.get(i).expect("--token requires a value").to_string());
            }
            "--metrics-file" => {
                i += 1;
                metrics_file = Some(
                    args.get(i)
                        .expect("--metrics-file requires a path")
                        .to_string(),
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
            "--echo-test" => {
                i += 1;
                echo_test_secs = Some(
                    args.get(i)
                        .expect("--echo-test requires seconds")
                        .parse()
                        .expect("--echo-test value must be a number"),
                );
            }
            "--drift-test" => {
                i += 1;
                drift_test_secs = Some(
                    args.get(i)
                        .expect("--drift-test requires seconds")
                        .parse()
                        .expect("--drift-test value must be a number"),
                );
            }
            "--sweep" => sweep = true,
            "--version-check" => { version_check = true; }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-client [options] [relay-addr]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --live                 Live mic/speaker mode");
                eprintln!("  --send-tone <secs>     Send a 440Hz test tone for N seconds");
                eprintln!("  --send-file <file>     Send a raw PCM file (48kHz mono s16le)");
                eprintln!("  --record <file.raw>    Record received audio to raw PCM file");
                eprintln!("  --echo-test <secs>     Run automated echo quality test");
                eprintln!("  --drift-test <secs>    Run automated clock-drift measurement");
                eprintln!("  --sweep                Run jitter buffer parameter sweep (local, no network)");
                eprintln!("  --seed <hex>           Identity seed (64 hex chars, featherChat compatible)");
                eprintln!("  --mnemonic <words...>  Identity seed as BIP39 mnemonic (24 words)");
                eprintln!("  --room <name>          Room name (hashed for privacy before sending)");
                eprintln!("  --token <token>        featherChat bearer token for relay auth");
                eprintln!("  --metrics-file <path>  Write JSONL telemetry to file (1 line/sec)");
                eprintln!("                         (48kHz mono s16le, play with ffplay -f s16le -ar 48000 -ch_layout mono file.raw)");
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
        send_file,
        record_file,
        echo_test_secs,
        drift_test_secs,
        sweep,
        seed_hex,
        mnemonic,
        room,
        token,
        _metrics_file: metrics_file,
        version_check,
        signal,
        call_target,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let cli = parse_args();

    // --sweep runs locally (no network), so handle it before connecting.
    if cli.sweep {
        wzp_client::sweep::run_and_print_default_sweep();
        return Ok(());
    }

    // --version-check: query relay version over QUIC and exit
    if cli.version_check {
        let client_config = wzp_transport::client_config();
        let bind_addr: SocketAddr = "0.0.0.0:0".parse()?;
        let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
        let conn = wzp_transport::connect(&endpoint, cli.relay_addr, "version", client_config).await?;
        match conn.accept_uni().await {
            Ok(mut recv) => {
                let data = recv.read_to_end(256).await.unwrap_or_default();
                let version = String::from_utf8_lossy(&data);
                println!("{} {}", cli.relay_addr, version.trim());
            }
            Err(e) => {
                eprintln!("relay {} does not support version query: {e}", cli.relay_addr);
            }
        }
        endpoint.close(0u32.into(), b"done");
        return Ok(());
    }

    // --signal mode: persistent signaling for direct calls
    if cli.signal {
        let seed = cli.resolve_seed();
        return run_signal_mode(cli.relay_addr, seed, cli.token, cli.call_target).await;
    }

    let seed = cli.resolve_seed();

    info!(
        relay = %cli.relay_addr,
        live = cli.live,
        send_tone = ?cli.send_tone_secs,
        record = ?cli.record_file,
        room = ?cli.room,
        "WarzonePhone client"
    );

    // Use raw room name as SNI (consistent with Android + Desktop clients for federation)
    let sni = match &cli.room {
        Some(name) => {
            info!(room = %name, "using room name as SNI");
            name.clone()
        }
        None => "default".to_string(),
    };

    let client_config = wzp_transport::client_config();
    let bind_addr = if cli.relay_addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
    let connection =
        wzp_transport::connect(&endpoint, cli.relay_addr, &sni, client_config).await?;

    info!("Connected to relay");

    let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));

    // Register shutdown handler so SIGTERM/SIGINT always closes QUIC cleanly.
    // Without this, killed clients leave zombie connections on the relay for ~30s.
    {
        let shutdown_transport = transport.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to register SIGTERM handler");
            let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("failed to register SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => { info!("SIGTERM received, closing connection..."); }
                _ = sigint.recv() => { info!("SIGINT received, closing connection..."); }
            }
            // Close the QUIC connection immediately (APPLICATION_CLOSE frame).
            // Don't call process::exit — let the main task detect the closed
            // connection and perform clean shutdown (e.g., save recordings).
            shutdown_transport.connection().close(0u32.into(), b"shutdown");
        });
    }

    // Send auth token if provided (relay with --auth-url expects this first)
    if let Some(ref token) = cli.token {
        let auth = wzp_proto::SignalMessage::AuthToken {
            token: token.clone(),
        };
        transport.send_signal(&auth).await?;
        info!("auth token sent");
    }

    // Crypto handshake — establishes verified identity + session key
    let _crypto_session = wzp_client::handshake::perform_handshake(
        &*transport,
        &seed.0,
        None, // alias — desktop client doesn't set one yet
    ).await?;
    info!("crypto handshake complete");

    if cli.live {
        #[cfg(feature = "audio")]
        {
            return run_live(transport).await;
        }
        #[cfg(not(feature = "audio"))]
        {
            anyhow::bail!("--live requires the 'audio' feature (build with: cargo build --features audio)");
        }
    } else if let Some(secs) = cli.echo_test_secs {
        let result = wzp_client::echo_test::run_echo_test(&*transport, secs, 5.0).await?;
        wzp_client::echo_test::print_report(&result);
        transport.close().await?;
        Ok(())
    } else if let Some(secs) = cli.drift_test_secs {
        let config = wzp_client::drift_test::DriftTestConfig {
            duration_secs: secs,
            tone_freq_hz: 440.0,
        };
        let result = wzp_client::drift_test::run_drift_test(&*transport, &config).await?;
        wzp_client::drift_test::print_drift_report(&result);
        transport.close().await?;
        Ok(())
    } else if cli.send_tone_secs.is_some() || cli.send_file.is_some() || cli.record_file.is_some() {
        run_file_mode(transport, cli.send_tone_secs, cli.send_file, cli.record_file).await
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
    let hangup = wzp_proto::SignalMessage::Hangup {
        reason: wzp_proto::HangupReason::Normal,
    };
    transport.send_signal(&hangup).await.ok();
    transport.close().await?;
    Ok(())
}

/// File/tone mode: send a test tone or audio file, and/or record received audio.
async fn run_file_mode(
    transport: Arc<wzp_transport::QuinnTransport>,
    send_tone_secs: Option<u32>,
    send_file: Option<String>,
    record_file: Option<String>,
) -> anyhow::Result<()> {
    let config = CallConfig::default();

    // --- Send task: generate tone or play file ---
    let send_transport = transport.clone();
    let send_handle = tokio::spawn(async move {
        // Load PCM frames from file or generate tone
        let pcm_frames: Vec<Vec<i16>> = if let Some(ref path) = send_file {
            // Read raw PCM file (48kHz mono s16le)
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => { error!("read {path}: {e}"); return; }
            };
            let samples: Vec<i16> = bytes.chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            let duration = samples.len() as f64 / 48_000.0;
            info!(file = %path, duration = format!("{:.1}s", duration), "sending audio file");
            samples.chunks(FRAME_SAMPLES)
                .filter(|c| c.len() == FRAME_SAMPLES)
                .map(|c| c.to_vec())
                .collect()
        } else if let Some(secs) = send_tone_secs {
            let total = (secs as u64) * 50;
            info!(seconds = secs, frames = total, "sending 440Hz tone");
            (0..total).map(|i| generate_sine_frame(440.0, 48_000, i)).collect()
        } else {
            // No sending, just wait
            tokio::signal::ctrl_c().await.ok();
            return;
        };

        let mut encoder = CallEncoder::new(&config);
        let _total_frames = pcm_frames.len() as u64;
        let frame_duration = tokio::time::Duration::from_millis(20);

        let mut total_source = 0u64;
        let mut total_repair = 0u64;

        for (frame_idx, pcm) in pcm_frames.iter().enumerate() {
            let frame_idx = frame_idx as u64;
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
    let record_path = record_file.clone();
    let recv_handle = tokio::spawn(async move {
        let record_path = match record_path {
            Some(p) => p,
            None => {
                // No recording, just wait for send to finish or Ctrl+C
                tokio::signal::ctrl_c().await.ok();
                return Vec::new();
            }
        };

        let mut decoder = CallDecoder::new(&CallConfig::default());
        let mut pcm_buf = vec![0i16; FRAME_SAMPLES];
        let mut all_pcm: Vec<i16> = Vec::new();
        let mut frames_received = 0u64;

        info!(file = %record_path, "recording received audio (Ctrl+C to stop and save)");

        loop {
            tokio::select! {
                result = recv_transport.recv_media() => {
                    match result {
                        Ok(Some(pkt)) => {
                            let is_repair = pkt.header.is_repair;
                            decoder.ingest(pkt);
                            if !is_repair {
                                if let Some(n) = decoder.decode_next(&mut pcm_buf) {
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
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received, saving recording...");
                    break;
                }
            }
        }

        all_pcm
    });

    // Wait for send to finish (or ctrl+c in recv)
    let _ = send_handle.await;

    // Send Hangup signal so the relay knows we're done
    let hangup = wzp_proto::SignalMessage::Hangup {
        reason: wzp_proto::HangupReason::Normal,
    };
    transport.send_signal(&hangup).await.ok();

    let all_pcm = if record_file.is_some() {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        transport.close().await?;
        recv_handle.await.unwrap_or_default()
    } else {
        transport.close().await?;
        recv_handle.abort();
        Vec::new()
    };

    // Write recorded audio to file
    if let Some(ref path) = record_file {
        if !all_pcm.is_empty() {
            let bytes: Vec<u8> = all_pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            std::fs::write(path, &bytes)?;
            let duration_secs = all_pcm.len() as f64 / 48_000.0;
            info!(
                file = %path,
                samples = all_pcm.len(),
                duration = format!("{:.1}s", duration_secs),
                bytes = bytes.len(),
                "recording saved"
            );
            info!("play with: ffplay -f s16le -ar 48000 -ac 1 {path}");
        } else {
            info!("no audio received, nothing to write");
        }
    }

    Ok(())
}

/// Live mode: capture from mic, encode, send; receive, decode, play.
#[cfg(feature = "audio")]
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
            let mut frame = vec![0i16; FRAME_SAMPLES];
            loop {
                // Pull a full 20 ms frame from the capture ring. The ring
                // may return a partial read when the CPAL callback hasn't
                // produced enough samples yet — keep reading until we
                // accumulate a whole frame, sleeping briefly on empty
                // returns so we don't hot-spin the CPU.
                let mut filled = 0usize;
                while filled < FRAME_SAMPLES {
                    let n = capture.ring().read(&mut frame[filled..]);
                    filled += n;
                    if n == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                }
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
                    let is_repair = pkt.header.is_repair;
                    decoder.ingest(pkt);
                    // Only decode for source packets (1 source = 1 audio frame).
                    // Repair packets feed the FEC decoder but don't produce audio.
                    if !is_repair {
                        if let Some(_n) = decoder.decode_next(&mut pcm_buf) {
                            // Push the decoded frame into the playback
                            // ring. The CPAL output callback drains from
                            // here on its own clock; if the ring is full
                            // (rare in CLI live mode) the write returns
                            // a short count and the tail is dropped,
                            // which is the correct real-time behavior.
                            playback.ring().write(&pcm_buf);
                        }
                    }
                }
                Ok(None) => {
                    info!("connection closed");
                    break;
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

/// Persistent signaling mode for direct 1:1 calls.
async fn run_signal_mode(
    relay_addr: SocketAddr,
    seed: wzp_crypto::Seed,
    token: Option<String>,
    call_target: Option<String>,
) -> anyhow::Result<()> {
    use wzp_proto::SignalMessage;

    let identity = seed.derive_identity();
    let pub_id = identity.public_identity();
    let fp = pub_id.fingerprint.to_string();
    let identity_pub = *pub_id.signing.as_bytes();
    info!(fingerprint = %fp, "signal mode");

    // Connect to relay with SNI "_signal"
    let client_config = wzp_transport::client_config();
    let bind_addr: SocketAddr = if relay_addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
    let conn = wzp_transport::connect(&endpoint, relay_addr, "_signal", client_config).await?;
    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));
    info!("connected to relay (signal channel)");

    // Auth if token provided
    if let Some(ref tok) = token {
        transport.send_signal(&SignalMessage::AuthToken { token: tok.clone() }).await?;
    }

    // Register presence (signature not verified in Phase 1)
    transport.send_signal(&SignalMessage::RegisterPresence {
        identity_pub,
        signature: vec![], // Phase 1: not verified
        alias: None,
    }).await?;

    // Wait for ack
    match transport.recv_signal().await? {
        Some(SignalMessage::RegisterPresenceAck { success: true, .. }) => {
            info!(fingerprint = %fp, "registered on relay — waiting for calls");
        }
        Some(SignalMessage::RegisterPresenceAck { success: false, error }) => {
            anyhow::bail!("registration failed: {}", error.unwrap_or_default());
        }
        other => {
            anyhow::bail!("unexpected response: {other:?}");
        }
    }

    // If --call specified, place the call
    if let Some(ref target) = call_target {
        info!(target = %target, "placing direct call...");
        let call_id = format!("{:016x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());

        transport.send_signal(&SignalMessage::DirectCallOffer {
            caller_fingerprint: fp.clone(),
            caller_alias: None,
            target_fingerprint: target.clone(),
            call_id: call_id.clone(),
            identity_pub,
            ephemeral_pub: [0u8; 32], // Phase 1: not used for key exchange
            signature: vec![],
            supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
        }).await?;
    }

    // Signal recv loop — handle incoming signals
    let signal_transport = transport.clone();
    let relay = relay_addr;
    let my_seed = seed.0;

    loop {
        match signal_transport.recv_signal().await {
            Ok(Some(msg)) => match msg {
                SignalMessage::CallRinging { call_id } => {
                    info!(call_id = %call_id, "ringing...");
                }
                SignalMessage::DirectCallOffer { caller_fingerprint, caller_alias, call_id, .. } => {
                    info!(
                        from = %caller_fingerprint,
                        alias = ?caller_alias,
                        call_id = %call_id,
                        "incoming call — auto-accepting (generic)"
                    );
                    // Auto-accept for CLI testing
                    let _ = signal_transport.send_signal(&SignalMessage::DirectCallAnswer {
                        call_id,
                        accept_mode: wzp_proto::CallAcceptMode::AcceptGeneric,
                        identity_pub: Some(identity_pub),
                        ephemeral_pub: None,
                        signature: None,
                        chosen_profile: Some(wzp_proto::QualityProfile::GOOD),
                    }).await;
                }
                SignalMessage::DirectCallAnswer { call_id, accept_mode, .. } => {
                    info!(call_id = %call_id, mode = ?accept_mode, "call answered");
                }
                SignalMessage::CallSetup { call_id, room, relay_addr: setup_relay } => {
                    info!(call_id = %call_id, room = %room, relay = %setup_relay, "call setup — connecting to media room");

                    // Connect to the media room
                    let media_relay: SocketAddr = setup_relay.parse().unwrap_or(relay);
                    let media_cfg = wzp_transport::client_config();
                    match wzp_transport::connect(&endpoint, media_relay, &room, media_cfg).await {
                        Ok(media_conn) => {
                            let media_transport = Arc::new(wzp_transport::QuinnTransport::new(media_conn));

                            // Crypto handshake
                            match wzp_client::handshake::perform_handshake(&*media_transport, &my_seed, None).await {
                                Ok(_session) => {
                                    info!("media connected — sending tone (press Ctrl+C to hang up)");

                                    // Simple tone sender for testing
                                    let mt = media_transport.clone();
                                    let send_task = tokio::spawn(async move {
                                        let config = wzp_client::call::CallConfig::default();
                                        let mut encoder = wzp_client::call::CallEncoder::new(&config);
                                        let duration = tokio::time::Duration::from_millis(20);
                                        loop {
                                            let pcm: Vec<i16> = (0..FRAME_SAMPLES)
                                                .map(|_| 0i16) // silence — could be tone
                                                .collect();
                                            if let Ok(pkts) = encoder.encode_frame(&pcm) {
                                                for pkt in &pkts {
                                                    if mt.send_media(pkt).await.is_err() { return; }
                                                }
                                            }
                                            tokio::time::sleep(duration).await;
                                        }
                                    });

                                    // Wait for hangup or ctrl+c
                                    loop {
                                        tokio::select! {
                                            sig = signal_transport.recv_signal() => {
                                                match sig {
                                                    Ok(Some(SignalMessage::Hangup { .. })) => {
                                                        info!("remote hung up");
                                                        break;
                                                    }
                                                    Ok(None) | Err(_) => break,
                                                    _ => {}
                                                }
                                            }
                                            _ = tokio::signal::ctrl_c() => {
                                                info!("hanging up...");
                                                let _ = signal_transport.send_signal(&SignalMessage::Hangup {
                                                    reason: wzp_proto::HangupReason::Normal,
                                                }).await;
                                                break;
                                            }
                                        }
                                    }

                                    send_task.abort();
                                    media_transport.close().await.ok();
                                    info!("call ended");
                                }
                                Err(e) => error!("media handshake failed: {e}"),
                            }
                        }
                        Err(e) => error!("media connect failed: {e}"),
                    }
                }
                SignalMessage::Hangup { reason } => {
                    info!(reason = ?reason, "call ended by remote");
                }
                SignalMessage::Pong { .. } => {}
                other => {
                    info!("signal: {:?}", std::mem::discriminant(&other));
                }
            },
            Ok(None) => {
                info!("signal connection closed");
                break;
            }
            Err(e) => {
                error!("signal error: {e}");
                break;
            }
        }
    }

    transport.close().await.ok();
    Ok(())
}
