//! Relay federation — global room routing between peer relays.
//!
//! Each relay maintains a forwarding table per global room. When a local participant
//! sends media in a global room, it's forwarded to all peer relays that have the room
//! active. Incoming federated media is delivered to local participants and optionally
//! forwarded to other active peers (multi-hop).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use sha2::{Sha256, Digest};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use wzp_proto::{MediaTransport, SignalMessage};
use wzp_transport::QuinnTransport;

use crate::config::{PeerConfig, TrustedConfig};
use crate::room::{self, FederationMediaOut, RoomEvent, RoomManager};

/// Compute 8-byte room hash for federation datagram tagging.
pub fn room_hash(room_name: &str) -> [u8; 8] {
    let h = Sha256::digest(room_name.as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&h[..8]);
    out
}

/// Normalize a fingerprint string (remove colons, lowercase).
fn normalize_fp(fp: &str) -> String {
    fp.replace(':', "").to_lowercase()
}

/// Sliding-window dedup filter for federation datagrams.
/// Tracks recently seen (room_hash, seq) pairs to discard duplicates
/// arriving via multiple federation paths (e.g., A↔B↔C and A↔C).
struct Deduplicator {
    /// Ring buffer of recent packet fingerprints (room_hash XOR'd with seq).
    seen: HashSet<u64>,
    /// Ordered list for eviction.
    order: std::collections::VecDeque<u64>,
    capacity: usize,
}

impl Deduplicator {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashSet::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns true if this packet is a duplicate (already seen).
    fn is_dup(&mut self, room_hash: &[u8; 8], seq: u16) -> bool {
        let key = u64::from_be_bytes(*room_hash) ^ (seq as u64);
        if self.seen.contains(&key) {
            return true;
        }
        if self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        self.seen.insert(key);
        self.order.push_back(key);
        false
    }
}

/// Per-room token bucket rate limiter for federation forwarding.
struct RateLimiter {
    /// Max packets per second per room.
    max_pps: u32,
    /// Tokens remaining in current window.
    tokens: u32,
    /// When the current window started.
    window_start: Instant,
}

impl RateLimiter {
    fn new(max_pps: u32) -> Self {
        Self {
            max_pps,
            tokens: max_pps,
            window_start: Instant::now(),
        }
    }

