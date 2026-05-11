use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::{CodecId, MediaType};

/// v2 media header alias. All production code uses this type.
pub type MediaHeader = MediaHeaderV2;

/// 16-byte v2 media header. See docs/PRD/PRD-wire-format-v2.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaHeaderV2 {
    /// Protocol version (always 2 for v2).
    pub version: u8,
    /// Bit flags: bit 7 T (repair), bit 6 Q (quality report), bit 5 KeyFrame, bit 4 FrameEnd.
    pub flags: u8,
    /// Media stream type (Audio, Video, Data, Control).
    pub media_type: MediaType,
    /// Codec identifier.
    pub codec_id: CodecId,
    /// Stream identifier within the session (0 for default audio).
    pub stream_id: u8,
    /// FEC ratio encoded as 0..200, mapping to 0.0..2.0.
    pub fec_ratio: u8,
    /// Wrapping packet sequence number (32-bit in v2).
    pub seq: u32,
    /// Milliseconds since session start.
    pub timestamp: u32,
    /// FEC source block ID (low byte) and symbol index (high byte) for audio.
    pub fec_block: u16,
}

impl MediaHeaderV2 {
    /// Header size in bytes on the wire (16 for v2).
    pub const WIRE_SIZE: usize = 16;
    /// Protocol version byte (always 2).
    pub const VERSION: u8 = 2;

    /// Serialize the header to a buffer in big-endian wire format.
    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u8(self.version);
        buf.put_u8(self.flags);
        buf.put_u8(self.media_type.to_wire());
        buf.put_u8(self.codec_id.to_wire());
        buf.put_u8(self.stream_id);
        buf.put_u8(self.fec_ratio);
        buf.put_u32(self.seq);
        buf.put_u32(self.timestamp);
        buf.put_u16(self.fec_block);
    }

    /// Deserialize from a buffer. Returns `None` if the buffer is too short
    /// or the version byte is not 2.
    pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::WIRE_SIZE {
            return None;
        }
        let version = buf.get_u8();
        if version != Self::VERSION {
            return None;
        }
        let flags = buf.get_u8();
        let media_type = MediaType::from_wire(buf.get_u8())?;
        let codec_id = CodecId::from_wire(buf.get_u8())?;
        let stream_id = buf.get_u8();
        let fec_ratio = buf.get_u8();
        let seq = buf.get_u32();
        let timestamp = buf.get_u32();
        let fec_block = buf.get_u16();
        Some(Self {
            version,
            flags,
            media_type,
            codec_id,
            stream_id,
            fec_ratio,
            seq,
            timestamp,
            fec_block,
        })
    }

    /// Bit 7: set when this packet is an FEC repair packet, not source media.
    pub const FLAG_REPAIR: u8 = 0b1000_0000;
    /// Bit 6: set when a [`QualityReport`] trailer is appended to the payload.
    pub const FLAG_QUALITY: u8 = 0b0100_0000;
    /// Bit 5: set for video keyframes (reserved for future video use).
    pub const FLAG_KEYFRAME: u8 = 0b0010_0000;
    /// Bit 4: set when this packet is the final fragment of a frame.
    pub const FLAG_FRAME_END: u8 = 0b0001_0000;

    /// Returns true if the repair flag is set.
    pub fn is_repair(&self) -> bool {
        self.flags & Self::FLAG_REPAIR != 0
    }
    /// Returns true if the quality-report flag is set.
    pub fn has_quality(&self) -> bool {
        self.flags & Self::FLAG_QUALITY != 0
    }
    /// Returns true if the keyframe flag is set.
    pub fn is_keyframe(&self) -> bool {
        self.flags & Self::FLAG_KEYFRAME != 0
    }
    /// Returns true if the frame-end flag is set.
    pub fn is_frame_end(&self) -> bool {
        self.flags & Self::FLAG_FRAME_END != 0
    }

    /// Encode the FEC ratio float (0.0-2.0) to an 8-bit value (0-200).
    pub fn encode_fec_ratio(ratio: f32) -> u8 {
        (ratio * 100.0).round() as u8
    }

    /// Decode the 8-bit FEC ratio value back to a float.
    pub fn decode_fec_ratio(encoded: u8) -> f32 {
        encoded as f32 / 100.0
    }

    /// Serialize header to a new Bytes value.
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::WIRE_SIZE);
        self.write_to(&mut buf);
        buf.freeze()
    }
}

/// A user visible in the signal presence list.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PresenceUser {
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// Quality report appended to a media packet when Q flag is set (4 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityReport {
    /// Observed loss percentage (0-255 maps to 0-100%).
    pub loss_pct: u8,
    /// RTT estimate in 4ms units (0-255 = 0-1020ms).
    pub rtt_4ms: u8,
    /// Jitter in milliseconds.
    pub jitter_ms: u8,
    /// Maximum receive bitrate in kbps.
    pub bitrate_cap_kbps: u8,
}

impl QualityReport {
    pub const WIRE_SIZE: usize = 4;

    pub fn loss_percent(&self) -> f32 {
        self.loss_pct as f32 / 255.0 * 100.0
    }

    pub fn rtt_ms(&self) -> u16 {
        self.rtt_4ms as u16 * 4
    }

    /// Construct a QualityReport from locally-observed path statistics.
    ///
    /// Used by the send task to embed quality data in outgoing packets so
    /// the peer's recv task (or relay) can drive adaptive quality switching.
    pub fn from_path_stats(loss_pct: f32, rtt_ms: u32, jitter_ms: u32) -> Self {
        Self {
            loss_pct: (loss_pct / 100.0 * 255.0).clamp(0.0, 255.0) as u8,
            rtt_4ms: (rtt_ms / 4).min(255) as u8,
            jitter_ms: jitter_ms.min(255) as u8,
            bitrate_cap_kbps: 200,
        }
    }

    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u8(self.loss_pct);
        buf.put_u8(self.rtt_4ms);
        buf.put_u8(self.jitter_ms);
        buf.put_u8(self.bitrate_cap_kbps);
    }

    pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::WIRE_SIZE {
            return None;
        }
        Some(Self {
            loss_pct: buf.get_u8(),
            rtt_4ms: buf.get_u8(),
            jitter_ms: buf.get_u8(),
            bitrate_cap_kbps: buf.get_u8(),
        })
    }
}

/// A complete media packet (header + payload + optional quality report).
#[derive(Clone, Debug)]
pub struct MediaPacket {
    pub header: MediaHeader,
    pub payload: Bytes,
    pub quality_report: Option<QualityReport>,
}

/// Maximum number of mini-frames between full headers (1 second at 50 fps).
pub const MINI_FRAME_FULL_INTERVAL: u32 = 50;

impl MediaPacket {
    /// Serialize the entire packet to bytes.
    pub fn to_bytes(&self) -> Bytes {
        let qr_size = if self.quality_report.is_some() {
            QualityReport::WIRE_SIZE
        } else {
            0
        };
        let total = MediaHeader::WIRE_SIZE + self.payload.len() + qr_size;
        let mut buf = BytesMut::with_capacity(total);

        self.header.write_to(&mut buf);
        buf.put(self.payload.clone());
        if let Some(ref qr) = self.quality_report {
            qr.write_to(&mut buf);
        }

        buf.freeze()
    }

    /// Deserialize from bytes. `payload_len` must be known from context
    /// (e.g., total packet size minus header minus optional QR).
    pub fn from_bytes(data: Bytes) -> Option<Self> {
        let mut cursor = &data[..];
        let header = MediaHeader::read_from(&mut cursor)?;

        let remaining = data.len() - MediaHeader::WIRE_SIZE;
        let (payload_len, quality_report) = if header.has_quality() {
            if remaining < QualityReport::WIRE_SIZE {
                return None;
            }
            let pl = remaining - QualityReport::WIRE_SIZE;
            let qr_start = MediaHeader::WIRE_SIZE + pl;
            let mut qr_cursor = &data[qr_start..];
            let qr = QualityReport::read_from(&mut qr_cursor)?;
            (pl, Some(qr))
        } else {
            (remaining, None)
        };

        let payload = data.slice(MediaHeader::WIRE_SIZE..MediaHeader::WIRE_SIZE + payload_len);

        Some(Self {
            header,
            payload,
            quality_report,
        })
    }

    /// Serialize with mini-frame compression.
    ///
    /// Uses the `MiniFrameContext` to decide whether to emit a compact 4-byte
    /// mini-header or a full 12-byte header.  A full header is forced on the
    /// first frame and every `MINI_FRAME_FULL_INTERVAL` frames thereafter.
    pub fn encode_compact(&self, ctx: &mut MiniFrameContext, frames_since_full: &mut u32) -> Bytes {
        if *frames_since_full > 0 && *frames_since_full < MINI_FRAME_FULL_INTERVAL {
            if let Some(base) = ctx.last_header() {
                // --- mini frame ---
                let ts_delta = self.header.timestamp.wrapping_sub(base.timestamp) as u16;
                let mini = MiniHeader {
                    seq_delta: 1,
                    timestamp_delta_ms: ts_delta,
                    payload_len: self.payload.len() as u16,
                };
                let total = 1 + MiniHeader::WIRE_SIZE + self.payload.len();
                let mut buf = BytesMut::with_capacity(total);
                buf.put_u8(FRAME_TYPE_MINI);
                mini.write_to(&mut buf);
                buf.put(self.payload.clone());
                // Advance the context so the next mini-frame delta is relative
                // to this frame, mirroring what expand() does on the decoder side.
                ctx.update(&self.header);
                *frames_since_full += 1;
                return buf.freeze();
            }
        }

        // --- full frame ---
        let qr_size = if self.quality_report.is_some() {
            QualityReport::WIRE_SIZE
        } else {
            0
        };
        let total = 1 + MediaHeader::WIRE_SIZE + self.payload.len() + qr_size;
        let mut buf = BytesMut::with_capacity(total);
        buf.put_u8(FRAME_TYPE_FULL);
        self.header.write_to(&mut buf);
        buf.put(self.payload.clone());
        if let Some(ref qr) = self.quality_report {
            qr.write_to(&mut buf);
        }
        ctx.update(&self.header);
        *frames_since_full = 1; // next frame will be the 1st after full
        buf.freeze()
    }

