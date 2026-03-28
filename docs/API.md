# WarzonePhone Crate API Reference

## wzp-proto

**Path**: `crates/wzp-proto/src/`

The protocol definition crate. Contains all shared types, trait interfaces, and core logic. No implementation dependencies -- this is the hub of the star dependency graph.

### Traits (`traits.rs`)

```rust
/// Encodes PCM audio into compressed frames.
pub trait AudioEncoder: Send + Sync {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError>;
    fn codec_id(&self) -> CodecId;
    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError>;
    fn max_frame_bytes(&self) -> usize;
    fn set_inband_fec(&mut self, _enabled: bool) {}  // default no-op
    fn set_dtx(&mut self, _enabled: bool) {}          // default no-op
}

/// Decodes compressed frames back to PCM audio.
pub trait AudioDecoder: Send + Sync {
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError>;
    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError>;
    fn codec_id(&self) -> CodecId;
    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError>;
}

/// Encodes source symbols into FEC-protected blocks.
pub trait FecEncoder: Send + Sync {
    fn add_source_symbol(&mut self, data: &[u8]) -> Result<(), FecError>;
    fn generate_repair(&mut self, ratio: f32) -> Result<Vec<(u8, Vec<u8>)>, FecError>;
    fn finalize_block(&mut self) -> Result<u8, FecError>;
    fn current_block_id(&self) -> u8;
    fn current_block_size(&self) -> usize;
}

/// Decodes FEC-protected blocks, recovering lost source symbols.
pub trait FecDecoder: Send + Sync {
    fn add_symbol(&mut self, block_id: u8, symbol_index: u8, is_repair: bool, data: &[u8]) -> Result<(), FecError>;
    fn try_decode(&mut self, block_id: u8) -> Result<Option<Vec<Vec<u8>>>, FecError>;
    fn expire_before(&mut self, block_id: u8);
}

/// Per-call encryption session (symmetric, after key exchange).
pub trait CryptoSession: Send + Sync {
    fn encrypt(&mut self, header_bytes: &[u8], plaintext: &[u8], out: &mut Vec<u8>) -> Result<(), CryptoError>;
    fn decrypt(&mut self, header_bytes: &[u8], ciphertext: &[u8], out: &mut Vec<u8>) -> Result<(), CryptoError>;
    fn initiate_rekey(&mut self) -> Result<[u8; 32], CryptoError>;
    fn complete_rekey(&mut self, peer_ephemeral_pub: &[u8; 32]) -> Result<(), CryptoError>;
    fn overhead(&self) -> usize { 16 }  // ChaCha20-Poly1305 tag
}

/// Key exchange using the Warzone identity model.
pub trait KeyExchange: Send + Sync {
    fn from_identity_seed(seed: &[u8; 32]) -> Self where Self: Sized;
    fn generate_ephemeral(&mut self) -> [u8; 32];
    fn identity_public_key(&self) -> [u8; 32];
    fn fingerprint(&self) -> [u8; 16];
    fn sign(&self, data: &[u8]) -> Vec<u8>;
    fn verify(peer_identity_pub: &[u8; 32], data: &[u8], signature: &[u8]) -> bool where Self: Sized;
    fn derive_session(&self, peer_ephemeral_pub: &[u8; 32]) -> Result<Box<dyn CryptoSession>, CryptoError>;
}

/// Transport layer for sending/receiving media and signaling.
#[async_trait]
pub trait MediaTransport: Send + Sync {
    async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError>;
    async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError>;
    async fn send_signal(&self, msg: &SignalMessage) -> Result<(), TransportError>;
    async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError>;
    fn path_quality(&self) -> PathQuality;
    async fn close(&self) -> Result<(), TransportError>;
}

/// Wraps/unwraps packets for DPI evasion (Phase 2).
pub trait ObfuscationLayer: Send + Sync {
    fn obfuscate(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), ObfuscationError>;
    fn deobfuscate(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), ObfuscationError>;
}

/// Adaptive quality controller.
pub trait QualityController: Send + Sync {
    fn observe(&mut self, report: &QualityReport) -> Option<QualityProfile>;
    fn force_profile(&mut self, profile: QualityProfile);
    fn current_profile(&self) -> QualityProfile;
}
```

### Wire Format Types (`packet.rs`)

