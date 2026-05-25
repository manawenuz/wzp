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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;
use tauri::Emitter;
use tokio::sync::Mutex;
use tracing::{error, info};

// CPAL audio I/O is only available on desktop (wzp-client's `audio` feature).
#[cfg(not(target_os = "android"))]
use wzp_client::audio_io::{AudioCapture, AudioPlayback};

// Codec + handshake pipelines are platform-independent Rust (no CPAL
// dependency) so they're available from wzp-client on both desktop and
// Android (where wzp-client is pulled in with default-features=false).
use wzp_client::call::{CallConfig, CallEncoder};

use wzp_proto::traits::{AudioDecoder, QualityController};
use wzp_proto::{AdaptiveQualityController, CodecId, QualityProfile};

const FRAME_SAMPLES_40MS: usize = 1920;
const CAPTURE_POLL_MS: u64 = 5;
const RECV_TIMEOUT_MS: u64 = 100;
const SIGNAL_TIMEOUT_MS: u64 = 200;
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const CONNECT_TIMEOUT_SECS: u64 = 10;
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const HEARTBEAT_INTERVAL_SECS: u64 = 2;
const DRED_POLL_INTERVAL: u32 = 25;
/// Generate and attach a QualityReport every N frames (~1s at 20ms/frame).
const QUALITY_REPORT_INTERVAL: u32 = 50;

/// Profile index mapping for the AtomicU8 adaptive-quality bridge.
const PROFILE_NO_CHANGE: u8 = 0xFF;

/// Tracks Quinn's cumulative sent/lost counters so callers can compute
/// loss over a sliding window instead of since-connection-start. The
/// cumulative percentage is monotonically biased by handshake-era losses
/// and never recovers; the windowed percentage reflects current health.
#[derive(Default)]
struct LossWindow {
    prev_sent: u64,
    prev_lost: u64,
}

impl LossWindow {
    /// Returns the loss percentage observed since the last call. Falls back
    /// to the cumulative value while we don't yet have a delta to compare.
    fn observe(&mut self, sent_packets: u64, lost_packets: u64, cumulative_pct: f32) -> f32 {
        let d_sent = sent_packets.saturating_sub(self.prev_sent);
        let d_lost = lost_packets.saturating_sub(self.prev_lost);
        self.prev_sent = sent_packets;
        self.prev_lost = lost_packets;
        if d_sent >= 20 {
            (d_lost as f32 / d_sent as f32) * 100.0
        } else {
            cumulative_pct
        }
    }
}

fn profile_to_index(p: &QualityProfile) -> u8 {
    match p.codec {
        CodecId::Opus64k => 0,
        CodecId::Opus48k => 1,
        CodecId::Opus32k => 2,
        CodecId::Opus24k => 3,
        CodecId::Opus6k => 4,
        CodecId::Codec2_1200 => 5,
        _ => 3, // default to GOOD
    }
}

fn index_to_profile(idx: u8) -> Option<QualityProfile> {
    match idx {
        0 => Some(QualityProfile::STUDIO_64K),
        1 => Some(QualityProfile::STUDIO_48K),
        2 => Some(QualityProfile::STUDIO_32K),
        3 => Some(QualityProfile::GOOD),
        4 => Some(QualityProfile::DEGRADED),
        5 => Some(QualityProfile::CATASTROPHIC),
        _ => None,
    }
}

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
            ..QualityProfile::GOOD
        }),
        "studio-32k" => Some(QualityProfile::STUDIO_32K),
        "studio-48k" => Some(QualityProfile::STUDIO_48K),
        "studio-64k" => Some(QualityProfile::STUDIO_64K),
        _ => None, // "auto" or unknown
    }
}

/// Build a CallConfig from a quality string. Used by both Android and desktop send tasks.
fn build_call_config(quality: &str) -> CallConfig {
    let profile = resolve_quality(quality);
    match profile {
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
    }
}

/// Map a received codec ID to the corresponding QualityProfile.
/// Used by recv tasks when the peer switches codecs.
fn codec_to_profile(codec: CodecId) -> QualityProfile {
    match codec {
        CodecId::Opus24k => QualityProfile::GOOD,
        CodecId::Opus6k => QualityProfile::DEGRADED,
        CodecId::Opus32k => QualityProfile::STUDIO_32K,
        CodecId::Opus48k => QualityProfile::STUDIO_48K,
        CodecId::Opus64k => QualityProfile::STUDIO_64K,
        CodecId::Codec2_1200 => QualityProfile::CATASTROPHIC,
        CodecId::Codec2_3200 => QualityProfile {
            codec: CodecId::Codec2_3200,
            fec_ratio: 0.5,
            frame_duration_ms: 20,
            frames_per_block: 5,
            ..QualityProfile::GOOD
        },
        other => QualityProfile {
            codec: other,
            ..QualityProfile::GOOD
        },
    }
}

