//! Room management for multi-party calls.
//!
//! Each room holds N participants. When one participant sends a media packet,
//! the relay forwards it to all other participants in the room (SFU model).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Mutex;
use tracing::{debug, error, info, trace, warn};

use wzp_proto::packet::TrunkFrame;
use wzp_proto::MediaTransport;

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

    pub fn log_packet(&self, room: &str, dir: &str, addr: &std::net::SocketAddr, pkt: &wzp_proto::MediaPacket, fan_out: usize) {
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
            fec_sym = h.fec_symbol,
            repair = h.is_repair,
            len = pkt.payload.len(),
            fan_out,
            "TAP"
        );
    }
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
            ParticipantSender::WebSocket(tx) => {
                tx.try_send(Bytes::copy_from_slice(data))
                    .map_err(|e| format!("ws send: {e}"))
            }
            ParticipantSender::Quic(transport) => {
                let pkt = wzp_proto::MediaPacket {
                    header: wzp_proto::packet::MediaHeader::default_pcm(),
                    payload: Bytes::copy_from_slice(data),
                    quality_report: None,
                };
                transport.send_media(&pkt).await.map_err(|e| format!("quic send: {e}"))
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
}

impl Room {
    fn new() -> Self {
        Self {
            participants: Vec::new(),
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
        self.participants.push(Participant { id, _addr: addr, sender, fingerprint, alias });
        id
    }

    fn remove(&mut self, id: ParticipantId) {
        self.participants.retain(|p| p.id != id);
        info!(room_size = self.participants.len(), participant = id, "left room");
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

/// Manages all rooms on the relay.
pub struct RoomManager {
    rooms: HashMap<String, Room>,
    /// Room access control list. Maps hashed room name → allowed fingerprints.
    /// When `None`, rooms are open (no auth mode). When `Some`, only listed
    /// fingerprints can join the corresponding room.
    acl: Option<HashMap<String, HashSet<String>>>,
    /// Channel for room lifecycle events (federation subscribes).
    event_tx: tokio::sync::broadcast::Sender<RoomEvent>,
}

impl RoomManager {
    pub fn new() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            rooms: HashMap::new(),
            acl: None,
            event_tx,
        }
    }

    /// Create a room manager with ACL enforcement enabled.
    pub fn with_acl() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            rooms: HashMap::new(),
            acl: Some(HashMap::new()),
            event_tx,
        }
    }

    /// Subscribe to room lifecycle events (for federation).
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<RoomEvent> {
        self.event_tx.subscribe()
    }

    /// Grant a fingerprint access to a room.
    pub fn allow(&mut self, room_name: &str, fingerprint: &str) {
        if let Some(ref mut acl) = self.acl {
            acl.entry(room_name.to_string())
                .or_default()
                .insert(fingerprint.to_string());
        }
    }

    /// Check if a fingerprint is authorized to join a room.
    /// Returns true if ACL is disabled (open mode) or the fingerprint is in the allow list.
    pub fn is_authorized(&self, room_name: &str, fingerprint: Option<&str>) -> bool {
        match (&self.acl, fingerprint) {
            (None, _) => true, // no ACL = open
            (Some(_), None) => false, // ACL enabled but no fingerprint
            (Some(acl), Some(fp)) => {
                // Room not in ACL = open room (allow anyone authenticated)
                match acl.get(room_name) {
                    None => true,
                    Some(allowed) => allowed.contains(fp),
                }
            }
        }
    }

    /// Join a room. Returns (participant_id, room_update_msg, all_senders) for broadcasting.
    pub fn join(
        &mut self,
        room_name: &str,
        addr: std::net::SocketAddr,
        sender: ParticipantSender,
        fingerprint: Option<&str>,
        alias: Option<&str>,
    ) -> Result<(ParticipantId, wzp_proto::SignalMessage, Vec<ParticipantSender>), String> {
        if !self.is_authorized(room_name, fingerprint) {
            warn!(room = room_name, fingerprint = ?fingerprint, "unauthorized room join attempt");
            return Err("not authorized for this room".to_string());
        }
        let was_empty = !self.rooms.contains_key(room_name)
            || self.rooms.get(room_name).map_or(true, |r| r.is_empty());
        let room = self.rooms.entry(room_name.to_string()).or_insert_with(Room::new);
        let id = room.add(addr, sender, fingerprint.map(|s| s.to_string()), alias.map(|s| s.to_string()));
        if was_empty {
            let _ = self.event_tx.send(RoomEvent::LocalJoin { room: room_name.to_string() });
        }
        let update = wzp_proto::SignalMessage::RoomUpdate {
            count: room.len() as u32,
            participants: room.participant_list(),
        };
        let senders = room.all_senders();
        Ok((id, update, senders))
    }

    /// Join a room via WebSocket. Convenience wrapper around `join()`.
    pub fn join_ws(
        &mut self,
        room_name: &str,
        addr: std::net::SocketAddr,
        sender: tokio::sync::mpsc::Sender<Bytes>,
        fingerprint: Option<&str>,
    ) -> Result<ParticipantId, String> {
        let (id, _update, _senders) = self.join(room_name, addr, ParticipantSender::WebSocket(sender), fingerprint, None)?;
        Ok(id)
    }

    /// Get list of active room names.
    pub fn active_rooms(&self) -> Vec<String> {
        self.rooms.keys().cloned().collect()
    }

    /// Get all senders for participants in a room (for federation inbound media delivery).
    pub fn local_senders(&self, room_name: &str) -> Vec<ParticipantSender> {
        self.rooms.get(room_name)
            .map(|room| room.participants.iter()
                .map(|p| p.sender.clone())
                .collect())
            .unwrap_or_default()
    }

    /// Leave a room. Returns (room_update_msg, remaining_senders) for broadcasting, or None if room is now empty.
    pub fn leave(&mut self, room_name: &str, participant_id: ParticipantId) -> Option<(wzp_proto::SignalMessage, Vec<ParticipantSender>)> {
        if let Some(room) = self.rooms.get_mut(room_name) {
            room.remove(participant_id);
            if room.is_empty() {
                self.rooms.remove(room_name);
                let _ = self.event_tx.send(RoomEvent::LocalLeave { room: room_name.to_string() });
                info!(room = room_name, "room closed (empty)");
                return None;
            }
            let update = wzp_proto::SignalMessage::RoomUpdate {
                count: room.len() as u32,
                participants: room.participant_list(),
            };
            let senders = room.all_senders();
            Some((update, senders))
        } else {
            None
        }
    }

    /// Get senders for all OTHER participants in a room.
    pub fn others(
        &self,
        room_name: &str,
        participant_id: ParticipantId,
    ) -> Vec<ParticipantSender> {
        self.rooms
            .get(room_name)
            .map(|r| r.others(participant_id))
            .unwrap_or_default()
    }

    /// Get room size.
    pub fn room_size(&self, room_name: &str) -> usize {
        self.rooms.get(room_name).map(|r| r.len()).unwrap_or(0)
    }

    /// List all rooms with their sizes.
    pub fn list(&self) -> Vec<(String, usize)> {
        self.rooms.iter().map(|(k, v)| (k.clone(), v.len())).collect()
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
    /// can demultiplex packets by session.
    pub fn new(transport: Arc<wzp_transport::QuinnTransport>, session_id: [u8; 2]) -> Self {
        Self {
            transport,
            batcher: TrunkBatcher::new(),
            session_id,
        }
    }

    /// Push a media packet into the batcher.  If the batcher is full it will
    /// flush automatically and the resulting trunk frame is sent immediately.
    pub async fn send(&mut self, pkt: &wzp_proto::MediaPacket) -> anyhow::Result<()> {
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
        self.transport.send_trunk(frame).map_err(|e| anyhow::anyhow!(e))
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
    room_mgr: Arc<Mutex<RoomManager>>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: &str,
    trunking_enabled: bool,
    debug_tap: Option<DebugTap>,
    federation_tx: Option<tokio::sync::mpsc::Sender<FederationMediaOut>>,
) {
    if trunking_enabled {
        run_participant_trunked(
            room_mgr, room_name, participant_id, transport, metrics, session_id,
        )
        .await;
    } else {
        run_participant_plain(
            room_mgr, room_name, participant_id, transport, metrics, session_id, debug_tap, federation_tx,
        )
        .await;
    }
}

/// Plain (non-trunked) forwarding loop — original behaviour.
async fn run_participant_plain(
    room_mgr: Arc<Mutex<RoomManager>>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: &str,
    debug_tap: Option<DebugTap>,
    federation_tx: Option<tokio::sync::mpsc::Sender<FederationMediaOut>>,
) {
    let addr = transport.connection().remote_address();
    let mut packets_forwarded = 0u64;
    let mut last_recv_instant = std::time::Instant::now();
    let mut max_recv_gap_ms = 0u64;
    let mut max_forward_ms = 0u64;
    let mut send_errors = 0u64;
    let mut last_log_instant = std::time::Instant::now();

    info!(
        room = %room_name,
        participant = participant_id,
        %addr,
        session = session_id,
        "forwarding loop started (plain)"
    );

    loop {
        let recv_start = std::time::Instant::now();
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

        // Update per-session quality metrics if a quality report is present
        if let Some(ref report) = pkt.quality_report {
            metrics.update_session_quality(session_id, report);
        }

        // Get current list of other participants
        let lock_start = std::time::Instant::now();
        let others = {
            let mgr = room_mgr.lock().await;
            mgr.others(&room_name, participant_id)
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

        // Debug tap: log packet metadata
        if let Some(ref tap) = debug_tap {
            if tap.matches(&room_name) {
                tap.log_packet(&room_name, "in", &addr, &pkt, others.len());
            }
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
                room_hash: crate::federation::room_hash(&room_name),
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
            let room_size = {
                let mgr = room_mgr.lock().await;
                mgr.room_size(&room_name)
            };
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
            max_recv_gap_ms = 0;
            max_forward_ms = 0;
            last_log_instant = std::time::Instant::now();
        }
    }

    // Clean up — leave room and broadcast update to remaining participants
    let mut mgr = room_mgr.lock().await;
    if let Some((update, senders)) = mgr.leave(&room_name, participant_id) {
        drop(mgr); // release lock before async broadcast
        broadcast_signal(&senders, &update).await;
    }
}

/// Trunked forwarding loop — batches outgoing packets per peer.
async fn run_participant_trunked(
    room_mgr: Arc<Mutex<RoomManager>>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: &str,
) {
    use std::collections::HashMap;

    let addr = transport.connection().remote_address();
    let mut packets_forwarded = 0u64;
    let mut last_recv_instant = std::time::Instant::now();
    let mut max_recv_gap_ms = 0u64;
    let mut max_forward_ms = 0u64;
    let mut send_errors = 0u64;
    let mut last_log_instant = std::time::Instant::now();

    info!(
        room = %room_name,
        participant = participant_id,
        %addr,
        session = session_id,
        "forwarding loop started (trunked)"
    );

    // Per-peer TrunkedForwarders, keyed by the raw pointer of the peer
    // transport (stable for the Arc's lifetime).  We use the remote address
    // string as the key since it is unique per connection.
    let mut forwarders: HashMap<std::net::SocketAddr, TrunkedForwarder> = HashMap::new();

    // Derive a 2-byte session tag from the session_id hex string.
    let sid_bytes: [u8; 2] = parse_session_id_bytes(session_id);

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

                if let Some(ref report) = pkt.quality_report {
                    metrics.update_session_quality(session_id, report);
                }

                let lock_start = std::time::Instant::now();
                let others = {
                    let mgr = room_mgr.lock().await;
                    mgr.others(&room_name, participant_id)
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
                    let room_size = {
                        let mgr = room_mgr.lock().await;
                        mgr.room_size(&room_name)
                    };
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

    let mut mgr = room_mgr.lock().await;
    if let Some((update, senders)) = mgr.leave(&room_name, participant_id) {
        drop(mgr);
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
        let mut mgr = RoomManager::new();
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
        let mut mgr = RoomManager::with_acl();
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
                version: 0,
                is_repair: false,
                codec_id: wzp_proto::CodecId::Opus16k,
                has_quality_report: false,
                fec_ratio_encoded: 0,
                seq: 1,
                timestamp: 100,
                fec_block: 0,
                fec_symbol: 0,
                reserved: 0,
                csrc_count: 0,
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
}