```rust
pub struct MediaHeader { /* 12 bytes */ }
pub struct QualityReport { /* 4 bytes */ }
pub struct MediaPacket { pub header: MediaHeader, pub payload: Bytes, pub quality_report: Option<QualityReport> }
pub enum SignalMessage { CallOffer{..}, CallAnswer{..}, IceCandidate{..}, Rekey{..}, QualityUpdate{..}, Ping{..}, Pong{..}, Hangup{..} }
pub enum HangupReason { Normal, Busy, Declined, Timeout, Error }
```

Key methods:
- `MediaHeader::write_to(&self, buf: &mut impl BufMut)` -- serialize to 12 bytes
- `MediaHeader::read_from(buf: &mut impl Buf) -> Option<Self>` -- deserialize
- `MediaHeader::encode_fec_ratio(ratio: f32) -> u8` -- float to 7-bit wire encoding
- `MediaHeader::decode_fec_ratio(encoded: u8) -> f32` -- 7-bit wire to float
- `MediaPacket::to_bytes(&self) -> Bytes` -- serialize complete packet
- `MediaPacket::from_bytes(data: Bytes) -> Option<Self>` -- deserialize

### Codec Identifiers (`codec_id.rs`)

```rust
pub enum CodecId { Opus24k = 0, Opus16k = 1, Opus6k = 2, Codec2_3200 = 3, Codec2_1200 = 4 }

pub struct QualityProfile {
    pub codec: CodecId,
    pub fec_ratio: f32,
    pub frame_duration_ms: u8,
    pub frames_per_block: u8,
}
```

Constants: `QualityProfile::GOOD`, `QualityProfile::DEGRADED`, `QualityProfile::CATASTROPHIC`

Key methods:
- `CodecId::bitrate_bps(self) -> u32`
- `CodecId::frame_duration_ms(self) -> u8`
- `CodecId::sample_rate_hz(self) -> u32`
- `CodecId::from_wire(val: u8) -> Option<Self>`
- `CodecId::to_wire(self) -> u8`
- `QualityProfile::total_bitrate_kbps(&self) -> f32`

### Quality Controller (`quality.rs`)

```rust
pub enum Tier { Good, Degraded, Catastrophic }
pub struct AdaptiveQualityController { /* ... */ }
```

Key methods:
- `AdaptiveQualityController::new() -> Self` -- starts at Tier::Good
- `AdaptiveQualityController::tier(&self) -> Tier`
- `Tier::classify(report: &QualityReport) -> Self`
- `Tier::profile(self) -> QualityProfile`

### Jitter Buffer (`jitter.rs`)

```rust
pub struct JitterBuffer { /* ... */ }
pub struct JitterStats { pub packets_received: u64, pub packets_played: u64, pub packets_lost: u64, pub packets_late: u64, pub packets_duplicate: u64, pub current_depth: usize }
pub enum PlayoutResult { Packet(MediaPacket), Missing { seq: u16 }, NotReady }
```

Key methods:
- `JitterBuffer::new(target_depth: usize, max_depth: usize, min_depth: usize) -> Self`
- `JitterBuffer::default_5s() -> Self` -- target=50, max=250, min=25
- `JitterBuffer::push(&mut self, packet: MediaPacket)`
- `JitterBuffer::pop(&mut self) -> PlayoutResult`
- `JitterBuffer::depth(&self) -> usize`
- `JitterBuffer::stats(&self) -> &JitterStats`
- `JitterBuffer::reset(&mut self)`
- `JitterBuffer::set_target_depth(&mut self, depth: usize)`

### Session State Machine (`session.rs`)

```rust
pub enum SessionState { Idle, Connecting, Handshaking, Active, Rekeying, Closed }
pub enum SessionEvent { Initiate, Connected, HandshakeComplete, RekeyStart, RekeyComplete, Terminate{reason}, ConnectionLost }
pub struct Session { /* ... */ }
```

Key methods:
- `Session::new(session_id: [u8; 16]) -> Self`
- `Session::state(&self) -> SessionState`
- `Session::transition(&mut self, event: SessionEvent, now_ms: u64) -> Result<SessionState, TransitionError>`
- `Session::is_media_active(&self) -> bool` -- true for Active and Rekeying

### Error Types (`error.rs`)

