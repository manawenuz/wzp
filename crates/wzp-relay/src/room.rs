//! Room management for multi-party calls.
//!
//! Each room holds N participants. When one participant sends a media packet,
//! the relay forwards it to all other participants in the room (SFU model).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use tracing::{debug, error, info, warn};

use wzp_proto::packet::TrunkFrame;
use wzp_proto::quality::{AdaptiveQualityController, Tier};
use wzp_proto::traits::QualityController;
use wzp_proto::{MediaTransport, default_signal_version};

use crate::conformance::ConformanceMeter;
use crate::metrics::RelayMetrics;
use crate::trunk::TrunkBatcher;

/// Debug tap: logs packet metadata for matching rooms.
#[derive(Clone)]
pub struct DebugTap {
    /// Room name filter ("*" = all rooms, or specific room name/hash).
    pub room_filter: String,
}

impl DebugTap {
    pub fn matches(&self, room_name: &str) -> bool {
        self.room_filter == "*" || self.room_filter == room_name
    }

    pub fn log_packet(
        &self,
        room: &str,
        dir: &str,
        addr: &std::net::SocketAddr,
        pkt: &wzp_proto::MediaPacket,
        fan_out: usize,
    ) {
        let h = &pkt.header;
        info!(
            target: "debug_tap",
            room = %room,
            dir = dir,
            addr = %addr,
            seq = h.seq,
            codec = ?h.codec_id,
            ts = h.timestamp,
            fec_block = h.fec_block,
            repair = h.is_repair(),
            len = pkt.payload.len(),
            fan_out,
            "TAP"
        );
    }

    pub fn log_signal(&self, room: &str, signal: &wzp_proto::SignalMessage) {
        match signal {
            wzp_proto::SignalMessage::RoomUpdate {
                count,
                participants,
                ..
            } => {
                let names: Vec<&str> = participants
                    .iter()
                    .map(|p| p.alias.as_deref().unwrap_or("?"))
                    .collect();
                info!(
                    target: "debug_tap",
                    room = %room,
                    signal = "RoomUpdate",
                    count,
                    participants = ?names,
                    "TAP SIGNAL"
                );
            }
            wzp_proto::SignalMessage::QualityDirective {
                recommended_profile,
                reason,
                ..
            } => {
                info!(
                    target: "debug_tap",
                    room = %room,
                    signal = "QualityDirective",
                    codec = ?recommended_profile.codec,
                    reason = reason.as_deref().unwrap_or(""),
                    "TAP SIGNAL"
                );
            }
            other => {
                info!(
                    target: "debug_tap",
                    room = %room,
                    signal = ?std::mem::discriminant(other),
                    "TAP SIGNAL"
                );
            }
        }
    }

    pub fn log_event(&self, room: &str, event: &str, detail: &str) {
        info!(
            target: "debug_tap",
            room = %room,
            event,
            detail,
            "TAP EVENT"
        );
    }

    pub fn log_stats(&self, room: &str, stats: &TapStats) {
        let codecs: Vec<String> = stats.codecs_seen.iter().map(|c| format!("{c:?}")).collect();
        info!(
            target: "debug_tap",
            room = %room,
            period = "5s",
            in_pkts = stats.in_pkts,
            out_pkts = stats.out_pkts,
            fan_out_avg = format!("{:.1}", if stats.in_pkts > 0 { stats.out_pkts as f64 / stats.in_pkts as f64 } else { 0.0 }),
            seq_gaps = stats.seq_gaps,
            codecs_seen = ?codecs,
            "TAP STATS"
        );
    }
}

/// Per-participant stats for the debug tap periodic summary.
pub struct TapStats {
    pub in_pkts: u64,
    pub out_pkts: u64,
    pub seq_gaps: u64,
    pub codecs_seen: std::collections::HashSet<wzp_proto::CodecId>,
    last_seq: Option<u32>,
}

impl TapStats {
    pub fn new() -> Self {
        Self {
            in_pkts: 0,
            out_pkts: 0,
            seq_gaps: 0,
            codecs_seen: std::collections::HashSet::new(),
            last_seq: None,
        }
    }

    pub fn record_in(&mut self, pkt: &wzp_proto::MediaPacket, fan_out: usize) {
        self.in_pkts += 1;
        self.out_pkts += fan_out as u64;
        self.codecs_seen.insert(pkt.header.codec_id);
        if let Some(prev) = self.last_seq {
            let expected = prev.wrapping_add(1);
            if pkt.header.seq != expected {
                self.seq_gaps += 1;
            }
        }
        self.last_seq = Some(pkt.header.seq);
    }

    pub fn reset_period(&mut self) {
        self.in_pkts = 0;
        self.out_pkts = 0;
        self.seq_gaps = 0;
        // Keep codecs_seen and last_seq across periods
    }
}

/// Tracks network quality for a single participant in a room.
struct ParticipantQuality {
    controller: AdaptiveQualityController,
    current_tier: Tier,
}

impl ParticipantQuality {
    fn new() -> Self {
        Self {
            controller: AdaptiveQualityController::new(),
            current_tier: Tier::Good,
        }
    }

    /// Feed a quality report and return the new tier if it changed.
    fn observe(&mut self, report: &wzp_proto::packet::QualityReport) -> Option<Tier> {
        let _ = self.controller.observe(report);
        let new_tier = self.controller.tier();
        if new_tier != self.current_tier {
            self.current_tier = new_tier;
            Some(new_tier)
        } else {
            None
        }
    }
}

/// Compute the weakest (worst) quality tier across all tracked participants.
fn weakest_tier<'a>(qualities: impl Iterator<Item = &'a ParticipantQuality>) -> Tier {
    qualities
        .map(|pq| pq.current_tier)
        .min()
        .unwrap_or(Tier::Good)
}

/// Unique participant ID within a room.
pub type ParticipantId = u64;