/// Signal handler task -- shared between Android and desktop.
/// Handles RoomUpdate (participant list), QualityDirective (relay-pushed
/// codec switch), and Hangup from the relay signal stream.
async fn run_signal_task(
    app: tauri::AppHandle,
    transport: Arc<dyn wzp_proto::MediaTransport>,
    running: Arc<AtomicBool>,
    pending_profile: Arc<AtomicU8>,
    participants: Arc<Mutex<Vec<ParticipantInfo>>>,
    event_cb: Arc<dyn Fn(&str, &str) + Send + Sync>,
) {
    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(SIGNAL_TIMEOUT_MS),
            transport.recv_signal(),
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
                let event_participants = unique
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "fingerprint": p.fingerprint,
                            "alias": p.alias,
                            "relay_label": p.relay_label,
                        })
                    })
                    .collect::<Vec<_>>();
                *participants.lock().await = unique;
                crate::emit_call_debug(
                    &app,
                    "media:room_update",
                    serde_json::json!({
                        "participants": event_participants.clone(),
                        "count": count,
                    }),
                );
                let _ = app.emit(
                    "call-event",
                    serde_json::json!({
                        "kind": "participants",
                        "participants": event_participants,
                    }),
                );
                event_cb("room-update", &format!("{count} participants"));
            }
            Ok(Ok(Some(wzp_proto::SignalMessage::QualityDirective {
                recommended_profile,
                reason,
                ..
            }))) => {
                let idx = profile_to_index(&recommended_profile);
                info!(
                    codec = ?recommended_profile.codec,
                    reason = reason.as_deref().unwrap_or(""),
                    "relay quality directive: switching profile"
                );
                pending_profile.store(idx, Ordering::Release);
            }
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => {}
        }
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
    transport: Arc<dyn wzp_proto::MediaTransport>,
    start_time: Instant,
    fingerprint: String,
    /// Keep audio handles alive for the duration of the call.
    /// Wrapped in SyncWrapper because AudioUnit isn't Sync.
    _audio_handle: SyncWrapper,
    /// Push raw YUV frames here to be encoded and sent to peers.
    /// `None` when video was not negotiated or the remote is audio-only.
    pub camera_tx: Option<tokio::sync::mpsc::Sender<wzp_video::encoder::VideoFrame>>,
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
    last_good_seq: Option<u32>,
    expected_seq: Option<u32>,
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
    fn ingest_opus(&mut self, seq: u32, payload: &[u8]) {
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
                let should_log = self.parses_with_data == 1 || self.parses_with_data % 100 == 0;
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
        current_seq: u32,
        frame_samples: usize,
        pcm_scratch: &mut [i16],
        mut emit: F,
    ) where
        F: FnMut(&mut [i16]),
    {
        const MAX_GAP_FRAMES: u32 = 16;
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
        // Phase 6: explicit flag for whether the agreed media path
        // is truly direct P2P (skip handshake) or relay-mediated
        // (must run handshake). Previously derived from
        // pre_connected_transport.is_some() which was WRONG: when
        // Phase 6 negotiated relay but delivered the relay transport
        // via pre_connected_transport, the engine skipped the
        // handshake → relay couldn't authenticate the participant
        // → silent call.
        is_direct_p2p: bool,
        // Phase 5.6: Tauri AppHandle for emitting call-debug
        // events from inside the send/recv tasks. Lets the
        // debug log pane show first-send/first-recv/heartbeat
        // events when the user has call debug logs enabled.
        app: tauri::AppHandle,
        active_quality: Arc<std::sync::Mutex<wzp_proto::QualityProfile>>,
        peer_max_quality: Arc<std::sync::Mutex<Option<wzp_proto::QualityProfile>>>,
        event_cb: F,
    ) -> Result<Self, anyhow::Error>
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        let call_t0 = std::time::Instant::now();
        info!(
            %relay, %room, %alias, %quality,
            has_reuse = reuse_endpoint.is_some(),
            has_pre_connected = pre_connected_transport.is_some(),
            is_direct_p2p,
            t_ms = 0u128,
            "CallEngine::start (android) invoked"
        );
        let _ = rustls::crypto::ring::default_provider().install_default();

        let relay_addr: SocketAddr = relay.parse()?;
        info!(%relay_addr, "resolved relay addr");

        let seed = crate::load_or_create_seed().map_err(|e| anyhow::anyhow!("identity: {e}"))?;
        let fp = seed.derive_identity().public_identity().fingerprint;
        let fingerprint = fp.to_string();
        info!(%fp, "identity loaded");

        // Transport source: either the pre-connected one from the
        // dual-path race or build a fresh one here.
        let transport = if let Some(t) = pre_connected_transport {
            info!(
                t_ms = call_t0.elapsed().as_millis(),
                is_direct_p2p, "first-join diag: using pre-connected transport"
            );
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
                let ep = wzp_transport::create_endpoint(bind_addr, None).map_err(|e| {
                    error!("create_endpoint failed: {e}");
                    e
                })?;
                info!(local_addr = ?ep.local_addr().ok(), "created new endpoint, dialing relay");
                ep
            };
            let client_config = wzp_transport::client_config();
            let conn = match tokio::time::timeout(
                std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS),
                wzp_transport::connect(&endpoint, relay_addr, &room, client_config),
            )
            .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    error!("connect failed: {e}");
                    return Err(e.into());
                }
                Err(_) => {
                    error!(
                        "connect TIMED OUT after {CONNECT_TIMEOUT_SECS}s — QUIC handshake never completed. Relay may be unreachable from this endpoint."
                    );
                    return Err(anyhow::anyhow!(
                        "QUIC connect timeout ({CONNECT_TIMEOUT_SECS}s)"
                    ));
                }
            };
            info!(
                t_ms = call_t0.elapsed().as_millis(),
                "first-join diag: QUIC connection established, performing handshake"
            );
            Arc::new(wzp_transport::QuinnTransport::new(conn))
        };

        // The media handshake (CallOffer/CallAnswer + crypto key
        // exchange) is a relay-specific protocol: the relay runs
        // `accept_handshake` on its side. On a direct P2P
        // connection the peer is a phone, not a relay — nobody on
        // the other end handles the handshake. So skip it when
        // is_direct_p2p. The QUIC transport already provides TLS
        // encryption, and both peers' identities were verified
        // through the signal channel (DirectCallOffer/Answer carry
        // identity_pub + ephemeral_pub + signature).
        let quinn_transport = transport.clone();
        let (_negotiated_video_codec, transport): (Option<wzp_proto::CodecId>, Arc<dyn wzp_proto::MediaTransport>) = if !is_direct_p2p {
            crate::emit_call_debug(
                &app,
                "connect:handshake_start",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "room": room,
                    "remote": transport.remote_address().to_string(),
                }),
            );
            let hs =
                match wzp_client::handshake::perform_handshake(&*transport, &seed.0, Some(&alias))
                    .await
                {
                    Ok(hs) => hs,
                    Err(e) => {
                        error!("perform_handshake failed: {e}");
                        crate::emit_call_debug(
                            &app,
                            "connect:handshake_failed",
                            serde_json::json!({
                                "t_ms": call_t0.elapsed().as_millis(),
                                "error": e.to_string(),
                            }),
                        );
                        return Err(e.into());
                    }
                };
            crate::emit_call_debug(
                &app,
                "connect:handshake_done",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "video_codec": hs.video_codec.map(|c| format!("{:?}", c)),
                }),
            );
            info!(
                t_ms = call_t0.elapsed().as_millis(),
                video_codec = ?hs.video_codec,
                "first-join diag: connected to relay, handshake complete"
            );
            // NOTE: see comment in CallEngine::start (~line 1585) — we intentionally
            // do NOT wrap with EncryptingTransport. The pairwise client↔relay session
            // key can't be used end-to-end without MLS or relay re-encryption.
            drop(hs.session);
            (hs.video_codec, transport)
        } else {
            info!(
                t_ms = call_t0.elapsed().as_millis(),
                "first-join diag: direct P2P — skipping relay handshake (QUIC TLS is the encryption layer)"
            );
            (None, transport)
        };
        crate::emit_call_debug(
            &app,
            "video:negotiated",
            serde_json::json!({
                "t_ms": call_t0.elapsed().as_millis(),
                "codec": _negotiated_video_codec.map(|c| format!("{:?}", c)),
                "enabled": _negotiated_video_codec.is_some(),
                "direct_p2p": is_direct_p2p,
            }),
        );
        // Do not emit the legacy "connected" call-event here. The frontend
        // ignores it and enters voice only after the command resolves; on
        // Android this synchronous emit was the only operation between
        // handshake_done and audio preflight in failing traces.
        crate::emit_call_debug(
            &app,
            "connect:connected_event_skipped",
            serde_json::json!({ "t_ms": call_t0.elapsed().as_millis() }),
        );

        // Oboe audio via the wzp-native cdylib that was dlopen'd at
        // startup. `wzp_native::audio_start()` brings up the capture +
        // playout streams; send/recv tasks below pull/push PCM through
        // the extern "C" bridge rings.
        crate::emit_call_debug(
            &app,
            "connect:android_audio_preflight_start",
            serde_json::json!({ "t_ms": call_t0.elapsed().as_millis() }),
        );
        let native_loaded = crate::wzp_native::is_loaded();
        crate::emit_call_debug(
            &app,
            "connect:android_audio_preflight",
            serde_json::json!({
                "t_ms": call_t0.elapsed().as_millis(),
                "wzp_native_loaded": native_loaded,
            }),
        );
        if !native_loaded {
            return Err(anyhow::anyhow!(
                "wzp-native not loaded — dlopen failed at startup"
            ));
        }

        // Fix D (task #37): explicit stop+start cycle on EVERY call
        // start — not just rejoin. Empirically, the first call after
        // app launch on Nothing Phone has the Oboe playout callback
        // fire once (cb#0) and then stop draining the ring, causing
        // written_samples to freeze at 7679 (ring capacity minus
        // one burst). Rejoin (second call) always works because
        // audio_stop tears down the streams and audio_start rebuilds
        // them in a state that the audio driver accepts. By always
        // running stop first (no-op on cold start when not yet
        // started), we get the same "fresh rebuild" behavior on
        // every call.
        crate::emit_call_debug(
            &app,
            "connect:audio_stop_start",
            serde_json::json!({ "t_ms": call_t0.elapsed().as_millis() }),
        );
        crate::wzp_native::audio_stop();
        crate::emit_call_debug(
            &app,
            "connect:audio_stop_done",
            serde_json::json!({ "t_ms": call_t0.elapsed().as_millis() }),
        );
        // Brief pause to let Android's audio routing + AudioManager
        // settle after the stop. 50ms is enough for the driver to
        // release the audio session; shorter risks the new start
        // hitting a "device busy" on some HALs.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Set MODE_IN_COMMUNICATION right before audio starts — NOT at
        // app launch. Setting it early hijacks system audio routing
        // (music drops from BT A2DP to earpiece, etc.).
        #[cfg(target_os = "android")]
        {
            crate::emit_call_debug(
                &app,
                "connect:audio_mode_start",
                serde_json::json!({ "t_ms": call_t0.elapsed().as_millis() }),
            );
            match crate::android_audio::set_audio_mode_communication_on_main(app.clone()).await {
                Ok(()) => crate::emit_call_debug(
                    &app,
                    "connect:audio_mode_done",
                    serde_json::json!({ "t_ms": call_t0.elapsed().as_millis() }),
                ),
                Err(e) => {
                    tracing::warn!("set_audio_mode_communication failed: {e}");
                    crate::emit_call_debug(
                        &app,
                        "connect:audio_mode_failed",
                        serde_json::json!({
                            "t_ms": call_t0.elapsed().as_millis(),
                            "error": e,
                        }),
                    );
                }
            }
        }

        // Run audio_start on a blocking thread — wzp_oboe_start is a
        // sync FFI call that can stall waiting for the Android audio
        // HAL. Calling it directly blocks the tokio worker thread,
        // which freezes all async tasks including our own timeouts.
        let t_pre_audio = call_t0.elapsed().as_millis();
        crate::emit_call_debug(
            &app,
            "connect:audio_start_start",
            serde_json::json!({ "t_ms": t_pre_audio }),
        );
        let audio_start_task = tokio::task::spawn_blocking(crate::wzp_native::audio_start);
        let audio_start_result =
            match tokio::time::timeout(std::time::Duration::from_secs(8), audio_start_task).await {
                Ok(join_result) => join_result.map_err(|e| {
                    crate::emit_call_debug(
                        &app,
                        "connect:audio_start_panic",
                        serde_json::json!({
                            "t_ms": call_t0.elapsed().as_millis(),
                            "error": e.to_string(),
                        }),
                    );
                    anyhow::anyhow!("audio_start task panic: {e}")
                })?,
                Err(_) => {
                    crate::emit_call_debug(
                        &app,
                        "connect:audio_start_timeout",
                        serde_json::json!({
                            "t_ms": call_t0.elapsed().as_millis(),
                            "timeout_ms": 8000,
                        }),
                    );
                    return Err(anyhow::anyhow!("wzp_native_audio_start timed out after 8s"));
                }
            };
        if let Err(code) = audio_start_result {
            crate::emit_call_debug(
                &app,
                "connect:audio_start_failed",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "code": code,
                }),
            );
            return Err(anyhow::anyhow!(
                "wzp_native_audio_start failed: code {code}"
            ));
        }

        // Fix C (task #36): prime the playout ring with 20ms of
        // silence immediately after audio_start so the Oboe playout
        // callback has data to drain on its FIRST invocation. On
        // devices where the callback only fires when the ring is
        // non-empty (or where an empty-ring callback causes the
        // stream to self-pause), this ensures the callback keeps
        // running until real decoded audio arrives.
        {
            let silence = vec![0i16; 960]; // 20ms @ 48kHz mono
            let _ = crate::wzp_native::audio_write_playout(&silence);
        }

        let t_audio_start_done = call_t0.elapsed().as_millis();
        info!(
            t_ms = t_audio_start_done,
            audio_start_ms = t_audio_start_done.saturating_sub(t_pre_audio),
            "first-join diag: wzp-native audio started (with stop+prime cycle)"
        );
        crate::emit_call_debug(
            &app,
            "connect:audio_start_done",
            serde_json::json!({
                "t_ms": t_audio_start_done,
                "audio_start_ms": t_audio_start_done.saturating_sub(t_pre_audio),
            }),
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

        // Adaptive quality: shared pending-profile bridge between recv → send.
        let pending_profile = Arc::new(AtomicU8::new(PROFILE_NO_CHANGE));
        let auto_profile = resolve_quality(&quality).is_none();

        // Send task — drain Oboe capture ring, Opus-encode, push to transport.
        let send_t = transport.clone();
        let quinn_t = quinn_transport.clone();
        let send_r = running.clone();
        let send_mic = mic_muted.clone();
        let send_fs = frames_sent.clone();
        let send_level = audio_level.clone();
        let send_drops = Arc::new(AtomicU64::new(0));
        let send_last_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let send_quality = quality.clone();
        let send_tx_codec = tx_codec.clone();
        let send_t0 = call_t0;
        let send_app = app.clone();
        let send_pending_profile = pending_profile.clone();
        let send_active_quality = active_quality.clone();
        let send_peer_max = peer_max_quality.clone();
        tokio::spawn(async move {
            let config = build_call_config(&send_quality);
            let mut frame_samples = (config.profile.frame_duration_ms as usize) * 48;
            info!(codec = ?config.profile.codec, frame_samples, t_ms = send_t0.elapsed().as_millis(), "first-join diag: send task spawned (android/oboe)");
            *send_tx_codec.lock().await = format!("{:?}", config.profile.codec);
            let mut encoder = CallEncoder::new(&config);
            encoder.set_aec_enabled(false);
            // Sized for max frame (40ms = 1920 samples) so profile
            // switches between 20ms ↔ 40ms codecs don't need realloc.
            let mut buf = vec![0i16; 1920];

            // Continuous DRED tuning: poll quinn path stats every 25
            // frames (~500 ms at 20 ms/frame) and adjust DRED duration +
            // expected-loss hint based on real-time network conditions.
            let mut dred_tuner = wzp_proto::DredTuner::new(config.profile.codec);
            let mut frames_since_dred_poll: u32 = 0;
            let mut frames_since_quality_report: u32 = 0;
            let mut send_loss_window = LossWindow::default();

            let mut heartbeat = std::time::Instant::now();
            let mut last_rms: u32;
            let mut last_pkt_bytes: usize = 0;
            let mut short_reads: u64 = 0;
            // First-join diagnostic: latch the wall-clock offset of the
            // first full-frame capture read and the first non-zero RMS
            // reading separately. The gap between them tells us how long
            // Oboe input took to actually start delivering real samples
            // after returning a "started" status from audio_start.
            let mut first_full_read_logged = false;
            let mut first_nonzero_rms_logged = false;
            let mut last_applied_profile: Option<QualityProfile> = None;

            loop {
                // Quality upgrade flow: apply active_quality / peer_max_quality.
                let effective_profile = {
                    let active = send_active_quality.lock().unwrap().clone();
                    let peer_cap = send_peer_max.lock().unwrap().clone();
                    match peer_cap {
                        Some(cap) if cap.codec.bitrate_bps() < active.codec.bitrate_bps() => cap,
                        _ => active,
                    }
                };
                if Some(&effective_profile) != last_applied_profile.as_ref() {
                    let new_fs = (effective_profile.frame_duration_ms as usize) * 48;
                    info!(to = ?effective_profile.codec, frame_samples = new_fs, "quality: switching encoder profile (android)");
                    if encoder.set_profile(effective_profile).is_ok() {
                        frame_samples = new_fs;
                        dred_tuner.set_codec(effective_profile.codec);
                        *send_tx_codec.lock().await = format!("{:?}", effective_profile.codec);
                        last_applied_profile = Some(effective_profile);
                    }
                }
                if !send_r.load(Ordering::Relaxed) {
                    break;
                }
                // Check ring has enough samples before reading to avoid
                // partial reads that consume samples and then get
                // overwritten on the next attempt (caused 40ms codecs
                // like Opus6k to produce ~11 frames/s instead of 25).
                if crate::wzp_native::audio_capture_available() < frame_samples {
                    short_reads += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(CAPTURE_POLL_MS)).await;
                    continue;
                }
                let read = crate::wzp_native::audio_read_capture(&mut buf[..frame_samples]);
                if read < frame_samples {
                    // Shouldn't happen after available() check, but guard anyway.
                    short_reads += 1;
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
                let sum_sq: f64 = buf[..frame_samples]
                    .iter()
                    .map(|&s| (s as f64) * (s as f64))
                    .sum();
                let rms = (sum_sq / frame_samples as f64).sqrt() as u32;
                send_level.store(rms, Ordering::Relaxed);
                last_rms = rms;
                if !first_nonzero_rms_logged && rms > 0 {
                    info!(
                        t_ms = send_t0.elapsed().as_millis(),
                        rms, "first-join diag: send first non-zero capture RMS"
                    );
                    first_nonzero_rms_logged = true;
                }

                if send_mic.load(Ordering::Relaxed) {
                    buf[..frame_samples].fill(0);
                }
                match encoder.encode_frame(&buf[..frame_samples]) {
                    Ok(pkts) => {
                        for pkt in &pkts {
                            last_pkt_bytes = pkt.payload.len();
                            if let Err(e) = send_t.send_media(pkt).await {
                                send_drops.fetch_add(1, Ordering::Relaxed);
                                let count = send_drops.load(Ordering::Relaxed);
                                if count <= 3 {
                                    tracing::warn!("send_media error (dropping packet): {e}");
                                }
                                // Latch last error for heartbeat
                                if count == 1 {
                                    *send_last_err.lock().await = Some(format!("{e}"));
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

                // Adaptive quality: check if recv task recommended a profile switch.
                if auto_profile {
                    let p = send_pending_profile.swap(PROFILE_NO_CHANGE, Ordering::Acquire);
                    if p != PROFILE_NO_CHANGE {
                        if let Some(new_profile) = index_to_profile(p) {
                            let new_fs = (new_profile.frame_duration_ms as usize) * 48;
                            info!(to = ?new_profile.codec, frame_samples = new_fs, "auto: switching encoder profile (android)");
                            if encoder.set_profile(new_profile).is_ok() {
                                frame_samples = new_fs;
                                dred_tuner.set_codec(new_profile.codec);
                                *send_tx_codec.lock().await = format!("{:?}", new_profile.codec);
                            }
                        }
                    }
                }

                // DRED tuner: poll quinn path stats periodically and
                // adjust encoder DRED duration + expected-loss hint.
                frames_since_dred_poll += 1;
                if frames_since_dred_poll >= DRED_POLL_INTERVAL {
                    frames_since_dred_poll = 0;
                    let snap = quinn_t.quinn_path_stats();
                    let pq = send_t.path_quality();
                    let win_loss = send_loss_window.observe(
                        snap.sent_packets,
                        snap.lost_packets,
                        snap.loss_pct,
                    );
                    if let Some(tuning) =
                        dred_tuner.update(win_loss, snap.rtt_ms, pq.jitter_ms)
                    {
                        encoder.apply_dred_tuning(tuning);
                        if wzp_codec::dred_verbose_logs() {
                            info!(
                                dred_frames = tuning.dred_frames,
                                dred_ms = tuning.dred_frames as u32 * 10,
                                expected_loss = tuning.expected_loss_pct,
                                quinn_loss = format!("{:.1}", win_loss),
                                quinn_rtt = snap.rtt_ms,
                                jitter = pq.jitter_ms,
                                spike = dred_tuner.spike_boost_active(),
                                "DRED tuner adjusted encoder"
                            );
                        }
                    }
                }

                // Quality report: generate from quinn stats and attach to next packet.
                // The peer's recv task (or relay) uses this for adaptive quality.
                frames_since_quality_report += 1;
                if frames_since_quality_report >= QUALITY_REPORT_INTERVAL {
                    frames_since_quality_report = 0;
                    let snap = quinn_t.quinn_path_stats();
                    let pq = send_t.path_quality();
                    let win_loss = send_loss_window.observe(
                        snap.sent_packets,
                        snap.lost_packets,
                        snap.loss_pct,
                    );
                    let report = wzp_proto::QualityReport::from_path_stats(
                        win_loss,
                        snap.rtt_ms,
                        pq.jitter_ms,
                    );
                    encoder.set_pending_quality_report(report);
                }

                // Heartbeat every 2s with capture+encode+send state
                if heartbeat.elapsed() >= std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS) {
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
                    let err_str = send_last_err.lock().await.clone();
                    crate::emit_call_debug(
                        &send_app,
                        "media:send_heartbeat",
                        serde_json::json!({
                            "frames_sent": fs,
                            "last_rms": last_rms,
                            "last_pkt_bytes": last_pkt_bytes,
                            "short_reads": short_reads,
                            "drops": drops,
                            "last_send_err": err_str,
                        }),
                    );
                    heartbeat = std::time::Instant::now();
                }
            }
        });

        // Recv task — decode incoming packets, push PCM into Oboe playout.
        let recv_t = transport.clone();
        let quinn_t = quinn_transport.clone();
        let recv_r = running.clone();
        let recv_spk = spk_muted.clone();
        let recv_fr = frames_received.clone();
        let recv_rx_codec = rx_codec.clone();
        let recv_t0 = call_t0;
        let recv_app = app.clone();
        let pending_profile_recv = pending_profile.clone();
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
            let mut quality_ctrl = AdaptiveQualityController::new();
            let mut recv_quality_counter: u32 = 0;
            let mut recv_loss_window = LossWindow::default();
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
            let recorder_path = crate::APP_DATA_DIR.get().map(|p| p.join("decoded.pcm"));
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
            // Phase 5.6: media health watchdog — track consecutive
            // heartbeat ticks where recv_fr hasn't advanced. If
            // media doesn't arrive for 3 consecutive heartbeats
            // (6s), emit a user-facing "media-degraded" call-event
            // so the UI can show a warning like "No audio — try
            // reconnecting?". Covers the case where P2P direct
            // established but the underlying network path died
            // (e.g., phone switched from WiFi to LTE mid-call).
            let mut last_recv_fr_for_watchdog: u64 = 0;
            let mut no_recv_ticks: u32 = 0;
            let mut media_degraded_emitted = false;
            // Video pipeline state — mirror of the desktop recv task.
            let mut video_reassembler = wzp_video::transport::VideoReassembler::new();
            let mut video_decoder: Option<Box<dyn wzp_video::decoder::VideoDecoder>> = None;
            let mut video_decoder_codec: Option<wzp_proto::CodecId> = None;
            let mut video_first_recv_logged = false;
            let mut video_first_reassembled_logged = false;
            let mut video_reassembled_samples: u64 = 0;
            let mut video_first_decoded_logged = false;
            let mut video_decoder_buffering_count: u64 = 0;

            loop {
                if !recv_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(RECV_TIMEOUT_MS),
                    recv_t.recv_media(),
                )
                .await
                {
                    Ok(Ok(Some(pkt))) => {
                        // Route Video packets through the reassembler/decoder and emit
                        // a JPEG-encoded frame to the WebView. Done before audio path so
                        // we don't drop into the audio decoder branches.
                        if pkt.header.media_type == wzp_proto::MediaType::Video {
                            if !video_first_recv_logged {
                                video_first_recv_logged = true;
                                crate::emit_call_debug(
                                    &recv_app,
                                    "video:first_recv",
                                    serde_json::json!({
                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                        "codec": format!("{:?}", pkt.header.codec_id),
                                        "payload_bytes": pkt.payload.len(),
                                        "stream_id": pkt.header.stream_id,
                                    }),
                                );
                            }
                            if let Some((codec_id, is_kf, frame)) =
                                video_reassembler.push(&pkt)
                            {
                                video_reassembled_samples += 1;
                                if !video_first_reassembled_logged {
                                    video_first_reassembled_logged = true;
                                    crate::emit_call_debug(
                                        &recv_app,
                                        "video:first_reassembled",
                                        serde_json::json!({
                                            "t_ms": recv_t0.elapsed().as_millis() as u64,
                                            "codec": format!("{:?}", codec_id),
                                            "is_keyframe": is_kf,
                                            "frame_bytes": frame.len(),
                                            "platform": "android",
                                        }),
                                    );
                                }
                                if video_reassembled_samples <= 5 {
                                    crate::emit_call_debug(
                                        &recv_app,
                                        "video:reassembled_frame",
                                        serde_json::json!({
                                            "t_ms": recv_t0.elapsed().as_millis() as u64,
                                            "codec": format!("{:?}", codec_id),
                                            "is_keyframe": is_kf,
                                            "frame_bytes": frame.len(),
                                            "frame_no": video_reassembled_samples,
                                            "platform": "android",
                                        }),
                                    );
                                }
                                if video_decoder_codec != Some(codec_id) {
                                    crate::emit_call_debug(
                                        &recv_app,
                                        "video:decoder_init_start",
                                        serde_json::json!({
                                            "t_ms": recv_t0.elapsed().as_millis() as u64,
                                            "codec": format!("{:?}", codec_id),
                                            "width": 1280,
                                            "height": 720,
                                            "platform": "android",
                                        }),
                                    );
                                    match wzp_video::factory::create_video_decoder(codec_id, 1280, 720) {
                                        Ok(d) => {
                                            info!(codec = ?codec_id, "video decoder created (android)");
                                            crate::emit_call_debug(
                                                &recv_app,
                                                "video:decoder_started",
                                                serde_json::json!({
                                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                    "codec": format!("{:?}", codec_id),
                                                    "platform": "android",
                                                }),
                                            );
                                            video_decoder = Some(d);
                                            video_decoder_codec = Some(codec_id);
                                        }
                                        Err(e) => {
                                            error!("video decoder init failed: {e}");
                                            crate::emit_call_debug(
                                                &recv_app,
                                                "video:decoder_init_failed",
                                                serde_json::json!({
                                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                    "codec": format!("{:?}", codec_id),
                                                    "error": e.to_string(),
                                                    "platform": "android",
                                                }),
                                            );
                                        }
                                    }
                                }
                                if let Some(ref mut dec) = video_decoder {
                                    match dec.decode(&frame) {
                                        Ok(Some(yuv_frame)) => {
                                            let jpeg_b64 = crate::i420_to_jpeg_b64(
                                                &yuv_frame.data,
                                                yuv_frame.width,
                                                yuv_frame.height,
                                            );
                                            let jpeg_ok = jpeg_b64.is_some();
                                            if !video_first_decoded_logged {
                                                video_first_decoded_logged = true;
                                                crate::emit_call_debug(
                                                    &recv_app,
                                                    "video:first_decoded_frame",
                                                    serde_json::json!({
                                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                        "codec": format!("{:?}", codec_id),
                                                        "width": yuv_frame.width,
                                                        "height": yuv_frame.height,
                                                        "yuv_bytes": yuv_frame.data.len(),
                                                        "jpeg_ok": jpeg_ok,
                                                        "platform": "android",
                                                    }),
                                                );
                                            }
                                            if !jpeg_ok {
                                                crate::emit_call_debug(
                                                    &recv_app,
                                                    "video:jpeg_encode_failed",
                                                    serde_json::json!({
                                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                        "codec": format!("{:?}", codec_id),
                                                        "width": yuv_frame.width,
                                                        "height": yuv_frame.height,
                                                        "yuv_bytes": yuv_frame.data.len(),
                                                        "platform": "android",
                                                    }),
                                                );
                                            }
                                            let _ = recv_app.emit(
                                                "video:frame",
                                                serde_json::json!({
                                                    "is_keyframe": is_kf,
                                                    "width": yuv_frame.width,
                                                    "height": yuv_frame.height,
                                                    "jpeg_b64": jpeg_b64,
                                                    "codec": format!("{:?}", codec_id),
                                                }),
                                            );
                                        }
                                        Ok(None) => {
                                            video_decoder_buffering_count += 1;
                                            if video_decoder_buffering_count == 1
                                                || video_decoder_buffering_count % 30 == 0
                                            {
                                                crate::emit_call_debug(
                                                    &recv_app,
                                                    "video:decoder_buffering",
                                                    serde_json::json!({
                                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                        "codec": format!("{:?}", codec_id),
                                                        "buffering": video_decoder_buffering_count,
                                                        "platform": "android",
                                                    }),
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            error!("video decode error: {e}");
                                            crate::emit_call_debug(
                                                &recv_app,
                                                "video:decode_error",
                                                serde_json::json!({
                                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                    "codec": format!("{:?}", codec_id),
                                                    "error": e.to_string(),
                                                    "platform": "android",
                                                }),
                                            );
                                        }
                                    }
                                }
                                video_reassembler.evict_stale(pkt.header.timestamp, 5_000);
                            }
                            continue; // handled — skip audio path
                        }

                        if !first_packet_logged {
                            info!(
                                t_ms = recv_t0.elapsed().as_millis(),
                                codec_id = ?pkt.header.codec_id,
                                payload_bytes = pkt.payload.len(),
                                is_repair = pkt.header.is_repair(),
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
                                    "is_repair": pkt.header.is_repair(),
                                }),
                            );
                        }
                        if !pkt.header.is_repair() && pkt.header.codec_id != CodecId::ComfortNoise {
                            {
                                let mut rx = recv_rx_codec.lock().await;
                                let codec_name = format!("{:?}", pkt.header.codec_id);
                                if *rx != codec_name {
                                    *rx = codec_name;
                                }
                            }
                            if pkt.header.codec_id != current_codec {
                                let new_profile = codec_to_profile(pkt.header.codec_id);
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
                                let frame_samples_now =
                                    (48_000 * current_profile.frame_duration_ms as usize) / 1000;
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

                            // Adaptive quality: ingest quality reports from peer
                            if let Some(ref qr) = pkt.quality_report {
                                if let Some(new_profile) = quality_ctrl.observe(qr) {
                                    let idx = profile_to_index(&new_profile);
                                    info!(to = ?new_profile.codec, "auto: quality adapter recommends switch");
                                    pending_profile_recv.store(idx, Ordering::Release);
                                }
                            }

                            // P2P self-observation: if no quality reports from peer,
                            // generate local observations from our own QUIC path stats.
                            // This ensures adaptive quality works even on P2P calls
                            // where the peer hasn't been updated to send reports yet.
                            recv_quality_counter += 1;
                            if recv_quality_counter >= QUALITY_REPORT_INTERVAL {
                                recv_quality_counter = 0;
                                let snap = quinn_t.quinn_path_stats();
                                let pq = recv_t.path_quality();
                                let win_loss = recv_loss_window.observe(
                                    snap.sent_packets,
                                    snap.lost_packets,
                                    snap.loss_pct,
                                );
                                let local_report = wzp_proto::QualityReport::from_path_stats(
                                    win_loss,
                                    snap.rtt_ms,
                                    pq.jitter_ms,
                                );
                                if auto_profile {
                                    if let Some(new_profile) = quality_ctrl.observe(&local_report) {
                                        let idx = profile_to_index(&new_profile);
                                        info!(to = ?new_profile.codec, "auto: local quality observation recommends switch");
                                        pending_profile_recv.store(idx, Ordering::Release);
                                    }
                                }
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
                                        let (mut lo, mut hi, mut sumsq) =
                                            (i16::MAX, i16::MIN, 0i64);
                                        for &s in slice.iter() {
                                            if s < lo {
                                                lo = s;
                                            }
                                            if s > hi {
                                                hi = s;
                                            }
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
                                                info!(
                                                    recorder_bytes,
                                                    "decoded-pcm recorder: stopped after limit"
                                                );
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
                                            tracing::warn!(
                                                n,
                                                w,
                                                "recv: partial playout write (ring nearly full)"
                                            );
                                        }
                                    } else if decoded_frames <= 3 || decoded_frames % 100 == 0 {
                                        // User clicked spk-mute — log it so we don't chase ghost bugs
                                        tracing::info!(
                                            decoded_frames,
                                            "recv: spk_muted=true, skipping playout write"
                                        );
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
                if heartbeat.elapsed() >= std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS) {
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

                    // Media health watchdog: if recv_fr hasn't
                    // advanced in 3 consecutive heartbeats (6s) and
                    // we've been "connected" for at least 4s (give
                    // the first few frames time to arrive), emit a
                    // user-facing "media-degraded" event so the UI
                    // can show "No audio — connection may be lost".
                    if fr == last_recv_fr_for_watchdog {
                        no_recv_ticks += 1;
                    } else {
                        no_recv_ticks = 0;
                        if media_degraded_emitted {
                            // Was degraded but recovered — clear
                            // the banner.
                            media_degraded_emitted = false;
                            let _ = recv_app.emit(
                                "call-event",
                                serde_json::json!({
                                    "kind": "media-recovered",
                                }),
                            );
                            crate::emit_call_debug(
                                &recv_app,
                                "media:recovered",
                                serde_json::json!({}),
                            );
                        }
                    }
                    last_recv_fr_for_watchdog = fr;

                    if no_recv_ticks >= 3 && !media_degraded_emitted {
                        media_degraded_emitted = true;
                        tracing::warn!(
                            recv_fr = fr,
                            no_recv_ticks,
                            "media watchdog: no inbound packets for 6s"
                        );
                        let _ = recv_app.emit(
                            "call-event",
                            serde_json::json!({
                                "kind": "media-degraded",
                            }),
                        );
                        crate::emit_call_debug(
                            &recv_app,
                            "media:no_recv_timeout",
                            serde_json::json!({
                                "recv_fr": fr,
                                "no_recv_ticks": no_recv_ticks,
                            }),
                        );
                    }

                    heartbeat = std::time::Instant::now();
                }
            }
        });

        // Signal task (presence + quality directives).
        let event_cb = Arc::new(event_cb);
        tokio::spawn(run_signal_task(
            app.clone(),
            transport.clone(),
            running.clone(),
            pending_profile.clone(),
            participants.clone(),
            event_cb.clone(),
        ));

        // Video send task (Android) — mirror of the desktop branch. Only
        // spawns when the relay handshake negotiated a video codec; on
        // direct P2P video is currently disabled.
        let camera_tx = if let Some(vid_codec) = _negotiated_video_codec {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<wzp_video::encoder::VideoFrame>(4);
            let vid_transport = transport.clone();
            let vid_running = running.clone();
            let vid_t0 = call_t0;
            let vid_app = app.clone();
            crate::emit_call_debug(
                &app,
                "video:sender_channel_ready",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "codec": format!("{:?}", vid_codec),
                    "queue_depth": 4,
                    "platform": "android",
                }),
            );
            tokio::spawn(async move {
                crate::emit_call_debug(
                    &vid_app,
                    "video:encoder_init_start",
                    serde_json::json!({
                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                        "codec": format!("{:?}", vid_codec),
                        "width": 1280,
                        "height": 720,
                        "bitrate_bps": 1_500_000,
                        "platform": "android",
                    }),
                );
                let mut encoder = match wzp_video::factory::create_video_encoder(
                    vid_codec, 1280, 720, 1_500_000,
                ) {
                    Ok(e) => {
                        crate::emit_call_debug(
                            &vid_app,
                            "video:encoder_started",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "platform": "android",
                            }),
                        );
                        e
                    }
                    Err(e) => {
                        error!("video encoder init failed (android): {e}");
                        crate::emit_call_debug(
                            &vid_app,
                            "video:encoder_init_failed",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "platform": "android",
                                "error": e.to_string(),
                            }),
                        );
                        return;
                    }
                };
                let mut seq: u32 = 0;
                let mut frames_since_keyframe: u32 = 0;
                let mut first_send_logged = false;
                let mut first_camera_frame_logged = false;
                let mut camera_frames: u64 = 0;
                let mut empty_encodes: u64 = 0;
                let mut encoded_frame_samples: u64 = 0;
                let mut wait_ticks: u64 = 0;
                encoder.request_keyframe();
                crate::emit_call_debug(
                    &vid_app,
                    "video:keyframe_requested",
                    serde_json::json!({
                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                        "codec": format!("{:?}", vid_codec),
                        "reason": "initial",
                        "platform": "android",
                    }),
                );
                info!(codec = ?vid_codec, "video send task started (android)");
                while vid_running.load(Ordering::Relaxed) {
                    let frame = match tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        rx.recv(),
                    )
                    .await
                    {
                        Ok(Some(f)) => {
                            wait_ticks = 0;
                            camera_frames += 1;
                            if !first_camera_frame_logged {
                                first_camera_frame_logged = true;
                                crate::emit_call_debug(
                                    &vid_app,
                                    "video:first_camera_frame",
                                    serde_json::json!({
                                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                                        "codec": format!("{:?}", vid_codec),
                                        "width": f.width,
                                        "height": f.height,
                                        "data_bytes": f.data.len(),
                                        "platform": "android",
                                    }),
                                );
                            }
                            f
                        }
                        Ok(None) => break,
                        Err(_) => {
                            wait_ticks += 1;
                            if wait_ticks == 10 || wait_ticks % 50 == 0 {
                                crate::emit_call_debug(
                                    &vid_app,
                                    "video:waiting_for_camera_frames",
                                    serde_json::json!({
                                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                                        "wait_ms": wait_ticks * 200,
                                        "codec": format!("{:?}", vid_codec),
                                        "platform": "android",
                                    }),
                                );
                            }
                            continue;
                        }
                    };

                    if frames_since_keyframe >= 150 {
                        encoder.request_keyframe();
                        crate::emit_call_debug(
                            &vid_app,
                            "video:keyframe_requested",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "reason": "periodic",
                                "camera_frames": camera_frames,
                                "platform": "android",
                            }),
                        );
                        frames_since_keyframe = 0;
                    }

                    let encoded = match encoder.encode(&frame) {
                        Ok(b) => b,
                        Err(e) => {
                            error!("video encode error (android): {e}");
                            crate::emit_call_debug(
                                &vid_app,
                                "video:encode_error",
                                serde_json::json!({
                                    "t_ms": vid_t0.elapsed().as_millis() as u64,
                                    "codec": format!("{:?}", vid_codec),
                                    "camera_frames": camera_frames,
                                    "error": e.to_string(),
                                    "platform": "android",
                                }),
                            );
                            continue;
                        }
                    };
                    if encoded.is_empty() {
                        empty_encodes += 1;
                        if empty_encodes == 1 || empty_encodes % 30 == 0 {
                            crate::emit_call_debug(
                                &vid_app,
                                "video:encode_empty",
                                serde_json::json!({
                                    "t_ms": vid_t0.elapsed().as_millis() as u64,
                                    "codec": format!("{:?}", vid_codec),
                                    "camera_frames": camera_frames,
                                    "empty_encodes": empty_encodes,
                                    "platform": "android",
                                }),
                            );
                        }
                        continue;
                    }

                    let is_keyframe = encoder.is_keyframe(&encoded);
                    let ts_ms = vid_t0.elapsed().as_millis() as u32;
                    let pkts = wzp_video::transport::packetize_video_frame(
                        &encoded, vid_codec, is_keyframe, &mut seq, ts_ms,
                    );
                    if encoded_frame_samples < 5 {
                        encoded_frame_samples += 1;
                        let packet_payload_bytes: usize =
                            pkts.iter().map(|pkt| pkt.payload.len()).sum();
                        crate::emit_call_debug(
                            &vid_app,
                            "video:encoded_frame",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "camera_frames": camera_frames,
                                "encoded_bytes": encoded.len(),
                                "packet_payload_bytes": packet_payload_bytes,
                                "packets": pkts.len(),
                                "is_keyframe": is_keyframe,
                                "sample_no": encoded_frame_samples,
                                "platform": "android",
                            }),
                        );
                    }
                    if !first_send_logged && !pkts.is_empty() {
                        first_send_logged = true;
                        crate::emit_call_debug(
                            &vid_app,
                            "video:first_send",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "packets": pkts.len(),
                                "first_pkt_bytes": pkts[0].payload.len(),
                                "last_pkt_bytes": pkts.last().map(|pkt| pkt.payload.len()).unwrap_or(0),
                                "encoded_bytes": encoded.len(),
                                "stream_id": pkts[0].header.stream_id,
                                "is_keyframe": is_keyframe,
                            }),
                        );
                    }
                    for pkt in &pkts {
                        if let Err(e) = vid_transport.send_media(pkt).await {
                            crate::emit_call_debug(
                                &vid_app,
                                "video:send_error",
                                serde_json::json!({"error": e.to_string()}),
                            );
                            break;
                        }
                    }
                    frames_since_keyframe += 1;
                }
                crate::emit_call_debug(
                    &vid_app,
                    "video:sender_exit",
                    serde_json::json!({
                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                        "codec": format!("{:?}", vid_codec),
                        "camera_frames": camera_frames,
                        "empty_encodes": empty_encodes,
                        "platform": "android",
                    }),
                );
                info!("video send task exited (android)");
            });
            Some(tx)
        } else {
            crate::emit_call_debug(
                &app,
                "video:send_disabled",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "reason": if is_direct_p2p {
                        "direct_p2p_skips_relay_handshake"
                    } else {
                        "no_video_codec_negotiated"
                    },
                    "platform": "android",
                }),
            );
            None
        };

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
            camera_tx,
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
        // Phase 6: explicit is_direct_p2p flag (see android branch).
        is_direct_p2p: bool,
        _app: tauri::AppHandle,
        active_quality: Arc<std::sync::Mutex<wzp_proto::QualityProfile>>,
        peer_max_quality: Arc<std::sync::Mutex<Option<wzp_proto::QualityProfile>>>,
        event_cb: F,
    ) -> Result<Self, anyhow::Error>
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        info!(
            %relay, %room, %alias, %quality,
            has_reuse = reuse_endpoint.is_some(),
            has_pre_connected = pre_connected_transport.is_some(),
            is_direct_p2p,
            "CallEngine::start (desktop) invoked"
        );
        let call_t0 = Instant::now();
        let _ = rustls::crypto::ring::default_provider().install_default();

        let relay_addr: SocketAddr = relay.parse()?;

        let seed = crate::load_or_create_seed().map_err(|e| anyhow::anyhow!("identity: {e}"))?;
        let fp = seed.derive_identity().public_identity().fingerprint;
        let fingerprint = fp.to_string();
        info!(%fp, "identity loaded");

        // Transport source: either pre-connected or fresh.
        let transport = if let Some(t) = pre_connected_transport {
            info!(
                is_direct_p2p,
                remote = %t.remote_address(),
                max_datagram = ?t.max_datagram_size(),
                "using pre-connected transport"
            );
            t
        } else {
            // Connect — reuse the signal endpoint if the direct-call path gave
            // us one, otherwise create a fresh one (SFU room join path).
            let endpoint = if let Some(ep) = reuse_endpoint {
                info!(local_addr = ?ep.local_addr().ok(), "reusing signal endpoint for media connection");
                ep
            } else {
                let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
                let ep = wzp_transport::create_endpoint(bind_addr, None).map_err(|e| {
                    error!("create_endpoint failed: {e}");
                    e
                })?;
                info!(local_addr = ?ep.local_addr().ok(), "created new endpoint, dialing relay");
                ep
            };
            let client_config = wzp_transport::client_config();
            let conn = wzp_transport::connect(&endpoint, relay_addr, &room, client_config)
                .await
                .map_err(|e| {
                    error!("connect failed: {e}");
                    e
                })?;
            info!("QUIC connection established, performing handshake");
            Arc::new(wzp_transport::QuinnTransport::new(conn))
        };

        // Handshake — relay-specific. Direct P2P connections skip
        // this because the peer is a phone, not a relay with an
        // accept_handshake handler. See the android branch's
        // comment for the full rationale.
        let quinn_transport = transport.clone();
        // NOTE: EncryptingTransport is intentionally NOT wrapping the transport here.
        // The client↔relay handshake derives a pairwise session key, but the relay
        // forwards media without decrypt+re-encrypt — so a recipient with a different
        // pairwise key cannot decrypt the sender's ciphertext. True E2E for the SFU
        // model needs MLS group keys (or hop-by-hop relay re-encryption); until that
        // PRD lands, media goes plaintext-over-QUIC-TLS to the relay.
        let (_negotiated_video_codec, transport): (_, Arc<dyn wzp_proto::MediaTransport>) =
            if !is_direct_p2p {
                let hs =
                    wzp_client::handshake::perform_handshake(&*transport, &seed.0, Some(&alias))
                        .await
                        .map_err(|e| {
                            error!("perform_handshake failed: {e}");
                            e
                        })?;
                crate::emit_call_debug(
                    &_app,
                    "connect:handshake_done",
                    serde_json::json!({
                        "t_ms": call_t0.elapsed().as_millis(),
                        "video_codec": hs.video_codec.map(|c| format!("{:?}", c)),
                    }),
                );
                info!(video_codec = ?hs.video_codec, "handshake complete");
                drop(hs.session);
                (hs.video_codec, transport)
            } else {
                info!("direct P2P — skipping relay handshake (QUIC TLS is the encryption layer)");
                (None, transport)
            };
        crate::emit_call_debug(
            &_app,
            "video:negotiated",
            serde_json::json!({
                "t_ms": call_t0.elapsed().as_millis(),
                "codec": _negotiated_video_codec.map(|c| format!("{:?}", c)),
                "enabled": _negotiated_video_codec.is_some(),
                "direct_p2p": is_direct_p2p,
            }),
        );

        info!("connected to relay, handshake complete");
        event_cb("connected", &format!("joined room {room}"));

        // Audio I/O — VPIO (OS AEC) on macOS, plain CPAL otherwise.
        // The audio handle must be stored in CallEngine to keep streams alive.
        let mut vpio_stats_for_debug = None;
        let (capture_ring, playout_ring, audio_handle): (_, _, Box<dyn std::any::Any + Send>) =
            if _os_aec {
                #[cfg(target_os = "macos")]
                {
                    match wzp_client::audio_vpio::VpioAudio::start() {
                        Ok(v) => {
                            let cr = v.capture_ring().clone();
                            let pr = v.playout_ring().clone();
                            vpio_stats_for_debug = Some(v.stats());
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

        // Adaptive quality: shared pending-profile bridge between recv → send.
        let pending_profile = Arc::new(AtomicU8::new(PROFILE_NO_CHANGE));
        let auto_profile = resolve_quality(&quality).is_none();

        if let Some(vpio_stats) = vpio_stats_for_debug {
            let app = _app.clone();
            let running = running.clone();
            tokio::spawn(async move {
                while running.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS))
                        .await;
                    let s = vpio_stats.snapshot();
                    crate::emit_call_debug(
                        &app,
                        "vpio:render_heartbeat",
                        serde_json::json!({
                            "capture_callbacks": s.capture_callbacks,
                            "capture_samples": s.capture_samples,
                            "render_callbacks": s.render_callbacks,
                            "render_requested_samples": s.render_requested_samples,
                            "render_read_samples": s.render_read_samples,
                            "render_underrun_callbacks": s.render_underrun_callbacks,
                            "render_nonzero_callbacks": s.render_nonzero_callbacks,
                            "render_last_requested": s.render_last_requested,
                            "render_last_read": s.render_last_read,
                            "render_last_rms": s.render_last_rms,
                            "render_last_ring_available": s.render_last_ring_available,
                        }),
                    );
                }
            });
        }

        // Send task
        let send_t = transport.clone();
        let quinn_t = quinn_transport.clone();
        let send_r = running.clone();
        let send_mic = mic_muted.clone();
        let send_fs = frames_sent.clone();
        let send_level = audio_level.clone();
        let send_drops = Arc::new(AtomicU64::new(0));
        let send_quality = quality.clone();
        let send_tx_codec = tx_codec.clone();
        let send_pending_profile = pending_profile.clone();
        let send_app = _app.clone();
        let send_t0 = call_t0;
        let send_active_quality = active_quality.clone();
        let send_peer_max = peer_max_quality.clone();
        tokio::spawn(async move {
            let config = build_call_config(&send_quality);
            let mut frame_samples = (config.profile.frame_duration_ms as usize) * 48;
            info!(codec = ?config.profile.codec, frame_samples, "send task starting");
            *send_tx_codec.lock().await = format!("{:?}", config.profile.codec);
            let mut encoder = CallEncoder::new(&config);
            encoder.set_aec_enabled(false); // OS AEC or none
            let mut buf = vec![0i16; 1920]; // max frame (40ms)

            // Continuous DRED tuning (same as Android send task).
            let mut dred_tuner = wzp_proto::DredTuner::new(config.profile.codec);
            let mut frames_since_dred_poll: u32 = 0;
            let mut frames_since_quality_report: u32 = 0;
            let mut send_loss_window = LossWindow::default();
            let mut heartbeat = std::time::Instant::now();
            let mut last_rms: u32;
            let mut last_pkt_bytes: usize = 0;
            let mut short_reads: u64 = 0;
            let mut last_applied_profile: Option<QualityProfile> = None;

            loop {
                // Quality upgrade flow: apply active_quality / peer_max_quality.
                let effective_profile = {
                    let active = send_active_quality.lock().unwrap().clone();
                    let peer_cap = send_peer_max.lock().unwrap().clone();
                    match peer_cap {
                        Some(cap) if cap.codec.bitrate_bps() < active.codec.bitrate_bps() => cap,
                        _ => active,
                    }
                };
                if Some(&effective_profile) != last_applied_profile.as_ref() {
                    let new_fs = (effective_profile.frame_duration_ms as usize) * 48;
                    info!(to = ?effective_profile.codec, frame_samples = new_fs, "quality: switching encoder profile (desktop)");
                    if encoder.set_profile(effective_profile).is_ok() {
                        frame_samples = new_fs;
                        dred_tuner.set_codec(effective_profile.codec);
                        *send_tx_codec.lock().await = format!("{:?}", effective_profile.codec);
                        last_applied_profile = Some(effective_profile);
                    }
                }
                if !send_r.load(Ordering::Relaxed) {
                    break;
                }
                if capture_ring.available() < frame_samples {
                    short_reads += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(CAPTURE_POLL_MS)).await;
                    continue;
                }
                capture_ring.read(&mut buf[..frame_samples]);

                // Compute RMS audio level for UI meter
                {
                    let pcm = &buf[..frame_samples];
                    let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
                    let rms = (sum_sq / pcm.len() as f64).sqrt() as u32;
                    send_level.store(rms, Ordering::Relaxed);
                    last_rms = rms;
                }

                if send_mic.load(Ordering::Relaxed) {
                    buf[..frame_samples].fill(0);
                }
                match encoder.encode_frame(&buf[..frame_samples]) {
                    Ok(pkts) => {
                        for pkt in &pkts {
                            last_pkt_bytes = pkt.payload.len();
                            if let Err(e) = send_t.send_media(pkt).await {
                                // Transient congestion (Blocked) — drop packet, keep going
                                send_drops.fetch_add(1, Ordering::Relaxed);
                                if send_drops.load(Ordering::Relaxed) <= 3 {
                                    tracing::warn!("send_media error (dropping packet): {e}");
                                }
                            }
                        }
                        let before = send_fs.fetch_add(1, Ordering::Relaxed);
                        if before == 0 {
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

                // Adaptive quality: check if recv task recommended a profile switch.
                if auto_profile {
                    let p = send_pending_profile.swap(PROFILE_NO_CHANGE, Ordering::Acquire);
                    if p != PROFILE_NO_CHANGE {
                        if let Some(new_profile) = index_to_profile(p) {
                            let new_fs = (new_profile.frame_duration_ms as usize) * 48;
                            info!(to = ?new_profile.codec, frame_samples = new_fs, "auto: switching encoder profile (desktop)");
                            if encoder.set_profile(new_profile).is_ok() {
                                frame_samples = new_fs;
                                dred_tuner.set_codec(new_profile.codec);
                                *send_tx_codec.lock().await = format!("{:?}", new_profile.codec);
                            }
                        }
                    }
                }

                // DRED tuner: poll quinn path stats periodically.
                frames_since_dred_poll += 1;
                if frames_since_dred_poll >= DRED_POLL_INTERVAL {
                    frames_since_dred_poll = 0;
                    let snap = quinn_t.quinn_path_stats();
                    let pq = send_t.path_quality();
                    let win_loss = send_loss_window.observe(
                        snap.sent_packets,
                        snap.lost_packets,
                        snap.loss_pct,
                    );
                    if let Some(tuning) =
                        dred_tuner.update(win_loss, snap.rtt_ms, pq.jitter_ms)
                    {
                        encoder.apply_dred_tuning(tuning);
                    }
                }

                // Quality report: generate from quinn stats and attach to next packet.
                // The peer's recv task (or relay) uses this for adaptive quality.
                frames_since_quality_report += 1;
                if frames_since_quality_report >= QUALITY_REPORT_INTERVAL {
                    frames_since_quality_report = 0;
                    let snap = quinn_t.quinn_path_stats();
                    let pq = send_t.path_quality();
                    let win_loss = send_loss_window.observe(
                        snap.sent_packets,
                        snap.lost_packets,
                        snap.loss_pct,
                    );
                    let report = wzp_proto::QualityReport::from_path_stats(
                        win_loss,
                        snap.rtt_ms,
                        pq.jitter_ms,
                    );
                    encoder.set_pending_quality_report(report);
                }

                if heartbeat.elapsed() >= std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS) {
                    let fs = send_fs.load(Ordering::Relaxed);
                    let drops = send_drops.load(Ordering::Relaxed);
                    crate::emit_call_debug(
                        &send_app,
                        "media:send_heartbeat",
                        serde_json::json!({
                            "frames_sent": fs,
                            "last_rms": last_rms,
                            "last_pkt_bytes": last_pkt_bytes,
                            "short_reads": short_reads,
                            "drops": drops,
                            "last_send_err": serde_json::Value::Null,
                        }),
                    );
                    heartbeat = std::time::Instant::now();
                }
            }
        });

        // Recv task (direct playout with auto codec switch)
        let recv_t = transport.clone();
        let quinn_t = quinn_transport.clone();
        let recv_r = running.clone();
        let recv_spk = spk_muted.clone();
        let recv_fr = frames_received.clone();
        let recv_rx_codec = rx_codec.clone();
        let pending_profile_recv = pending_profile.clone();
        let recv_app = _app.clone();
        let recv_t0 = call_t0;
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
            let mut quality_ctrl = AdaptiveQualityController::new();
            let mut recv_quality_counter: u32 = 0;
            let mut recv_loss_window = LossWindow::default();
            let mut heartbeat = std::time::Instant::now();
            let mut first_packet_logged = false;
            let mut video_reassembler = wzp_video::transport::VideoReassembler::new();
            let mut video_decoder: Option<Box<dyn wzp_video::decoder::VideoDecoder>> = None;
            let mut video_decoder_codec: Option<wzp_proto::CodecId> = None;
            let mut video_first_recv_logged_desktop = false;
            let mut video_first_reassembled_logged = false;
            let mut video_reassembled_samples: u64 = 0;
            let mut video_first_decoded_logged = false;
            let mut video_decoder_buffering_count: u64 = 0;
            let mut decoded_frames: u64 = 0;
            let mut decode_errs: u64 = 0;
            let mut last_written: usize = 0;
            let mut written_samples: u64 = 0;
            let mut last_recv_fr_for_watchdog: u64 = 0;
            let mut no_recv_ticks: u32 = 0;
            let mut media_degraded_emitted = false;

            loop {
                if !recv_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(RECV_TIMEOUT_MS),
                    recv_t.recv_media(),
                )
                .await
                {
                    Ok(Ok(Some(pkt))) => {
                        // Route video packets to the reassembler before any audio processing.
                        if pkt.header.media_type == wzp_proto::MediaType::Video {
                            if !video_first_recv_logged_desktop {
                                video_first_recv_logged_desktop = true;
                                crate::emit_call_debug(
                                    &recv_app,
                                    "video:first_recv",
                                    serde_json::json!({
                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                        "codec": format!("{:?}", pkt.header.codec_id),
                                        "payload_bytes": pkt.payload.len(),
                                        "stream_id": pkt.header.stream_id,
                                    }),
                                );
                            }
                            if let Some((codec_id, is_kf, frame)) =
                                video_reassembler.push(&pkt)
                            {
                                video_reassembled_samples += 1;
                                if !video_first_reassembled_logged {
                                    video_first_reassembled_logged = true;
                                    crate::emit_call_debug(
                                        &recv_app,
                                        "video:first_reassembled",
                                        serde_json::json!({
                                            "t_ms": recv_t0.elapsed().as_millis() as u64,
                                            "codec": format!("{:?}", codec_id),
                                            "is_keyframe": is_kf,
                                            "frame_bytes": frame.len(),
                                            "platform": "desktop",
                                        }),
                                    );
                                }
                                if video_reassembled_samples <= 5 {
                                    crate::emit_call_debug(
                                        &recv_app,
                                        "video:reassembled_frame",
                                        serde_json::json!({
                                            "t_ms": recv_t0.elapsed().as_millis() as u64,
                                            "codec": format!("{:?}", codec_id),
                                            "is_keyframe": is_kf,
                                            "frame_bytes": frame.len(),
                                            "frame_no": video_reassembled_samples,
                                            "platform": "desktop",
                                        }),
                                    );
                                }
                                // Lazy-init or switch decoder on codec change.
                                if video_decoder_codec != Some(codec_id) {
                                    crate::emit_call_debug(
                                        &recv_app,
                                        "video:decoder_init_start",
                                        serde_json::json!({
                                            "t_ms": recv_t0.elapsed().as_millis() as u64,
                                            "codec": format!("{:?}", codec_id),
                                            "width": 1280,
                                            "height": 720,
                                            "platform": "desktop",
                                        }),
                                    );
                                    match wzp_video::factory::create_video_decoder(codec_id, 1280, 720) {
                                        Ok(d) => {
                                            info!(codec = ?codec_id, "video decoder created");
                                            crate::emit_call_debug(
                                                &recv_app,
                                                "video:decoder_started",
                                                serde_json::json!({
                                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                    "codec": format!("{:?}", codec_id),
                                                    "platform": "desktop",
                                                }),
                                            );
                                            video_decoder = Some(d);
                                            video_decoder_codec = Some(codec_id);
                                        }
                                        Err(e) => {
                                            error!("video decoder init failed: {e}");
                                            crate::emit_call_debug(
                                                &recv_app,
                                                "video:decoder_init_failed",
                                                serde_json::json!({
                                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                    "codec": format!("{:?}", codec_id),
                                                    "error": e.to_string(),
                                                    "platform": "desktop",
                                                }),
                                            );
                                        }
                                    }
                                }
                                if let Some(ref mut dec) = video_decoder {
                                    match dec.decode(&frame) {
                                        Ok(Some(yuv_frame)) => {
                                            recv_fr.fetch_add(1, Ordering::Relaxed);
                                            // Emit video frame to WebView for rendering.
                                            // Always-on (not gated on debug flag) so the UI can show video.
                                            let jpeg_b64 = crate::i420_to_jpeg_b64(
                                                &yuv_frame.data,
                                                yuv_frame.width,
                                                yuv_frame.height,
                                            );
                                            let jpeg_ok = jpeg_b64.is_some();
                                            if !video_first_decoded_logged {
                                                video_first_decoded_logged = true;
                                                crate::emit_call_debug(
                                                    &recv_app,
                                                    "video:first_decoded_frame",
                                                    serde_json::json!({
                                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                        "codec": format!("{:?}", codec_id),
                                                        "width": yuv_frame.width,
                                                        "height": yuv_frame.height,
                                                        "yuv_bytes": yuv_frame.data.len(),
                                                        "jpeg_ok": jpeg_ok,
                                                        "platform": "desktop",
                                                    }),
                                                );
                                            }
                                            if !jpeg_ok {
                                                crate::emit_call_debug(
                                                    &recv_app,
                                                    "video:jpeg_encode_failed",
                                                    serde_json::json!({
                                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                        "codec": format!("{:?}", codec_id),
                                                        "width": yuv_frame.width,
                                                        "height": yuv_frame.height,
                                                        "yuv_bytes": yuv_frame.data.len(),
                                                        "platform": "desktop",
                                                    }),
                                                );
                                            }
                                            let _ = recv_app.emit(
                                                "video:frame",
                                                serde_json::json!({
                                                    "is_keyframe": is_kf,
                                                    "width": yuv_frame.width,
                                                    "height": yuv_frame.height,
                                                    "jpeg_b64": jpeg_b64,
                                                    "codec": format!("{:?}", codec_id),
                                                }),
                                            );
                                        }
                                        Ok(None) => {
                                            video_decoder_buffering_count += 1;
                                            if video_decoder_buffering_count == 1
                                                || video_decoder_buffering_count % 30 == 0
                                            {
                                                crate::emit_call_debug(
                                                    &recv_app,
                                                    "video:decoder_buffering",
                                                    serde_json::json!({
                                                        "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                        "codec": format!("{:?}", codec_id),
                                                        "buffering": video_decoder_buffering_count,
                                                        "platform": "desktop",
                                                    }),
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            error!("video decode error: {e}");
                                            crate::emit_call_debug(
                                                &recv_app,
                                                "video:decode_error",
                                                serde_json::json!({
                                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                                    "codec": format!("{:?}", codec_id),
                                                    "error": e.to_string(),
                                                    "platform": "desktop",
                                                }),
                                            );
                                        }
                                    }
                                }
                                // Evict stale partial frames every ~10 frames received.
                                video_reassembler.evict_stale(
                                    pkt.header.timestamp,
                                    5_000,
                                );
                            }
                            continue; // video packet handled — skip audio path
                        }

                        if !first_packet_logged {
                            first_packet_logged = true;
                            crate::emit_call_debug(
                                &recv_app,
                                "media:first_recv",
                                serde_json::json!({
                                    "t_ms": recv_t0.elapsed().as_millis() as u64,
                                    "codec": format!("{:?}", pkt.header.codec_id),
                                    "payload_bytes": pkt.payload.len(),
                                    "is_repair": pkt.header.is_repair(),
                                }),
                            );
                        }
                        if !pkt.header.is_repair() && pkt.header.codec_id != CodecId::ComfortNoise {
                            // Track RX codec
                            {
                                let mut rx = recv_rx_codec.lock().await;
                                let codec_name = format!("{:?}", pkt.header.codec_id);
                                if *rx != codec_name {
                                    *rx = codec_name;
                                }
                            }
                            // Auto-switch decoder if incoming codec differs
                            if pkt.header.codec_id != current_codec {
                                let new_profile = codec_to_profile(pkt.header.codec_id);
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
                                let frame_samples_now =
                                    (48_000 * current_profile.frame_duration_ms as usize) / 1000;
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

                            // Adaptive quality: ingest quality reports from peer
                            if let Some(ref qr) = pkt.quality_report {
                                if let Some(new_profile) = quality_ctrl.observe(qr) {
                                    let idx = profile_to_index(&new_profile);
                                    info!(to = ?new_profile.codec, "auto: quality adapter recommends switch");
                                    pending_profile_recv.store(idx, Ordering::Release);
                                }
                            }

                            // P2P self-observation: if no quality reports from peer,
                            // generate local observations from our own QUIC path stats.
                            // This ensures adaptive quality works even on P2P calls
                            // where the peer hasn't been updated to send reports yet.
                            recv_quality_counter += 1;
                            if recv_quality_counter >= QUALITY_REPORT_INTERVAL {
                                recv_quality_counter = 0;
                                let snap = quinn_t.quinn_path_stats();
                                let pq = recv_t.path_quality();
                                let win_loss = recv_loss_window.observe(
                                    snap.sent_packets,
                                    snap.lost_packets,
                                    snap.loss_pct,
                                );
                                let local_report = wzp_proto::QualityReport::from_path_stats(
                                    win_loss,
                                    snap.rtt_ms,
                                    pq.jitter_ms,
                                );
                                if auto_profile {
                                    if let Some(new_profile) = quality_ctrl.observe(&local_report) {
                                        let idx = profile_to_index(&new_profile);
                                        info!(to = ?new_profile.codec, "auto: local quality observation recommends switch");
                                        pending_profile_recv.store(idx, Ordering::Release);
                                    }
                                }
                            }

                            match decoder.decode(&pkt.payload, &mut pcm) {
                                Ok(n) => {
                                    decoded_frames += 1;
                                    agc.process_frame(&mut pcm[..n]);
                                    if !recv_spk.load(Ordering::Relaxed) {
                                        playout_ring.write(&pcm[..n]);
                                        last_written = n;
                                        written_samples = written_samples.saturating_add(n as u64);
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

                if heartbeat.elapsed() >= std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS) {
                    let fr = recv_fr.load(Ordering::Relaxed);
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

                    if fr == last_recv_fr_for_watchdog {
                        no_recv_ticks += 1;
                    } else {
                        no_recv_ticks = 0;
                        if media_degraded_emitted {
                            media_degraded_emitted = false;
                            let _ = recv_app.emit(
                                "call-event",
                                serde_json::json!({
                                    "kind": "media-recovered",
                                }),
                            );
                            crate::emit_call_debug(
                                &recv_app,
                                "media:recovered",
                                serde_json::json!({}),
                            );
                        }
                    }
                    last_recv_fr_for_watchdog = fr;

                    if no_recv_ticks >= 3 && !media_degraded_emitted {
                        media_degraded_emitted = true;
                        let _ = recv_app.emit(
                            "call-event",
                            serde_json::json!({
                                "kind": "media-degraded",
                            }),
                        );
                        crate::emit_call_debug(
                            &recv_app,
                            "media:no_recv_timeout",
                            serde_json::json!({
                                "recv_fr": fr,
                                "no_recv_ticks": no_recv_ticks,
                            }),
                        );
                    }

                    heartbeat = std::time::Instant::now();
                }
            }
        });

        // Signal task (presence + quality directives)
        let event_cb = Arc::new(event_cb);
        tokio::spawn(run_signal_task(
            _app.clone(),
            transport.clone(),
            running.clone(),
            pending_profile.clone(),
            participants.clone(),
            event_cb.clone(),
        ));

        // Video send task — active only when the handshake negotiated a video codec.
        // Camera frames arrive via camera_tx; the task encodes and packetizes them.
        // Blocker 4 (camera capture) will push frames into this channel.
        let camera_tx = if let Some(vid_codec) = _negotiated_video_codec {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<wzp_video::encoder::VideoFrame>(4);
            let vid_transport = transport.clone();
            let vid_running = running.clone();
            let vid_t0 = call_t0;
            let vid_app = _app.clone();
            crate::emit_call_debug(
                &_app,
                "video:sender_channel_ready",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "codec": format!("{:?}", vid_codec),
                    "queue_depth": 4,
                    "platform": "desktop",
                }),
            );
            tokio::spawn(async move {
                crate::emit_call_debug(
                    &vid_app,
                    "video:encoder_init_start",
                    serde_json::json!({
                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                        "codec": format!("{:?}", vid_codec),
                        "width": 1280,
                        "height": 720,
                        "bitrate_bps": 1_500_000,
                        "platform": "desktop",
                    }),
                );
                let mut encoder = match wzp_video::factory::create_video_encoder(
                    vid_codec, 1280, 720, 1_500_000,
                ) {
                    Ok(e) => {
                        crate::emit_call_debug(
                            &vid_app,
                            "video:encoder_started",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "platform": "desktop",
                            }),
                        );
                        e
                    }
                    Err(e) => {
                        error!("video encoder init failed: {e}");
                        crate::emit_call_debug(
                            &vid_app,
                            "video:encoder_init_failed",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "platform": "desktop",
                                "error": e.to_string(),
                            }),
                        );
                        return;
                    }
                };
                let mut seq: u32 = 0;
                let mut frames_since_keyframe: u32 = 0;
                let mut first_send_logged = false;
                let mut first_camera_frame_logged = false;
                let mut camera_frames: u64 = 0;
                let mut empty_encodes: u64 = 0;
                let mut encoded_frame_samples: u64 = 0;
                let mut wait_ticks: u64 = 0;
                encoder.request_keyframe();
                crate::emit_call_debug(
                    &vid_app,
                    "video:keyframe_requested",
                    serde_json::json!({
                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                        "codec": format!("{:?}", vid_codec),
                        "reason": "initial",
                        "platform": "desktop",
                    }),
                );
                info!(codec = ?vid_codec, "video send task started");
                while vid_running.load(Ordering::Relaxed) {
                    let frame = match tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        rx.recv(),
                    )
                    .await
                    {
                        Ok(Some(f)) => {
                            wait_ticks = 0;
                            camera_frames += 1;
                            if !first_camera_frame_logged {
                                first_camera_frame_logged = true;
                                crate::emit_call_debug(
                                    &vid_app,
                                    "video:first_camera_frame",
                                    serde_json::json!({
                                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                                        "codec": format!("{:?}", vid_codec),
                                        "width": f.width,
                                        "height": f.height,
                                        "data_bytes": f.data.len(),
                                        "platform": "desktop",
                                    }),
                                );
                            }
                            f
                        }
                        Ok(None) => break, // sender dropped
                        Err(_) => {
                            wait_ticks += 1;
                            if wait_ticks == 10 || wait_ticks % 50 == 0 {
                                crate::emit_call_debug(
                                    &vid_app,
                                    "video:waiting_for_camera_frames",
                                    serde_json::json!({
                                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                                        "wait_ms": wait_ticks * 200,
                                        "codec": format!("{:?}", vid_codec),
                                        "platform": "desktop",
                                    }),
                                );
                            }
                            continue;
                        }
                    };

                    if frames_since_keyframe >= 150 {
                        encoder.request_keyframe();
                        crate::emit_call_debug(
                            &vid_app,
                            "video:keyframe_requested",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "reason": "periodic",
                                "camera_frames": camera_frames,
                                "platform": "desktop",
                            }),
                        );
                        frames_since_keyframe = 0;
                    }

                    let encoded = match encoder.encode(&frame) {
                        Ok(b) => b,
                        Err(e) => {
                            error!("video encode error: {e}");
                            crate::emit_call_debug(
                                &vid_app,
                                "video:encode_error",
                                serde_json::json!({
                                    "t_ms": vid_t0.elapsed().as_millis() as u64,
                                    "codec": format!("{:?}", vid_codec),
                                    "camera_frames": camera_frames,
                                    "error": e.to_string(),
                                    "platform": "desktop",
                                }),
                            );
                            continue;
                        }
                    };
                    if encoded.is_empty() {
                        empty_encodes += 1;
                        if empty_encodes == 1 || empty_encodes % 30 == 0 {
                            crate::emit_call_debug(
                                &vid_app,
                                "video:encode_empty",
                                serde_json::json!({
                                    "t_ms": vid_t0.elapsed().as_millis() as u64,
                                    "codec": format!("{:?}", vid_codec),
                                    "camera_frames": camera_frames,
                                    "empty_encodes": empty_encodes,
                                    "platform": "desktop",
                                }),
                            );
                        }
                        continue;
                    }

                    let is_keyframe = encoder.is_keyframe(&encoded);
                    let ts_ms = vid_t0.elapsed().as_millis() as u32;
                    let pkts = wzp_video::transport::packetize_video_frame(
                        &encoded, vid_codec, is_keyframe, &mut seq, ts_ms,
                    );
                    if encoded_frame_samples < 5 {
                        encoded_frame_samples += 1;
                        let packet_payload_bytes: usize =
                            pkts.iter().map(|pkt| pkt.payload.len()).sum();
                        crate::emit_call_debug(
                            &vid_app,
                            "video:encoded_frame",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "camera_frames": camera_frames,
                                "encoded_bytes": encoded.len(),
                                "packet_payload_bytes": packet_payload_bytes,
                                "packets": pkts.len(),
                                "is_keyframe": is_keyframe,
                                "sample_no": encoded_frame_samples,
                                "platform": "desktop",
                            }),
                        );
                    }
                    if !first_send_logged && !pkts.is_empty() {
                        first_send_logged = true;
                        crate::emit_call_debug(
                            &vid_app,
                            "video:first_send",
                            serde_json::json!({
                                "t_ms": vid_t0.elapsed().as_millis() as u64,
                                "codec": format!("{:?}", vid_codec),
                                "packets": pkts.len(),
                                "first_pkt_bytes": pkts[0].payload.len(),
                                "last_pkt_bytes": pkts.last().map(|pkt| pkt.payload.len()).unwrap_or(0),
                                "encoded_bytes": encoded.len(),
                                "stream_id": pkts[0].header.stream_id,
                                "is_keyframe": is_keyframe,
                            }),
                        );
                    }
                    for pkt in &pkts {
                        if let Err(e) = vid_transport.send_media(pkt).await {
                            crate::emit_call_debug(
                                &vid_app,
                                "video:send_error",
                                serde_json::json!({"error": e.to_string()}),
                            );
                            break;
                        }
                    }
                    frames_since_keyframe += 1;
                }
                crate::emit_call_debug(
                    &vid_app,
                    "video:sender_exit",
                    serde_json::json!({
                        "t_ms": vid_t0.elapsed().as_millis() as u64,
                        "codec": format!("{:?}", vid_codec),
                        "camera_frames": camera_frames,
                        "empty_encodes": empty_encodes,
                        "platform": "desktop",
                    }),
                );
                info!("video send task exited");
            });
            Some(tx)
        } else {
            crate::emit_call_debug(
                &_app,
                "video:send_disabled",
                serde_json::json!({
                    "t_ms": call_t0.elapsed().as_millis(),
                    "reason": if is_direct_p2p {
                        "direct_p2p_skips_relay_handshake"
                    } else {
                        "no_video_codec_negotiated"
                    },
                    "platform": "desktop",
                }),
            );
            None
        };

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
            camera_tx,
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
            // Release the BT SCO communication device so Android can
            // route media (video, music) back to BT A2DP. Without this,
            // setCommunicationDevice locks BT to SCO mode and other apps
            // can't use the headset for media playback until reboot.
            if let Err(e) = crate::android_audio::stop_bluetooth_sco() {
                tracing::warn!("stop_bluetooth_sco on call end failed: {e}");
            }
            // Restore MODE_NORMAL so other apps' audio routes normally.
            if let Err(e) = crate::android_audio::set_audio_mode_normal() {
                tracing::warn!("set_audio_mode_normal failed: {e}");
            }
        }
    }
}