    /// Decode from compact wire format (auto-detects full vs mini).
    ///
    /// Returns `None` on malformed input or if a mini-frame arrives before any
    /// full header baseline has been established.
    pub fn decode_compact(buf: &[u8], ctx: &mut MiniFrameContext) -> Option<Self> {
        if buf.is_empty() {
            return None;
        }
        let frame_type = buf[0];
        let rest = &buf[1..];

        match frame_type {
            FRAME_TYPE_FULL => {
                let pkt = Self::from_bytes(Bytes::copy_from_slice(rest))?;
                ctx.update(&pkt.header);
                Some(pkt)
            }
            FRAME_TYPE_MINI => {
                if rest.len() < MiniHeader::WIRE_SIZE {
                    return None;
                }
                let mut cursor = rest;
                let mini = MiniHeader::read_from(&mut cursor)?;
                let payload_start = 1 + MiniHeader::WIRE_SIZE;
                let payload_end = payload_start + mini.payload_len as usize;
                if buf.len() < payload_end {
                    return None;
                }
                let payload = Bytes::copy_from_slice(&buf[payload_start..payload_end]);
                let header = ctx.expand(&mini)?;
                Some(Self {
                    header,
                    payload,
                    quality_report: None,
                })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Trunking — multiplex multiple session packets into one QUIC datagram
// ---------------------------------------------------------------------------

/// A single entry inside a [`TrunkFrame`].
#[derive(Clone, Debug)]
pub struct TrunkEntry {
    /// 2-byte session identifier (up to 65 536 sessions).
    pub session_id: [u8; 2],
    /// Encoded MediaPacket payload (already compressed).
    pub payload: Bytes,
}

impl TrunkEntry {
    /// Per-entry wire overhead: 2 (session_id) + 2 (len).
    pub const OVERHEAD: usize = 4;
}

/// A trunked frame carrying multiple session packets in one datagram.
///
/// Wire format:
/// ```text
/// [count:u16] [entry1] [entry2] ...
/// ```
/// Each entry:
/// ```text
/// [session_id:2] [len:u16] [payload:len]
/// ```
#[derive(Clone, Debug)]
pub struct TrunkFrame {
    pub packets: Vec<TrunkEntry>,
}

impl Default for TrunkFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl TrunkFrame {
    /// Create an empty trunk frame.
    pub fn new() -> Self {
        Self {
            packets: Vec::new(),
        }
    }

    /// Append a session packet to the frame.
    pub fn push(&mut self, session_id: [u8; 2], payload: Bytes) {
        self.packets.push(TrunkEntry {
            session_id,
            payload,
        });
    }

    /// Number of entries in the frame.
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// Whether the frame is empty.
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Total wire size of the encoded frame.
    pub fn wire_size(&self) -> usize {
        // 2 bytes for count + each entry
        2 + self
            .packets
            .iter()
            .map(|e| TrunkEntry::OVERHEAD + e.payload.len())
            .sum::<usize>()
    }

    /// Encode to wire bytes.
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.wire_size());
        buf.put_u16(self.packets.len() as u16);
        for entry in &self.packets {
            buf.put_slice(&entry.session_id);
            buf.put_u16(entry.payload.len() as u16);
            buf.put(entry.payload.clone());
        }
        buf.freeze()
    }

    /// Decode from wire bytes. Returns `None` on malformed input.
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 2 {
            return None;
        }
        let mut cursor = buf;
        let count = cursor.get_u16() as usize;
        let mut packets = Vec::with_capacity(count);
        for _ in 0..count {
            if cursor.remaining() < TrunkEntry::OVERHEAD {
                return None;
            }
            let mut session_id = [0u8; 2];
            session_id[0] = cursor.get_u8();
            session_id[1] = cursor.get_u8();
            let len = cursor.get_u16() as usize;
            if cursor.remaining() < len {
                return None;
            }
            let payload = Bytes::copy_from_slice(&cursor[..len]);
            cursor.advance(len);
            packets.push(TrunkEntry {
                session_id,
                payload,
            });
        }
        Some(Self { packets })
    }
}

// ---------------------------------------------------------------------------
// Mini-frames — compact header for steady-state media packets
// ---------------------------------------------------------------------------

/// Frame type tag: full MediaHeader follows.
pub const FRAME_TYPE_FULL: u8 = 0x00;
/// Frame type tag: MiniHeader follows (requires prior baseline).
pub const FRAME_TYPE_MINI: u8 = 0x01;

/// v2 mini header alias. All production code uses this type.
pub type MiniHeader = MiniHeaderV2;

/// Compact 5-byte v2 mini header with explicit `seq_delta`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MiniHeaderV2 {
    /// Packets since the baseline full header (typically 1 in steady state).
    /// Explicit deltas resolve audit W4: one missed full header no longer desyncs.
    pub seq_delta: u8,
    /// Milliseconds elapsed since the last baseline header's timestamp.
    pub timestamp_delta_ms: u16,
    /// Length of the payload that follows this mini header.
    pub payload_len: u16,
}

impl MiniHeaderV2 {
    /// Header size in bytes on the wire (5 for v2).
    pub const WIRE_SIZE: usize = 5;

    /// Serialize the mini header to a buffer in big-endian wire format.
    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u8(self.seq_delta);
        buf.put_u16(self.timestamp_delta_ms);
        buf.put_u16(self.payload_len);
    }

    /// Deserialize from a buffer. Returns `None` if the buffer is too short.
    pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::WIRE_SIZE {
            return None;
        }
        Some(Self {
            seq_delta: buf.get_u8(),
            timestamp_delta_ms: buf.get_u16(),
            payload_len: buf.get_u16(),
        })
    }
}

/// v2 mini frame context alias. All production code uses this type.
pub type MiniFrameContext = MiniFrameContextV2;

/// Stateful v2 context that expands [`MiniHeaderV2`]s back into full
/// [`MediaHeaderV2`]s by tracking the last baseline header.
#[derive(Clone, Debug, Default)]
pub struct MiniFrameContextV2 {
    last: Option<MediaHeaderV2>,
}

impl MiniFrameContextV2 {
    /// Record a full v2 header as the new baseline for subsequent mini-frames.
    pub fn update(&mut self, h: &MediaHeaderV2) {
        self.last = Some(*h);
    }

    /// Expand a mini-header into a full [`MediaHeaderV2`] using the stored
    /// baseline. Returns `None` if no baseline has been set yet.
    pub fn expand(&mut self, m: &MiniHeaderV2) -> Option<MediaHeaderV2> {
        let base = self.last.as_ref()?;
        let mut e = *base;
        e.seq = base.seq.wrapping_add(m.seq_delta as u32);
        e.timestamp = base.timestamp.wrapping_add(m.timestamp_delta_ms as u32);
        self.last = Some(e);
        Some(e)
    }

    /// Return a reference to the last baseline header, if any.
    pub fn last_header(&self) -> Option<&MediaHeaderV2> {
        self.last.as_ref()
    }
}