```rust
pub enum CodecError { EncodeFailed(String), DecodeFailed(String), UnsupportedTransition{from, to} }
pub enum FecError { BlockFull{max}, InsufficientSymbols{needed, have}, InvalidBlock(u8), Internal(String) }
pub enum CryptoError { DecryptionFailed, InvalidPublicKey, RekeyFailed(String), ReplayDetected{seq}, Internal(String) }
pub enum TransportError { ConnectionLost, DatagramTooLarge{size, max}, Timeout{ms}, Io(io::Error), Internal(String) }
pub enum ObfuscationError { Failed(String), InvalidFraming }
```

### PathQuality (`traits.rs`)

```rust
pub struct PathQuality {
    pub loss_pct: f32,      // 0.0-100.0
    pub rtt_ms: u32,
    pub jitter_ms: u32,
    pub bandwidth_kbps: u32,
}
```

---

## wzp-codec

**Path**: `crates/wzp-codec/src/`

### Factory Functions (`lib.rs`)

```rust
/// Create an adaptive encoder (accepts 48 kHz PCM, handles resampling for Codec2).
pub fn create_encoder(profile: QualityProfile) -> Box<dyn AudioEncoder>

/// Create an adaptive decoder (outputs 48 kHz PCM, handles upsampling from Codec2).
pub fn create_decoder(profile: QualityProfile) -> Box<dyn AudioDecoder>
```

### Public Types

```rust
pub struct AdaptiveEncoder { /* wraps OpusEncoder + Codec2Encoder */ }
pub struct AdaptiveDecoder { /* wraps OpusDecoder + Codec2Decoder */ }
pub struct OpusEncoder { /* audiopus::coder::Encoder wrapper */ }
pub struct OpusDecoder { /* audiopus::coder::Decoder wrapper */ }
pub struct Codec2Encoder { /* codec2::Codec2 wrapper */ }
pub struct Codec2Decoder { /* codec2::Codec2 wrapper */ }
```

Key methods on concrete types:
- `OpusEncoder::new(profile: QualityProfile) -> Result<Self, CodecError>`
- `OpusEncoder::frame_samples(&self) -> usize` -- 960 for 20ms, 1920 for 40ms
- `Codec2Encoder::new(profile: QualityProfile) -> Result<Self, CodecError>`
- `Codec2Encoder::frame_samples(&self) -> usize` -- 160 for 20ms/3200bps, 320 for 40ms/1200bps

### Resampler (`resample.rs`)

```rust
pub fn resample_48k_to_8k(input: &[i16]) -> Vec<i16>  // 6:1 decimation with box filter
pub fn resample_8k_to_48k(input: &[i16]) -> Vec<i16>  // 1:6 linear interpolation
```

---

## wzp-fec

**Path**: `crates/wzp-fec/src/`

### Factory Functions (`lib.rs`)

```rust
/// Create an encoder/decoder pair configured for the given quality profile.
pub fn create_fec_pair(profile: &QualityProfile) -> (RaptorQFecEncoder, RaptorQFecDecoder)

/// Create an encoder configured for the given quality profile.
pub fn create_encoder(profile: &QualityProfile) -> RaptorQFecEncoder

/// Create a decoder configured for the given quality profile.
pub fn create_decoder(profile: &QualityProfile) -> RaptorQFecDecoder
```

### RaptorQFecEncoder (`encoder.rs`)

```rust
pub struct RaptorQFecEncoder { /* block_id, frames_per_block, source_symbols, symbol_size */ }
```

Key methods:
- `RaptorQFecEncoder::new(frames_per_block: usize, symbol_size: u16) -> Self`
- `RaptorQFecEncoder::with_defaults(frames_per_block: usize) -> Self` -- symbol_size=256
- Implements `FecEncoder` trait

### RaptorQFecDecoder (`decoder.rs`)

```rust
pub struct RaptorQFecDecoder { /* blocks: HashMap<u8, BlockState>, symbol_size, frames_per_block */ }
```

Key methods:
- `RaptorQFecDecoder::new(frames_per_block: usize, symbol_size: u16) -> Self`
- `RaptorQFecDecoder::with_defaults(frames_per_block: usize) -> Self`
- Implements `FecDecoder` trait

### Interleaver (`interleave.rs`)

```rust
pub type Symbol = (u8, u8, bool, Vec<u8>);  // (block_id, symbol_index, is_repair, data)
pub struct Interleaver { depth: usize }
```

