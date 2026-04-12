//! Call engine for the desktop app — wraps wzp-client audio + transport
//! into a clean async interface for Tauri commands.
//!
//! Step C of the incremental Android rewrite: the module now compiles on
//! Android too (previously cfg-gated out entirely in lib.rs), but the
//! actual `CallEngine::start()` body uses CPAL via `wzp_client::audio_io`
//! which is only available on desktop. On Android we expose a stub
//! `start()` that returns an error, so the frontend's `connect` command
//! still fails cleanly but the rest of the engine code links in.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::{error, info};

// CPAL audio I/O is only available on desktop (wzp-client's `audio` feature).
#[cfg(not(target_os = "android"))]
use wzp_client::audio_io::{AudioCapture, AudioPlayback};

// Codec + handshake pipelines are platform-independent Rust (no CPAL
// dependency) so they're available from wzp-client on both desktop and
// Android (where wzp-client is pulled in with default-features=false).
use wzp_client::call::{CallConfig, CallEncoder};

use wzp_proto::traits::AudioDecoder;
use wzp_proto::{CodecId, MediaTransport, QualityProfile};

const FRAME_SAMPLES_40MS: usize = 1920;

/// Resolve a quality string from the UI to a QualityProfile.
/// Returns None for "auto" (use default adaptive behavior).
fn resolve_quality(quality: &str) -> Option<QualityProfile> {
    match quality {
        "good" | "opus" => Some(QualityProfile::GOOD),
        "degraded" | "opus6k" => Some(QualityProfile::DEGRADED),
        "catastrophic" | "codec2-1200" => Some(QualityProfile::CATASTROPHIC),
        "codec2-3200" => Some(QualityProfile {
            codec: CodecId::Codec2_3200,
            fec_ratio: 0.5,
            frame_duration_ms: 20,
            frames_per_block: 5,
        }),
        "studio-32k" => Some(QualityProfile::STUDIO_32K),
        "studio-48k" => Some(QualityProfile::STUDIO_48K),
        "studio-64k" => Some(QualityProfile::STUDIO_64K),
        _ => None, // "auto" or unknown
    }
}

/// Wrapper to make non-Sync audio handles safe to store in shared state.
/// The audio handle is only accessed from the thread that created it (drop),
/// never shared across threads — Sync is safe.
#[allow(dead_code)]
struct SyncWrapper(Box<dyn std::any::Any + Send>);
unsafe impl Sync for SyncWrapper {}

pub struct ParticipantInfo {
    pub fingerprint: String,
    pub alias: Option<String>,
    pub relay_label: Option<String>,
}

pub struct EngineStatus {
    pub mic_muted: bool,
    pub spk_muted: bool,
    pub participants: Vec<ParticipantInfo>,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub audio_level: u32,
    pub call_duration_secs: f64,
    pub fingerprint: String,
    pub tx_codec: String,
    pub rx_codec: String,
}

pub struct CallEngine {
    running: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    spk_muted: Arc<AtomicBool>,
    participants: Arc<Mutex<Vec<ParticipantInfo>>>,
    frames_sent: Arc<AtomicU64>,
    frames_received: Arc<AtomicU64>,
    audio_level: Arc<AtomicU32>,
    tx_codec: Arc<Mutex<String>>,
    rx_codec: Arc<Mutex<String>>,
    transport: Arc<wzp_transport::QuinnTransport>,
    start_time: Instant,
    fingerprint: String,
    /// Keep audio handles alive for the duration of the call.
    /// Wrapped in SyncWrapper because AudioUnit isn't Sync.
    _audio_handle: SyncWrapper,
}

/// Phase 3b/3c DRED reconstruction state for a recv task.
///
/// Wraps the libopus 1.5 DRED decoder + two `DredState` buffers (scratch +
/// cached last-good) + sequence tracking needed to fill packet-loss gaps
/// with neural redundancy reconstruction. Lives inside the recv task of
/// `CallEngine::start` and is reset on codec/profile switches.
///
/// The original Phase 3c port landed on `crates/wzp-android/src/engine.rs`,
/// which turned out to be dead code on the Tauri mobile pipeline — the
/// live Android audio recv path is in *this* file. This helper rehomes
/// the same logic to the correct engine.
struct DredRecvState {
    dred_decoder: wzp_codec::dred_ffi::DredDecoderHandle,
    scratch: wzp_codec::dred_ffi::DredState,
    last_good: wzp_codec::dred_ffi::DredState,
    last_good_seq: Option<u16>,
    expected_seq: Option<u16>,
    pub dred_reconstructions: u64,
    pub classical_plc_invocations: u64,
    /// Number of arriving Opus packets we have parsed for DRED so far —
    /// used to throttle the periodic "DRED state observed" log to one
    /// line every N packets so logcat doesn't drown.
    parses_total: u64,
    /// Counter of parses that yielded a non-zero `samples_available`.
    parses_with_data: u64,
}

impl DredRecvState {
    fn new() -> Self {
        Self {
            dred_decoder: wzp_codec::dred_ffi::DredDecoderHandle::new()
                .expect("opus_dred_decoder_create failed at call setup"),
            scratch: wzp_codec::dred_ffi::DredState::new()
                .expect("opus_dred_alloc failed at call setup (scratch)"),
            last_good: wzp_codec::dred_ffi::DredState::new()
                .expect("opus_dred_alloc failed at call setup (good state)"),
            last_good_seq: None,
            expected_seq: None,
            dred_reconstructions: 0,
            classical_plc_invocations: 0,
            parses_total: 0,
            parses_with_data: 0,
        }
    }

    /// Parse DRED side-channel data from an arriving Opus source packet
    /// into the scratch state; on success, swap it into the cached good
    /// state and record the sequence number as the new anchor.
    ///
    /// Call this BEFORE `fill_gap_to` so the anchor reflects the freshest
    /// DRED source available for gap reconstruction.
    fn ingest_opus(&mut self, seq: u16, payload: &[u8]) {
        self.parses_total += 1;
        match self.dred_decoder.parse_into(&mut self.scratch, payload) {
            Ok(available) if available > 0 => {
                self.parses_with_data += 1;
                std::mem::swap(&mut self.scratch, &mut self.last_good);
                self.last_good_seq = Some(seq);

                // First successful parse on this call: log loudly so the
                // user can see "DRED is on the wire" in logcat. After
                // that, sample every 100th parse to confirm the window
                // is steady-state without drowning the log.
                let should_log = self.parses_with_data == 1
                    || self.parses_with_data % 100 == 0;
                if should_log && wzp_codec::dred_verbose_logs() {
                    info!(
                        seq,
                        samples_available = available,
                        ms = available / 48,
                        parses_with_data = self.parses_with_data,
                        parses_total = self.parses_total,
                        "DRED state parsed from Opus packet"
                    );
                }
            }
            _ => {
                // Packet carried no DRED data, or parse failed — keep
                // the cached good state (it may still cover upcoming
                // gaps from a warm-up period).
            }
        }
    }

