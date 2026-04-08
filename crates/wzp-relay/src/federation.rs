//! Relay federation — global room routing between peer relays.
//!
//! Each relay maintains a forwarding table per global room. When a local participant
//! sends media in a global room, it's forwarded to all peer relays that have the room
//! active. Incoming federated media is delivered to local participants and optionally
//! forwarded to other active peers (multi-hop).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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

/// Active link to a peer relay.
struct PeerLink {
    transport: Arc<QuinnTransport>,
    label: String,
    /// Global rooms that this peer has reported as active.
    active_rooms: HashSet<String>,
}

/// Manages federation connections and global room forwarding.
pub struct FederationManager {
    peers: Vec<PeerConfig>,
    trusted: Vec<TrustedConfig>,
    global_rooms: HashSet<String>,
    room_mgr: Arc<Mutex<RoomManager>>,
    endpoint: quinn::Endpoint,
    local_tls_fp: String,
    /// Active peer connections, keyed by normalized fingerprint.
    peer_links: Arc<Mutex<HashMap<String, PeerLink>>>,
}

impl FederationManager {
    pub fn new(
        peers: Vec<PeerConfig>,
        trusted: Vec<TrustedConfig>,
        global_rooms: HashSet<String>,
        room_mgr: Arc<Mutex<RoomManager>>,
        endpoint: quinn::Endpoint,
        local_tls_fp: String,
    ) -> Self {
        Self {
            peers,
            trusted,
            global_rooms,
            room_mgr,
            endpoint,
            local_tls_fp,
            peer_links: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check if a room name (which may be hashed) is a global room.
    pub fn is_global_room(&self, room: &str) -> bool {
        // Check both the raw name and the hashed version
        if self.global_rooms.contains(room) {
            return true;
        }
        // The room name in the room manager is the hashed SNI.
        // Check if any configured global room hashes to this value.
        self.global_rooms.iter().any(|name| {
            wzp_crypto::hash_room_name(name) == room
        })
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

    /// Forward locally-generated media to active peers for a global room.
    pub async fn forward_to_peers(&self, room_name: &str, room_hash: &[u8; 8], media_data: &Bytes) {
        let links = self.peer_links.lock().await;
        if links.is_empty() {
            return;
        }
        let mut sent = 0u32;
        for (fp, link) in links.iter() {
            if link.active_rooms.contains(room_name) {
                let mut tagged = Vec::with_capacity(8 + media_data.len());
                tagged.extend_from_slice(room_hash);
                tagged.extend_from_slice(media_data);
                match link.transport.send_raw_datagram(&tagged) {
                    Ok(()) => sent += 1,
                    Err(e) => warn!(peer = %link.label, "federation send error: {e}"),
                }
            }
        }
        if sent == 0 && !links.is_empty() {
            // Debug: no peer had this room active
            let active_rooms: Vec<_> = links.values()
                .flat_map(|l| l.active_rooms.iter().cloned())
                .collect();
            warn!(room = %room_name, peer_count = links.len(), ?active_rooms, "no peer has this room active");
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
                    info!(room = %room, "global room now active, announcing to peers");
                    let msg = SignalMessage::GlobalRoomActive { room };
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
    // Register peer link
    {
        let mut links = fm.peer_links.lock().await;
        links.insert(peer_fp.clone(), PeerLink {
            transport: transport.clone(),
            label: peer_label.clone(),
            active_rooms: HashSet::new(),
        });
    }

    // Announce our currently active global rooms
    {
        let mgr = fm.room_mgr.lock().await;
        for room_name in mgr.active_rooms() {
            if fm.is_global_room(&room_name) {
                let msg = SignalMessage::GlobalRoomActive { room: room_name };
                let _ = transport.send_signal(&msg).await;
            }
        }
    }

    // Two concurrent tasks: signal recv + media recv
    let signal_transport = transport.clone();
    let media_transport = transport.clone();
    let fm_signal = fm.clone();
    let fm_media = fm.clone();
    let peer_fp_signal = peer_fp.clone();
    let peer_fp_media = peer_fp.clone();
    let label_signal = peer_label.clone();

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

    let media_task = async move {
        loop {
            match media_transport.connection().read_datagram().await {
                Ok(data) => {
                    handle_datagram(&fm_media, &peer_fp_media, data).await;
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = signal_task => {}
        _ = media_task => {}
    }

    // Cleanup: remove peer link
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
        SignalMessage::GlobalRoomActive { room } => {
            if fm.is_global_room(&room) {
                info!(peer = %peer_label, room = %room, "peer has global room active");
                let mut links = fm.peer_links.lock().await;
                if let Some(link) = links.get_mut(peer_fp) {
                    link.active_rooms.insert(room.clone());
                }
                // Propagate: tell all OTHER peers this room is routable through us.
                // This enables multi-hop: A→B→C where B relays A's announcement to C and vice versa.
                for (fp, link) in links.iter() {
                    if fp != peer_fp {
                        let _ = link.transport.send_signal(&SignalMessage::GlobalRoomActive { room: room.clone() }).await;
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

    // Find room by hash
    let room_name = {
        let mgr = fm.room_mgr.lock().await;
        mgr.active_rooms().into_iter().find(|r| room_hash(r) == rh)
    };

    let room_name = match room_name {
        Some(r) => r,
        None => return, // room not active locally
    };

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
            let _ = link.transport.send_raw_datagram(&tagged);
        }
    }
}