/// Signaling messages sent over the reliable QUIC stream.
///
/// Compatible with Warzone messenger's identity model:
/// - Identity keys are Ed25519 (signing) + X25519 (encryption) derived from a 32-byte seed via HKDF
/// - Fingerprint = SHA-256(Ed25519 public key)[:16]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SignalMessage {
    /// Call initiation (analogous to Warzone's WireMessage::CallOffer).
    CallOffer {
        /// Caller's Ed25519 identity public key (32 bytes).
        identity_pub: [u8; 32],
        /// Ephemeral X25519 public key for this call.
        ephemeral_pub: [u8; 32],
        /// Ed25519 signature over (ephemeral_pub || callee_fingerprint).
        signature: Vec<u8>,
        /// Supported quality profiles.
        supported_profiles: Vec<crate::QualityProfile>,
        /// Optional display name set by the caller.
        #[serde(default)]
        alias: Option<String>,
    },

    /// Call acceptance (analogous to Warzone's WireMessage::CallAnswer).
    CallAnswer {
        /// Callee's Ed25519 identity public key (32 bytes).
        identity_pub: [u8; 32],
        /// Callee's ephemeral X25519 public key.
        ephemeral_pub: [u8; 32],
        /// Ed25519 signature over (ephemeral_pub || caller_fingerprint).
        signature: Vec<u8>,
        /// Chosen quality profile.
        chosen_profile: crate::QualityProfile,
    },

    /// ICE candidate for NAT traversal.
    IceCandidate {
        candidate: String,
    },

    /// Periodic rekeying (forward secrecy).
    Rekey {
        /// New ephemeral X25519 public key.
        new_ephemeral_pub: [u8; 32],
        /// Ed25519 signature over (new_ephemeral_pub || session_id).
        signature: Vec<u8>,
    },

    /// Quality/profile change request.
    QualityUpdate {
        report: QualityReport,
        recommended_profile: crate::QualityProfile,
    },

    /// Phase 4 telemetry: loss-recovery counts for the current session.
    /// Sent periodically from receivers to the relay so Prometheus metrics
    /// can distinguish DRED reconstructions from classical PLC invocations.
    /// Fields default to 0 on old receivers (`#[serde(default)]`), so
    /// introducing this variant is backward-compatible with pre-Phase-4
    /// relays — they'll just log "unknown signal variant" on receipt.
    LossRecoveryUpdate {
        /// Total frames reconstructed via DRED since call start (monotonic).
        #[serde(default)]
        dred_reconstructions: u64,
        /// Total frames filled via classical Opus/Codec2 PLC since call
        /// start (monotonic).
        #[serde(default)]
        classical_plc_invocations: u64,
        /// Total frames decoded since call start. Used by the relay to
        /// compute recovery rates as a fraction of total frames.
        #[serde(default)]
        frames_decoded: u64,
    },

    /// Connection keepalive / RTT measurement.
    Ping {
        timestamp_ms: u64,
    },
    Pong {
        timestamp_ms: u64,
    },

    /// End the call. `call_id` is optional for backwards compatibility
    /// with older clients that send Hangup without it — the relay falls
    /// back to ending ALL active calls for the sender in that case.
    Hangup {
        reason: HangupReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },

    /// featherChat bearer token for relay authentication.
    /// Sent as the first signal message when --auth-url is configured.
    AuthToken {
        token: String,
    },

    /// Put the call on hold (stop sending media, keep session alive).
    Hold,
    /// Resume a held call.
    Unhold,
    /// Mute request from the remote side (server-initiated mute, like IAX2 QUELCH).
    Mute,
    /// Unmute request from the remote side (like IAX2 UNQUELCH).
    Unmute,
    /// Transfer the call to another peer.
    Transfer {
        target_fingerprint: String,
        /// Optional relay address for the transfer target.
        relay_addr: Option<String>,
    },
    /// Acknowledge a transfer request.
    TransferAck,

    /// Presence update from a peer relay (gossip protocol).
    /// Sent periodically over probe connections to share which fingerprints
    /// are connected to the sending relay.
    PresenceUpdate {
        /// Fingerprints currently connected to the sending relay.
        fingerprints: Vec<String>,
        /// Address of the sending relay (e.g., "192.168.1.10:4433").
        relay_addr: String,
    },

    /// Ask a peer relay to look up a fingerprint in its registry.
    RouteQuery {
        fingerprint: String,
        ttl: u8,
    },
    /// Response to a route query.
    RouteResponse {
        fingerprint: String,
        found: bool,
        relay_chain: Vec<String>,
    },

    /// Request to set up a forwarding session for a specific fingerprint.
    /// Sent over a relay link (`_relay` SNI) to ask the peer relay to
    /// create a room and forward media for the given session.
    SessionForward {
        session_id: String,
        target_fingerprint: String,
        source_relay: String,
    },
    /// Confirm that the forwarding session has been set up on the peer relay.
    /// The `room_name` tells the source relay which room to address media to.
    SessionForwardAck {
        session_id: String,
        room_name: String,
    },

    /// Room membership update — sent by relay to all participants when someone joins or leaves.
    RoomUpdate {
        /// Current participant count.
        count: u32,
        /// List of participants currently in the room.
        participants: Vec<RoomParticipant>,
    },

    // ── Federation signals (relay-to-relay) ──
    /// Federation: initial handshake — the connecting relay identifies itself.
    FederationHello {
        /// TLS certificate fingerprint of the connecting relay.
        tls_fingerprint: String,
    },

    /// Federation: this relay now has local participants in a global room.
    GlobalRoomActive {
        room: String,
        /// Participants on the announcing relay (for federated presence).
        #[serde(default)]
        participants: Vec<RoomParticipant>,
    },

    /// Federation: this relay's last local participant left a global room.
    GlobalRoomInactive {
        room: String,
    },

    // ── Direct calling signals (client ↔ relay signaling) ──
    /// Register on relay for direct calls. Sent on `_signal` connections
    /// after optional AuthToken.
    RegisterPresence {
        /// Client's Ed25519 identity public key.
        identity_pub: [u8; 32],
        /// Signature over ("register-presence" || identity_pub).
        signature: Vec<u8>,
        /// Optional display name.
        alias: Option<String>,
    },

    /// Relay confirms presence registration.
    RegisterPresenceAck {
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Relay's build version (git short hash).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_build: Option<String>,
        /// Phase 8: relay's geographic region (e.g., "us-east", "eu-west").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_region: Option<String>,
        /// Phase 8: other relays the client can use, sorted by relay
        /// mesh proximity. Each entry is "name|addr" (e.g., "eu-west|203.0.113.5:4433").
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        available_relays: Vec<String>,
    },

    /// Direct call offer routed through the relay to a specific peer.
    DirectCallOffer {
        /// Caller's fingerprint.
        caller_fingerprint: String,
        /// Caller's display name.
        caller_alias: Option<String>,
        /// Target's fingerprint.
        target_fingerprint: String,
        /// Unique call session ID (UUID).
        call_id: String,
        /// Caller's Ed25519 identity pub.
        identity_pub: [u8; 32],
        /// Caller's ephemeral X25519 pub (for key exchange on media connect).
        ephemeral_pub: [u8; 32],
        /// Signature over (ephemeral_pub || target_fingerprint || call_id).
        signature: Vec<u8>,
        /// Supported quality profiles.
        supported_profiles: Vec<crate::QualityProfile>,
        /// Phase 3 (hole-punching): caller's own server-reflexive
        /// address as learned via `SignalMessage::Reflect`. The
        /// relay stashes this in its call registry and later
        /// injects it into the callee's `CallSetup.peer_direct_addr`
        /// so the callee can try a direct QUIC handshake to the
        /// caller instead of routing media through the relay.
        /// `None` means "caller doesn't want P2P, use relay only".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_reflexive_addr: Option<String>,
        /// Phase 5.5 (ICE host candidates): caller's LAN-local
        /// interface addresses paired with its signal endpoint's
        /// port. Peers on the same physical LAN can direct-dial
        /// these without going through the WAN reflex addr,
        /// which is important because most consumer NATs
        /// (including MikroTik masquerade) don't support NAT
        /// hairpinning — the reflex addr is unreachable from
        /// the same LAN.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        caller_local_addrs: Vec<String>,
        /// Phase 8 (Tailscale-inspired): caller's port-mapped external
        /// address from NAT-PMP/PCP/UPnP. When the router supports
        /// port mapping, this gives a stable external address even
        /// behind symmetric NATs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_mapped_addr: Option<String>,
        /// Build version (git short hash) for debugging.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_build_version: Option<String>,
    },

    /// Callee's response to a direct call.
    DirectCallAnswer {
        call_id: String,
        /// How the callee accepts (or rejects).
        accept_mode: CallAcceptMode,
        /// Callee's identity pub (present when accepting).
        #[serde(skip_serializing_if = "Option::is_none")]
        identity_pub: Option<[u8; 32]>,
        /// Callee's ephemeral pub (present when accepting).
        #[serde(skip_serializing_if = "Option::is_none")]
        ephemeral_pub: Option<[u8; 32]>,
        /// Signature (present when accepting).
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<Vec<u8>>,
        /// Chosen quality profile (present when accepting).
        #[serde(skip_serializing_if = "Option::is_none")]
        chosen_profile: Option<crate::QualityProfile>,
        /// Phase 3 (hole-punching): callee's own server-reflexive
        /// address, only populated on `AcceptTrusted` — privacy-mode
        /// answers leave this `None` so the callee's real IP stays
        /// hidden (the whole point of `AcceptGeneric`). The relay
        /// carries it opaquely into the caller's `CallSetup`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callee_reflexive_addr: Option<String>,
        /// Phase 5.5 (ICE host candidates): callee's LAN-local
        /// interface addresses. Same purpose as
        /// `caller_local_addrs` in `DirectCallOffer`. Only
        /// populated on `AcceptTrusted` alongside
        /// `callee_reflexive_addr`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        callee_local_addrs: Vec<String>,
        /// Phase 8 (Tailscale-inspired): callee's port-mapped external
        /// address from NAT-PMP/PCP/UPnP.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callee_mapped_addr: Option<String>,
        /// Build version (git short hash) for debugging.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callee_build_version: Option<String>,
    },

    /// Relay tells both parties: media room is ready.
    CallSetup {
        call_id: String,
        /// Room name on the relay for the media session (e.g., "_call:a1b2c3d4").
        room: String,
        /// Relay address for the QUIC media connection.
        relay_addr: String,
        /// Phase 3 (hole-punching): the OTHER party's server-reflexive
        /// address as the relay learned it from the offer/answer
        /// exchange. When populated, clients attempt a direct QUIC
        /// handshake to this address in parallel with the existing
        /// relay path and use whichever connects first. `None`
        /// means the relay path is the only option — either because
        /// a peer didn't advertise its addr (Phase 1/2 relay or
        /// privacy-mode answer) or because the relay decided P2P
        /// wasn't viable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_direct_addr: Option<String>,
        /// Phase 5.5 (ICE host candidates): the OTHER party's LAN
        /// host addresses (RFC1918 IPv4 + CGNAT + non-link-local
        /// IPv6). On same-LAN calls these are directly dialable
        /// and bypass the NAT-hairpinning problem that blocks
        /// same-LAN peers from using `peer_direct_addr`.
        /// Client-side race tries all of these in parallel.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        peer_local_addrs: Vec<String>,
        /// Phase 8 (Tailscale-inspired): the OTHER party's port-mapped
        /// external address from NAT-PMP/PCP/UPnP. Added to the
        /// candidate dial order between host and reflexive addrs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        peer_mapped_addr: Option<String>,
    },

    /// Ringing notification (relay → caller, callee received the offer).
    CallRinging {
        call_id: String,
    },

    // ── NAT reflection ("STUN for QUIC") ──────────────────────────────
    /// Client → relay: "please tell me the source IP:port you see on
    /// this connection". A QUIC-native replacement for classic STUN
    /// that reuses the TLS-authenticated signal channel to the relay
    /// instead of running a separate UDP reflection service on port
    /// 3478. The relay answers with `ReflectResponse`.
    ///
    /// No payload — the relay already knows which connection the
    /// request arrived on, and `connection.remote_address()` gives it
    /// the exact source address (post-NAT) as observed from the
    /// server side of the TLS session.
    Reflect,

    /// Relay → client: response to `Reflect`. Carries the socket
    /// address the relay observes as the client's source for this
    /// QUIC connection in `SocketAddr::to_string()` form — "a.b.c.d:p"
    /// for IPv4, "[::1]:p" for IPv6. Clients parse it with
    /// `SocketAddr::from_str`.
    ReflectResponse {
        observed_addr: String,
    },

    // ── Phase 6: ICE-style path negotiation ─────────────────────
    /// Phase 6: each side reports the result of its local dual-
    /// path race to the other side through the relay. Both peers
    /// send this after their race completes; both wait for the
    /// other's report before committing a transport to the
    /// CallEngine.
    ///
    /// The decision rule is: if BOTH sides report `direct_ok =
    /// true`, use the direct P2P connection. If EITHER reports
    /// `direct_ok = false`, BOTH fall back to relay. This
    /// eliminates the race condition where one side picks Direct
    /// and the other picks Relay — they now agree on the path
    /// before any media flows.
    MediaPathReport {
        call_id: String,
        /// Did the direct QUIC connection (P2P dial or accept)
        /// complete successfully on this side?
        direct_ok: bool,
        /// Which future won the local tokio::select race?
        /// "Direct" or "Relay" — informational for debug logs.
        #[serde(default)]
        race_winner: String,
    },

    // ── Phase 8: mid-call ICE re-gathering ────────────────────────
    /// Phase 8 (Tailscale-inspired): mid-call candidate update sent
    /// when a client's network changes (WiFi → cellular, IP change,
    /// etc.). The relay forwards this to the call peer, who can
    /// re-race with the new candidates to upgrade or maintain the
    /// direct path.
    ///
    /// The `generation` counter is monotonically increasing per call
    /// — peers ignore updates with a generation <= their last-seen
    /// generation to handle reordering.
    CandidateUpdate {
        call_id: String,
        /// New server-reflexive address (STUN-discovered or relay-reflected).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reflexive_addr: Option<String>,
        /// New LAN host addresses.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        local_addrs: Vec<String>,
        /// New port-mapped address (NAT-PMP/PCP/UPnP).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mapped_addr: Option<String>,
        /// Monotonic generation counter.
        generation: u32,
    },

    // ── Hard NAT traversal (port prediction) ──────────────────────
    /// Hard NAT probe coordination — exchanged when both peers
    /// detect symmetric NAT. Carries the port allocation pattern
    /// and recent port sequence so the peer can predict which port
    /// to dial.
    HardNatProbe {
        call_id: String,
        /// Last observed external ports (most recent first).
        /// Typically 3-5 entries from sequential STUN probes.
        port_sequence: Vec<u16>,
        /// Detected allocation pattern as string:
        /// "sequential:N" (N=delta), "random", "preserving"
        allocation: String,
        /// Probe timestamp (ms since epoch) for synchronization.
        probe_time_ms: u64,
        /// External IP from STUN.
        external_ip: String,
    },

    /// Birthday attack coordination — Acceptor tells Dialer which
    /// ports it has open. The Dialer then sprays QUIC connects to
    /// these ports (and optionally random ports) on the Acceptor's IP.
    HardNatBirthdayStart {
        call_id: String,
        /// Number of sockets the Acceptor opened.
        acceptor_port_count: u16,
        /// External ports discovered via STUN (the "hit list").
        acceptor_ports: Vec<u16>,
        /// Acceptor's external IP.
        external_ip: String,
    },

    // ── Phase 4: cross-relay direct-call signaling ────────────────────
    /// Phase 4: relay-to-relay envelope for forwarding direct-call
    /// signaling across a federation link. When Alice on Relay A
    /// sends a `DirectCallOffer` for Bob whose fingerprint isn't
    /// in A's local SignalHub, Relay A wraps the offer in this
    /// envelope and broadcasts it over every active federation
    /// peer link. Whichever peer has Bob registered unwraps the
    /// inner message and delivers it locally.
    ///
    /// Never originated by clients — only relays create and
    /// consume this variant.
    ///
    /// Loop prevention: the receiving relay drops any forward
    /// where `origin_relay_fp` matches its own federation TLS
    /// fingerprint. With broadcast-to-all-peers this prevents
    /// A→B→A echo loops; proper TTL + dedup will land when
    /// multi-hop federation is added (Phase 4.2).
    FederatedSignalForward {
        /// The signal message being forwarded
        /// (`DirectCallOffer`, `DirectCallAnswer`, `CallRinging`,
        /// `Hangup`, ...). Boxed because `SignalMessage` is
        /// relatively large and JSON serde handles recursion
        /// cleanly.
        inner: Box<SignalMessage>,
        /// Federation TLS fingerprint of the sending relay.
        /// Used (a) for loop prevention by the receiver and (b)
        /// to route the peer's reply back through the same
        /// federation link via `send_signal_to_peer`.
        origin_relay_fp: String,
    },

    /// Relay-initiated quality directive: all participants should switch
    /// to the recommended profile to match the weakest link.
    QualityDirective {
        recommended_profile: crate::QualityProfile,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    // ── Signal presence ───────────────────────────────────────────
    /// Relay broadcasts the list of currently registered signal
    /// users to all connected clients. Sent on every register/
    /// deregister so clients can maintain a live lobby user list.
    PresenceList {
        /// List of online users. Each entry is { fingerprint, alias }.
        users: Vec<PresenceUser>,
    },

    // ── Quality upgrade negotiation (#28, #29) ──────────────────
    /// Peer proposes upgrading to a higher quality profile.
    /// The other side can accept or reject based on its own network
    /// conditions. Used for consensual upgrades that require both
    /// sides to agree (e.g., switching from Opus24k to Studio48k).
    UpgradeProposal {
        call_id: String,
        /// Unique ID for this proposal (to match response).
        proposal_id: String,
        /// The profile being proposed.
        proposed_profile: crate::QualityProfile,
        /// Current local network quality to justify the upgrade.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_loss_pct: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_rtt_ms: Option<u32>,
    },

    /// Response to an UpgradeProposal.
    UpgradeResponse {
        call_id: String,
        proposal_id: String,
        /// true = accepted, both sides switch. false = rejected.
        accepted: bool,
        /// Reason for rejection (if any).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// Confirmation that the upgrade is committed — both sides
    /// should switch encoder at the next frame boundary.
    UpgradeConfirm {
        call_id: String,
        proposal_id: String,
        confirmed_profile: crate::QualityProfile,
    },

    // ── Per-participant quality (#30) ───────────────────────────
    /// Peer reports its own quality capability — allows asymmetric
    /// encoding where each side uses the best quality its connection
    /// supports, rather than forcing all to the weakest link.
    QualityCapability {
        call_id: String,
        /// The best profile this peer can sustain based on its
        /// current network conditions.
        max_profile: crate::QualityProfile,
        /// Current loss/RTT for context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loss_pct: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtt_ms: Option<u32>,
    },
}

/// How the callee responds to a direct call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallAcceptMode {
    /// Reject the call.
    Reject,
    /// Accept with trust — in Phase 2, this enables P2P (reveals IP).
    /// In Phase 1, behaves the same as AcceptGeneric.
    AcceptTrusted,
    /// Accept with privacy — relay always mediates media.
    AcceptGeneric,
}