static NEXT_PARTICIPANT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> ParticipantId {
    NEXT_PARTICIPANT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Events emitted by RoomManager for federation to observe.
#[derive(Clone, Debug)]
pub enum RoomEvent {
    /// First local participant joined this room.
    LocalJoin { room: String },
    /// Last local participant left this room.
    LocalLeave { room: String },
}

/// Outbound federation media from a local participant.
pub struct FederationMediaOut {
    pub room_name: String,
    pub room_hash: [u8; 8],
    pub data: Bytes,
}

/// How to send data to a participant — either via QUIC transport or WebSocket channel.
#[derive(Clone)]
pub enum ParticipantSender {
    Quic(Arc<wzp_transport::QuinnTransport>),
    WebSocket(tokio::sync::mpsc::Sender<Bytes>),
}

impl ParticipantSender {
    /// Send raw bytes to this participant.
    pub async fn send_raw(&self, data: &[u8]) -> Result<(), String> {
        match self {
            ParticipantSender::WebSocket(tx) => tx
                .try_send(Bytes::copy_from_slice(data))
                .map_err(|e| format!("ws send: {e}")),
            ParticipantSender::Quic(transport) => {
                let pkt = wzp_proto::MediaPacket {
                    header: wzp_proto::packet::MediaHeader {
                        version: 2,
                        flags: 0,
                        media_type: wzp_proto::MediaType::Audio,
                        codec_id: wzp_proto::CodecId::Opus24k,
                        stream_id: 0,
                        fec_ratio: 0,
                        seq: 0,
                        timestamp: 0,
                        fec_block: 0,
                    },
                    payload: Bytes::copy_from_slice(data),
                    quality_report: None,
                };
                transport
                    .send_media(&pkt)
                    .await
                    .map_err(|e| format!("quic send: {e}"))
            }
        }
    }

    /// Check if this is a QUIC participant.
    pub fn is_quic(&self) -> bool {
        matches!(self, ParticipantSender::Quic(_))
    }

    /// Get the QUIC transport if this is a QUIC participant.
    pub fn as_quic(&self) -> Option<&Arc<wzp_transport::QuinnTransport>> {
        match self {
            ParticipantSender::Quic(t) => Some(t),
            _ => None,
        }
    }
}

/// Broadcast a signal message to a list of participant senders.
pub async fn broadcast_signal(senders: &[ParticipantSender], msg: &wzp_proto::SignalMessage) {
    for sender in senders {
        if let ParticipantSender::Quic(t) = sender {
            if let Err(e) = t.send_signal(msg).await {
                warn!("broadcast_signal error: {e}");
            }
        }
    }
}

/// A participant in a room.
struct Participant {
    id: ParticipantId,
    _addr: std::net::SocketAddr,
    sender: ParticipantSender,
    fingerprint: Option<String>,
    alias: Option<String>,
}

/// A room holding multiple participants.
struct Room {
    participants: Vec<Participant>,
    /// Per-participant quality tracking, keyed by participant_id.
    qualities: HashMap<ParticipantId, ParticipantQuality>,
    /// Current room-wide tier (to avoid repeated broadcasts).
    current_tier: Tier,
}

impl Room {
    fn new() -> Self {
        Self {
            participants: Vec::new(),
            qualities: HashMap::new(),
            current_tier: Tier::Good,
        }
    }

    fn add(
        &mut self,
        addr: std::net::SocketAddr,
        sender: ParticipantSender,
        fingerprint: Option<String>,
        alias: Option<String>,
    ) -> ParticipantId {
        let id = next_id();
        info!(room_size = self.participants.len() + 1, participant = id, %addr, "joined room");
        self.participants.push(Participant {
            id,
            _addr: addr,
            sender,
            fingerprint,
            alias,
        });
        id
    }

    fn remove(&mut self, id: ParticipantId) {
        self.participants.retain(|p| p.id != id);
        info!(
            room_size = self.participants.len(),
            participant = id,
            "left room"
        );
    }

    fn others(&self, exclude_id: ParticipantId) -> Vec<ParticipantSender> {
        self.participants
            .iter()
            .filter(|p| p.id != exclude_id)
            .map(|p| p.sender.clone())
            .collect()
    }

    /// Build a RoomUpdate participant list.
    fn participant_list(&self) -> Vec<wzp_proto::packet::RoomParticipant> {
        self.participants
            .iter()
            .map(|p| wzp_proto::packet::RoomParticipant {
                fingerprint: p.fingerprint.clone().unwrap_or_default(),
                alias: p.alias.clone(),
                relay_label: None, // local participant
            })
            .collect()
    }

    /// Get all senders (for broadcasting to everyone including the joiner).
    fn all_senders(&self) -> Vec<ParticipantSender> {
        self.participants.iter().map(|p| p.sender.clone()).collect()
    }

    fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    fn len(&self) -> usize {
        self.participants.len()
    }
}

/// Maximum bytes to cache per `(room, sender, stream)` keyframe.
const KEYFRAME_CACHE_MAX_BYTES: usize = 200_000;

/// Cached complete keyframe for fast join-to-first-frame replay.
#[derive(Clone)]
#[allow(dead_code)]
struct KeyframeCacheEntry {
    packets: Vec<wzp_proto::MediaPacket>,
    sequence_first: u32,
    timestamp_ms: u32,
    total_bytes: usize,
}

/// In-progress keyframe buffer while accumulating packets.
struct KeyframeBuffer {
    packets: Vec<wzp_proto::MediaPacket>,
    sequence_first: u32,
    timestamp_ms: u32,
    total_bytes: usize,
}

/// Suppression window for PictureLossIndication per (room, stream_id).
struct PliState {
    last_pli: std::time::Instant,
}

/// Manages all rooms on the relay.
///
/// Uses `DashMap` for per-room sharded locking -- rooms are independently
/// lockable so the media hot-path never contends on a single mutex.
///
/// Each `Room` is further wrapped in `Arc<RwLock<Room>>` so that the
/// DashMap guard is held only long enough to retrieve the Arc; all
/// per-room operations (fan-out, quality updates, join/leave) then
/// acquire the room-level RwLock.  This lets concurrent `others()`
/// calls share a read lock while `observe_quality()` or join/leave
/// hold the write lock.
pub struct RoomManager {
    rooms: DashMap<String, Arc<RwLock<Room>>>,
    /// Room access control list. Maps hashed room name -> allowed fingerprints.
    /// When `None`, rooms are open (no auth mode). When `Some`, only listed
    /// fingerprints can join the corresponding room. Protected by std Mutex
    /// since ACL mutations are rare (only during call setup).
    acl: Option<std::sync::Mutex<HashMap<String, HashSet<String>>>>,
    /// Channel for room lifecycle events (federation subscribes).
    event_tx: tokio::sync::broadcast::Sender<RoomEvent>,
    /// Per `(room, sender, stream)` cache of the most recent complete keyframe.
    keyframe_cache: DashMap<(String, ParticipantId, u8), KeyframeCacheEntry>,
    /// Per `(room, sender, stream)` buffer for a keyframe currently being received.
    keyframe_buffer: DashMap<(String, ParticipantId, u8), KeyframeBuffer>,
    /// Per `(room, stream_id)` last PLI timestamp for suppression.
    pli_state: DashMap<(String, ParticipantId, u8), PliState>,
    /// Maps `(room, stream_id)` -> participant_id of the sender currently
    /// publishing on that stream.  Updated on every non-repair media packet.
    stream_owner: DashMap<(String, u8), ParticipantId>,
}

impl RoomManager {
    pub fn new() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            rooms: DashMap::new(),
            acl: None,
            event_tx,
            keyframe_cache: DashMap::new(),
            keyframe_buffer: DashMap::new(),
            pli_state: DashMap::new(),
            stream_owner: DashMap::new(),
        }
    }

    /// Create a room manager with ACL enforcement enabled.
    pub fn with_acl() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            rooms: DashMap::new(),
            acl: Some(std::sync::Mutex::new(HashMap::new())),
            event_tx,
            keyframe_cache: DashMap::new(),
            keyframe_buffer: DashMap::new(),
            pli_state: DashMap::new(),
            stream_owner: DashMap::new(),
        }
    }

    /// Subscribe to room lifecycle events (for federation).
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<RoomEvent> {
        self.event_tx.subscribe()
    }

    /// Grant a fingerprint access to a room.
    pub fn allow(&self, room_name: &str, fingerprint: &str) {
        if let Some(ref acl) = self.acl {
            acl.lock()
                .unwrap()
                .entry(room_name.to_string())
                .or_default()
                .insert(fingerprint.to_string());
        }
    }

    /// Check if a fingerprint is authorized to join a room.
    /// Returns true if ACL is disabled (open mode) or the fingerprint is in the allow list.
    pub fn is_authorized(&self, room_name: &str, fingerprint: Option<&str>) -> bool {
        match (&self.acl, fingerprint) {
            (None, _) => true,        // no ACL = open
            (Some(_), None) => false, // ACL enabled but no fingerprint
            (Some(acl), Some(fp)) => {
                let acl = acl.lock().unwrap();
                // Room not in ACL = open room (allow anyone authenticated)
                match acl.get(room_name) {
                    None => true,
                    Some(allowed) => allowed.contains(fp),
                }
            }
        }
    }

    /// Join a room. Returns (participant_id, room_update_msg, all_senders, cached_keyframes) for broadcasting.
    pub fn join(
        &self,
        room_name: &str,
        addr: std::net::SocketAddr,
        sender: ParticipantSender,
        fingerprint: Option<&str>,
        alias: Option<&str>,
    ) -> Result<
        (
            ParticipantId,
            wzp_proto::SignalMessage,
            Vec<ParticipantSender>,
            Vec<Vec<wzp_proto::MediaPacket>>,
        ),
        String,
    > {
        if !self.is_authorized(room_name, fingerprint) {
            warn!(room = room_name, fingerprint = ?fingerprint, "unauthorized room join attempt");
            return Err("not authorized for this room".to_string());
        }
        let was_empty = self
            .rooms
            .get(room_name)
            .map_or(true, |arc| arc.read().unwrap().is_empty());
        let arc = self
            .rooms
            .entry(room_name.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(Room::new())));
        let mut room = arc.write().unwrap();
        let id = room.add(
            addr,
            sender,
            fingerprint.map(|s| s.to_string()),
            alias.map(|s| s.to_string()),
        );
        room.qualities.insert(id, ParticipantQuality::new());
        let update = wzp_proto::SignalMessage::RoomUpdate {
            version: default_signal_version(),
            count: room.len() as u32,
            participants: room.participant_list(),
        };
        let senders = room.all_senders();
        drop(room); // release room lock before event_tx send
        if was_empty {
            let _ = self.event_tx.send(RoomEvent::LocalJoin {
                room: room_name.to_string(),
            });
        }
        let keyframes = self.cached_keyframes_for_room(room_name);
        Ok((id, update, senders, keyframes))
    }

    /// Join a room via WebSocket. Convenience wrapper around `join()`.
    pub fn join_ws(
        &self,
        room_name: &str,
        addr: std::net::SocketAddr,
        sender: tokio::sync::mpsc::Sender<Bytes>,
        fingerprint: Option<&str>,
    ) -> Result<ParticipantId, String> {
        let (id, _update, _senders, _keyframes) = self.join(
            room_name,
            addr,
            ParticipantSender::WebSocket(sender),
            fingerprint,
            None,
        )?;
        Ok(id)
    }

    /// Get list of active room names.
    pub fn active_rooms(&self) -> Vec<String> {
        self.rooms.iter().map(|r| r.key().clone()).collect()
    }

    /// Get participant list for a room (fingerprint + alias).
    pub fn local_participant_list(
        &self,
        room_name: &str,
    ) -> Vec<wzp_proto::packet::RoomParticipant> {
        self.rooms
            .get(room_name)
            .map(|arc| arc.read().unwrap().participant_list())
            .unwrap_or_default()
    }

    /// Get all senders for participants in a room (for federation inbound media delivery).
    pub fn local_senders(&self, room_name: &str) -> Vec<ParticipantSender> {
        self.rooms
            .get(room_name)
            .map(|arc| arc.read().unwrap().all_senders())
            .unwrap_or_default()
    }

    /// Leave a room. Returns (room_update_msg, remaining_senders) for broadcasting, or None if room is now empty.
    pub fn leave(
        &self,
        room_name: &str,
        participant_id: ParticipantId,
    ) -> Option<(wzp_proto::SignalMessage, Vec<ParticipantSender>)> {
        let result = {
            if let Some(arc) = self.rooms.get(room_name) {
                let mut room = arc.write().unwrap();
                room.qualities.remove(&participant_id);
                room.remove(participant_id);
                if room.is_empty() {
                    drop(room); // release room lock
                    drop(arc); // release DashMap guard
                    self.rooms.remove(room_name);
                    self.clear_room_state(room_name);
                    let _ = self.event_tx.send(RoomEvent::LocalLeave {
                        room: room_name.to_string(),
                    });
                    info!(room = room_name, "room closed (empty)");
                    return None;
                }
                let update = wzp_proto::SignalMessage::RoomUpdate {
                    version: default_signal_version(),
                    count: room.len() as u32,
                    participants: room.participant_list(),
                };
                let senders = room.all_senders();
                Some((update, senders))
            } else {
                None
            }
        };
        result
    }

    /// Update the keyframe cache from an incoming media packet.
    ///
    /// Called from the forwarding hot-path.  If the packet belongs to a
    /// keyframe we buffer it; when the frame-end flag arrives we store the
    /// complete keyframe.  Non-keyframe packets flush any stale partial buffer.
    pub fn update_keyframe_cache(
        &self,
        room_name: &str,
        sender_id: ParticipantId,
        pkt: &wzp_proto::MediaPacket,
    ) {
        let h = &pkt.header;
        if h.is_repair() {
            // Never cache repair packets.
            return;
        }
        let key = (room_name.to_string(), sender_id, h.stream_id);

        if h.is_keyframe() {
            let mut entry =
                self.keyframe_buffer
                    .entry(key.clone())
                    .or_insert_with(|| KeyframeBuffer {
                        packets: Vec::new(),
                        sequence_first: h.seq,
                        timestamp_ms: h.timestamp,
                        total_bytes: 0,
                    });

            let pkt_bytes = pkt.payload.len();
            // If this would overflow the per-stream cap, drop the partial buffer
            // and start fresh.
            if entry.total_bytes + pkt_bytes > KEYFRAME_CACHE_MAX_BYTES {
                entry.packets.clear();
                entry.total_bytes = 0;
                entry.sequence_first = h.seq;
                entry.timestamp_ms = h.timestamp;
            }

            entry.packets.push(pkt.clone());
            entry.total_bytes += pkt_bytes;

            if h.is_frame_end() {
                let completed = KeyframeCacheEntry {
                    packets: std::mem::take(&mut entry.packets),
                    sequence_first: entry.sequence_first,
                    timestamp_ms: entry.timestamp_ms,
                    total_bytes: entry.total_bytes,
                };
                self.keyframe_cache.insert(key.clone(), completed);
                entry.total_bytes = 0;
            }
        } else {
            // Non-keyframe packet: discard any partial buffer for this stream.
            self.keyframe_buffer.remove(&key);
        }
    }

    /// Return a copy of all completed keyframes for a given room.
    ///
    /// Used to replay keyframes to a newly-joined participant before live
    /// forwarding starts.
    pub fn cached_keyframes_for_room(&self, room_name: &str) -> Vec<Vec<wzp_proto::MediaPacket>> {
        self.keyframe_cache
            .iter()
            .filter(|e| e.key().0 == room_name)
            .map(|e| e.value().packets.clone())
            .collect()
    }

    /// Remove all per-room state when a room is closed.
    fn clear_room_state(&self, room_name: &str) {
        self.keyframe_cache.retain(|k, _| k.0 != room_name);
        self.keyframe_buffer.retain(|k, _| k.0 != room_name);
        self.pli_state.retain(|k, _| k.0 != room_name);
        self.stream_owner.retain(|k, _| k.0 != room_name);
    }

    /// PLI suppression window (PRD-video-v1 T4.7).
    const PLI_SUPPRESS_MS: u64 = 200;

    /// Returns `Some(sender_id)` if this PLI should be forwarded upstream,
    /// or `None` if it is suppressed (duplicate within 200 ms) or no sender
    /// is mapped to the given stream.
    ///
    /// Suppresses duplicate PLIs for the same `(room, sender, stream_id)`
    /// within 200 ms. `now` is taken as a parameter so the dedup window can
    /// be exercised deterministically by tests.
    pub fn should_forward_pli(
        &self,
        room_name: &str,
        stream_id: u8,
        now: std::time::Instant,
    ) -> Option<ParticipantId> {
        let owner = self.stream_owner.get(&(room_name.to_string(), stream_id))?;
        let sender_id = *owner;
        drop(owner);
        let key = (room_name.to_string(), sender_id, stream_id);
        if let Some(entry) = self.pli_state.get(&key) {
            let elapsed = now.saturating_duration_since(entry.last_pli).as_millis() as u64;
            if elapsed < Self::PLI_SUPPRESS_MS {
                return None;
            }
        }
        self.pli_state.insert(key, PliState { last_pli: now });
        Some(sender_id)
    }

    /// Get senders for all OTHER participants in a room.
    pub fn others(&self, room_name: &str, participant_id: ParticipantId) -> Vec<ParticipantSender> {
        self.rooms
            .get(room_name)
            .map(|arc| arc.read().unwrap().others(participant_id))
            .unwrap_or_default()
    }

    /// Get room size.
    pub fn room_size(&self, room_name: &str) -> usize {
        self.rooms
            .get(room_name)
            .map(|arc| arc.read().unwrap().len())
            .unwrap_or(0)
    }

    /// Check if a room exists and has participants.
    pub fn is_room_active(&self, room_name: &str) -> bool {
        self.rooms.contains_key(room_name)
    }

    /// List all rooms with their sizes.
    pub fn list(&self) -> Vec<(String, usize)> {
        self.rooms
            .iter()
            .map(|r| (r.key().clone(), r.value().read().unwrap().len()))
            .collect()
    }

    /// Feed a quality report from a participant. If the room-wide weakest
    /// tier changes, returns `(QualityDirective signal, all senders)` for
    /// broadcasting.
    pub fn observe_quality(
        &self,
        room_name: &str,
        participant_id: ParticipantId,
        report: &wzp_proto::packet::QualityReport,
    ) -> Option<(wzp_proto::SignalMessage, Vec<ParticipantSender>)> {
        let arc = self.rooms.get(room_name)?;
        let mut room = arc.write().unwrap();

        let tier_changed = room
            .qualities
            .get_mut(&participant_id)
            .and_then(|pq| pq.observe(report))
            .is_some();

        if !tier_changed {
            return None;
        }

        // Compute the weakest tier across all participants in this room
        let weakest = weakest_tier(room.qualities.values());

        if weakest == room.current_tier {
            return None;
        }

        // Room-wide tier changed -- update and broadcast directive
        let old_tier = room.current_tier;
        room.current_tier = weakest;
        let profile = weakest.profile();
        info!(
            room = room_name,
            old_tier = ?old_tier,
            new_tier = ?weakest,
            codec = ?profile.codec,
            fec_ratio = profile.fec_ratio,
            "room quality directive"
        );

        let directive = wzp_proto::SignalMessage::QualityDirective {
            version: default_signal_version(),
            recommended_profile: profile,
            reason: Some(format!("weakest link: {weakest:?}")),
        };
        let senders = room.all_senders();
        Some((directive, senders))
    }
}