    /// On an arriving packet with sequence `current_seq`, detect any gap
    /// from `expected_seq` to `current_seq - 1` and fill the missing
    /// frames via DRED reconstruction (if state covers them) or classical
    /// Opus PLC fallback. The `emit` callback is invoked once per
    /// reconstructed/concealed frame with a `&mut [i16]` slice of length
    /// `frame_samples`; the caller is responsible for AGC + playout.
    ///
    /// Updates `expected_seq` to `current_seq + 1` on return.
    fn fill_gap_to<F>(
        &mut self,
        decoder: &mut wzp_codec::AdaptiveDecoder,
        current_seq: u16,
        frame_samples: usize,
        pcm_scratch: &mut [i16],
        mut emit: F,
    ) where
        F: FnMut(&mut [i16]),
    {
        const MAX_GAP_FRAMES: u16 = 16;
        if let Some(expected) = self.expected_seq {
            let gap = current_seq.wrapping_sub(expected);
            if gap > 0 && gap <= MAX_GAP_FRAMES {
                let available = self.last_good.samples_available();
                for gap_idx in 0..gap {
                    let missing_seq = expected.wrapping_add(gap_idx);
                    let offset_samples = match self.last_good_seq {
                        Some(anchor) => {
                            let delta = anchor.wrapping_sub(missing_seq);
                            if delta == 0 || delta > MAX_GAP_FRAMES {
                                -1 // skip DRED, fall through to PLC
                            } else {
                                delta as i32 * frame_samples as i32
                            }
                        }
                        None => -1,
                    };
                    let out = &mut pcm_scratch[..frame_samples];
                    let reconstructed = if offset_samples > 0 && offset_samples <= available {
                        decoder
                            .reconstruct_from_dred(&self.last_good, offset_samples, out)
                            .ok()
                    } else {
                        None
                    };
                    match reconstructed {
                        Some(_n) => {
                            self.dred_reconstructions += 1;
                            // Log every DRED reconstruction (gated behind
                            // the GUI verbose-logs toggle). When enabled,
                            // we want to know exactly which gap was
                            // filled and how the offset math played out.
                            if wzp_codec::dred_verbose_logs() {
                                info!(
                                    missing_seq,
                                    anchor_seq = ?self.last_good_seq,
                                    offset_samples,
                                    offset_ms = offset_samples / 48,
                                    samples_available = available,
                                    gap_size = gap,
                                    total_dred_recoveries = self.dred_reconstructions,
                                    "DRED reconstruction fired for missing frame"
                                );
                            }
                            emit(out);
                        }
                        None => {
                            if decoder.decode_lost(out).is_ok() {
                                self.classical_plc_invocations += 1;
                                // Log the first few classical PLC fills
                                // and then sample, so we can see when
                                // DRED couldn't cover a gap. The reason
                                // is whichever check failed in the if
                                // above (offset out of range, no good
                                // state, or reconstruct error).
                                if (self.classical_plc_invocations <= 3
                                    || self.classical_plc_invocations % 50 == 0)
                                    && wzp_codec::dred_verbose_logs()
                                {
                                    info!(
                                        missing_seq,
                                        anchor_seq = ?self.last_good_seq,
                                        offset_samples,
                                        samples_available = available,
                                        total_classical_plc = self.classical_plc_invocations,
                                        "classical PLC fill (DRED could not cover gap)"
                                    );
                                }
                                emit(out);
                            }
                        }
                    }
                }
            }
        }
        self.expected_seq = Some(current_seq.wrapping_add(1));
    }

    /// Invalidate sequence tracking on profile switch. The cached DRED
    /// state is tied to the old profile's frame rate so offsets would
    /// produce wrong reconstructions until the next good-state parse.
    fn reset_on_profile_switch(&mut self) {
        self.last_good_seq = None;
        self.expected_seq = None;
    }
}

impl CallEngine {
    /// Android engine path — uses the standalone `wzp-native` cdylib
    /// (loaded at startup via `crate::wzp_native::init()`) for Oboe-backed
    /// capture and playout instead of CPAL. Mirrors the desktop send/recv
    /// task structure otherwise.
    #[cfg(target_os = "android")]
    pub async fn start<F>(
        relay: String,
        room: String,
        alias: String,
        _os_aec: bool,
        quality: String,
        reuse_endpoint: Option<wzp_transport::Endpoint>,
        // Phase 3.5: caller did the dual-path race and picked a
        // winning transport (direct or relay). If Some, we skip
        // our own wzp_transport::connect step and use this
        // directly. If None, existing Phase 0 behavior.
        pre_connected_transport: Option<Arc<wzp_transport::QuinnTransport>>,
        // Phase 5.6: Tauri AppHandle for emitting call-debug
        // events from inside the send/recv tasks. Lets the
        // debug log pane show first-send/first-recv/heartbeat
        // events when the user has call debug logs enabled.
        app: tauri::AppHandle,
        event_cb: F,
    ) -> Result<Self, anyhow::Error>
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        // Single "call epoch" timestamp threaded through send + recv tasks
        // so every milestone log can carry t_ms_since_call_start. Used to
        // diagnose the first-join no-audio regression by giving us a clean
        // ordering between audio_start, first capture, first recv, first
        // decode, first playout-ring write, and the C++ Oboe first-callback
        // logs (which already exist in cpp/oboe_bridge.cpp).
        let call_t0 = std::time::Instant::now();
        info!(
            %relay, %room, %alias, %quality,
            has_reuse = reuse_endpoint.is_some(),
            has_pre_connected = pre_connected_transport.is_some(),
            t_ms = 0u128,
            "CallEngine::start (android) invoked"
        );
        let _ = rustls::crypto::ring::default_provider().install_default();

        let relay_addr: SocketAddr = relay.parse()?;
        info!(%relay_addr, "resolved relay addr");

