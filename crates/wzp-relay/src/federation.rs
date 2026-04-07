//! Relay federation — connects to peer relays and bridges rooms with matching names.
//!
//! Each federated peer is represented as a virtual participant in shared rooms.
//! Media from local participants is forwarded to the peer via room-tagged datagrams.
//! Media from the peer is received, demuxed by room hash, and forwarded to local participants.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use sha2::{Sha256, Digest};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use wzp_proto::{MediaTransport, SignalMessage};
use wzp_transport::QuinnTransport;

use crate::config::PeerConfig;
use crate::room::{self, ParticipantSender, RoomManager};

/// Compute 8-byte room hash for federation datagram tagging.
pub fn room_hash(room_name: &str) -> [u8; 8] {
    let h = Sha256::digest(room_name.as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&h[..8]);
    out
}

/// Manages federation connections to peer relays.
pub struct FederationManager {
    peers: Vec<PeerConfig>,
    room_mgr: Arc<Mutex<RoomManager>>,
    endpoint: quinn::Endpoint,
    local_tls_fp: String,
}

impl FederationManager {
    pub fn new(
        peers: Vec<PeerConfig>,
        room_mgr: Arc<Mutex<RoomManager>>,
        endpoint: quinn::Endpoint,
        local_tls_fp: String,
    ) -> Self {
        Self {
            peers,
            room_mgr,
            endpoint,
            local_tls_fp,
        }
    }