// ---------------------------------------------------------------------------
// TrunkedForwarder — wraps a transport and batches outgoing media into trunk
// frames so multiple packets ride a single QUIC datagram.
// ---------------------------------------------------------------------------

/// Wraps a [`QuinnTransport`] with a [`TrunkBatcher`] so that small media
/// packets are accumulated and sent together in a single QUIC datagram.
pub struct TrunkedForwarder {
    transport: Arc<wzp_transport::QuinnTransport>,
    batcher: TrunkBatcher,
    session_id: [u8; 2],
}

impl TrunkedForwarder {
    /// Create a new trunked forwarder.
    ///
    /// `session_id` tags every entry pushed into the batcher so the receiver
    /// can demultiplex packets by session. The batcher's `max_bytes` is
    /// initialized from the transport's current PMTUD-discovered MTU so that
    /// trunk frames fill the largest datagram the path supports (instead of
    /// the conservative 1200-byte default).
    pub fn new(transport: Arc<wzp_transport::QuinnTransport>, session_id: [u8; 2]) -> Self {
        let mut batcher = TrunkBatcher::new();
        if let Some(mtu) = transport.max_datagram_size() {
            batcher.max_bytes = mtu;
        }
        Self {
            transport,
            batcher,
            session_id,
        }
    }

    /// Push a media packet into the batcher.  If the batcher is full it will
    /// flush automatically and the resulting trunk frame is sent immediately.
    ///
    /// Also refreshes `max_bytes` from the transport's PMTUD-discovered MTU
    /// so the batcher fills larger datagrams as the path MTU grows.
    pub async fn send(&mut self, pkt: &wzp_proto::MediaPacket) -> anyhow::Result<()> {
        // Refresh batcher limit from PMTUD (cheap: reads an atomic in quinn).
        if let Some(mtu) = self.transport.max_datagram_size() {
            self.batcher.max_bytes = mtu;
        }
        let payload: Bytes = pkt.to_bytes();
        if let Some(frame) = self.batcher.push(self.session_id, payload) {
            self.send_frame(&frame)?;
        }
        Ok(())
    }

