//! Room management for multi-party calls.
//!
//! Each room holds N participants. When one participant sends a media packet,
//! the relay forwards it to all other participants in the room (SFU model).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{error, info, warn};

use wzp_proto::MediaTransport;

use crate::metrics::RelayMetrics;

/// Unique participant ID within a room.
pub type ParticipantId = u64;

static NEXT_PARTICIPANT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> ParticipantId {
    NEXT_PARTICIPANT_ID.fetch_add(1, Ordering::Relaxed)
}

/// A participant in a room.
struct Participant {
    id: ParticipantId,
    addr: std::net::SocketAddr,
    transport: Arc<wzp_transport::QuinnTransport>,
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

    fn add(&mut self, addr: std::net::SocketAddr, transport: Arc<wzp_transport::QuinnTransport>) -> ParticipantId {
        let id = next_id();
        info!(room_size = self.participants.len() + 1, participant = id, %addr, "joined room");
        self.participants.push(Participant { id, addr, transport });
        id
    }

    fn remove(&mut self, id: ParticipantId) {
        self.participants.retain(|p| p.id != id);
        info!(room_size = self.participants.len(), participant = id, "left room");
    }

    fn others(&self, exclude_id: ParticipantId) -> Vec<Arc<wzp_transport::QuinnTransport>> {
        self.participants
            .iter()
            .filter(|p| p.id != exclude_id)
            .map(|p| p.transport.clone())
            .collect()
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
}

impl RoomManager {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
            acl: None,
        }
    }

    /// Create a room manager with ACL enforcement enabled.
    pub fn with_acl() -> Self {
        Self {
            rooms: HashMap::new(),
            acl: Some(HashMap::new()),
        }
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

    /// Join a room. Returns the participant ID or an error if unauthorized.
    pub fn join(
        &mut self,
        room_name: &str,
        addr: std::net::SocketAddr,
        transport: Arc<wzp_transport::QuinnTransport>,
        fingerprint: Option<&str>,
    ) -> Result<ParticipantId, String> {
        if !self.is_authorized(room_name, fingerprint) {
            warn!(room = room_name, fingerprint = ?fingerprint, "unauthorized room join attempt");
            return Err("not authorized for this room".to_string());
        }
        let room = self.rooms.entry(room_name.to_string()).or_insert_with(Room::new);
        Ok(room.add(addr, transport))
    }

    /// Leave a room. Removes the room if empty.
    pub fn leave(&mut self, room_name: &str, participant_id: ParticipantId) {
        if let Some(room) = self.rooms.get_mut(room_name) {
            room.remove(participant_id);
            if room.is_empty() {
                self.rooms.remove(room_name);
                info!(room = room_name, "room closed (empty)");
            }
        }
    }

    /// Get transports for all OTHER participants in a room.
    pub fn others(
        &self,
        room_name: &str,
        participant_id: ParticipantId,
    ) -> Vec<Arc<wzp_transport::QuinnTransport>> {
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

/// Run the receive loop for one participant in a room.
/// Forwards all received packets to every other participant.
pub async fn run_participant(
    room_mgr: Arc<Mutex<RoomManager>>,
    room_name: String,
    participant_id: ParticipantId,
    transport: Arc<wzp_transport::QuinnTransport>,
    metrics: Arc<RelayMetrics>,
    session_id: &str,
) {
    let addr = transport.connection().remote_address();
    let mut packets_forwarded = 0u64;

    loop {
        let pkt = match transport.recv_media().await {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                info!(%addr, participant = participant_id, "disconnected");
                break;
            }
            Err(e) => {
                error!(%addr, participant = participant_id, "recv error: {e}");
                break;
            }
        };

        // Update per-session quality metrics if a quality report is present
        if let Some(ref report) = pkt.quality_report {
            metrics.update_session_quality(session_id, report);
        }

        // Get current list of other participants
        let others = {
            let mgr = room_mgr.lock().await;
            mgr.others(&room_name, participant_id)
        };

        // Forward to all others
        let pkt_bytes = pkt.payload.len() as u64;
        for other in &others {
            // Best-effort: if one send fails, continue to others
            if let Err(e) = other.send_media(&pkt).await {
                // Don't log every failure — they'll be cleaned up when their recv loop breaks
                let _ = e;
            }
        }

        let fan_out = others.len() as u64;
        metrics.packets_forwarded.inc_by(fan_out);
        metrics.bytes_forwarded.inc_by(pkt_bytes * fan_out);
        packets_forwarded += 1;
        if packets_forwarded % 500 == 0 {
            let room_size = {
                let mgr = room_mgr.lock().await;
                mgr.room_size(&room_name)
            };
            info!(
                room = %room_name,
                participant = participant_id,
                forwarded = packets_forwarded,
                room_size,
                "participant stats"
            );
        }
    }

    // Clean up
    let mut mgr = room_mgr.lock().await;
    mgr.leave(&room_name, participant_id);
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
}