Key methods:
- `Interleaver::new(depth: usize) -> Self`
- `Interleaver::with_default_depth() -> Self` -- depth=3
- `Interleaver::interleave(&self, blocks: &[Vec<Symbol>]) -> Vec<Symbol>`
- `Interleaver::depth(&self) -> usize`

### AdaptiveFec (`adaptive.rs`)

```rust
pub struct AdaptiveFec { pub frames_per_block: usize, pub repair_ratio: f32, pub symbol_size: u16 }
```

Key methods:
- `AdaptiveFec::from_profile(profile: &QualityProfile) -> Self`
- `AdaptiveFec::build_encoder(&self) -> RaptorQFecEncoder`
- `AdaptiveFec::ratio(&self) -> f32`
- `AdaptiveFec::overhead_factor(&self) -> f32` -- 1.0 + repair_ratio

### Block Managers (`block_manager.rs`)

```rust
pub enum EncoderBlockState { Building, Pending, Sent, Acknowledged }
pub enum DecoderBlockState { Assembling, Complete, Expired }
pub struct EncoderBlockManager { /* ... */ }
pub struct DecoderBlockManager { /* ... */ }
```

Key methods:
- `EncoderBlockManager::next_block_id(&mut self) -> u8`
- `EncoderBlockManager::mark_sent(&mut self, block_id: u8)`
- `EncoderBlockManager::mark_acknowledged(&mut self, block_id: u8)`
- `DecoderBlockManager::touch(&mut self, block_id: u8)`
- `DecoderBlockManager::mark_complete(&mut self, block_id: u8)`
- `DecoderBlockManager::expire_before(&mut self, block_id: u8)`

### Helper Functions (`encoder.rs`)

```rust
/// Build source EncodingPackets for a given block (for testing/interleaving).
pub fn source_packets_for_block(block_id: u8, symbols: &[Vec<u8>], symbol_size: u16) -> Vec<EncodingPacket>

/// Generate repair packets for the given source symbols.
pub fn repair_packets_for_block(block_id: u8, symbols: &[Vec<u8>], symbol_size: u16, ratio: f32) -> Vec<EncodingPacket>
```

---

## wzp-crypto

**Path**: `crates/wzp-crypto/src/`

### Re-exports (`lib.rs`)

```rust
pub use anti_replay::AntiReplayWindow;
pub use handshake::WarzoneKeyExchange;
pub use nonce::{build_nonce, Direction};
pub use rekey::RekeyManager;
pub use session::ChaChaSession;
pub use wzp_proto::{CryptoError, CryptoSession, KeyExchange};
```

### WarzoneKeyExchange (`handshake.rs`)

```rust
pub struct WarzoneKeyExchange { /* signing_key, x25519_static, ephemeral_secret */ }
```

Implements `KeyExchange` trait. Key derivation:
- Ed25519: `HKDF(seed, "warzone-ed25519-identity")`
- X25519: `HKDF(seed, "warzone-x25519-identity")`
- Session: `HKDF(X25519_DH_shared_secret, "warzone-session-key")`

### ChaChaSession (`session.rs`)

```rust
pub struct ChaChaSession { /* cipher, session_id, send_seq, recv_seq, rekey_mgr, pending_rekey_secret */ }
```

Key methods:
- `ChaChaSession::new(shared_secret: [u8; 32]) -> Self`
- Implements `CryptoSession` trait

### AntiReplayWindow (`anti_replay.rs`)

```rust
pub struct AntiReplayWindow { /* highest: u16, bitmap: Vec<u64>, initialized: bool */ }
```

Key methods:
- `AntiReplayWindow::new() -> Self` -- 1024-packet window
- `AntiReplayWindow::check_and_update(&mut self, seq: u16) -> Result<(), CryptoError>`

### Nonce Construction (`nonce.rs`)

```rust
pub enum Direction { Send = 0, Recv = 1 }
pub fn build_nonce(session_id: &[u8; 4], seq: u32, direction: Direction) -> [u8; 12]
```

### RekeyManager (`rekey.rs`)

```rust
pub struct RekeyManager { /* current_key, last_rekey_at */ }
```

Key methods:
- `RekeyManager::new(initial_key: [u8; 32]) -> Self`
- `RekeyManager::should_rekey(&self, packet_count: u64) -> bool` -- every 2^16 packets
- `RekeyManager::perform_rekey(&mut self, new_peer_pub: &[u8; 32], our_new_secret: StaticSecret, packet_count: u64) -> [u8; 32]`

