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
    raw_room: bool,
    alias: Option<String>,
    token: Option<String>,
    _metrics_file: Option<String>,
}

/// Default identity file path: ~/.wzp/identity
fn default_identity_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".wzp").join("identity")
}

impl CliArgs {
    /// Resolve the identity seed from --seed, --mnemonic, or persistent file.
    ///
    /// Priority: --seed > --mnemonic > ~/.wzp/identity > generate + save.
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
            let path = default_identity_path();
            // Try loading existing identity
            if path.exists() {
                if let Ok(hex_str) = std::fs::read_to_string(&path) {
                    let hex_str = hex_str.trim();
                    if let Ok(seed) = wzp_crypto::Seed::from_hex(hex_str) {
                        let id = seed.derive_identity();
                        let fp = id.public_identity().fingerprint;
                        info!(fingerprint = %fp, path = %path.display(), "loaded persistent identity");
                        return seed;
                    }
                }
            }
            // Generate new and save
            let seed = wzp_crypto::Seed::generate();
            let id = seed.derive_identity();
            let fp = id.public_identity().fingerprint;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            // Encode seed as hex manually (avoid dep on `hex` crate in binary)
            let hex_str: String = seed.0.iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(&path, hex_str).ok();
            info!(fingerprint = %fp, path = %path.display(), "generated and saved new identity");
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
    let mut raw_room = false;
    let mut alias = None;
    let mut token = None;
    let mut metrics_file = None;
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
            "--raw-room" => raw_room = true,
            "--alias" => {
                i += 1;
                alias = Some(args.get(i).expect("--alias requires a name").to_string());
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
                eprintln!("  --raw-room             Send room name as-is (no hash, for Android compat)");
                eprintln!("  --alias <name>         Display name shown to other participants");
                eprintln!("  --token <token>        featherChat bearer token for relay auth");
                eprintln!("  --metrics-file <path>  Write JSONL telemetry to file (1 line/sec)");
                eprintln!("                         (48kHz mono s16le, play with ffplay -f s16le -ar 48000 -ch_layout mono file.raw)");
                eprintln!();
                eprintln!("Identity is auto-saved to ~/.wzp/identity on first run.");
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
        raw_room,
        alias,
        token,
        _metrics_file: metrics_file,
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

    let seed = cli.resolve_seed();

    info!(
        relay = %cli.relay_addr,
        live = cli.live,
        send_tone = ?cli.send_tone_secs,
        record = ?cli.record_file,
        room = ?cli.room,
        "WarzonePhone client"
    );

    // Compute SNI from room name.
    // --raw-room sends the name as-is (for Android compat — Android doesn't hash).
    // Default behaviour hashes for privacy.
    let sni = match &cli.room {
        Some(name) if cli.raw_room => {
            info!(room = %name, "using raw room name as SNI (no hash)");
            name.clone()
        }
        Some(name) => {
            let hashed = wzp_crypto::hash_room_name(name);
            info!(room = %name, hashed = %hashed, "room name hashed for SNI");
            hashed
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
        cli.alias.as_deref(),
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
///
/// Architecture (mirrors wzp-android/engine.rs):
///   CPAL capture callback → AudioRing → send task (5ms poll) → QUIC
///   QUIC → recv task → jitter buffer → decode tick (20ms) → AudioRing → CPAL playback callback
///
/// All lock-free: CPAL callbacks use atomic ring buffers, no Mutex on the audio path.
#[cfg(feature = "audio")]
async fn run_live(
    transport: Arc<wzp_transport::QuinnTransport>,
) -> anyhow::Result<()> {
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use wzp_client::audio_io::{AudioCapture, AudioPlayback};
    use wzp_client::call::JitterTelemetry;

    let capture = AudioCapture::start()?;
    let playback = AudioPlayback::start()?;
    info!("audio I/O started (lock-free ring buffers) — press Ctrl+C to stop");

    let capture_ring = capture.ring().clone();
    let playout_ring = playback.ring().clone();

    let running = StdArc::new(AtomicBool::new(true));

    // --- Signal handler: set running=false on first Ctrl+C, force-quit on second ---
    let signal_running = running.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!(); // newline after ^C
        info!("Ctrl+C received, shutting down...");
        signal_running.store(false, Ordering::SeqCst);

        tokio::signal::ctrl_c().await.ok();
        eprintln!("\nForce quit");
        std::process::exit(1);
    });

    let config = CallConfig::default();

    // --- Send task: poll capture ring → encode → send via async ---
    let send_transport = transport.clone();
    let send_running = running.clone();
    let send_task = async move {
        let mut encoder = CallEncoder::new(&config);
        let mut capture_buf = vec![0i16; FRAME_SAMPLES];
        let mut frames_sent: u64 = 0;

        loop {
            if !send_running.load(Ordering::Relaxed) {
                break;
            }

            let avail = capture_ring.available();
            if avail < FRAME_SAMPLES {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                continue;
            }

            let read = capture_ring.read(&mut capture_buf);
            if read < FRAME_SAMPLES {
                continue;
            }

            let packets = match encoder.encode_frame(&capture_buf) {
                Ok(p) => p,
                Err(e) => {
                    error!("encode error: {e}");
                    continue;
                }
            };

            for pkt in &packets {
                if let Err(e) = send_transport.send_media(pkt).await {
                    error!("send error: {e}");
                    return;
                }
            }

            frames_sent += 1;
            if frames_sent == 1 || frames_sent % 500 == 0 {
                info!(frames_sent, "send progress");
            }
        }
    };

    // --- Recv task: receive packets → ingest into jitter buffer ---
    // Uses timeout so it can check the running flag and exit on Ctrl+C.
    let recv_transport = transport.clone();
    let recv_running = running.clone();
    let config = CallConfig::default();
    let decoder = StdArc::new(tokio::sync::Mutex::new(CallDecoder::new(&config)));
    let decoder_recv = decoder.clone();

    let recv_task = async move {
        let mut packets_received: u64 = 0;
        loop {
            if !recv_running.load(Ordering::Relaxed) {
                break;
            }
            // Timeout so we can check running flag periodically
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                recv_transport.recv_media(),
            )
            .await;
            match result {
                Ok(Ok(Some(pkt))) => {
                    let mut dec = decoder_recv.lock().await;
                    dec.ingest(pkt);
                    packets_received += 1;
                    if packets_received == 1 || packets_received % 500 == 0 {
                        info!(packets_received, depth = dec.stats().current_depth, "recv progress");
                    }
                }
                Ok(Ok(None)) => {
                    info!("connection closed");
                    break;
                }
                Ok(Err(e)) => {
                    error!("recv error: {e}");
                    break;
                }
                Err(_) => {} // timeout — loop and check running flag
            }
        }
    };

    // --- Playout tick: decode from jitter buffer at steady 20ms intervals ---
    let playout_running = running.clone();
    let decoder_playout = decoder.clone();
    let playout_task = async move {
        let mut pcm_buf = vec![0i16; FRAME_SAMPLES];
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(20));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut telemetry = JitterTelemetry::new(5);
        loop {
            interval.tick().await;
            if !playout_running.load(Ordering::Relaxed) {
                break;
            }

            let mut dec = decoder_playout.lock().await;

            // Drain ready frames from jitter buffer into playout ring.
            let mut decoded_this_tick = 0;
            while let Some(n) = dec.decode_next(&mut pcm_buf) {
                playout_ring.write(&pcm_buf[..n]);
                decoded_this_tick += 1;
                if decoded_this_tick >= 2 {
                    break; // Don't drain too aggressively in one tick
                }
            }

            telemetry.maybe_log(dec.stats());
        }
    };