    /// Start federation — spawns one task per configured peer.
    pub async fn run(self: Arc<Self>) {
        if self.peers.is_empty() {
            return;
        }
        info!(peers = self.peers.len(), "federation starting");
        let mut handles = Vec::new();
        for peer in &self.peers {
            let this = self.clone();
            let peer = peer.clone();
            handles.push(tokio::spawn(async move {
                run_peer_loop(this, peer).await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
    }

    /// Handle an inbound federation connection from a peer that we recognize.
    pub async fn handle_inbound(
        self: &Arc<Self>,
        transport: Arc<QuinnTransport>,
        peer_config: PeerConfig,
    ) {
        let addr: SocketAddr = peer_config.url.parse().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
        info!(peer = ?peer_config.label, %addr, "inbound federation link active");
        if let Err(e) = run_federation_link(self.clone(), transport, addr, &peer_config).await {
            warn!(peer = ?peer_config.label, "inbound federation link ended: {e}");
        }
    }

    /// Find a configured peer by TLS fingerprint.
    pub fn find_peer_by_fingerprint(&self, fp: &str) -> Option<&PeerConfig> {
        self.peers.iter().find(|p| normalize_fp(&p.fingerprint) == normalize_fp(fp))
    }
}

/// Normalize a fingerprint string (remove colons, lowercase).
fn normalize_fp(fp: &str) -> String {
    fp.replace(':', "").to_lowercase()
}

/// Persistent connection loop for one peer — reconnects with backoff.
async fn run_peer_loop(fm: Arc<FederationManager>, peer: PeerConfig) {
    let mut backoff = Duration::from_secs(5);
    loop {
        info!(peer_url = %peer.url, label = ?peer.label, "federation: connecting to peer...");
        match connect_to_peer(&fm, &peer).await {
            Ok(transport) => {
                backoff = Duration::from_secs(5); // reset on success
                let addr: SocketAddr = peer.url.parse().unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap());
                if let Err(e) = run_federation_link(fm.clone(), transport, addr, &peer).await {
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

/// Connect to a peer relay.
async fn connect_to_peer(fm: &FederationManager, peer: &PeerConfig) -> Result<Arc<QuinnTransport>, anyhow::Error> {
    let addr: SocketAddr = peer.url.parse()?;
    let client_cfg = wzp_transport::client_config();
    let conn = wzp_transport::connect(&fm.endpoint, addr, "_federation", client_cfg).await?;
    // TODO: verify peer TLS fingerprint once we have cert access
    let transport = Arc::new(QuinnTransport::new(conn));
    info!(peer_url = %peer.url, label = ?peer.label, "federation: connected to peer");
    Ok(transport)
}

/// Run the federation link: exchange room info and forward media.
async fn run_federation_link(
    fm: Arc<FederationManager>,
    transport: Arc<QuinnTransport>,
    peer_addr: SocketAddr,
    peer: &PeerConfig,
) -> Result<(), anyhow::Error> {
    // Announce our active rooms to the peer
    let rooms = {
        let mgr = fm.room_mgr.lock().await;
        mgr.active_rooms()
    };
    for room_name in &rooms {
        let participants = {
            let mgr = fm.room_mgr.lock().await;
            mgr.local_participants(room_name)
        };
        let msg = SignalMessage::FederationRoomJoin {
            room: room_name.clone(),
            participants,
        };
        transport.send_signal(&msg).await?;
    }

    // Track virtual participants we create on behalf of this peer
    let mut peer_room_participants: HashMap<String, room::ParticipantId> = HashMap::new();
    // Map room_hash -> room_name for incoming media demux
    let mut hash_to_room: HashMap<[u8; 8], String> = HashMap::new();

    // Run two tasks: recv signals + recv media datagrams
    let signal_transport = transport.clone();
    let media_transport = transport.clone();
    let fm_signal = fm.clone();
    let fm_media = fm.clone();
    let peer_label = peer.label.clone().unwrap_or_else(|| peer.url.clone());

    let signal_task = async move {
        loop {
            match signal_transport.recv_signal().await {
                Ok(Some(msg)) => {
                    match msg {
                        SignalMessage::FederationRoomJoin { room, participants } => {
                            info!(peer = %peer_label, room = %room, count = participants.len(), "federation: peer room join");
                            let rh = room_hash(&room);
                            hash_to_room.insert(rh, room.clone());

                            let sender = ParticipantSender::Federation {
                                transport: signal_transport.clone(),
                                room_hash: rh,
                            };
                            let (pid, update, senders) = {
                                let mut mgr = fm_signal.room_mgr.lock().await;
                                mgr.join_federated(&room, peer_addr, sender, participants)
                            };
                            peer_room_participants.insert(room, pid);
                            room::broadcast_signal(&senders, &update).await;
                        }
                        SignalMessage::FederationRoomLeave { room } => {
                            info!(peer = %peer_label, room = %room, "federation: peer room leave");
                            if let Some(pid) = peer_room_participants.remove(&room) {
                                let result = {
                                    let mut mgr = fm_signal.room_mgr.lock().await;
                                    mgr.leave(&room, pid)
                                };
                                if let Some((update, senders)) = result {
                                    room::broadcast_signal(&senders, &update).await;
                                }
                            }
                            hash_to_room.retain(|_, v| v != &room);
                        }
                        SignalMessage::FederationParticipantUpdate { room, participants } => {
                            let result = {
                                let mut mgr = fm_signal.room_mgr.lock().await;
                                mgr.update_federated_participants(&room, peer_addr, participants)
                            };
                            if let Some((update, senders)) = result {
                                room::broadcast_signal(&senders, &update).await;
                            }
                        }
                        _ => {} // ignore other signals
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!(peer = %peer_label, "federation signal recv error: {e}");
                    break;
                }
            }
        }
        // Cleanup: remove all virtual participants for this peer
        for (room, pid) in &peer_room_participants {
            let result = {
                let mut mgr = fm_signal.room_mgr.lock().await;
                mgr.leave(room, *pid)
            };
            if let Some((update, senders)) = result {
                room::broadcast_signal(&senders, &update).await;
            }
        }
        info!(peer = %peer_label, "federation signal task ended");
    };

    let media_task = async move {
        loop {
            match media_transport.connection().read_datagram().await {
                Ok(data) => {
                    if data.len() < 8 + 4 {
                        continue; // too short (need room_hash + min header)
                    }
                    let mut rh = [0u8; 8];
                    rh.copy_from_slice(&data[..8]);
                    let media_bytes = &data[8..];

                    // Deserialize media packet
                    let pkt = match wzp_proto::MediaPacket::from_bytes(Bytes::copy_from_slice(media_bytes)) {
                        Some(pkt) => pkt,
                        None => continue,
                    };

                    // Look up room by hash — we need to get the room name from the signal task's hash_to_room
                    // For simplicity, we forward to all local participants via the room manager
                    // The virtual participant approach means we don't need the room name here —
                    // the SFU loop handles it. But since inbound media doesn't go through run_participant,
                    // we need to manually fan out.

                    // For now, just use the room manager to find local participants
                    // This is a simplified approach — full implementation would maintain
                    // a shared hash_to_room map between signal and media tasks
                    let mgr = fm_media.room_mgr.lock().await;
                    for room_name in mgr.active_rooms() {
                        if room_hash(&room_name) == rh {
                            // Forward to all local participants in this room
                            let locals: Vec<_> = mgr.local_senders(&room_name);
                            drop(mgr); // release lock before sending
                            for sender in &locals {
                                if let ParticipantSender::Quic(t) = sender {
                                    let _ = t.send_media(&pkt).await;
                                }
                            }
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    };

    tokio::select! {
        _ = signal_task => {}
        _ = media_task => {}
    }

    Ok(())
}