---

## wzp-transport

**Path**: `crates/wzp-transport/src/`

### Re-exports (`lib.rs`)

```rust
pub use config::{client_config, server_config};
pub use connection::{accept, connect, create_endpoint};
pub use path_monitor::PathMonitor;
pub use quic::QuinnTransport;
pub use wzp_proto::{MediaTransport, PathQuality, TransportError};
```

### QuinnTransport (`quic.rs`)

```rust
pub struct QuinnTransport { /* connection: quinn::Connection, path_monitor: Mutex<PathMonitor> */ }
```

Key methods:
- `QuinnTransport::new(connection: quinn::Connection) -> Self`
- `QuinnTransport::connection(&self) -> &quinn::Connection`
- `QuinnTransport::max_datagram_size(&self) -> Option<usize>`
- Implements `MediaTransport` trait

### Configuration (`config.rs`)

```rust
/// Create a server configuration with a self-signed certificate.
pub fn server_config() -> (quinn::ServerConfig, Vec<u8>)

/// Create a client configuration that trusts any certificate (testing).
pub fn client_config() -> quinn::ClientConfig
```

QUIC parameters: ALPN `wzp`, 30s idle timeout, 5s keepalive, 256KB receive window, 128KB send window, 300ms initial RTT.

### Connection Lifecycle (`connection.rs`)

```rust
pub fn create_endpoint(bind_addr: SocketAddr, server_config: Option<quinn::ServerConfig>) -> Result<quinn::Endpoint, TransportError>
pub async fn connect(endpoint: &quinn::Endpoint, addr: SocketAddr, server_name: &str, config: quinn::ClientConfig) -> Result<quinn::Connection, TransportError>
pub async fn accept(endpoint: &quinn::Endpoint) -> Result<quinn::Connection, TransportError>
```

### PathMonitor (`path_monitor.rs`)

```rust
pub struct PathMonitor { /* EWMA state for loss, RTT, jitter, bandwidth */ }
```

Key methods:
- `PathMonitor::new() -> Self`
- `PathMonitor::observe_sent(&mut self, seq: u16, timestamp_ms: u64)`
- `PathMonitor::observe_received(&mut self, seq: u16, timestamp_ms: u64)`
- `PathMonitor::observe_rtt(&mut self, rtt_ms: u32)`
- `PathMonitor::quality(&self) -> PathQuality`

### Datagram Helpers (`datagram.rs`)

```rust
pub fn serialize_media(packet: &MediaPacket) -> Bytes
pub fn deserialize_media(data: Bytes) -> Option<MediaPacket>
pub fn max_datagram_payload(connection: &quinn::Connection) -> Option<usize>
```

### Reliable Stream Framing (`reliable.rs`)

```rust
pub async fn send_signal(connection: &Connection, msg: &SignalMessage) -> Result<(), TransportError>
pub async fn recv_signal(recv: &mut quinn::RecvStream) -> Result<SignalMessage, TransportError>
```

Framing: 4-byte big-endian length prefix + serde_json payload. Max message size: 1 MB.

---

## wzp-relay

**Path**: `crates/wzp-relay/src/`

### Re-exports (`lib.rs`)

```rust
pub use config::RelayConfig;
pub use handshake::accept_handshake;
pub use pipeline::{PipelineConfig, PipelineStats, RelayPipeline};
pub use session_mgr::{RelaySession, SessionId, SessionManager};
```

### RoomManager (`room.rs`)

```rust
pub type ParticipantId = u64;
pub struct RoomManager { /* rooms: HashMap<String, Room> */ }
```

Key methods:
- `RoomManager::new() -> Self`
- `RoomManager::join(&mut self, room_name: &str, addr: SocketAddr, transport: Arc<QuinnTransport>) -> ParticipantId`
- `RoomManager::leave(&mut self, room_name: &str, participant_id: ParticipantId)`
- `RoomManager::others(&self, room_name: &str, participant_id: ParticipantId) -> Vec<Arc<QuinnTransport>>`
- `RoomManager::room_size(&self, room_name: &str) -> usize`
- `RoomManager::list(&self) -> Vec<(String, usize)>`

```rust
/// Run the receive loop for one participant in a room (forwards to all others).
pub async fn run_participant(room_mgr: Arc<Mutex<RoomManager>>, room_name: String, participant_id: ParticipantId, transport: Arc<QuinnTransport>)
```