        // Identity via shared helper (uses Tauri path().app_data_dir()).
        let seed = crate::load_or_create_seed()
            .map_err(|e| anyhow::anyhow!("identity: {e}"))?;
        let fp = seed.derive_identity().public_identity().fingerprint;
        let fingerprint = fp.to_string();
        info!(%fp, "identity loaded");

        // Transport source: either the pre-connected one from the
        // dual-path race (Phase 3.5) or build a fresh one here.
        let transport = if let Some(t) = pre_connected_transport {
            info!(t_ms = call_t0.elapsed().as_millis(), "first-join diag: using pre-connected transport from dual-path race");
            t
        } else {
            // QUIC transport + handshake (Phase 0 relay-only path).
            //
            // If a `reuse_endpoint` was passed in (the direct-call path, where we
            // already opened a quinn::Endpoint for the signal connection), reuse
            // it: a second quinn::Endpoint on Android silently fails to complete
            // the QUIC handshake against the same relay. Reusing the existing
            // socket lets quinn multiplex the signal + media connections on one
            // UDP port.
            let endpoint = if let Some(ep) = reuse_endpoint {
                info!(local_addr = ?ep.local_addr().ok(), "reusing signal endpoint for media connection");
                ep
            } else {
                let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
                let ep = wzp_transport::create_endpoint(bind_addr, None)
                    .map_err(|e| { error!("create_endpoint failed: {e}"); e })?;
                info!(local_addr = ?ep.local_addr().ok(), "created new endpoint, dialing relay");
                ep
            };
            let client_config = wzp_transport::client_config();
            let conn = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                wzp_transport::connect(&endpoint, relay_addr, &room, client_config),
            ).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    error!("connect failed: {e}");
                    return Err(e.into());
                }
                Err(_) => {
                    error!("connect TIMED OUT after 10s — QUIC handshake never completed. Relay may be unreachable from this endpoint.");
                    return Err(anyhow::anyhow!("QUIC connect timeout (10s)"));
                }
            };
            info!(t_ms = call_t0.elapsed().as_millis(), "first-join diag: QUIC connection established, performing handshake");
            Arc::new(wzp_transport::QuinnTransport::new(conn))
        };

        let _session = wzp_client::handshake::perform_handshake(
            &*transport,
            &seed.0,
            Some(&alias),
        )
        .await
        .map_err(|e| { error!("perform_handshake failed: {e}"); e })?;
        info!(t_ms = call_t0.elapsed().as_millis(), "first-join diag: connected to relay, handshake complete");
        event_cb("connected", &format!("joined room {room}"));

        // Oboe audio via the wzp-native cdylib that was dlopen'd at
        // startup. `wzp_native::audio_start()` brings up the capture +
        // playout streams; send/recv tasks below pull/push PCM through
        // the extern "C" bridge rings.
        if !crate::wzp_native::is_loaded() {
            return Err(anyhow::anyhow!(
                "wzp-native not loaded — dlopen failed at startup"
            ));
        }
        let t_pre_audio = call_t0.elapsed().as_millis();
        if let Err(code) = crate::wzp_native::audio_start() {
            return Err(anyhow::anyhow!("wzp_native_audio_start failed: code {code}"));
        }
        // Diagnostic: how long did audio_start() take, and at what
        // wall-clock offset from CallEngine::start did it complete?
        // Compare to the C++ "playout cb#0" log timestamp in logcat to
        // see whether the Oboe playout callback fires before or after
        // the recv task starts pushing decoded frames.
        let t_audio_start_done = call_t0.elapsed().as_millis();
        info!(
            t_ms = t_audio_start_done,
            audio_start_ms = t_audio_start_done.saturating_sub(t_pre_audio),
            "first-join diag: wzp-native audio started"
        );

        let running = Arc::new(AtomicBool::new(true));
        let mic_muted = Arc::new(AtomicBool::new(false));
        let spk_muted = Arc::new(AtomicBool::new(false));
        let participants: Arc<Mutex<Vec<ParticipantInfo>>> = Arc::new(Mutex::new(vec![]));
        let frames_sent = Arc::new(AtomicU64::new(0));
        let frames_received = Arc::new(AtomicU64::new(0));
        let audio_level = Arc::new(AtomicU32::new(0));
        let tx_codec = Arc::new(Mutex::new(String::new()));
        let rx_codec = Arc::new(Mutex::new(String::new()));

        // Send task — drain Oboe capture ring, Opus-encode, push to transport.
        let send_t = transport.clone();
        let send_r = running.clone();
        let send_mic = mic_muted.clone();
        let send_fs = frames_sent.clone();
        let send_level = audio_level.clone();
        let send_drops = Arc::new(AtomicU64::new(0));
        let send_quality = quality.clone();
        let send_tx_codec = tx_codec.clone();
        let send_t0 = call_t0;
        let send_app = app.clone();
        tokio::spawn(async move {
            let profile = resolve_quality(&send_quality);
            let config = match profile {
                Some(p) => CallConfig {
                    noise_suppression: false,
                    suppression_enabled: false,
                    ..CallConfig::from_profile(p)
                },
                None => CallConfig {
                    noise_suppression: false,
                    suppression_enabled: false,
                    ..CallConfig::default()
                },
            };
            let frame_samples = (config.profile.frame_duration_ms as usize) * 48;
            info!(codec = ?config.profile.codec, frame_samples, t_ms = send_t0.elapsed().as_millis(), "first-join diag: send task spawned (android/oboe)");
            *send_tx_codec.lock().await = format!("{:?}", config.profile.codec);
            let mut encoder = CallEncoder::new(&config);
            encoder.set_aec_enabled(false);
            let mut buf = vec![0i16; frame_samples];

            let mut heartbeat = std::time::Instant::now();
            let mut last_rms: u32 = 0;
            let mut last_pkt_bytes: usize = 0;
            let mut short_reads: u64 = 0;
            // First-join diagnostic: latch the wall-clock offset of the
            // first full-frame capture read and the first non-zero RMS
            // reading separately. The gap between them tells us how long
            // Oboe input took to actually start delivering real samples
            // after returning a "started" status from audio_start.
            let mut first_full_read_logged = false;
            let mut first_nonzero_rms_logged = false;

            loop {
                if !send_r.load(Ordering::Relaxed) {
                    break;
                }
                // wzp-native doesn't expose `available()`, so we just try
                // to read a full frame and sleep briefly if the ring is
                // short. Oboe's capture callback fills at a steady rate
                // so in steady state this spins once per frame.
                let read = crate::wzp_native::audio_read_capture(&mut buf);
                if read < frame_samples {
                    short_reads += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    continue;
                }
                if !first_full_read_logged {
                    info!(
                        t_ms = send_t0.elapsed().as_millis(),
                        short_reads_before = short_reads,
                        frame_samples,
                        "first-join diag: send first full capture frame read"
                    );
                    first_full_read_logged = true;
                }

                // RMS for UI meter
                let sum_sq: f64 = buf.iter().map(|&s| (s as f64) * (s as f64)).sum();
                let rms = (sum_sq / buf.len() as f64).sqrt() as u32;
                send_level.store(rms, Ordering::Relaxed);
                last_rms = rms;
                if !first_nonzero_rms_logged && rms > 0 {
                    info!(
                        t_ms = send_t0.elapsed().as_millis(),
                        rms,
                        "first-join diag: send first non-zero capture RMS"
                    );
                    first_nonzero_rms_logged = true;
                }

                if send_mic.load(Ordering::Relaxed) {
                    buf.fill(0);
                }
                match encoder.encode_frame(&buf) {
                    Ok(pkts) => {
                        for pkt in &pkts {
                            last_pkt_bytes = pkt.payload.len();
                            if let Err(e) = send_t.send_media(pkt).await {
                                send_drops.fetch_add(1, Ordering::Relaxed);
                                if send_drops.load(Ordering::Relaxed) <= 3 {
                                    tracing::warn!("send_media error (dropping packet): {e}");
                                }
                            }
                        }
                        let before = send_fs.fetch_add(1, Ordering::Relaxed);
                        if before == 0 {
                            // First encoded frame successfully handed
                            // to the transport. Useful for diagnosing
                            // 1-way audio: if this fires but the
                            // peer's media:first_recv never does,
                            // outbound is broken on our side.
                            crate::emit_call_debug(
                                &send_app,
                                "media:first_send",
                                serde_json::json!({
                                    "t_ms": send_t0.elapsed().as_millis() as u64,
                                    "pkt_bytes": last_pkt_bytes,
                                }),
                            );
                        }
                    }
                    Err(e) => error!("encode: {e}"),
                }

                // Heartbeat every 2s with capture+encode+send state
                if heartbeat.elapsed() >= std::time::Duration::from_secs(2) {
                    let fs = send_fs.load(Ordering::Relaxed);
                    let drops = send_drops.load(Ordering::Relaxed);
                    info!(
                        frames_sent = fs,
                        last_rms,
                        last_pkt_bytes,
                        short_reads,
                        send_drops = drops,
                        "send heartbeat (android)"
                    );
                    // Phase 5.6: also emit to the GUI debug log
                    // when call debug is enabled. Helps diagnose
                    // 1-way audio — a stalled send heartbeat
                    // (frames_sent == 0 or last_rms == 0) tells
                    // you capture/mic is broken; a live one with
                    // no peer recv tells you outbound is being
                    // dropped somewhere in the media path.
                    crate::emit_call_debug(
                        &send_app,
                        "media:send_heartbeat",
                        serde_json::json!({
                            "frames_sent": fs,
                            "last_rms": last_rms,
                            "last_pkt_bytes": last_pkt_bytes,
                            "short_reads": short_reads,
                            "drops": drops,
                        }),
                    );
                    heartbeat = std::time::Instant::now();
                }
            }
        });

        // Recv task — decode incoming packets, push PCM into Oboe playout.
        let recv_t = transport.clone();
        let recv_r = running.clone();
        let recv_spk = spk_muted.clone();
        let recv_fr = frames_received.clone();
        let recv_rx_codec = rx_codec.clone();
        let recv_t0 = call_t0;
        let recv_app = app.clone();
        tokio::spawn(async move {
            let initial_profile = resolve_quality(&quality).unwrap_or(QualityProfile::GOOD);
            // Phase 3b/3c: use concrete AdaptiveDecoder (not Box<dyn
            // AudioDecoder>) so we can call the inherent
            // reconstruct_from_dred method on packet-loss gaps.
            let mut decoder = wzp_codec::AdaptiveDecoder::new(initial_profile)
                .expect("failed to create adaptive decoder");
            let mut current_profile = initial_profile;
            let mut current_codec = initial_profile.codec;
            let mut agc = wzp_codec::AutoGainControl::new();
            let mut pcm = vec![0i16; FRAME_SAMPLES_40MS];
            // Phase 3b/3c DRED reconstruction state — see DredRecvState
            // above for the full flow.
            let mut dred_recv = DredRecvState::new();
            info!(codec = ?current_codec, t_ms = recv_t0.elapsed().as_millis(), "first-join diag: recv task spawned (android/oboe)");
            // First-join diagnostic latches — see send task above for the
            // sibling capture milestones.
            let mut first_decode_logged = false;
            let mut first_playout_write_logged = false;

            // ─── Decoded-PCM recorder (debug) ────────────────────────────
            // Dumps the first ~10 seconds of post-AGC PCM to a raw i16 LE
            // file in the app's private data dir so we can adb pull it and
            // play it back to prove the pipeline is producing real audio
            // independent of Oboe routing. Convert locally with e.g.
            //   ffmpeg -f s16le -ar 48000 -ac 1 -i decoded.pcm decoded.wav
            use std::io::Write;
            let recorder_path = crate::APP_DATA_DIR
                .get()
                .map(|p| p.join("decoded.pcm"));
            let mut recorder = match recorder_path.as_ref() {
                Some(p) => match std::fs::File::create(p) {
                    Ok(f) => {
                        info!(path = %p.display(), "decoded-pcm recorder open");
                        Some(std::io::BufWriter::new(f))
                    }
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e, "decoded-pcm recorder open failed");
                        None
                    }
                },
                None => None,
            };
            let mut recorder_bytes: u64 = 0;
            // Stop writing after ~10 seconds @ 48kHz mono i16 = ~960KB.
            const RECORDER_MAX_BYTES: u64 = 48_000 * 2 * 10;

            let mut heartbeat = std::time::Instant::now();
            let mut decoded_frames: u64 = 0;
            let mut written_samples: u64 = 0;
            let mut last_decode_n: usize = 0;
            let mut last_written: usize = 0;
            let mut decode_errs: u64 = 0;
            let mut first_packet_logged = false;

            loop {
                if !recv_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    recv_t.recv_media(),
                )
                .await
                {
                    Ok(Ok(Some(pkt))) => {
                        if !first_packet_logged {
                            info!(
                                t_ms = recv_t0.elapsed().as_millis(),
                                codec_id = ?pkt.header.codec_id,
                                payload_bytes = pkt.payload.len(),
                                is_repair = pkt.header.is_repair,
                                "first-join diag: recv first media packet"
                            );
                            first_packet_logged = true;
                            // Phase 5.6 GUI debug: first packet from
                            // the peer. Useful for diagnosing 1-way
                            // audio — if this fires and the peer
                            // never sees media:first_recv, our
                            // inbound path is fine and theirs is
                            // broken, and vice versa.
                            crate::emit_call_debug(
                                &recv_app,
                                "media:first_recv",
                                serde_json::json!({
                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                    "codec": format!("{:?}", pkt.header.codec_id),
                                    "payload_bytes": pkt.payload.len(),
                                    "is_repair": pkt.header.is_repair,
                                }),
                            );
                        }
                        if !pkt.header.is_repair && pkt.header.codec_id != CodecId::ComfortNoise {
                            {
                                let mut rx = recv_rx_codec.lock().await;
                                let codec_name = format!("{:?}", pkt.header.codec_id);
                                if *rx != codec_name { *rx = codec_name; }
                            }
                            if pkt.header.codec_id != current_codec {
                                let new_profile = match pkt.header.codec_id {
                                    CodecId::Opus24k => QualityProfile::GOOD,
                                    CodecId::Opus6k => QualityProfile::DEGRADED,
                                    CodecId::Opus32k => QualityProfile::STUDIO_32K,
                                    CodecId::Opus48k => QualityProfile::STUDIO_48K,
                                    CodecId::Opus64k => QualityProfile::STUDIO_64K,
                                    CodecId::Codec2_1200 => QualityProfile::CATASTROPHIC,
                                    CodecId::Codec2_3200 => QualityProfile {
                                        codec: CodecId::Codec2_3200,
                                        fec_ratio: 0.5, frame_duration_ms: 20, frames_per_block: 5,
                                    },
                                    other => QualityProfile { codec: other, ..QualityProfile::GOOD },
                                };
                                info!(from = ?current_codec, to = ?pkt.header.codec_id, "recv: switching decoder");
                                let _ = decoder.set_profile(new_profile);
                                current_profile = new_profile;
                                current_codec = pkt.header.codec_id;
                                // Phase 3c: new profile → offsets in the
                                // cached DRED state are invalid; reset.
                                dred_recv.reset_on_profile_switch();
                            }

                            // Phase 3b/3c DRED flow for Opus packets:
                            //   1. parse DRED from this packet → last_good
                            //   2. detect gap back to expected_seq and
                            //      reconstruct missing frames via DRED
                            //      (or classical PLC if no state covers)
                            //   3. then decode the current packet normally
                            //      (unchanged fall-through below)
                            //
                            // Codec2 packets skip DRED entirely — libopus
                            // can't reconstruct them and the parse is a
                            // no-op.
                            if pkt.header.codec_id.is_opus() {
                                dred_recv.ingest_opus(pkt.header.seq, &pkt.payload);
                                let frame_samples_now = (48_000
                                    * current_profile.frame_duration_ms as usize)
                                    / 1000;
                                let spk_muted_flag = recv_spk.load(Ordering::Relaxed);
                                dred_recv.fill_gap_to(
                                    &mut decoder,
                                    pkt.header.seq,
                                    frame_samples_now,
                                    &mut pcm,
                                    |samples| {
                                        agc.process_frame(samples);
                                        if !spk_muted_flag {
                                            let _ = crate::wzp_native::audio_write_playout(samples);
                                        }
                                    },
                                );
                            }

                            match decoder.decode(&pkt.payload, &mut pcm) {
                                Ok(n) => {
                                    last_decode_n = n;
                                    decoded_frames += 1;
                                    if !first_decode_logged {
                                        info!(
                                            t_ms = recv_t0.elapsed().as_millis(),
                                            n,
                                            codec = ?current_codec,
                                            "first-join diag: recv first successful decode"
                                        );
                                        first_decode_logged = true;
                                    }
                                    // Log sample range for the first few decoded frames and periodically
                                    if decoded_frames <= 3 || decoded_frames % 100 == 0 {
                                        let slice = &pcm[..n];
                                        let (mut lo, mut hi, mut sumsq) = (i16::MAX, i16::MIN, 0i64);
                                        for &s in slice.iter() {
                                            if s < lo { lo = s; }
                                            if s > hi { hi = s; }
                                            sumsq += (s as i64) * (s as i64);
                                        }
                                        let rms = (sumsq as f64 / n as f64).sqrt() as i32;
                                        info!(
                                            decoded_frames,
                                            n,
                                            sample_lo = lo,
                                            sample_hi = hi,
                                            rms,
                                            codec = ?current_codec,
                                            "recv: decoded PCM sample range"
                                        );
                                    }
                                    agc.process_frame(&mut pcm[..n]);

                                    // Dump to debug recorder before playout
                                    // so we capture post-AGC samples that
                                    // are exactly what we hand to Oboe.
                                    if let Some(rec) = recorder.as_mut() {
                                        if recorder_bytes < RECORDER_MAX_BYTES {
                                            let slice = &pcm[..n];
                                            // SAFETY: i16 is Plain Old Data;
                                            // writing its little-endian bytes
                                            // is well-defined on all targets
                                            // we build for.
                                            let byte_slice: &[u8] = unsafe {
                                                std::slice::from_raw_parts(
                                                    slice.as_ptr() as *const u8,
                                                    slice.len() * 2,
                                                )
                                            };
                                            let _ = rec.write_all(byte_slice);
                                            recorder_bytes = recorder_bytes
                                                .saturating_add(byte_slice.len() as u64);
                                            if recorder_bytes >= RECORDER_MAX_BYTES {
                                                let _ = rec.flush();
                                                info!(recorder_bytes, "decoded-pcm recorder: stopped after limit");
                                            }
                                        }
                                    }

                                    if !recv_spk.load(Ordering::Relaxed) {
                                        let w = crate::wzp_native::audio_write_playout(&pcm[..n]);
                                        if !first_playout_write_logged {
                                            info!(
                                                t_ms = recv_t0.elapsed().as_millis(),
                                                n,
                                                w,
                                                "first-join diag: recv first playout-ring write"
                                            );
                                            first_playout_write_logged = true;
                                        }
                                        last_written = w;
                                        written_samples = written_samples.saturating_add(w as u64);
                                        if w < n && decoded_frames <= 10 {
                                            tracing::warn!(n, w, "recv: partial playout write (ring nearly full)");
                                        }
                                    } else if decoded_frames <= 3 || decoded_frames % 100 == 0 {
                                        // User clicked spk-mute — log it so we don't chase ghost bugs
                                        tracing::info!(decoded_frames, "recv: spk_muted=true, skipping playout write");
                                    }
                                }
                                Err(e) => {
                                    decode_errs += 1;
                                    if decode_errs <= 3 {
                                        tracing::warn!("decode error: {e}");
                                    }
                                }
                            }
                        }
                        recv_fr.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        if msg.contains("closed") || msg.contains("reset") {
                            error!("recv fatal: {e}");
                            break;
                        }
                    }
                    Err(_) => {}
                }

                // Heartbeat every 2s with decode+playout state
                if heartbeat.elapsed() >= std::time::Duration::from_secs(2) {
                    let fr = recv_fr.load(Ordering::Relaxed);
                    if wzp_codec::dred_verbose_logs() {
                        info!(
                            recv_fr = fr,
                            decoded_frames,
                            last_decode_n,
                            last_written,
                            written_samples,
                            decode_errs,
                            codec = ?current_codec,
                            dred_recv = dred_recv.dred_reconstructions,
                            classical_plc = dred_recv.classical_plc_invocations,
                            dred_parses_with_data = dred_recv.parses_with_data,
                            dred_parses_total = dred_recv.parses_total,
                            "recv heartbeat (android)"
                        );
                    } else {
                        info!(
                            recv_fr = fr,
                            decoded_frames,
                            last_decode_n,
                            last_written,
                            written_samples,
                            decode_errs,
                            codec = ?current_codec,
                            "recv heartbeat (android)"
                        );
                    }
                    // Phase 5.6: compact GUI debug emit.
                    // recv_fr == 0 over time indicates inbound
                    // media is not reaching the client — either
                    // nothing is being sent by the peer, or the
                    // transport is dropping packets, or we're
                    // connected to the wrong side of the media
                    // path. Combined with the peer's send_heartbeat
                    // from the other log, this tells us exactly
                    // where 1-way audio breaks.
                    crate::emit_call_debug(
                        &recv_app,
                        "media:recv_heartbeat",
                        serde_json::json!({
                            "recv_fr": fr,
                            "decoded_frames": decoded_frames,
                            "last_written": last_written,
                            "written_samples": written_samples,
                            "decode_errs": decode_errs,
                            "codec": format!("{:?}", current_codec),
                        }),
                    );
                    heartbeat = std::time::Instant::now();
                }
            }
        });

        // Signal task (presence — same shape as desktop).
        let sig_t = transport.clone();
        let sig_r = running.clone();
        let sig_p = participants.clone();
        let event_cb = Arc::new(event_cb);
        let sig_cb = event_cb.clone();
        tokio::spawn(async move {
            loop {
                if !sig_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sig_t.recv_signal(),
                )
                .await
                {
                    Ok(Ok(Some(wzp_proto::SignalMessage::RoomUpdate {
                        participants: parts,
                        ..
                    }))) => {
                        let mut seen = std::collections::HashSet::new();
                        let unique: Vec<ParticipantInfo> = parts
                            .into_iter()
                            .filter(|p| seen.insert((p.fingerprint.clone(), p.alias.clone())))
                            .map(|p| ParticipantInfo {
                                fingerprint: p.fingerprint,
                                alias: p.alias,
                                relay_label: p.relay_label,
                            })
                            .collect();
                        let count = unique.len();
                        *sig_p.lock().await = unique;
                        sig_cb("room-update", &format!("{count} participants"));
                    }
                    Ok(Ok(Some(_))) => {}
                    Ok(Ok(None)) => break,
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
        });

        Ok(Self {
            running,
            mic_muted,
            spk_muted,
            participants,
            frames_sent,
            frames_received,
            audio_level,
            transport,
            start_time: Instant::now(),
            fingerprint,
            tx_codec,
            rx_codec,
            // No CPAL / VPIO handle to keep alive on Android — wzp_native
            // is a static dlopen'd library, the audio streams live inside
            // the standalone cdylib's process-global singleton.
            _audio_handle: SyncWrapper(Box::new(())),
        })
    }

    #[cfg(not(target_os = "android"))]
    pub async fn start<F>(
        relay: String,
        room: String,
        alias: String,
        _os_aec: bool,
        quality: String,
        reuse_endpoint: Option<wzp_transport::Endpoint>,
        // Phase 3.5: caller did the dual-path race and picked a
        // winning transport. If Some, skip our own connect step.
        pre_connected_transport: Option<Arc<wzp_transport::QuinnTransport>>,
        // Phase 5.6: Tauri AppHandle for call-debug event emits
        // from inside the send/recv tasks. See android branch for
        // the full rationale. Desktop branch accepts it for API
        // symmetry but doesn't yet thread it into the send/recv
        // tasks — android is where the reporter actually sees the
        // 1-way audio regression.
        _app: tauri::AppHandle,
        event_cb: F,
    ) -> Result<Self, anyhow::Error>
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        info!(
            %relay, %room, %alias, %quality,
            has_reuse = reuse_endpoint.is_some(),
            has_pre_connected = pre_connected_transport.is_some(),
            "CallEngine::start (desktop) invoked"
        );
        let _ = rustls::crypto::ring::default_provider().install_default();

        let relay_addr: SocketAddr = relay.parse()?;

        // Identity via the SHARED helper — same path resolution as
        // register_signal (Tauri app_data_dir, e.g. on macOS
        // ~/Library/Application Support/com.wzp.desktop/.wzp/identity).
        //
        // The previous implementation loaded the seed manually from
        // $HOME/.wzp/identity which is a DIFFERENT file on macOS, so
        // register_signal and CallEngine::start were using different
        // identities — direct calls placed from desktop were routed
        // by the relay under the CallEngine fingerprint but the callee
        // had registered under a different fingerprint, making the
        // call unroutable.
        let seed = crate::load_or_create_seed()
            .map_err(|e| anyhow::anyhow!("identity: {e}"))?;
        let fp = seed.derive_identity().public_identity().fingerprint;
        let fingerprint = fp.to_string();
        info!(%fp, "identity loaded");

        // Transport source: either the pre-connected dual-path
        // winner (Phase 3.5) or build a fresh relay connection here.
        let transport = if let Some(t) = pre_connected_transport {
            info!("using pre-connected transport from dual-path race");
            t
        } else {
            // Connect — reuse the signal endpoint if the direct-call path gave
            // us one, otherwise create a fresh one (SFU room join path).
            let endpoint = if let Some(ep) = reuse_endpoint {
                info!(local_addr = ?ep.local_addr().ok(), "reusing signal endpoint for media connection");
                ep
            } else {
                let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
                let ep = wzp_transport::create_endpoint(bind_addr, None)
                    .map_err(|e| { error!("create_endpoint failed: {e}"); e })?;
                info!(local_addr = ?ep.local_addr().ok(), "created new endpoint, dialing relay");
                ep
            };
            let client_config = wzp_transport::client_config();
            let conn = wzp_transport::connect(&endpoint, relay_addr, &room, client_config)
                .await
                .map_err(|e| { error!("connect failed: {e}"); e })?;
            info!("QUIC connection established, performing handshake");
            Arc::new(wzp_transport::QuinnTransport::new(conn))
        };

        // Handshake
        let _session = wzp_client::handshake::perform_handshake(
            &*transport,
            &seed.0,
            Some(&alias),
        )
        .await
        .map_err(|e| { error!("perform_handshake failed: {e}"); e })?;

        info!("connected to relay, handshake complete");
        event_cb("connected", &format!("joined room {room}"));

        // Audio I/O — VPIO (OS AEC) on macOS, plain CPAL otherwise.
        // The audio handle must be stored in CallEngine to keep streams alive.
        let (capture_ring, playout_ring, audio_handle): (_, _, Box<dyn std::any::Any + Send>) =
            if _os_aec {
                #[cfg(target_os = "macos")]
                {
                    match wzp_client::audio_vpio::VpioAudio::start() {
                        Ok(v) => {
                            let cr = v.capture_ring().clone();
                            let pr = v.playout_ring().clone();
                            info!("using VoiceProcessingIO (OS AEC)");
                            (cr, pr, Box::new(v))
                        }
                        Err(e) => {
                            info!("VPIO failed ({e}), falling back to CPAL");
                            let capture = AudioCapture::start()?;
                            let playback = AudioPlayback::start()?;
                            let cr = capture.ring().clone();
                            let pr = playback.ring().clone();
                            (cr, pr, Box::new((capture, playback)))
                        }
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    info!("OS AEC not available on this platform, using CPAL");
                    let capture = AudioCapture::start()?;
                    let playback = AudioPlayback::start()?;
                    let cr = capture.ring().clone();
                    let pr = playback.ring().clone();
                    (cr, pr, Box::new((capture, playback)))
                }
            } else {
                let capture = AudioCapture::start()?;
                let playback = AudioPlayback::start()?;
                let cr = capture.ring().clone();
                let pr = playback.ring().clone();
                (cr, pr, Box::new((capture, playback)))
            };

        let running = Arc::new(AtomicBool::new(true));
        let mic_muted = Arc::new(AtomicBool::new(false));
        let spk_muted = Arc::new(AtomicBool::new(false));
        let participants: Arc<Mutex<Vec<ParticipantInfo>>> = Arc::new(Mutex::new(vec![]));
        let frames_sent = Arc::new(AtomicU64::new(0));
        let frames_received = Arc::new(AtomicU64::new(0));
        let audio_level = Arc::new(AtomicU32::new(0));
        let tx_codec = Arc::new(Mutex::new(String::new()));
        let rx_codec = Arc::new(Mutex::new(String::new()));

        // Send task
        let send_t = transport.clone();
        let send_r = running.clone();
        let send_mic = mic_muted.clone();
        let send_fs = frames_sent.clone();
        let send_level = audio_level.clone();
        let send_drops = Arc::new(AtomicU64::new(0));
        let send_quality = quality.clone();
        let send_tx_codec = tx_codec.clone();
        tokio::spawn(async move {
            let profile = resolve_quality(&send_quality);
            let config = match profile {
                Some(p) => CallConfig {
                    noise_suppression: false,
                    suppression_enabled: false,
                    ..CallConfig::from_profile(p)
                },
                None => CallConfig {
                    noise_suppression: false,
                    suppression_enabled: false,
                    ..CallConfig::default()
                },
            };
            let frame_samples = (config.profile.frame_duration_ms as usize) * 48;
            info!(codec = ?config.profile.codec, frame_samples, "send task starting");
            *send_tx_codec.lock().await = format!("{:?}", config.profile.codec);
            let mut encoder = CallEncoder::new(&config);
            encoder.set_aec_enabled(false); // OS AEC or none
            let mut buf = vec![0i16; frame_samples];

            loop {
                if !send_r.load(Ordering::Relaxed) {
                    break;
                }
                if capture_ring.available() < frame_samples {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    continue;
                }
                capture_ring.read(&mut buf);

                // Compute RMS audio level for UI meter
                if !buf.is_empty() {
                    let sum_sq: f64 = buf.iter().map(|&s| (s as f64) * (s as f64)).sum();
                    let rms = (sum_sq / buf.len() as f64).sqrt() as u32;
                    send_level.store(rms, Ordering::Relaxed);
                }

                if send_mic.load(Ordering::Relaxed) {
                    buf.fill(0);
                }
                match encoder.encode_frame(&buf) {
                    Ok(pkts) => {
                        for pkt in &pkts {
                            if let Err(e) = send_t.send_media(pkt).await {
                                // Transient congestion (Blocked) — drop packet, keep going
                                send_drops.fetch_add(1, Ordering::Relaxed);
                                if send_drops.load(Ordering::Relaxed) <= 3 {
                                    tracing::warn!("send_media error (dropping packet): {e}");
                                }
                            }
                        }
                        send_fs.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => error!("encode: {e}"),
                }
            }
        });

        // Recv task (direct playout with auto codec switch)
        let recv_t = transport.clone();
        let recv_r = running.clone();
        let recv_spk = spk_muted.clone();
        let recv_fr = frames_received.clone();
        let recv_rx_codec = rx_codec.clone();
        tokio::spawn(async move {
            let initial_profile = resolve_quality(&quality).unwrap_or(QualityProfile::GOOD);
            // Phase 3b/3c: concrete AdaptiveDecoder (not Box<dyn>) so we
            // can call reconstruct_from_dred. Same reasoning as the
            // Android recv path above.
            let mut decoder = wzp_codec::AdaptiveDecoder::new(initial_profile)
                .expect("failed to create adaptive decoder");
            let mut current_profile = initial_profile;
            let mut current_codec = initial_profile.codec;
            let mut agc = wzp_codec::AutoGainControl::new();
            let mut pcm = vec![0i16; FRAME_SAMPLES_40MS]; // big enough for any codec
            let mut dred_recv = DredRecvState::new();

            loop {
                if !recv_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    recv_t.recv_media(),
                )
                .await
                {
                    Ok(Ok(Some(pkt))) => {
                        if !pkt.header.is_repair && pkt.header.codec_id != CodecId::ComfortNoise {
                            // Track RX codec
                            {
                                let mut rx = recv_rx_codec.lock().await;
                                let codec_name = format!("{:?}", pkt.header.codec_id);
                                if *rx != codec_name { *rx = codec_name; }
                            }
                            // Auto-switch decoder if incoming codec differs
                            if pkt.header.codec_id != current_codec {
                                let new_profile = match pkt.header.codec_id {
                                    CodecId::Opus24k => QualityProfile::GOOD,
                                    CodecId::Opus6k => QualityProfile::DEGRADED,
                                    CodecId::Opus32k => QualityProfile::STUDIO_32K,
                                    CodecId::Opus48k => QualityProfile::STUDIO_48K,
                                    CodecId::Opus64k => QualityProfile::STUDIO_64K,
                                    CodecId::Codec2_1200 => QualityProfile::CATASTROPHIC,
                                    CodecId::Codec2_3200 => QualityProfile {
                                        codec: CodecId::Codec2_3200,
                                        fec_ratio: 0.5, frame_duration_ms: 20, frames_per_block: 5,
                                    },
                                    other => QualityProfile { codec: other, ..QualityProfile::GOOD },
                                };
                                info!(from = ?current_codec, to = ?pkt.header.codec_id, "recv: switching decoder");
                                let _ = decoder.set_profile(new_profile);
                                current_profile = new_profile;
                                current_codec = pkt.header.codec_id;
                                dred_recv.reset_on_profile_switch();
                            }

                            // Phase 3b/3c: parse DRED + fill gaps before
                            // decoding the current packet. See the Android
                            // start() recv task for full commentary.
                            if pkt.header.codec_id.is_opus() {
                                dred_recv.ingest_opus(pkt.header.seq, &pkt.payload);
                                let frame_samples_now = (48_000
                                    * current_profile.frame_duration_ms as usize)
                                    / 1000;
                                let spk_muted_flag = recv_spk.load(Ordering::Relaxed);
                                dred_recv.fill_gap_to(
                                    &mut decoder,
                                    pkt.header.seq,
                                    frame_samples_now,
                                    &mut pcm,
                                    |samples| {
                                        agc.process_frame(samples);
                                        if !spk_muted_flag {
                                            playout_ring.write(samples);
                                        }
                                    },
                                );
                            }

                            if let Ok(n) = decoder.decode(&pkt.payload, &mut pcm) {
                                agc.process_frame(&mut pcm[..n]);
                                if !recv_spk.load(Ordering::Relaxed) {
                                    playout_ring.write(&pcm[..n]);
                                }
                            }
                        }
                        recv_fr.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        let msg = e.to_string();
                        if msg.contains("closed") || msg.contains("reset") {
                            error!("recv fatal: {e}");
                            break;
                        }
                    }
                    Err(_) => {}
                }
            }
        });

        // Signal task (presence)
        let sig_t = transport.clone();
        let sig_r = running.clone();
        let sig_p = participants.clone();
        let event_cb = Arc::new(event_cb);
        let sig_cb = event_cb.clone();
        tokio::spawn(async move {
            loop {
                if !sig_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sig_t.recv_signal(),
                )
                .await
                {
                    Ok(Ok(Some(wzp_proto::SignalMessage::RoomUpdate {
                        participants: parts,
                        ..
                    }))) => {
                        let mut seen = std::collections::HashSet::new();
                        let unique: Vec<ParticipantInfo> = parts
                            .into_iter()
                            .filter(|p| seen.insert((p.fingerprint.clone(), p.alias.clone())))
                            .map(|p| ParticipantInfo {
                                fingerprint: p.fingerprint,
                                alias: p.alias,
                                relay_label: p.relay_label,
                            })
                            .collect();
                        let count = unique.len();
                        *sig_p.lock().await = unique;
                        sig_cb("room-update", &format!("{count} participants"));
                    }
                    Ok(Ok(Some(_))) => {}
                    Ok(Ok(None)) => break,
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
        });

        Ok(Self {
            running,
            mic_muted,
            spk_muted,
            participants,
            frames_sent,
            frames_received,
            audio_level,
            transport,
            start_time: Instant::now(),
            fingerprint,
            tx_codec,
            rx_codec,
            _audio_handle: SyncWrapper(audio_handle),
        })
    }

    pub fn toggle_mic(&self) -> bool {
        let was = self.mic_muted.load(Ordering::Relaxed);
        self.mic_muted.store(!was, Ordering::Relaxed);
        !was
    }

    pub fn toggle_speaker(&self) -> bool {
        let was = self.spk_muted.load(Ordering::Relaxed);
        self.spk_muted.store(!was, Ordering::Relaxed);
        !was
    }

    pub async fn status(&self) -> EngineStatus {
        let participants = {
            let parts = self.participants.lock().await;
            parts
                .iter()
                .map(|p| ParticipantInfo {
                    fingerprint: p.fingerprint.clone(),
                    alias: p.alias.clone(),
                    relay_label: p.relay_label.clone(),
                })
                .collect()
        }; // lock dropped here
        EngineStatus {
            mic_muted: self.mic_muted.load(Ordering::Relaxed),
            spk_muted: self.spk_muted.load(Ordering::Relaxed),
            participants,
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            frames_received: self.frames_received.load(Ordering::Relaxed),
            audio_level: self.audio_level.load(Ordering::Relaxed),
            call_duration_secs: self.start_time.elapsed().as_secs_f64(),
            fingerprint: self.fingerprint.clone(),
            tx_codec: self.tx_codec.lock().await.clone(),
            rx_codec: self.rx_codec.lock().await.clone(),
        }
    }

    pub async fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        self.transport.close().await.ok();
        // On Android, the Oboe capture/playout streams live inside the
        // wzp-native cdylib as a process-global singleton. Explicitly stop
        // them here so the mic + speaker are released between calls, matching
        // the desktop behaviour where dropping _audio_handle tears down CPAL.
        #[cfg(target_os = "android")]
        {
            crate::wzp_native::audio_stop();
        }
    }
}