    /// Flush any pending packets — called on the 5 ms timer tick.
    pub async fn flush(&mut self) -> anyhow::Result<()> {
        if let Some(frame) = self.batcher.flush() {
            self.send_frame(&frame)?;
        }
        Ok(())
    }

    /// Return the flush interval configured on the inner batcher.
    pub fn flush_interval(&self) -> Duration {
        self.batcher.flush_interval
    }

    fn send_frame(&self, frame: &TrunkFrame) -> anyhow::Result<()> {
        self.transport
            .send_trunk(frame)
            .map_err(|e| anyhow::anyhow!(e))
    }
}

// ---------------------------------------------------------------------------
// Signal handling for room-mode participants
// ---------------------------------------------------------------------------

/// Receive signal loop for one participant in a room.
///
/// Currently handles `PictureLossIndication` suppression (T4.7): if multiple
/// receivers PLI the same stream within 200 ms, only the first is forwarded
/// upstream.
pub async fn run_participant_signals(
    room_mgr: Arc<RoomManager>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
) {
    let addr = transport.connection().remote_address();
    info!(
        room = %room_name,
        participant = participant_id,
        %addr,
        "signal loop started"
    );

    loop {
        match transport.recv_signal().await {
            Ok(Some(wzp_proto::SignalMessage::PictureLossIndication { stream_id, .. })) => {
                match room_mgr.should_forward_pli(&room_name, stream_id, std::time::Instant::now())
                {
                    Some(_target_id) => {
                        // Forward PLI to the specific sender that owns this stream.
                        let others = room_mgr.others(&room_name, participant_id);
                        for sender in &others {
                            if let ParticipantSender::Quic(t) = sender {
                                let msg = wzp_proto::SignalMessage::PictureLossIndication {
                                    version: default_signal_version(),
                                    stream_id,
                                };
                                if let Err(e) = t.send_signal(&msg).await {
                                    warn!(
                                        room = %room_name,
                                        participant = participant_id,
                                        peer = %t.connection().remote_address(),
                                        "PLI forward error: {e}"
                                    );
                                }
                            }
                        }
                    }
                    None => {
                        debug!(
                            room = %room_name,
                            participant = participant_id,
                            stream_id,
                            "PLI suppressed (within 200 ms window)"
                        );
                    }
                }
            }
            Ok(Some(_other)) => {
                // Other signals are not handled in room mode yet.
            }
            Ok(None) => {
                info!(%addr, participant = participant_id, "signal stream closed");
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("timed out") || msg.contains("reset") || msg.contains("closed") {
                    info!(%addr, participant = participant_id, "signal connection closed: {e}");
                } else {
                    error!(%addr, participant = participant_id, "signal recv error: {e}");
                }
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// run_participant — the hot-path forwarding loop
// ---------------------------------------------------------------------------

/// Run the receive loop for one participant in a room.
/// Forwards all received packets to every other participant.
///
/// When `trunking_enabled` is true, outgoing packets are accumulated per-peer
/// into [`TrunkedForwarder`]s and flushed every 5 ms or when the batcher is
/// full, reducing QUIC datagram overhead.
pub async fn run_participant(
    room_mgr: Arc<RoomManager>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: String,
    trunking_enabled: bool,
    debug_tap: Option<DebugTap>,
    federation_tx: Option<tokio::sync::mpsc::Sender<FederationMediaOut>>,
    federation_room_hash: Option<[u8; 8]>,
    is_authenticated: bool,
) {
    if trunking_enabled {
        run_participant_trunked(
            room_mgr,
            room_name,
            participant_id,
            transport,
            metrics,
            session_id,
            is_authenticated,
        )
        .await;
    } else {
        run_participant_plain(
            room_mgr,
            room_name,
            participant_id,
            transport,
            metrics,
            session_id,
            debug_tap,
            federation_tx,
            federation_room_hash,
            is_authenticated,
        )
        .await;
    }
}

/// Plain (non-trunked) forwarding loop — original behaviour.
async fn run_participant_plain(
    room_mgr: Arc<RoomManager>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: String,
    debug_tap: Option<DebugTap>,
    federation_tx: Option<tokio::sync::mpsc::Sender<FederationMediaOut>>,
    federation_room_hash: Option<[u8; 8]>,
    is_authenticated: bool,
) {
    let addr = transport.connection().remote_address();
    let mut packets_forwarded = 0u64;
    let mut last_recv_instant = std::time::Instant::now();
    let mut max_recv_gap_ms = 0u64;
    let mut max_forward_ms = 0u64;
    let mut send_errors = 0u64;
    let mut last_log_instant = std::time::Instant::now();
    let mut conformance = if is_authenticated {
        ConformanceMeter::with_token_bucket(crate::conformance::TokenBucket::for_audio_session())
    } else {
        // Anonymous participants get the same per-session audio cap.
        // Monthly quota (1 GB vs 50 GB) is tracked separately.
        ConformanceMeter::with_token_bucket(crate::conformance::TokenBucket::for_audio_session())
    };

    let mut tap_stats = if debug_tap.as_ref().map_or(false, |t| t.matches(&room_name)) {
        Some(TapStats::new())
    } else {
        None
    };

    info!(
        room = %room_name,
        participant = participant_id,
        %addr,
        session = %session_id,
        "forwarding loop started (plain)"
    );

    loop {
        let pkt = match transport.recv_media().await {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                info!(%addr, participant = participant_id, forwarded = packets_forwarded, "disconnected (stream ended)");
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("timed out") || msg.contains("reset") || msg.contains("closed") {
                    info!(%addr, participant = participant_id, forwarded = packets_forwarded, "connection closed: {e}");
                } else {
                    error!(%addr, participant = participant_id, forwarded = packets_forwarded, "recv error: {e}");
                }
                break;
            }
        };

        // Cache keyframe packets for fast join-to-first-frame replay.
        room_mgr.update_keyframe_cache(&room_name, participant_id, &pkt);
        // Register this participant as the owner of this stream for PLI routing.
        if !pkt.header.is_repair() {
            room_mgr
                .stream_owner
                .insert((room_name.clone(), pkt.header.stream_id), participant_id);
        }

        let recv_gap_ms = last_recv_instant.elapsed().as_millis() as u64;
        last_recv_instant = std::time::Instant::now();
        if recv_gap_ms > max_recv_gap_ms {
            max_recv_gap_ms = recv_gap_ms;
        }
        // Log if recv gap is suspiciously large (>200ms = missed ~10 packets)
        if recv_gap_ms > 200 {
            warn!(
                room = %room_name,
                participant = participant_id,
                recv_gap_ms,
                seq = pkt.header.seq,
                "large recv gap"
            );
        }

        // Conformance check (Tier A/B/C — observe-only)
        let violation = conformance
            .observe(&pkt.header, pkt.payload.len(), std::time::Instant::now())
            .err();
        metrics.record_conformance(&pkt.header, pkt.payload.len(), recv_gap_ms, violation);
        if let Some(v) = violation {
            warn!(
                room = %room_name,
                participant = participant_id,
                codec = ?pkt.header.codec_id,
                seq = pkt.header.seq,
                violation = ?v,
                "conformance violation"
            );
        }

        // Update per-session quality metrics if a quality report is present
        if let Some(ref report) = pkt.quality_report {
            metrics.update_session_quality(&session_id, report);
        }

        // Get current list of other participants + check quality directive
        let lock_start = std::time::Instant::now();
        let (others, quality_directive) = {
            let directive = if let Some(ref report) = pkt.quality_report {
                room_mgr.observe_quality(&room_name, participant_id, report)
            } else {
                None
            };
            let o = room_mgr.others(&room_name, participant_id);
            (o, directive)
        };
        let lock_ms = lock_start.elapsed().as_millis() as u64;
        if lock_ms > 10 {
            warn!(
                room = %room_name,
                participant = participant_id,
                lock_ms,
                "slow room_mgr lock"
            );
        }

        // Broadcast quality directive to all participants if tier changed
        if let Some((directive, all_senders)) = quality_directive {
            if let Some(ref tap) = debug_tap {
                if tap.matches(&room_name) {
                    tap.log_signal(&room_name, &directive);
                }
            }
            broadcast_signal(&all_senders, &directive).await;
        }

        // Debug tap: log packet metadata + record stats
        if let Some(ref tap) = debug_tap {
            if tap.matches(&room_name) {
                tap.log_packet(&room_name, "in", &addr, &pkt, others.len());
            }
        }
        if let Some(ref mut ts) = tap_stats {
            ts.record_in(&pkt, others.len());
        }

        // Forward to all others
        let fwd_start = std::time::Instant::now();
        let pkt_bytes = pkt.payload.len() as u64;
        for other in &others {
            match other {
                ParticipantSender::Quic(t) => {
                    if let Err(e) = t.send_media(&pkt).await {
                        send_errors += 1;
                        if send_errors <= 5 || send_errors % 100 == 0 {
                            warn!(
                                room = %room_name,
                                participant = participant_id,
                                peer = %t.connection().remote_address(),
                                total_send_errors = send_errors,
                                "send_media error: {e}"
                            );
                        }
                    }
                }
                ParticipantSender::WebSocket(_) => {
                    let _ = other.send_raw(&pkt.payload).await;
                }
            }
        }

        // Federation: forward to active peer relays via channel
        if let Some(ref fed_tx) = federation_tx {
            let data = pkt.to_bytes();
            let _ = fed_tx.try_send(FederationMediaOut {
                room_name: room_name.clone(),
                room_hash: federation_room_hash
                    .unwrap_or_else(|| crate::federation::room_hash(&room_name)),
                data,
            });
        }

        let fwd_ms = fwd_start.elapsed().as_millis() as u64;
        if fwd_ms > max_forward_ms {
            max_forward_ms = fwd_ms;
        }
        if fwd_ms > 50 {
            warn!(
                room = %room_name,
                participant = participant_id,
                fwd_ms,
                fan_out = others.len(),
                "slow forward"
            );
        }

        let fan_out = others.len() as u64;
        metrics.packets_forwarded.inc_by(fan_out);
        metrics.bytes_forwarded.inc_by(pkt_bytes * fan_out);
        packets_forwarded += 1;

        // Periodic stats log every 5 seconds
        if last_log_instant.elapsed() >= Duration::from_secs(5) {
            let room_size = room_mgr.room_size(&room_name);
            info!(
                room = %room_name,
                participant = participant_id,
                forwarded = packets_forwarded,
                room_size,
                fan_out,
                max_recv_gap_ms,
                max_forward_ms,
                send_errors,
                "participant stats"
            );
            if let (Some(tap), Some(ts)) = (&debug_tap, &mut tap_stats) {
                tap.log_stats(&room_name, ts);
                ts.reset_period();
            }
            max_recv_gap_ms = 0;
            max_forward_ms = 0;
            last_log_instant = std::time::Instant::now();
        }
    }

    // Clean up — leave room and broadcast update to remaining participants
    if let Some((update, senders)) = room_mgr.leave(&room_name, participant_id) {
        if let Some(ref tap) = debug_tap {
            if tap.matches(&room_name) {
                tap.log_event(
                    &room_name,
                    "leave",
                    &format!(
                        "participant={participant_id} addr={addr} forwarded={packets_forwarded}"
                    ),
                );
                tap.log_signal(&room_name, &update);
            }
        }
        broadcast_signal(&senders, &update).await;
    } else if let Some(ref tap) = debug_tap {
        if tap.matches(&room_name) {
            tap.log_event(
                &room_name,
                "leave",
                &format!("participant={participant_id} addr={addr} (room closed)"),
            );
        }
    }
}

/// Trunked forwarding loop — batches outgoing packets per peer.
async fn run_participant_trunked(
    room_mgr: Arc<RoomManager>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: String,
    _is_authenticated: bool,
) {
    use std::collections::HashMap;

    let addr = transport.connection().remote_address();
    let mut packets_forwarded = 0u64;
    let mut last_recv_instant = std::time::Instant::now();
    let mut max_recv_gap_ms = 0u64;
    let mut max_forward_ms = 0u64;
    let mut send_errors = 0u64;
    let mut last_log_instant = std::time::Instant::now();
    let mut conformance =
        ConformanceMeter::with_token_bucket(crate::conformance::TokenBucket::for_audio_session());

    info!(
        room = %room_name,
        participant = participant_id,
        %addr,
        session = %session_id,
        "forwarding loop started (trunked)"
    );

    // Per-peer TrunkedForwarders, keyed by the raw pointer of the peer
    // transport (stable for the Arc's lifetime).  We use the remote address
    // string as the key since it is unique per connection.
    let mut forwarders: HashMap<std::net::SocketAddr, TrunkedForwarder> = HashMap::new();

    // Derive a 2-byte session tag from the session_id hex string.
    let sid_bytes: [u8; 2] = parse_session_id_bytes(&session_id);

    let mut flush_interval = tokio::time::interval(Duration::from_millis(5));
    // Don't let missed ticks pile up — skip them and move on.
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;

            result = transport.recv_media() => {
                let pkt = match result {
                    Ok(Some(pkt)) => pkt,
                    Ok(None) => {
                        info!(%addr, participant = participant_id, forwarded = packets_forwarded, "disconnected (stream ended)");
                        break;
                    }
                    Err(e) => {
                        error!(%addr, participant = participant_id, forwarded = packets_forwarded, "recv error: {e}");
                        break;
                    }
                };

                // Cache keyframe packets for fast join-to-first-frame replay.
                room_mgr.update_keyframe_cache(&room_name, participant_id, &pkt);
                // Register this participant as the owner of this stream for PLI routing.
                if !pkt.header.is_repair() {
                    room_mgr.stream_owner.insert(
                        (room_name.clone(), pkt.header.stream_id),
                        participant_id,
                    );
                }

                let recv_gap_ms = last_recv_instant.elapsed().as_millis() as u64;
                last_recv_instant = std::time::Instant::now();
                if recv_gap_ms > max_recv_gap_ms {
                    max_recv_gap_ms = recv_gap_ms;
                }
                if recv_gap_ms > 200 {
                    warn!(
                        room = %room_name,
                        participant = participant_id,
                        recv_gap_ms,
                        seq = pkt.header.seq,
                        "large recv gap (trunked)"
                    );
                }

                // Conformance check (Tier A/B/C — observe-only)
                let violation = conformance
                    .observe(&pkt.header, pkt.payload.len(), std::time::Instant::now())
                    .err();
                metrics.record_conformance(&pkt.header, pkt.payload.len(), recv_gap_ms, violation);
                if let Some(v) = violation {
                    warn!(
                        room = %room_name,
                        participant = participant_id,
                        codec = ?pkt.header.codec_id,
                        seq = pkt.header.seq,
                        violation = ?v,
                        "conformance violation (trunked)"
                    );
                }

                if let Some(ref report) = pkt.quality_report {
                    metrics.update_session_quality(&session_id, report);
                }

                let lock_start = std::time::Instant::now();
                let (others, quality_directive) = {
                    let directive = if let Some(ref report) = pkt.quality_report {
                        room_mgr.observe_quality(&room_name, participant_id, report)
                    } else {
                        None
                    };
                    let o = room_mgr.others(&room_name, participant_id);
                    (o, directive)
                };
                let lock_ms = lock_start.elapsed().as_millis() as u64;
                if lock_ms > 10 {
                    warn!(
                        room = %room_name,
                        participant = participant_id,
                        lock_ms,
                        "slow room_mgr lock (trunked)"
                    );
                }

                // Broadcast quality directive to all participants if tier changed
                if let Some((directive, all_senders)) = quality_directive {
                    broadcast_signal(&all_senders, &directive).await;
                }

                let fwd_start = std::time::Instant::now();
                let pkt_bytes = pkt.payload.len() as u64;
                for other in &others {
                    match other {
                        ParticipantSender::Quic(t) => {
                            let peer_addr = t.connection().remote_address();
                            let fwd = forwarders
                                .entry(peer_addr)
                                .or_insert_with(|| TrunkedForwarder::new(t.clone(), sid_bytes));
                            if let Err(e) = fwd.send(&pkt).await {
                                send_errors += 1;
                                if send_errors <= 5 || send_errors % 100 == 0 {
                                    warn!(
                                        room = %room_name,
                                        participant = participant_id,
                                        peer = %peer_addr,
                                        total_send_errors = send_errors,
                                        "trunked send error: {e}"
                                    );
                                }
                            }
                        }
                        ParticipantSender::WebSocket(_) => {
                            let _ = other.send_raw(&pkt.payload).await;
                        }
                    }
                }
                let fwd_ms = fwd_start.elapsed().as_millis() as u64;
                if fwd_ms > max_forward_ms {
                    max_forward_ms = fwd_ms;
                }
                if fwd_ms > 50 {
                    warn!(
                        room = %room_name,
                        participant = participant_id,
                        fwd_ms,
                        fan_out = others.len(),
                        "slow forward (trunked)"
                    );
                }

                let fan_out = others.len() as u64;
                metrics.packets_forwarded.inc_by(fan_out);
                metrics.bytes_forwarded.inc_by(pkt_bytes * fan_out);
                packets_forwarded += 1;

                // Periodic stats every 5 seconds
                if last_log_instant.elapsed() >= Duration::from_secs(5) {
                    let room_size = room_mgr.room_size(&room_name);
                    info!(
                        room = %room_name,
                        participant = participant_id,
                        forwarded = packets_forwarded,
                        room_size,
                        fan_out,
                        max_recv_gap_ms,
                        max_forward_ms,
                        send_errors,
                        "participant stats (trunked)"
                    );
                    max_recv_gap_ms = 0;
                    max_forward_ms = 0;
                    last_log_instant = std::time::Instant::now();
                }
            }

            _ = flush_interval.tick() => {
                for fwd in forwarders.values_mut() {
                    if let Err(e) = fwd.flush().await {
                        send_errors += 1;
                        if send_errors <= 5 || send_errors % 100 == 0 {
                            warn!(
                                room = %room_name,
                                participant = participant_id,
                                total_send_errors = send_errors,
                                "trunk flush error: {e}"
                            );
                        }
                    }
                }
            }
        }
    }

    // Final flush — send any remaining buffered packets.
    for fwd in forwarders.values_mut() {
        let _ = fwd.flush().await;
    }

    if let Some((update, senders)) = room_mgr.leave(&room_name, participant_id) {
        broadcast_signal(&senders, &update).await;
    }
}

/// Parse up to the first 2 bytes of a hex session-id string into `[u8; 2]`.
fn parse_session_id_bytes(session_id: &str) -> [u8; 2] {
    let bytes: Vec<u8> = (0..session_id.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(session_id.get(i..i + 2)?, 16).ok())
        .collect();
    let mut out = [0u8; 2];
    for (i, b) in bytes.iter().take(2).enumerate() {
        out[i] = *b;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_join_leave() {
        let mgr = RoomManager::new();
        assert_eq!(mgr.room_size("test"), 0);
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn acl_open_mode_allows_all() {
        let mgr = RoomManager::new();
        assert!(mgr.is_authorized("any-room", None));
        assert!(mgr.is_authorized("any-room", Some("abc")));
    }

    #[test]
    fn acl_enforced_requires_fingerprint() {
        let mgr = RoomManager::with_acl();
        assert!(!mgr.is_authorized("room1", None));
        // Room not in ACL = open to any authenticated user
        assert!(mgr.is_authorized("room1", Some("abc")));
    }

    #[test]
    fn acl_restricts_to_allowed() {
        let mgr = RoomManager::with_acl();
        mgr.allow("room1", "alice");
        mgr.allow("room1", "bob");
        assert!(mgr.is_authorized("room1", Some("alice")));
        assert!(mgr.is_authorized("room1", Some("bob")));
        assert!(!mgr.is_authorized("room1", Some("eve")));
    }

    #[test]
    fn parse_session_id_bytes_works() {
        assert_eq!(parse_session_id_bytes("abcd"), [0xab, 0xcd]);
        assert_eq!(parse_session_id_bytes("ff00"), [0xff, 0x00]);
        assert_eq!(parse_session_id_bytes(""), [0x00, 0x00]);
        // Longer hex strings: only first 2 bytes taken
        assert_eq!(parse_session_id_bytes("aabbccdd"), [0xaa, 0xbb]);
    }

    /// Helper: create a minimal MediaPacket with the given payload bytes.
    fn make_test_packet(payload: &[u8]) -> wzp_proto::MediaPacket {
        wzp_proto::MediaPacket {
            header: wzp_proto::packet::MediaHeader {
                version: 2,
                flags: 0,
                media_type: wzp_proto::MediaType::Audio,
                codec_id: wzp_proto::CodecId::Opus16k,
                stream_id: 0,
                fec_ratio: 0,
                seq: 1,
                timestamp: 100,
                fec_block: 0,
            },
            payload: Bytes::from(payload.to_vec()),
            quality_report: None,
        }
    }

    /// Push 3 packets into a batcher (simulating TrunkedForwarder.send),
    /// then flush and verify all 3 appear in a single TrunkFrame.
    #[test]
    fn trunked_forwarder_batches() {
        let session_id: [u8; 2] = [0x00, 0x01];
        let mut batcher = TrunkBatcher::new();
        // Ensure max_entries is high enough that 3 packets don't auto-flush.
        batcher.max_entries = 10;
        batcher.max_bytes = 4096;

        let pkts = [
            make_test_packet(b"aaa"),
            make_test_packet(b"bbb"),
            make_test_packet(b"ccc"),
        ];

        for pkt in &pkts {
            let payload = pkt.to_bytes();
            let flushed = batcher.push(session_id, payload);
            // Should NOT auto-flush — we are below max_entries.
            assert!(flushed.is_none(), "unexpected auto-flush");
        }

        // Explicit flush (simulates the 5 ms timer tick).
        let frame = batcher.flush().expect("expected a frame with 3 entries");
        assert_eq!(frame.len(), 3);
        for entry in &frame.packets {
            assert_eq!(entry.session_id, session_id);
        }
    }

    /// Push exactly max_entries packets and verify the batcher auto-flushes
    /// on the last push (simulating TrunkedForwarder.send triggering a send).
    #[test]
    fn trunked_forwarder_auto_flushes() {
        let session_id: [u8; 2] = [0x00, 0x02];
        let mut batcher = TrunkBatcher::new();
        batcher.max_entries = 5;
        batcher.max_bytes = 8192;

        let pkt = make_test_packet(b"hello");
        let mut auto_flushed: Option<wzp_proto::packet::TrunkFrame> = None;

        for i in 0..5 {
            let payload = pkt.to_bytes();
            if let Some(frame) = batcher.push(session_id, payload) {
                assert!(auto_flushed.is_none(), "should auto-flush exactly once");
                auto_flushed = Some(frame);
                // The auto-flush should happen on the 5th push (max_entries = 5).
                assert_eq!(i, 4, "expected auto-flush on the last push");
            }
        }

        let frame = auto_flushed.expect("batcher should have auto-flushed at max_entries");
        assert_eq!(frame.len(), 5);
        for entry in &frame.packets {
            assert_eq!(entry.session_id, session_id);
        }

        // Batcher should now be empty — nothing to flush.
        assert!(batcher.flush().is_none());
    }

    fn make_report(loss_pct_f: f32, rtt_ms: u16) -> wzp_proto::packet::QualityReport {
        wzp_proto::packet::QualityReport {
            loss_pct: (loss_pct_f / 100.0 * 255.0) as u8,
            rtt_4ms: (rtt_ms / 4) as u8,
            jitter_ms: 10,
            bitrate_cap_kbps: 200,
        }
    }

    #[test]
    fn participant_quality_starts_good() {
        let pq = ParticipantQuality::new();
        assert_eq!(pq.current_tier, Tier::Good);
    }

    #[test]
    fn participant_quality_degrades_on_bad_reports() {
        let mut pq = ParticipantQuality::new();
        let bad = make_report(50.0, 300);
        // Feed enough bad reports to trigger downgrade (3 consecutive)
        for _ in 0..5 {
            pq.observe(&bad);
        }
        assert_ne!(pq.current_tier, Tier::Good, "should degrade from Good");
    }

    #[test]
    fn weakest_tier_picks_worst() {
        let good = ParticipantQuality::new();
        // good stays at Good tier

        let mut bad = ParticipantQuality::new();
        let bad_report = make_report(50.0, 300);
        for _ in 0..5 {
            bad.observe(&bad_report);
        }
        // bad should be degraded or catastrophic

        let participants = vec![good, bad];
        let weakest = weakest_tier(participants.iter());
        assert_ne!(
            weakest,
            Tier::Good,
            "weakest should not be Good when one participant is bad"
        );
    }

    // PLI suppression tests (T4.7 rework).
    //
    // `should_forward_pli` takes `now: Instant` as a parameter so we can
    // drive the dedup window deterministically. Each test uses a base
    // `Instant::now()` and offsets via `+ Duration::from_millis(N)`.

    fn seed_stream_owner(mgr: &RoomManager, room: &str, stream_id: u8, owner: ParticipantId) {
        mgr.stream_owner
            .insert((room.to_string(), stream_id), owner);
    }

    #[test]
    fn pli_first_forwards() {
        let mgr = RoomManager::new();
        let owner: ParticipantId = 1;
        seed_stream_owner(&mgr, "room", 0, owner);
        let t0 = std::time::Instant::now();
        assert_eq!(
            mgr.should_forward_pli("room", 0, t0),
            Some(owner),
            "first PLI for a stream should be forwarded"
        );
    }

    #[test]
    fn pli_within_window_suppressed() {
        let mgr = RoomManager::new();
        let owner: ParticipantId = 1;
        seed_stream_owner(&mgr, "room", 0, owner);
        let t0 = std::time::Instant::now();
        assert!(mgr.should_forward_pli("room", 0, t0).is_some());
        let t1 = t0 + std::time::Duration::from_millis(100);
        assert_eq!(
            mgr.should_forward_pli("room", 0, t1),
            None,
            "PLI within the 200 ms suppression window must be dropped"
        );
    }

    #[test]
    fn pli_after_window_forwards() {
        let mgr = RoomManager::new();
        let owner: ParticipantId = 1;
        seed_stream_owner(&mgr, "room", 0, owner);
        let t0 = std::time::Instant::now();
        assert!(mgr.should_forward_pli("room", 0, t0).is_some());
        let t1 = t0 + std::time::Duration::from_millis(300);
        assert_eq!(
            mgr.should_forward_pli("room", 0, t1),
            Some(owner),
            "PLI after the suppression window should forward again"
        );
    }

    #[test]
    fn pli_different_streams_independent() {
        let mgr = RoomManager::new();
        let owner_a: ParticipantId = 1;
        let owner_b: ParticipantId = 2;
        seed_stream_owner(&mgr, "room", 0, owner_a);
        seed_stream_owner(&mgr, "room", 1, owner_b);
        let t0 = std::time::Instant::now();
        assert!(mgr.should_forward_pli("room", 0, t0).is_some());
        assert!(
            mgr.should_forward_pli("room", 1, t0).is_some(),
            "PLI on a different stream within the window must not be suppressed"
        );
    }

    #[test]
    fn pli_different_rooms_independent() {
        let mgr = RoomManager::new();
        let owner_a: ParticipantId = 1;
        let owner_b: ParticipantId = 2;
        seed_stream_owner(&mgr, "room-a", 0, owner_a);
        seed_stream_owner(&mgr, "room-b", 0, owner_b);
        let t0 = std::time::Instant::now();
        assert!(mgr.should_forward_pli("room-a", 0, t0).is_some());
        assert!(
            mgr.should_forward_pli("room-b", 0, t0).is_some(),
            "PLI in a different room within the window must not be suppressed"
        );
    }

    #[test]
    fn pli_no_owner_returns_none() {
        let mgr = RoomManager::new();
        let t0 = std::time::Instant::now();
        assert_eq!(
            mgr.should_forward_pli("room", 0, t0),
            None,
            "PLI for a stream with no mapped owner should return None"
        );
    }
}