### RelayPipeline (`pipeline.rs`)

```rust
pub struct PipelineConfig { pub initial_profile: QualityProfile, pub jitter_target: usize, pub jitter_max: usize, pub jitter_min: usize }
pub struct PipelineStats { pub packets_received: u64, pub packets_forwarded: u64, pub packets_fec_recovered: u64, pub packets_lost: u64, pub profile_changes: u64 }
pub struct RelayPipeline { /* fec_encoder, fec_decoder, jitter, quality, profile, out_seq, stats */ }
```

Key methods:
- `RelayPipeline::new(config: PipelineConfig) -> Self`
- `RelayPipeline::ingest(&mut self, packet: MediaPacket) -> Vec<MediaPacket>` -- FEC decode + jitter pop
- `RelayPipeline::prepare_outbound(&mut self, packet: MediaPacket) -> Vec<MediaPacket>` -- assign seq + FEC encode
- `RelayPipeline::stats(&self) -> &PipelineStats`
- `RelayPipeline::profile(&self) -> QualityProfile`

### SessionManager (`session_mgr.rs`)

```rust
pub type SessionId = [u8; 16];
pub struct RelaySession { pub state: Session, pub upstream_pipeline: RelayPipeline, pub downstream_pipeline: RelayPipeline, pub profile: QualityProfile, pub last_activity_ms: u64 }
pub struct SessionManager { /* sessions: HashMap<SessionId, RelaySession>, max_sessions */ }
```

Key methods:
- `SessionManager::new(max_sessions: usize) -> Self`
- `SessionManager::create_session(&mut self, session_id: SessionId, config: PipelineConfig) -> Option<&mut RelaySession>`
- `SessionManager::get_session(&mut self, id: &SessionId) -> Option<&mut RelaySession>`
- `SessionManager::remove_session(&mut self, id: &SessionId) -> Option<RelaySession>`
- `SessionManager::expire_idle(&mut self, now_ms: u64, timeout_ms: u64) -> usize`

### Handshake (`handshake.rs`)

```rust
/// Accept the relay (callee) side of the cryptographic handshake.
pub async fn accept_handshake(transport: &dyn MediaTransport, seed: &[u8; 32]) -> Result<(Box<dyn CryptoSession>, QualityProfile), anyhow::Error>
```

### RelayConfig (`config.rs`)

```rust
pub struct RelayConfig {
    pub listen_addr: SocketAddr,         // default: 0.0.0.0:4433
    pub remote_relay: Option<SocketAddr>, // None = room mode
    pub max_sessions: usize,             // default: 100
    pub jitter_target_depth: usize,      // default: 50
    pub jitter_max_depth: usize,         // default: 250
    pub log_level: String,               // default: "info"
}
```

---

## wzp-client

**Path**: `crates/wzp-client/src/`

### Re-exports (`lib.rs`)

```rust
#[cfg(feature = "audio")]
pub use audio_io::{AudioCapture, AudioPlayback};
pub use call::{CallConfig, CallDecoder, CallEncoder};
pub use handshake::perform_handshake;
```

### CallEncoder (`call.rs`)

```rust
pub struct CallEncoder { /* audio_enc, fec_enc, profile, seq, block_id, frame_in_block, timestamp_ms */ }
```

Key methods:
- `CallEncoder::new(config: &CallConfig) -> Self`
- `CallEncoder::encode_frame(&mut self, pcm: &[i16]) -> Result<Vec<MediaPacket>, anyhow::Error>` -- returns source + repair packets
- `CallEncoder::set_profile(&mut self, profile: QualityProfile) -> Result<(), anyhow::Error>`

### CallDecoder (`call.rs`)

```rust
pub struct CallDecoder { /* audio_dec, fec_dec, jitter, quality, profile */ }
```

Key methods:
- `CallDecoder::new(config: &CallConfig) -> Self`
- `CallDecoder::ingest(&mut self, packet: MediaPacket)` -- feeds FEC decoder and jitter buffer
- `CallDecoder::decode_next(&mut self, pcm: &mut [i16]) -> Option<usize>` -- pops from jitter, decodes
- `CallDecoder::profile(&self) -> QualityProfile`
- `CallDecoder::jitter_stats(&self) -> JitterStats`

### CallConfig (`call.rs`)