    // --- Signal task: listen for RoomUpdate and display presence ---
    let signal_transport = transport.clone();
    let signal_running = running.clone();
    let signal_task = async move {
        loop {
            if !signal_running.load(Ordering::Relaxed) {
                break;
            }
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                signal_transport.recv_signal(),
            )
            .await;
            match result {
                Ok(Ok(Some(wzp_proto::SignalMessage::RoomUpdate { count, participants }))) => {
                    info!(count, "room update");
                    for p in &participants {
                        let name = p
                            .alias
                            .as_deref()
                            .unwrap_or("(no alias)");
                        let fp = if p.fingerprint.is_empty() {
                            "(no fingerprint)"
                        } else {
                            &p.fingerprint
                        };
                        info!("  participant: {name} [{fp}]");
                    }
                }
                Ok(Ok(Some(msg))) => {
                    info!("signal: {:?}", std::mem::discriminant(&msg));
                }
                Ok(Ok(None)) => {
                    info!("signal stream closed");
                    break;
                }
                Ok(Err(e)) => {
                    error!("signal recv error: {e}");
                    break;
                }
                Err(_) => {} // timeout — loop and check running flag
            }
        }
    };

    // --- Run all tasks, exit when any finishes (or running flag cleared by Ctrl+C) ---
    tokio::select! {
        _ = send_task => info!("send task ended"),
        _ = recv_task => info!("recv task ended"),
        _ = playout_task => info!("playout task ended"),
        _ = signal_task => info!("signal task ended"),
    }

    running.store(false, Ordering::SeqCst);
    capture.stop();
    playback.stop();

    // Give transport 2s to close gracefully, then bail
    match tokio::time::timeout(std::time::Duration::from_secs(2), transport.close()).await {
        Ok(Ok(())) => info!("done"),
        Ok(Err(e)) => info!("close error (non-fatal): {e}"),
        Err(_) => info!("close timed out, exiting anyway"),
    }
    Ok(())
}