impl Drop for CallEngine {
    fn drop(&mut self) {
        // Safety net: if stop() was never called (crash, app
        // backgrounding), signal tasks to exit so they don't
        // spin on a dropped transport.
        self.running.store(false, Ordering::SeqCst);
    }
}


#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use wzp_client::encrypted_transport::EncryptingTransport;
    use wzp_crypto::ChaChaSession;
    use wzp_proto::{
        CodecId, CryptoSession, MediaHeader, MediaPacket, MediaTransport, MediaType, PathQuality,
        SignalMessage, TransportError,
    };

    struct LoopbackTransport {
        sent: StdMutex<Vec<MediaPacket>>,
    }

    impl LoopbackTransport {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: StdMutex::new(Vec::new()),
            })
        }
        fn take_sent(&self) -> Vec<MediaPacket> {
            self.sent.lock().unwrap().drain(..).collect()
        }
    }

    #[async_trait]
    impl MediaTransport for LoopbackTransport {
        async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError> {
            self.sent.lock().unwrap().push(packet.clone());
            Ok(())
        }
        async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError> {
            Ok(None)
        }
        async fn send_signal(&self, _msg: &SignalMessage) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError> {
            Ok(None)
        }
        fn path_quality(&self) -> PathQuality {
            PathQuality::default()
        }
        async fn close(&self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    fn make_header(seq: u32) -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 0,
            seq,
            timestamp: seq * 20,
            fec_block: 0,
        }
    }

    #[tokio::test]
    async fn relay_path_encrypts_media_payload() {
        // Simulate the exact wrapping pattern used in engine.rs for the relay path.
        let key = [0x42u8; 32];
        let session: Box<dyn CryptoSession> = Box::new(ChaChaSession::new(key));
        let inner = LoopbackTransport::new();
        let transport: Arc<dyn MediaTransport> =
            Arc::new(EncryptingTransport::new(inner.clone(), session));

        let header = make_header(1);
        let plaintext = b"secret audio frame";
        let pkt = MediaPacket {
            header,
            payload: Bytes::from_static(plaintext),
            quality_report: None,
        };

        transport.send_media(&pkt).await.unwrap();

        let sent = inner.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].header, header, "header must be preserved");
        assert_ne!(
            sent[0].payload.as_ref(),
            plaintext.as_ref(),
            "plaintext must not appear on wire"
        );
        // Ciphertext is longer by exactly the AEAD tag (16 bytes)
        assert_eq!(sent[0].payload.len(), plaintext.len() + 16);
    }
}