/// A participant entry in a RoomUpdate message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomParticipant {
    /// Identity fingerprint (hex string, stable across reconnects if seed is persisted).
    pub fingerprint: String,
    /// Optional display name set by the client.
    pub alias: Option<String>,
    /// Relay label — identifies which relay this participant is connected to.
    /// None for local participants, Some("Relay B") for federated.
    #[serde(default)]
    pub relay_label: Option<String>,
}

/// Reasons for ending a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HangupReason {
    Normal,
    Busy,
    Declined,
    Timeout,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_report_from_path_stats_basic() {
        let qr = QualityReport::from_path_stats(10.0, 100, 20);
        // 10.0 / 100.0 * 255.0 = 25.5 → truncated to 25
        assert_eq!(qr.loss_pct, 25);
        assert_eq!(qr.rtt_4ms, 25); // 100 / 4 = 25
        assert_eq!(qr.jitter_ms, 20);
        assert_eq!(qr.bitrate_cap_kbps, 200);
    }

    #[test]
    fn quality_report_from_path_stats_zero() {
        let qr = QualityReport::from_path_stats(0.0, 0, 0);
        assert_eq!(qr.loss_pct, 0);
        assert_eq!(qr.rtt_4ms, 0);
        assert_eq!(qr.jitter_ms, 0);
    }

    #[test]
    fn quality_report_from_path_stats_clamps_high() {
        let qr = QualityReport::from_path_stats(100.0, 2000, 300);
        assert_eq!(qr.loss_pct, 255);
        assert_eq!(qr.rtt_4ms, 255); // 2000/4=500, clamped to 255
        assert_eq!(qr.jitter_ms, 255);
    }

    #[test]
    fn header_roundtrip() {
        let header = MediaHeader {
            version: 2,
            flags: MediaHeader::FLAG_QUALITY,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 42,
            seq: 12345,
            timestamp: 987654,
            fec_block: 7,
        };

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), MediaHeader::WIRE_SIZE);

        let mut cursor = &bytes[..];
        let decoded = MediaHeader::read_from(&mut cursor).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn header_repair_flag() {
        let header = MediaHeader {
            version: 2,
            flags: MediaHeader::FLAG_REPAIR,
            media_type: MediaType::Audio,
            codec_id: CodecId::Codec2_1200,
            stream_id: 0,
            fec_ratio: 127,
            seq: 0xDEAD_BEEF,
            timestamp: u32::MAX,
            fec_block: 0xABCD,
        };

        let bytes = header.to_bytes();
        let mut cursor = &bytes[..];
        let decoded = MediaHeader::read_from(&mut cursor).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn media_header_v2_roundtrip() {
        let h = MediaHeaderV2 {
            version: 2,
            flags: MediaHeaderV2::FLAG_QUALITY,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 50,
            seq: 0xDEAD_BEEF,
            timestamp: 0x1234_5678,
            fec_block: 0xABCD,
        };
        let mut buf = BytesMut::with_capacity(MediaHeaderV2::WIRE_SIZE);
        h.write_to(&mut buf);
        assert_eq!(buf.len(), 16);
        let mut cursor = std::io::Cursor::new(&buf[..]);
        let parsed = MediaHeaderV2::read_from(&mut cursor).unwrap();
        assert_eq!(h, parsed);
    }

    #[test]
    fn quality_report_roundtrip() {
        let qr = QualityReport {
            loss_pct: 128,
            rtt_4ms: 100,
            jitter_ms: 50,
            bitrate_cap_kbps: 200,
        };

        let mut buf = BytesMut::new();
        qr.write_to(&mut buf);
        assert_eq!(buf.len(), QualityReport::WIRE_SIZE);

        let mut cursor = &buf[..];
        let decoded = QualityReport::read_from(&mut cursor).unwrap();
        assert_eq!(qr, decoded);
    }

    #[test]
    fn media_packet_roundtrip() {
        let packet = MediaPacket {
            header: MediaHeader {
                version: 2,
                flags: MediaHeader::FLAG_QUALITY,
                media_type: MediaType::Audio,
                codec_id: CodecId::Opus6k,
                stream_id: 0,
                fec_ratio: 32,
                seq: 100,
                timestamp: 2000,
                fec_block: 1,
            },
            payload: Bytes::from_static(b"test audio data here"),
            quality_report: Some(QualityReport {
                loss_pct: 25,
                rtt_4ms: 75,
                jitter_ms: 10,
                bitrate_cap_kbps: 100,
            }),
        };

        let bytes = packet.to_bytes();
        let decoded = MediaPacket::from_bytes(bytes).unwrap();

        assert_eq!(packet.header, decoded.header);
        assert_eq!(packet.payload, decoded.payload);
        assert_eq!(packet.quality_report, decoded.quality_report);
    }

    #[test]
    fn reflect_serialize_roundtrip() {
        // Reflect is a unit variant — the client sends it with no
        // payload and the relay answers with the observed source addr.
        let req = SignalMessage::Reflect;
        let json = serde_json::to_string(&req).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SignalMessage::Reflect));

        // ReflectResponse carries a string — exercise both IPv4 and
        // IPv6 shapes because SocketAddr::to_string uses [::1]:port
        // for v6 and the client side has to parse that back.
        for addr in ["192.0.2.17:4433", "[2001:db8::1]:4433", "127.0.0.1:54321"] {
            let resp = SignalMessage::ReflectResponse {
                observed_addr: addr.to_string(),
            };
            let json = serde_json::to_string(&resp).unwrap();
            let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
            match decoded {
                SignalMessage::ReflectResponse { observed_addr } => {
                    assert_eq!(observed_addr, addr);
                    // Must parse back to a SocketAddr cleanly.
                    let _parsed: std::net::SocketAddr = observed_addr
                        .parse()
                        .expect("observed_addr must parse as SocketAddr");
                }
                _ => panic!("wrong variant after roundtrip"),
            }
        }
    }

    #[test]
    fn federated_signal_forward_roundtrip() {
        // Wrap a DirectCallOffer inside FederatedSignalForward and
        // prove both directions of serde preserve every field.
        let inner = SignalMessage::DirectCallOffer {
            caller_fingerprint: "alice".into(),
            caller_alias: Some("Alice".into()),
            target_fingerprint: "bob".into(),
            call_id: "c1".into(),
            identity_pub: [1u8; 32],
            ephemeral_pub: [2u8; 32],
            signature: vec![3u8; 64],
            supported_profiles: vec![],
            caller_reflexive_addr: Some("192.0.2.1:4433".into()),
            caller_local_addrs: Vec::new(),
            caller_mapped_addr: None,
            caller_build_version: None,
        };
        let forward = SignalMessage::FederatedSignalForward {
            inner: Box::new(inner),
            origin_relay_fp: "relay-a-tls-fp".into(),
        };
        let json = serde_json::to_string(&forward).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::FederatedSignalForward {
                inner,
                origin_relay_fp,
            } => {
                assert_eq!(origin_relay_fp, "relay-a-tls-fp");
                match *inner {
                    SignalMessage::DirectCallOffer {
                        caller_fingerprint,
                        target_fingerprint,
                        caller_reflexive_addr,
                        ..
                    } => {
                        assert_eq!(caller_fingerprint, "alice");
                        assert_eq!(target_fingerprint, "bob");
                        assert_eq!(caller_reflexive_addr.as_deref(), Some("192.0.2.1:4433"));
                    }
                    _ => panic!("inner was not DirectCallOffer after roundtrip"),
                }
            }
            _ => panic!("outer was not FederatedSignalForward"),
        }
    }

    #[test]
    fn federated_signal_forward_can_nest_any_inner() {
        // Sanity check that every direct-call signaling variant
        // we intend to forward survives being boxed + re-serialized.
        let cases: Vec<SignalMessage> = vec![
            SignalMessage::DirectCallAnswer {
                call_id: "c1".into(),
                accept_mode: CallAcceptMode::AcceptTrusted,
                identity_pub: None,
                ephemeral_pub: None,
                signature: None,
                chosen_profile: None,
                callee_reflexive_addr: Some("198.51.100.9:4433".into()),
                callee_local_addrs: Vec::new(),
                callee_mapped_addr: None,
                callee_build_version: None,
            },
            SignalMessage::CallRinging {
                call_id: "c1".into(),
            },
            SignalMessage::Hangup {
                reason: HangupReason::Normal,
                call_id: None,
            },
        ];
        for inner in cases {
            let inner_disc = std::mem::discriminant(&inner);
            let forward = SignalMessage::FederatedSignalForward {
                inner: Box::new(inner),
                origin_relay_fp: "r".into(),
            };
            let json = serde_json::to_string(&forward).unwrap();
            let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
            match decoded {
                SignalMessage::FederatedSignalForward { inner, .. } => {
                    assert_eq!(std::mem::discriminant(&*inner), inner_disc);
                }
                _ => panic!("outer variant lost"),
            }
        }
    }

    #[test]
    fn hole_punching_optional_fields_roundtrip() {
        // DirectCallOffer with Some(caller_reflexive_addr)
        let offer = SignalMessage::DirectCallOffer {
            caller_fingerprint: "alice".into(),
            caller_alias: None,
            target_fingerprint: "bob".into(),
            call_id: "c1".into(),
            identity_pub: [0; 32],
            ephemeral_pub: [0; 32],
            signature: vec![],
            supported_profiles: vec![],
            caller_reflexive_addr: Some("192.0.2.1:4433".into()),
            caller_local_addrs: Vec::new(),
            caller_mapped_addr: None,
            caller_build_version: None,
        };
        let json = serde_json::to_string(&offer).unwrap();
        assert!(
            json.contains("caller_reflexive_addr"),
            "Some field must serialize: {json}"
        );
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::DirectCallOffer {
                caller_reflexive_addr,
                ..
            } => {
                assert_eq!(caller_reflexive_addr.as_deref(), Some("192.0.2.1:4433"));
            }
            _ => panic!("wrong variant"),
        }

        // DirectCallOffer with None — skip_serializing_if must
        // OMIT the field from the JSON so older relays that don't
        // know about caller_reflexive_addr don't see it.
        let offer_none = SignalMessage::DirectCallOffer {
            caller_fingerprint: "alice".into(),
            caller_alias: None,
            target_fingerprint: "bob".into(),
            call_id: "c1".into(),
            identity_pub: [0; 32],
            ephemeral_pub: [0; 32],
            signature: vec![],
            supported_profiles: vec![],
            caller_reflexive_addr: None,
            caller_local_addrs: Vec::new(),
            caller_mapped_addr: None,
            caller_build_version: None,
        };
        let json_none = serde_json::to_string(&offer_none).unwrap();
        assert!(
            !json_none.contains("caller_reflexive_addr"),
            "None field must NOT serialize: {json_none}"
        );

        // DirectCallAnswer with callee_reflexive_addr.
        let answer = SignalMessage::DirectCallAnswer {
            call_id: "c1".into(),
            accept_mode: CallAcceptMode::AcceptTrusted,
            identity_pub: None,
            ephemeral_pub: None,
            signature: None,
            chosen_profile: None,
            callee_reflexive_addr: Some("198.51.100.9:4433".into()),
            callee_local_addrs: Vec::new(),
            callee_mapped_addr: None,
            callee_build_version: None,
        };
        let decoded: SignalMessage =
            serde_json::from_str(&serde_json::to_string(&answer).unwrap()).unwrap();
        match decoded {
            SignalMessage::DirectCallAnswer {
                callee_reflexive_addr,
                ..
            } => {
                assert_eq!(callee_reflexive_addr.as_deref(), Some("198.51.100.9:4433"));
            }
            _ => panic!("wrong variant"),
        }

        // CallSetup with peer_direct_addr.
        let setup = SignalMessage::CallSetup {
            call_id: "c1".into(),
            room: "call-c1".into(),
            relay_addr: "203.0.113.5:4433".into(),
            peer_direct_addr: Some("192.0.2.1:4433".into()),
            peer_local_addrs: Vec::new(),
            peer_mapped_addr: None,
        };
        let decoded: SignalMessage =
            serde_json::from_str(&serde_json::to_string(&setup).unwrap()).unwrap();
        match decoded {
            SignalMessage::CallSetup {
                peer_direct_addr, ..
            } => {
                assert_eq!(peer_direct_addr.as_deref(), Some("192.0.2.1:4433"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn hole_punching_backward_compat_old_json_parses() {
        // An older client/relay wouldn't include the new fields at
        // all — the new code must still accept that JSON because
        // of #[serde(default)] on the Option<String>.
        let old_offer_json = r#"{
            "DirectCallOffer": {
                "caller_fingerprint": "alice",
                "caller_alias": null,
                "target_fingerprint": "bob",
                "call_id": "c1",
                "identity_pub": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                "ephemeral_pub": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                "signature": [],
                "supported_profiles": []
            }
        }"#;
        let decoded: SignalMessage = serde_json::from_str(old_offer_json).unwrap();
        match decoded {
            SignalMessage::DirectCallOffer {
                caller_reflexive_addr,
                ..
            } => {
                assert!(caller_reflexive_addr.is_none());
            }
            _ => panic!("wrong variant"),
        }

        let old_setup_json = r#"{
            "CallSetup": {
                "call_id": "c1",
                "room": "call-c1",
                "relay_addr": "203.0.113.5:4433"
            }
        }"#;
        let decoded: SignalMessage = serde_json::from_str(old_setup_json).unwrap();
        match decoded {
            SignalMessage::CallSetup {
                peer_direct_addr, ..
            } => {
                assert!(peer_direct_addr.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn reflect_backward_compat_with_existing_variants() {
        // Adding Reflect/ReflectResponse at the end of the enum must
        // not break JSON round-tripping of existing variants. Smoke-
        // test a sample of the pre-existing ones.
        let cases = vec![
            SignalMessage::Ping {
                timestamp_ms: 12345,
            },
            SignalMessage::Hold,
            SignalMessage::Hangup {
                reason: HangupReason::Normal,
                call_id: None,
            },
            SignalMessage::CallRinging {
                call_id: "abcd".into(),
            },
        ];
        for m in cases {
            let json = serde_json::to_string(&m).unwrap();
            let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
            // Discriminant equality proves variant tag survived.
            assert_eq!(std::mem::discriminant(&m), std::mem::discriminant(&decoded));
        }
    }

    #[test]
    fn hold_unhold_serialize() {
        let hold = SignalMessage::Hold;
        let json = serde_json::to_string(&hold).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SignalMessage::Hold));

        let unhold = SignalMessage::Unhold;
        let json = serde_json::to_string(&unhold).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SignalMessage::Unhold));
    }

    #[test]
    fn mute_unmute_serialize() {
        let mute = SignalMessage::Mute;
        let json = serde_json::to_string(&mute).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SignalMessage::Mute));

        let unmute = SignalMessage::Unmute;
        let json = serde_json::to_string(&unmute).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SignalMessage::Unmute));
    }

    #[test]
    fn transfer_serialize() {
        let transfer = SignalMessage::Transfer {
            target_fingerprint: "abc123".to_string(),
            relay_addr: Some("relay.example.com:4433".to_string()),
        };
        let json = serde_json::to_string(&transfer).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::Transfer {
                target_fingerprint,
                relay_addr,
            } => {
                assert_eq!(target_fingerprint, "abc123");
                assert_eq!(relay_addr.unwrap(), "relay.example.com:4433");
            }
            _ => panic!("expected Transfer variant"),
        }

        // Also test with relay_addr = None
        let transfer_no_relay = SignalMessage::Transfer {
            target_fingerprint: "def456".to_string(),
            relay_addr: None,
        };
        let json = serde_json::to_string(&transfer_no_relay).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::Transfer {
                target_fingerprint,
                relay_addr,
            } => {
                assert_eq!(target_fingerprint, "def456");
                assert!(relay_addr.is_none());
            }
            _ => panic!("expected Transfer variant"),
        }
    }

    #[test]
    fn transfer_ack_serialize() {
        let ack = SignalMessage::TransferAck;
        let json = serde_json::to_string(&ack).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, SignalMessage::TransferAck));
    }

    #[test]
    fn presence_update_signal_roundtrip() {
        let msg = SignalMessage::PresenceUpdate {
            fingerprints: vec!["aabb".to_string(), "ccdd".to_string()],
            relay_addr: "10.0.0.1:4433".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::PresenceUpdate {
                fingerprints,
                relay_addr,
            } => {
                assert_eq!(fingerprints.len(), 2);
                assert!(fingerprints.contains(&"aabb".to_string()));
                assert!(fingerprints.contains(&"ccdd".to_string()));
                assert_eq!(relay_addr, "10.0.0.1:4433");
            }
            _ => panic!("expected PresenceUpdate variant"),
        }

        // Empty fingerprints list
        let msg_empty = SignalMessage::PresenceUpdate {
            fingerprints: vec![],
            relay_addr: "10.0.0.2:4433".to_string(),
        };
        let json = serde_json::to_string(&msg_empty).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::PresenceUpdate {
                fingerprints,
                relay_addr,
            } => {
                assert!(fingerprints.is_empty());
                assert_eq!(relay_addr, "10.0.0.2:4433");
            }
            _ => panic!("expected PresenceUpdate variant"),
        }
    }

    #[test]
    fn fec_ratio_encode_decode() {
        let ratio = 0.5;
        let encoded = MediaHeader::encode_fec_ratio(ratio);
        let decoded = MediaHeader::decode_fec_ratio(encoded);
        assert!((decoded - ratio).abs() < 0.01);

        let ratio_max = 2.0;
        let encoded_max = MediaHeader::encode_fec_ratio(ratio_max);
        assert_eq!(encoded_max, 200);
    }

    // ---------------------------------------------------------------
    // TrunkFrame tests
    // ---------------------------------------------------------------

    #[test]
    fn trunk_frame_encode_decode() {
        let mut frame = TrunkFrame::new();
        frame.push([0, 1], Bytes::from_static(b"hello"));
        frame.push([0, 2], Bytes::from_static(b"world!"));
        frame.push([1, 0], Bytes::from_static(b"x"));
        assert_eq!(frame.len(), 3);

        let encoded = frame.encode();
        let decoded = TrunkFrame::decode(&encoded).expect("decode failed");
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded.packets[0].session_id, [0, 1]);
        assert_eq!(decoded.packets[0].payload, Bytes::from_static(b"hello"));
        assert_eq!(decoded.packets[1].session_id, [0, 2]);
        assert_eq!(decoded.packets[1].payload, Bytes::from_static(b"world!"));
        assert_eq!(decoded.packets[2].session_id, [1, 0]);
        assert_eq!(decoded.packets[2].payload, Bytes::from_static(b"x"));
    }

    #[test]
    fn trunk_frame_empty() {
        let frame = TrunkFrame::new();
        assert!(frame.is_empty());
        assert_eq!(frame.len(), 0);

        let encoded = frame.encode();
        // Just the 2-byte count header with value 0.
        assert_eq!(encoded.len(), 2);
        assert_eq!(&encoded[..], &[0, 0]);

        let decoded = TrunkFrame::decode(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn trunk_entry_wire_size() {
        // Each entry overhead must be exactly 4 bytes (2 session_id + 2 len).
        assert_eq!(TrunkEntry::OVERHEAD, 4);

        // Verify empirically: one entry with a 10-byte payload should produce
        // 2 (count) + 4 (overhead) + 10 (payload) = 16 bytes total.
        let mut frame = TrunkFrame::new();
        frame.push([0xAB, 0xCD], Bytes::from(vec![0u8; 10]));
        let encoded = frame.encode();
        assert_eq!(encoded.len(), 2 + 4 + 10);
    }

    // ---------------------------------------------------------------
    // MiniHeader / MiniFrameContext tests
    // ---------------------------------------------------------------

    #[test]
    fn mini_header_encode_decode() {
        let mini = MiniHeader {
            seq_delta: 1,
            timestamp_delta_ms: 20,
            payload_len: 160,
        };
        let mut buf = BytesMut::new();
        mini.write_to(&mut buf);

        let mut cursor = &buf[..];
        let decoded = MiniHeader::read_from(&mut cursor).unwrap();
        assert_eq!(mini, decoded);
    }

    #[test]
    fn mini_header_wire_size() {
        let mini = MiniHeader {
            seq_delta: 0xFF,
            timestamp_delta_ms: 0xFFFF,
            payload_len: 0xFFFF,
        };
        let mut buf = BytesMut::new();
        mini.write_to(&mut buf);
        assert_eq!(buf.len(), 5);
        assert_eq!(MiniHeader::WIRE_SIZE, 5);
    }

    #[test]
    fn mini_frame_context_expand() {
        let baseline = MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 10,
            seq: 100,
            timestamp: 1000,
            fec_block: 5,
        };

        let mut ctx = MiniFrameContext::default();
        ctx.update(&baseline);

        // First expansion
        let mini1 = MiniHeader {
            seq_delta: 1,
            timestamp_delta_ms: 20,
            payload_len: 80,
        };
        let h1 = ctx.expand(&mini1).unwrap();
        assert_eq!(h1.seq, 101);
        assert_eq!(h1.timestamp, 1020);
        assert_eq!(h1.codec_id, CodecId::Opus24k);
        assert_eq!(h1.fec_block, 5);

        // Second expansion — builds on expanded h1
        let mini2 = MiniHeader {
            seq_delta: 1,
            timestamp_delta_ms: 20,
            payload_len: 80,
        };
        let h2 = ctx.expand(&mini2).unwrap();
        assert_eq!(h2.seq, 102);
        assert_eq!(h2.timestamp, 1040);
    }

    #[test]
    fn mini_frame_context_no_baseline() {
        let mut ctx = MiniFrameContext::default();
        let mini = MiniHeader {
            seq_delta: 1,
            timestamp_delta_ms: 20,
            payload_len: 80,
        };
        assert!(ctx.expand(&mini).is_none());
    }

    #[test]
    fn mini_header_v2_roundtrip() {
        let mini = MiniHeaderV2 {
            seq_delta: 3,
            timestamp_delta_ms: 20,
            payload_len: 160,
        };
        let mut buf = BytesMut::new();
        mini.write_to(&mut buf);
        assert_eq!(buf.len(), 5);

        let mut cursor = &buf[..];
        let decoded = MiniHeaderV2::read_from(&mut cursor).unwrap();
        assert_eq!(mini, decoded);
    }

    #[test]
    fn mini_frame_context_v2_expand() {
        let baseline = MediaHeaderV2 {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 50,
            seq: 100,
            timestamp: 1000,
            fec_block: 5,
        };

        let mut ctx = MiniFrameContextV2::default();
        ctx.update(&baseline);

        let mini = MiniHeaderV2 {
            seq_delta: 3,
            timestamp_delta_ms: 20,
            payload_len: 80,
        };
        let h1 = ctx.expand(&mini).unwrap();
        assert_eq!(h1.seq, 103);
        assert_eq!(h1.timestamp, 1020);
        assert_eq!(h1.codec_id, CodecId::Opus24k);
        assert_eq!(h1.fec_block, 5);

        // Second expansion — builds on expanded h1
        let mini2 = MiniHeaderV2 {
            seq_delta: 1,
            timestamp_delta_ms: 20,
            payload_len: 80,
        };
        let h2 = ctx.expand(&mini2).unwrap();
        assert_eq!(h2.seq, 104);
        assert_eq!(h2.timestamp, 1040);
    }

    #[test]
    fn mini_frame_context_v2_no_baseline() {
        let mut ctx = MiniFrameContextV2::default();
        let mini = MiniHeaderV2 {
            seq_delta: 1,
            timestamp_delta_ms: 20,
            payload_len: 80,
        };
        assert!(ctx.expand(&mini).is_none());
    }

    #[test]
    fn full_vs_mini_size_comparison() {
        // Full frame on wire: 1 byte type tag + 16 byte MediaHeader = 17
        let full_size = 1 + MediaHeader::WIRE_SIZE;
        assert_eq!(full_size, 17);

        // Mini frame on wire: 1 byte type tag + 5 byte MiniHeader = 6
        let mini_size = 1 + MiniHeader::WIRE_SIZE;
        assert_eq!(mini_size, 6);

        // Verify the constants match expectations
        assert_eq!(FRAME_TYPE_FULL, 0x00);
        assert_eq!(FRAME_TYPE_MINI, 0x01);
    }

    // ---------------------------------------------------------------
    // encode_compact / decode_compact tests
    // ---------------------------------------------------------------

    fn make_media_packet(seq: u32, ts: u32, payload: &[u8]) -> MediaPacket {
        MediaPacket {
            header: MediaHeader {
                version: 2,
                flags: 0,
                media_type: MediaType::Audio,
                codec_id: CodecId::Opus24k,
                stream_id: 0,
                fec_ratio: 10,
                seq,
                timestamp: ts,
                fec_block: 0,
            },
            payload: Bytes::from(payload.to_vec()),
            quality_report: None,
        }
    }

    #[test]
    fn mini_frame_encode_decode_sequence() {
        let mut enc_ctx = MiniFrameContext::default();
        let mut dec_ctx = MiniFrameContext::default();
        let mut frames_since_full: u32 = 0;

        let packets: Vec<MediaPacket> = (0..5)
            .map(|i| make_media_packet(i, i * 20, b"audio"))
            .collect();

        for (i, pkt) in packets.iter().enumerate() {
            let wire = pkt.encode_compact(&mut enc_ctx, &mut frames_since_full);

            if i == 0 {
                // First frame must be full
                assert_eq!(wire[0], FRAME_TYPE_FULL, "frame 0 should be FULL");
            } else {
                // Subsequent frames should be mini
                assert_eq!(wire[0], FRAME_TYPE_MINI, "frame {i} should be MINI");
                // Mini wire: 1 (tag) + 5 (mini header) + payload
                assert_eq!(wire.len(), 1 + MiniHeader::WIRE_SIZE + pkt.payload.len());
            }

            let decoded = MediaPacket::decode_compact(&wire, &mut dec_ctx)
                .unwrap_or_else(|| panic!("decode failed at frame {i}"));
            assert_eq!(decoded.header.seq, pkt.header.seq);
            assert_eq!(decoded.header.timestamp, pkt.header.timestamp);
            assert_eq!(decoded.payload, pkt.payload);
        }
    }

    #[test]
    fn mini_frame_periodic_full() {
        let mut ctx = MiniFrameContext::default();
        let mut frames_since_full: u32 = 0;

        // Encode MINI_FRAME_FULL_INTERVAL + 1 frames. Frame 0 and frame 50
        // should be FULL, everything in between should be MINI.
        for i in 0..=MINI_FRAME_FULL_INTERVAL {
            let pkt = make_media_packet(i, i * 20, b"data");
            let wire = pkt.encode_compact(&mut ctx, &mut frames_since_full);

            if i == 0 || i == MINI_FRAME_FULL_INTERVAL {
                assert_eq!(wire[0], FRAME_TYPE_FULL, "frame {i} should be FULL");
            } else {
                assert_eq!(wire[0], FRAME_TYPE_MINI, "frame {i} should be MINI");
            }
        }
    }

    #[test]
    fn quality_directive_roundtrip() {
        let msg = SignalMessage::QualityDirective {
            recommended_profile: crate::QualityProfile::DEGRADED,
            reason: Some("weakest link degraded".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::QualityDirective {
                recommended_profile,
                reason,
            } => {
                assert_eq!(recommended_profile.codec, CodecId::Opus6k);
                assert_eq!(reason.as_deref(), Some("weakest link degraded"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn quality_directive_without_reason_roundtrip() {
        let msg = SignalMessage::QualityDirective {
            recommended_profile: crate::QualityProfile::GOOD,
            reason: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        // None reason should be omitted from JSON
        assert!(!json.contains("reason"));
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::QualityDirective { reason, .. } => {
                assert!(reason.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mini_frame_disabled() {
        // Simulate disabled mini-frames by always keeping frames_since_full at 0
        // (which is what the encoder does when the feature is off).
        let mut ctx = MiniFrameContext::default();

        for i in 0..10u32 {
            let pkt = make_media_packet(i, i * 20, b"payload");
            // When mini-frames are disabled, the encoder always passes
            // frames_since_full = 0 equivalent by never using encode_compact.
            // We test the raw path: frames_since_full forced to 0 every time.
            let mut frames_since_full: u32 = 0;
            let wire = pkt.encode_compact(&mut ctx, &mut frames_since_full);
            assert_eq!(
                wire[0], FRAME_TYPE_FULL,
                "frame {i} should be FULL when disabled"
            );
        }
    }

    #[test]
    fn encode_compact_fallback_to_full_without_baseline() {
        // A fresh MiniFrameContext has no baseline header.  If the caller
        // somehow passes frames_since_full > 0 we must not panic; instead
        // fall back to a full frame and establish the baseline.
        let mut ctx = MiniFrameContext::default();
        let mut frames_since_full: u32 = 1; // claims we've seen a full frame

        let pkt = make_media_packet(0, 0, b"audio");
        let wire = pkt.encode_compact(&mut ctx, &mut frames_since_full);

        assert_eq!(wire[0], FRAME_TYPE_FULL, "must fall back to FULL when no baseline");
        // After the fallback the baseline is established.
        assert!(ctx.last_header().is_some());
    }

    // ── Quality negotiation roundtrip tests (#28, #29, #30) ─────

    #[test]
    fn upgrade_proposal_roundtrip() {
        let msg = SignalMessage::UpgradeProposal {
            call_id: "c1".into(),
            proposal_id: "p1".into(),
            proposed_profile: crate::QualityProfile::STUDIO_48K,
            local_loss_pct: Some(0.5),
            local_rtt_ms: Some(25),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::UpgradeProposal {
                proposal_id,
                proposed_profile,
                ..
            } => {
                assert_eq!(proposal_id, "p1");
                assert_eq!(proposed_profile, crate::QualityProfile::STUDIO_48K);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn upgrade_response_roundtrip() {
        let msg = SignalMessage::UpgradeResponse {
            call_id: "c1".into(),
            proposal_id: "p1".into(),
            accepted: true,
            reason: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::UpgradeResponse { accepted, .. } => assert!(accepted),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn upgrade_confirm_roundtrip() {
        let msg = SignalMessage::UpgradeConfirm {
            call_id: "c1".into(),
            proposal_id: "p1".into(),
            confirmed_profile: crate::QualityProfile::STUDIO_64K,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::UpgradeConfirm {
                confirmed_profile, ..
            } => {
                assert_eq!(confirmed_profile, crate::QualityProfile::STUDIO_64K);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn quality_capability_roundtrip() {
        let msg = SignalMessage::QualityCapability {
            call_id: "c1".into(),
            max_profile: crate::QualityProfile::GOOD,
            loss_pct: Some(2.5),
            rtt_ms: Some(80),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::QualityCapability {
                max_profile,
                loss_pct,
                ..
            } => {
                assert_eq!(max_profile, crate::QualityProfile::GOOD);
                assert!((loss_pct.unwrap() - 2.5).abs() < 0.01);
            }
            _ => panic!("wrong variant"),
        }
    }

    // ── Phase 8: Tailscale-inspired signal roundtrip tests ──────

    #[test]
    fn candidate_update_roundtrip() {
        let msg = SignalMessage::CandidateUpdate {
            call_id: "test-123".into(),
            reflexive_addr: Some("203.0.113.5:4433".into()),
            local_addrs: vec!["192.168.1.10:4433".into(), "10.0.0.5:4433".into()],
            mapped_addr: Some("198.51.100.42:12345".into()),
            generation: 7,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::CandidateUpdate {
                call_id,
                reflexive_addr,
                local_addrs,
                mapped_addr,
                generation,
            } => {
                assert_eq!(call_id, "test-123");
                assert_eq!(reflexive_addr.as_deref(), Some("203.0.113.5:4433"));
                assert_eq!(local_addrs.len(), 2);
                assert_eq!(mapped_addr.as_deref(), Some("198.51.100.42:12345"));
                assert_eq!(generation, 7);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn candidate_update_minimal_roundtrip() {
        let msg = SignalMessage::CandidateUpdate {
            call_id: "c".into(),
            reflexive_addr: None,
            local_addrs: vec![],
            mapped_addr: None,
            generation: 0,
        };
        let json = serde_json::to_string(&msg).unwrap();
        // skip_serializing_if should omit None/empty fields
        assert!(!json.contains("reflexive_addr"));
        assert!(!json.contains("local_addrs"));
        assert!(!json.contains("mapped_addr"));

        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::CandidateUpdate { generation, .. } => {
                assert_eq!(generation, 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn offer_with_mapped_addr_roundtrip() {
        let msg = SignalMessage::DirectCallOffer {
            caller_fingerprint: "alice".into(),
            caller_alias: None,
            target_fingerprint: "bob".into(),
            call_id: "c1".into(),
            identity_pub: [0; 32],
            ephemeral_pub: [0; 32],
            signature: vec![],
            supported_profiles: vec![],
            caller_reflexive_addr: Some("1.2.3.4:5".into()),
            caller_local_addrs: vec!["10.0.0.1:5".into()],
            caller_mapped_addr: Some("5.6.7.8:9999".into()),
            caller_build_version: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("caller_mapped_addr"));
        assert!(json.contains("5.6.7.8:9999"));

        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::DirectCallOffer {
                caller_mapped_addr, ..
            } => {
                assert_eq!(caller_mapped_addr.as_deref(), Some("5.6.7.8:9999"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn offer_without_mapped_addr_omits_field() {
        let msg = SignalMessage::DirectCallOffer {
            caller_fingerprint: "alice".into(),
            caller_alias: None,
            target_fingerprint: "bob".into(),
            call_id: "c1".into(),
            identity_pub: [0; 32],
            ephemeral_pub: [0; 32],
            signature: vec![],
            supported_profiles: vec![],
            caller_reflexive_addr: None,
            caller_local_addrs: vec![],
            caller_mapped_addr: None,
            caller_build_version: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("caller_mapped_addr"));
    }

    #[test]
    fn answer_with_mapped_addr_roundtrip() {
        let msg = SignalMessage::DirectCallAnswer {
            call_id: "c1".into(),
            accept_mode: CallAcceptMode::AcceptTrusted,
            identity_pub: None,
            ephemeral_pub: None,
            signature: None,
            chosen_profile: None,
            callee_reflexive_addr: Some("1.2.3.4:5".into()),
            callee_local_addrs: vec![],
            callee_mapped_addr: Some("9.8.7.6:1111".into()),
            callee_build_version: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::DirectCallAnswer {
                callee_mapped_addr, ..
            } => {
                assert_eq!(callee_mapped_addr.as_deref(), Some("9.8.7.6:1111"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn setup_with_mapped_addr_roundtrip() {
        let msg = SignalMessage::CallSetup {
            call_id: "c1".into(),
            room: "room".into(),
            relay_addr: "1.2.3.4:5".into(),
            peer_direct_addr: Some("5.6.7.8:9".into()),
            peer_local_addrs: vec!["10.0.0.1:9".into()],
            peer_mapped_addr: Some("11.12.13.14:15".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("peer_mapped_addr"));
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::CallSetup {
                peer_mapped_addr, ..
            } => {
                assert_eq!(peer_mapped_addr.as_deref(), Some("11.12.13.14:15"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn backward_compat_offer_without_mapped_addr_parses() {
        // Old client JSON that doesn't have caller_mapped_addr at all
        let json = r#"{
            "DirectCallOffer": {
                "caller_fingerprint": "alice",
                "target_fingerprint": "bob",
                "call_id": "c1",
                "identity_pub": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                "ephemeral_pub": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                "signature": [],
                "supported_profiles": [],
                "caller_reflexive_addr": "1.2.3.4:5"
            }
        }"#;
        let decoded: SignalMessage = serde_json::from_str(json).unwrap();
        match decoded {
            SignalMessage::DirectCallOffer {
                caller_mapped_addr,
                caller_reflexive_addr,
                ..
            } => {
                assert!(caller_mapped_addr.is_none());
                assert_eq!(caller_reflexive_addr.as_deref(), Some("1.2.3.4:5"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn backward_compat_setup_without_mapped_addr_parses() {
        let json = r#"{
            "CallSetup": {
                "call_id": "c1",
                "room": "room",
                "relay_addr": "1.2.3.4:5",
                "peer_direct_addr": "5.6.7.8:9"
            }
        }"#;
        let decoded: SignalMessage = serde_json::from_str(json).unwrap();
        match decoded {
            SignalMessage::CallSetup {
                peer_mapped_addr,
                peer_direct_addr,
                ..
            } => {
                assert!(peer_mapped_addr.is_none());
                assert_eq!(peer_direct_addr.as_deref(), Some("5.6.7.8:9"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn register_presence_ack_with_new_fields_roundtrip() {
        let msg = SignalMessage::RegisterPresenceAck {
            success: true,
            error: None,
            relay_build: Some("abc123".into()),
            relay_region: Some("us-east".into()),
            available_relays: vec![
                "eu-west|10.0.0.1:4433".into(),
                "ap-south|10.0.0.2:4433".into(),
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("relay_region"));
        assert!(json.contains("us-east"));
        assert!(json.contains("available_relays"));

        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::RegisterPresenceAck {
                relay_region,
                available_relays,
                ..
            } => {
                assert_eq!(relay_region.as_deref(), Some("us-east"));
                assert_eq!(available_relays.len(), 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn register_presence_ack_backward_compat() {
        // Old relay JSON without relay_region or available_relays
        let json = r#"{
            "RegisterPresenceAck": {
                "success": true,
                "relay_build": "old-build"
            }
        }"#;
        let decoded: SignalMessage = serde_json::from_str(json).unwrap();
        match decoded {
            SignalMessage::RegisterPresenceAck {
                relay_region,
                available_relays,
                relay_build,
                ..
            } => {
                assert!(relay_region.is_none());
                assert!(available_relays.is_empty());
                assert_eq!(relay_build.as_deref(), Some("old-build"));
            }
            _ => panic!("wrong variant"),
        }
    }
}