    /// Returns true if the packet should be allowed through.
    fn allow(&mut self) -> bool {
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.tokens = self.max_pps;
            self.window_start = Instant::now();
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

/// Active link to a peer relay.
struct PeerLink {
    transport: Arc<QuinnTransport>,
    label: String,
    /// Global rooms that this peer has reported as active.
    active_rooms: HashSet<String>,
    /// Remote participants per room (for federated presence in RoomUpdate).
    remote_participants: HashMap<String, Vec<wzp_proto::packet::RoomParticipant>>,
}

/// Max federation packets per second per room (0 = unlimited).
const FEDERATION_RATE_LIMIT_PPS: u32 = 500;
/// Dedup window size (number of recent packets to remember).
const DEDUP_WINDOW_SIZE: usize = 4096;

/// Manages federation connections and global room forwarding.
pub struct FederationManager {
    peers: Vec<PeerConfig>,
    trusted: Vec<TrustedConfig>,
    global_rooms: HashSet<String>,
    room_mgr: Arc<Mutex<RoomManager>>,
    endpoint: quinn::Endpoint,
    local_tls_fp: String,
    metrics: Arc<crate::metrics::RelayMetrics>,
    /// Active peer connections, keyed by normalized fingerprint.
    peer_links: Arc<Mutex<HashMap<String, PeerLink>>>,
    /// Dedup filter for incoming federation datagrams.
    dedup: Mutex<Deduplicator>,
    /// Per-room rate limiters for inbound federation media.
    rate_limiters: Mutex<HashMap<String, RateLimiter>>,
}

impl FederationManager {
    pub fn new(
        peers: Vec<PeerConfig>,
        trusted: Vec<TrustedConfig>,
        global_rooms: HashSet<String>,
        room_mgr: Arc<Mutex<RoomManager>>,
        endpoint: quinn::Endpoint,
        local_tls_fp: String,
        metrics: Arc<crate::metrics::RelayMetrics>,
    ) -> Self {
        Self {
            peers,
            trusted,
            global_rooms,
            room_mgr,
            endpoint,
            local_tls_fp,
            metrics,
            peer_links: Arc::new(Mutex::new(HashMap::new())),
            dedup: Mutex::new(Deduplicator::new(DEDUP_WINDOW_SIZE)),
            rate_limiters: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a room name (which may be hashed) is a global room.
    pub fn is_global_room(&self, room: &str) -> bool {
        self.resolve_global_room(room).is_some()
    }

    /// Resolve a room name (raw or hashed) to the canonical global room name.
    /// Returns the configured global room name if it matches.
    pub fn resolve_global_room(&self, room: &str) -> Option<&str> {
        // Direct match (raw room name, e.g. Android clients)
        if self.global_rooms.contains(room) {
            return Some(self.global_rooms.iter().find(|n| n.as_str() == room).unwrap());
        }
        // Hashed match (desktop clients hash room names for SNI privacy)
        self.global_rooms.iter().find(|name| {
            wzp_crypto::hash_room_name(name) == room
        }).map(|s| s.as_str())
    }

    /// Get the canonical federation room hash for a room.
    /// Always uses the configured global room name, not the client-provided name.
    pub fn global_room_hash(&self, room: &str) -> [u8; 8] {
        if let Some(canonical) = self.resolve_global_room(room) {
            room_hash(canonical)
        } else {
            room_hash(room)
        }
    }

    /// Start federation — spawns connection loops + event dispatcher.
    pub async fn run(self: Arc<Self>) {
        if self.peers.is_empty() && self.global_rooms.is_empty() {
            return;
        }
        info!(
            peers = self.peers.len(),
            global_rooms = self.global_rooms.len(),
            "federation starting"
        );

        let mut handles = Vec::new();

        // Per-peer outbound connection loops
        for peer in &self.peers {
            let this = self.clone();
            let peer = peer.clone();
            handles.push(tokio::spawn(async move {
                run_peer_loop(this, peer).await;
            }));
        }

        // Room event dispatcher
        let room_events = {
            let mgr = self.room_mgr.lock().await;
            mgr.subscribe_events()
        };
        let this = self.clone();
        handles.push(tokio::spawn(async move {
            run_room_event_dispatcher(this, room_events).await;
        }));

        for h in handles {
            let _ = h.await;
        }
    }

    /// Handle an inbound federation connection from a recognized peer.
    pub async fn handle_inbound(
        self: &Arc<Self>,
        transport: Arc<QuinnTransport>,
        peer_config: PeerConfig,
    ) {
        let peer_fp = normalize_fp(&peer_config.fingerprint);
        let label = peer_config.label.unwrap_or_else(|| peer_config.url.clone());
        info!(peer = %label, "inbound federation link active");
        if let Err(e) = run_federation_link(self.clone(), transport, peer_fp, label.clone()).await {
            warn!(peer = %label, "inbound federation link ended: {e}");
        }
    }

    /// Get all remote participants for a room from all peer links.
    pub async fn get_remote_participants(&self, room: &str) -> Vec<wzp_proto::packet::RoomParticipant> {
        let canonical = self.resolve_global_room(room);
        let links = self.peer_links.lock().await;
        let mut result = Vec::new();
        for link in links.values() {
            // Check canonical name
            if let Some(c) = canonical {
                if let Some(remote) = link.remote_participants.get(c) {
                    result.extend(remote.iter().cloned());
                }
            }
            // Also check raw room name
            if let Some(remote) = link.remote_participants.get(room) {
                result.extend(remote.iter().cloned());
            }
        }
        result
    }

    /// Forward locally-generated media to all connected peers.
    /// For locally-originated media, we send to ALL peers (they decide whether to deliver).
    /// For forwarded media (multi-hop), handle_datagram filters by active_rooms.
    pub async fn forward_to_peers(&self, room_name: &str, room_hash: &[u8; 8], media_data: &Bytes) {
        let links = self.peer_links.lock().await;
        if links.is_empty() {
            return;
        }
        for (_fp, link) in links.iter() {
            let mut tagged = Vec::with_capacity(8 + media_data.len());
            tagged.extend_from_slice(room_hash);
            tagged.extend_from_slice(media_data);
            match link.transport.send_raw_datagram(&tagged) {
                Ok(()) => {
                    self.metrics.federation_packets_forwarded
                        .with_label_values(&[&link.label, "out"]).inc();
                }
                Err(e) => warn!(peer = %link.label, "federation send error: {e}"),
            }
        }
    }

    // ── Trust verification (kept from previous implementation) ──

    pub fn find_peer_by_fingerprint(&self, fp: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| normalize_fp(&p.fingerprint) == normalize_fp(fp))
    }

    pub fn find_peer_by_addr(&self, addr: SocketAddr) -> Option<&PeerConfig> {
        let addr_ip = addr.ip();
        self.peers.iter().find(|p| {
            p.url.parse::<SocketAddr>()
                .map(|sa| sa.ip() == addr_ip)
                .unwrap_or(false)
        })
    }

    pub fn find_trusted_by_fingerprint(&self, fp: &str) -> Option<&TrustedConfig> {
        self.trusted.iter().find(|t| normalize_fp(&t.fingerprint) == normalize_fp(fp))
    }

    pub fn check_inbound_trust(&self, addr: SocketAddr, hello_fp: &str) -> Option<String> {
        if let Some(peer) = self.find_peer_by_addr(addr) {
            return Some(peer.label.clone().unwrap_or_else(|| peer.url.clone()));
        }
        if let Some(trusted) = self.find_trusted_by_fingerprint(hello_fp) {
            return Some(trusted.label.clone().unwrap_or_else(|| hello_fp[..16].to_string()));
        }
        None
    }
}

// ── Outbound media egress task ──

/// Drains the federation media channel and forwards to active peers.
pub async fn run_federation_media_egress(
    fm: Arc<FederationManager>,
    mut rx: tokio::sync::mpsc::Receiver<FederationMediaOut>,
) {
    let mut count: u64 = 0;
    while let Some(out) = rx.recv().await {
        count += 1;
        if count == 1 || count % 250 == 0 {
            info!(room = %out.room_name, count, "federation egress: forwarding media");
        }
        fm.forward_to_peers(&out.room_name, &out.room_hash, &out.data).await;
    }
    info!(total = count, "federation egress task ended");
}

// ── Room event dispatcher ──

/// Watches RoomManager events and sends GlobalRoomActive/Inactive to peers.
async fn run_room_event_dispatcher(
    fm: Arc<FederationManager>,
    mut events: tokio::sync::broadcast::Receiver<RoomEvent>,
) {
    loop {
        match events.recv().await {
            Ok(RoomEvent::LocalJoin { room }) => {
                if fm.is_global_room(&room) {
                    let participants = {
                        let mgr = fm.room_mgr.lock().await;
                        mgr.local_participant_list(&room)
                    };
                    info!(room = %room, count = participants.len(), "global room now active, announcing to peers");
                    let msg = SignalMessage::GlobalRoomActive { room, participants };
                    let links = fm.peer_links.lock().await;
                    for link in links.values() {
                        let _ = link.transport.send_signal(&msg).await;
                    }
                }
            }
            Ok(RoomEvent::LocalLeave { room }) => {
                if fm.is_global_room(&room) {
                    info!(room = %room, "global room now inactive, announcing to peers");
                    let msg = SignalMessage::GlobalRoomInactive { room };
                    let links = fm.peer_links.lock().await;
                    for link in links.values() {
                        let _ = link.transport.send_signal(&msg).await;
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(missed = n, "room event receiver lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Peer connection management ──

/// Persistent connection loop for one peer — reconnects with backoff.
async fn run_peer_loop(fm: Arc<FederationManager>, peer: PeerConfig) {
    let mut backoff = Duration::from_secs(5);
    loop {
        info!(peer_url = %peer.url, label = ?peer.label, "federation: connecting to peer...");
        match connect_to_peer(&fm, &peer).await {
            Ok(transport) => {
                backoff = Duration::from_secs(5);
                let peer_fp = normalize_fp(&peer.fingerprint);
                let label = peer.label.clone().unwrap_or_else(|| peer.url.clone());
                if let Err(e) = run_federation_link(fm.clone(), transport, peer_fp, label).await {
                    warn!(peer_url = %peer.url, "federation link ended: {e}");
                }
            }
            Err(e) => {
                warn!(peer_url = %peer.url, backoff_s = backoff.as_secs(), "federation connect failed: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

/// Connect to a peer relay and send hello.
async fn connect_to_peer(fm: &FederationManager, peer: &PeerConfig) -> Result<Arc<QuinnTransport>, anyhow::Error> {
    let addr: SocketAddr = peer.url.parse()?;
    let client_cfg = wzp_transport::client_config();
    let conn = wzp_transport::connect(&fm.endpoint, addr, "_federation", client_cfg).await?;
    let transport = Arc::new(QuinnTransport::new(conn));

    // Send hello with our TLS fingerprint
    let hello = SignalMessage::FederationHello {
        tls_fingerprint: fm.local_tls_fp.clone(),
    };
    transport.send_signal(&hello).await
        .map_err(|e| anyhow::anyhow!("federation hello send failed: {e}"))?;

    info!(peer_url = %peer.url, label = ?peer.label, "federation: connected (hello sent)");
    Ok(transport)
}

// ── Federation link (runs on a single QUIC connection) ──

/// Run the federation link: exchange global room state and forward media.
async fn run_federation_link(
    fm: Arc<FederationManager>,
    transport: Arc<QuinnTransport>,
    peer_fp: String,
    peer_label: String,
) -> Result<(), anyhow::Error> {
    // Register peer link + metrics
    fm.metrics.federation_peer_status.with_label_values(&[&peer_label]).set(1);
    {
        let mut links = fm.peer_links.lock().await;
        links.insert(peer_fp.clone(), PeerLink {
            transport: transport.clone(),
            label: peer_label.clone(),
            active_rooms: HashSet::new(),
            remote_participants: HashMap::new(),
        });
    }

    // Announce our currently active global rooms
    {
        let mgr = fm.room_mgr.lock().await;
        for room_name in mgr.active_rooms() {
            if fm.is_global_room(&room_name) {
                let participants = mgr.local_participant_list(&room_name);
                let msg = SignalMessage::GlobalRoomActive { room: room_name, participants };
                let _ = transport.send_signal(&msg).await;
            }
        }
    }

    // Three concurrent tasks: signal recv + media recv + RTT monitor
    let signal_transport = transport.clone();
    let media_transport = transport.clone();
    let rtt_transport = transport.clone();
    let fm_signal = fm.clone();
    let fm_media = fm.clone();
    let fm_rtt = fm.clone();
    let peer_fp_signal = peer_fp.clone();
    let peer_fp_media = peer_fp.clone();
    let label_signal = peer_label.clone();
    let label_rtt = peer_label.clone();

    let signal_task = async move {
        loop {
            match signal_transport.recv_signal().await {
                Ok(Some(msg)) => {
                    handle_signal(&fm_signal, &peer_fp_signal, &label_signal, msg).await;
                }
                Ok(None) => break,
                Err(e) => {
                    error!(peer = %label_signal, "federation signal error: {e}");
                    break;
                }
            }
        }
    };

    let peer_label_media = peer_label.clone();
    let media_task = async move {
        let mut media_count: u64 = 0;
        loop {
            match media_transport.connection().read_datagram().await {
                Ok(data) => {
                    media_count += 1;
                    if media_count == 1 || media_count % 250 == 0 {
                        info!(peer = %peer_label_media, media_count, len = data.len(), "federation: received datagram");
                    }
                    handle_datagram(&fm_media, &peer_fp_media, data).await;
                }
                Err(e) => {
                    info!(peer = %peer_label_media, "federation media task ended: {e}");
                    break;
                }
            }
        }
    };

    // RTT monitor: periodically sample QUIC RTT for this peer
    let rtt_task = async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let rtt_ms = rtt_transport.connection().stats().path.rtt.as_millis() as f64;
        }
    };

    tokio::select! {
        _ = signal_task => {}
        _ = media_task => {}
        _ = rtt_task => {}
    }

    // Cleanup: remove peer link + metrics
    fm.metrics.federation_peer_status.with_label_values(&[&peer_label]).set(0);
    {
        let mut links = fm.peer_links.lock().await;
        links.remove(&peer_fp);
    }
    info!(peer = %peer_label, "federation link ended");

    Ok(())
}

/// Handle an incoming federation signal.
async fn handle_signal(
    fm: &Arc<FederationManager>,
    peer_fp: &str,
    peer_label: &str,
    msg: SignalMessage,
) {
    match msg {
        SignalMessage::GlobalRoomActive { room, participants } => {
            if fm.is_global_room(&room) {
                info!(peer = %peer_label, room = %room, remote_participants = participants.len(), "peer has global room active");
                let mut links = fm.peer_links.lock().await;
                if let Some(link) = links.get_mut(peer_fp) {
                    link.active_rooms.insert(room.clone());
                }
                // Update active rooms metric
                let total: usize = links.values().map(|l| l.active_rooms.len()).sum();
                fm.metrics.federation_active_rooms.set(total as i64);
                if let Some(link) = links.get_mut(peer_fp) {
                    link.remote_participants.insert(room.clone(), participants.clone());
                }
                // Propagate to other peers
                for (fp, link) in links.iter() {
                    if fp != peer_fp {
                        let _ = link.transport.send_signal(&SignalMessage::GlobalRoomActive {
                            room: room.clone(),
                            participants: participants.clone(),
                        }).await;
                    }
                }
                drop(links);

                // Broadcast updated RoomUpdate to local clients in this room
                // Find the local room name (may be hashed or raw)
                let mgr = fm.room_mgr.lock().await;
                for local_room in mgr.active_rooms() {
                    if fm.is_global_room(&local_room) && fm.resolve_global_room(&local_room) == fm.resolve_global_room(&room) {
                        // Build merged participant list: local + all remote
                        let mut all_participants = mgr.local_participant_list(&local_room);
                        let links = fm.peer_links.lock().await;
                        for link in links.values() {
                            if let Some(canonical) = fm.resolve_global_room(&local_room) {
                                if let Some(remote) = link.remote_participants.get(canonical) {
                                    all_participants.extend(remote.iter().cloned());
                                }
                                // Also check raw room name
                                if let Some(remote) = link.remote_participants.get(&local_room) {
                                    all_participants.extend(remote.iter().cloned());
                                }
                            }
                        }
                        let update = SignalMessage::RoomUpdate {
                            count: all_participants.len() as u32,
                            participants: all_participants,
                        };
                        let senders = mgr.local_senders(&local_room);
                        drop(links);
                        drop(mgr);
                        room::broadcast_signal(&senders, &update).await;
                        break;
                    }
                }
            }
        }
        SignalMessage::GlobalRoomInactive { room } => {
            info!(peer = %peer_label, room = %room, "peer global room now inactive");
            let mut links = fm.peer_links.lock().await;
            if let Some(link) = links.get_mut(peer_fp) {
                link.active_rooms.remove(&room);
            }
            // Update active rooms gauge
            let total: usize = links.values().map(|l| l.active_rooms.len()).sum();
            // Check if any other peer still has this room — if none, propagate inactive
            let any_other_active = links.iter()
                .any(|(fp, l)| fp != peer_fp && l.active_rooms.contains(&room));
            let local_active = {
                let mgr = fm.room_mgr.lock().await;
                mgr.active_rooms().iter().any(|r| r == &room)
            };
            if !any_other_active && !local_active {
                for (fp, link) in links.iter() {
                    if fp != peer_fp {
                        let _ = link.transport.send_signal(&SignalMessage::GlobalRoomInactive { room: room.clone() }).await;
                    }
                }
            }
        }
        _ => {} // ignore other signals
    }
}

/// Handle an incoming federation datagram (room-hash-tagged media).
async fn handle_datagram(
    fm: &Arc<FederationManager>,
    source_peer_fp: &str,
    data: Bytes,
) {
    if data.len() < 12 { return; } // 8-byte hash + min packet

    let mut rh = [0u8; 8];
    rh.copy_from_slice(&data[..8]);
    let media_bytes = data.slice(8..);

    let pkt = match wzp_proto::MediaPacket::from_bytes(media_bytes.clone()) {
        Some(pkt) => pkt,
        None => return,
    };

    // Count inbound federation packet
    fm.metrics.federation_packets_forwarded
        .with_label_values(&[source_peer_fp, "in"]).inc();

    // Dedup: drop packets we've already seen (multi-path duplicates)
    {
        let mut dedup = fm.dedup.lock().await;
        if dedup.is_dup(&rh, pkt.header.seq) {
            return;
        }
    }

    // Find room by hash
    let room_name = {
        let mgr = fm.room_mgr.lock().await;
        {
        let active = mgr.active_rooms();
        active.iter().find(|r| room_hash(r) == rh).cloned()
            .or_else(|| active.iter().find(|r| fm.global_room_hash(r) == rh).cloned())
    }
    };

    let room_name = match room_name {
        Some(r) => r,
        None => return, // room not active locally
    };

    // Rate limit per room
    if FEDERATION_RATE_LIMIT_PPS > 0 {
        let mut limiters = fm.rate_limiters.lock().await;
        let limiter = limiters.entry(room_name.clone())
            .or_insert_with(|| RateLimiter::new(FEDERATION_RATE_LIMIT_PPS));
        if !limiter.allow() {
            return;
        }
    }

    // Deliver to all local participants
    let locals = {
        let mgr = fm.room_mgr.lock().await;
        mgr.local_senders(&room_name)
    };
    for sender in &locals {
        match sender {
            room::ParticipantSender::Quic(t) => { let _ = t.send_media(&pkt).await; }
            room::ParticipantSender::WebSocket(_) => { let _ = sender.send_raw(&pkt.payload).await; }
        }
    }

    // Multi-hop: forward to OTHER active peers (not the source)
    let links = fm.peer_links.lock().await;
    for (fp, link) in links.iter() {
        if fp != source_peer_fp && link.active_rooms.contains(&room_name) {
            let mut tagged = Vec::with_capacity(8 + media_bytes.len());
            tagged.extend_from_slice(&rh);
            tagged.extend_from_slice(&media_bytes);
            if let Err(e) = link.transport.send_raw_datagram(&tagged) {
                warn!(peer = %link.label, "multi-hop forward error: {e}");
            }
        }
    }
}