```rust
pub struct CallConfig {
    pub profile: QualityProfile,  // default: GOOD
    pub jitter_target: usize,     // default: 10
    pub jitter_max: usize,        // default: 250
    pub jitter_min: usize,        // default: 3
}
```

### Client Handshake (`handshake.rs`)

```rust
/// Perform the client (caller) side of the cryptographic handshake.
pub async fn perform_handshake(transport: &dyn MediaTransport, seed: &[u8; 32]) -> Result<Box<dyn CryptoSession>, anyhow::Error>
```

### Echo Test (`echo_test.rs`)

```rust
pub struct WindowResult { pub index: usize, pub time_offset_secs: f64, pub frames_sent: u32, pub frames_received: u32, pub loss_pct: f32, pub snr_db: f32, pub correlation: f32, pub peak_amplitude: i16, pub is_silent: bool }
pub struct EchoTestResult { pub duration_secs: f64, pub total_frames_sent: u64, pub total_frames_received: u64, pub overall_loss_pct: f32, pub windows: Vec<WindowResult>, /* ... */ }

pub async fn run_echo_test(transport: &(dyn MediaTransport + Send + Sync), duration_secs: u32, window_secs: f64) -> anyhow::Result<EchoTestResult>
pub fn print_report(result: &EchoTestResult)
```

### Audio I/O (`audio_io.rs`, requires `audio` feature)

```rust
pub struct AudioCapture { /* rx: mpsc::Receiver<Vec<i16>>, running: Arc<AtomicBool> */ }
pub struct AudioPlayback { /* tx: mpsc::SyncSender<Vec<i16>>, running: Arc<AtomicBool> */ }
```

Key methods:
- `AudioCapture::start() -> Result<Self, anyhow::Error>` -- opens default input at 48 kHz mono
- `AudioCapture::read_frame(&self) -> Option<Vec<i16>>` -- blocking, returns 960 samples
- `AudioCapture::stop(&self)`
- `AudioPlayback::start() -> Result<Self, anyhow::Error>` -- opens default output at 48 kHz mono
- `AudioPlayback::write_frame(&self, pcm: &[i16])`
- `AudioPlayback::stop(&self)`

### Benchmarks (`bench.rs`)

```rust
pub struct CodecResult { pub frames: usize, pub avg_encode_us: f64, pub avg_decode_us: f64, pub frames_per_sec: f64, pub compression_ratio: f64, /* ... */ }
pub struct FecResult { pub blocks_attempted: usize, pub blocks_recovered: usize, pub recovery_rate_pct: f64, /* ... */ }
pub struct CryptoResult { pub packets: usize, pub packets_per_sec: f64, pub megabytes_per_sec: f64, pub avg_latency_us: f64, /* ... */ }
pub struct PipelineResult { pub frames: usize, pub avg_e2e_latency_us: f64, pub overhead_ratio: f64, /* ... */ }

pub fn generate_sine_wave(freq_hz: f32, sample_rate: u32, num_samples: usize) -> Vec<i16>
pub fn bench_codec_roundtrip() -> CodecResult        // 1000 frames Opus 24kbps
pub fn bench_fec_recovery(loss_pct: f32) -> FecResult // 100 blocks with simulated loss
pub fn bench_encrypt_decrypt() -> CryptoResult        // 30000 packets ChaCha20
pub fn bench_full_pipeline() -> PipelineResult        // 50 frames E2E
```

---

## wzp-web

**Path**: `crates/wzp-web/src/`

The web bridge binary. No public library API -- it is a standalone Axum server.

### Binary: `wzp-web`

- Serves static files from `crates/wzp-web/static/`
- WebSocket endpoint: `GET /ws/{room}` -- upgrades to WebSocket
- Each WebSocket client gets a QUIC connection to the relay with the room name as SNI
- Browser -> relay: WebSocket binary messages (960 Int16 samples as raw bytes) -> `CallEncoder` -> `MediaTransport::send_media()`
- Relay -> browser: `MediaTransport::recv_media()` -> `CallDecoder` -> WebSocket binary messages

### Static Files

- `static/index.html` -- web UI with room input, connect/disconnect, PTT, level meter
- `static/audio-processor.js` -- AudioWorklet for microphone capture (960-sample frames)
- `static/playback-processor.js` -- AudioWorklet for audio playback (ring buffer, 200ms max)
