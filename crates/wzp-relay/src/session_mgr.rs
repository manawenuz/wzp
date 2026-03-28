//! Session manager — tracks active call sessions on the relay.

use std::collections::HashMap;
use std::time::Instant;

use wzp_proto::{QualityProfile, Session};

use crate::pipeline::{PipelineConfig, RelayPipeline};

/// Unique identifier for a relay session.
pub type SessionId = [u8; 16];

/// Lifecycle state of a concurrent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Closing,
}

/// Lightweight metadata for a concurrent session (room-mode tracking).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Which room this session belongs to.
    pub room_name: String,
    /// Client fingerprint (present when auth is enabled).
    pub fingerprint: Option<String>,
    /// When the session was created.
    pub connected_at: Instant,
    /// Current lifecycle state.
    pub state: SessionState,
}

/// A single active call session on the relay.
pub struct RelaySession {
    /// Protocol session state machine.
    pub state: Session,
    /// Pipeline for upstream → downstream direction.
    pub upstream_pipeline: RelayPipeline,
    /// Pipeline for downstream → upstream direction.
    pub downstream_pipeline: RelayPipeline,
    /// Quality profile currently in use.
    pub profile: QualityProfile,
    /// Timestamp of last activity (ms since epoch).
    pub last_activity_ms: u64,
}

impl RelaySession {
    pub fn new(session_id: SessionId, config: PipelineConfig) -> Self {
        let profile = config.initial_profile;
        Self {
            state: Session::new(session_id),
            upstream_pipeline: RelayPipeline::new(PipelineConfig {
                initial_profile: profile,
                ..config
            }),
            downstream_pipeline: RelayPipeline::new(PipelineConfig {
                initial_profile: profile,
                ..config
            }),
            profile,
            last_activity_ms: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.is_media_active()
    }
}

/// Manages all active sessions on a relay.
///
/// Combines two layers of tracking:
/// - `sessions`: heavy `RelaySession` objects (pipeline state machines, used in forward mode)
/// - `tracked`: lightweight `SessionInfo` entries (room + fingerprint, used in room mode to
///   enforce `max_sessions` and answer lifecycle queries)
pub struct SessionManager {
    sessions: HashMap<SessionId, RelaySession>,
    tracked: HashMap<SessionId, SessionInfo>,
    max_sessions: usize,
}

impl SessionManager {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: HashMap::new(),
            tracked: HashMap::new(),
            max_sessions,
        }
    }

    // ── Heavy session API (forward-mode pipelines) ──────────────────────

    /// Create a new pipeline session. Returns None if at capacity.
    pub fn create_pipeline_session(
        &mut self,
        session_id: SessionId,
        config: PipelineConfig,
    ) -> Option<&mut RelaySession> {
        if self.total_count() >= self.max_sessions {
            return None;
        }
        self.sessions
            .entry(session_id)
            .or_insert_with(|| RelaySession::new(session_id, config));
        self.sessions.get_mut(&session_id)
    }

    /// Get a pipeline session by ID.
    pub fn get_session(&mut self, id: &SessionId) -> Option<&mut RelaySession> {
        self.sessions.get_mut(id)
    }

    /// Remove a pipeline session.
    pub fn remove_pipeline_session(&mut self, id: &SessionId) -> Option<RelaySession> {
        self.sessions.remove(id)
    }

    /// Number of active pipeline sessions.
    pub fn pipeline_active_count(&self) -> usize {
        self.sessions.values().filter(|s| s.is_active()).count()
    }

    /// Total pipeline sessions (including inactive/closing).
    pub fn pipeline_total_count(&self) -> usize {
        self.sessions.len()
    }

    /// Remove pipeline sessions idle for longer than `timeout_ms`.
    pub fn expire_idle(&mut self, now_ms: u64, timeout_ms: u64) -> usize {
        let before = self.sessions.len();
        self.sessions
            .retain(|_, s| now_ms.saturating_sub(s.last_activity_ms) < timeout_ms);
        before - self.sessions.len()
    }

    // ── Lightweight concurrent-session API (room mode) ──────────────────

    /// Register a new concurrent session.
    /// Returns the `SessionId` on success, or an error string if `max_sessions` is exceeded.
    pub fn create_session(
        &mut self,
        room: &str,
        fingerprint: Option<String>,
    ) -> Result<SessionId, String> {
        if self.total_count() >= self.max_sessions {
            return Err(format!(
                "max sessions ({}) exceeded",
                self.max_sessions
            ));
        }
        let id = rand_session_id();
        self.tracked.insert(id, SessionInfo {
            room_name: room.to_string(),
            fingerprint,
            connected_at: Instant::now(),
            state: SessionState::Active,
        });
        Ok(id)
    }

    /// Remove a tracked session.
    pub fn remove_session(&mut self, id: SessionId) {
        self.tracked.remove(&id);
    }

    /// Number of currently tracked (room-mode) sessions.
    pub fn active_count(&self) -> usize {
        self.tracked.values().filter(|s| s.state == SessionState::Active).count()
    }

    /// Return all session IDs that belong to a given room.
    pub fn sessions_in_room(&self, room: &str) -> Vec<SessionId> {
        self.tracked
            .iter()
            .filter(|(_, info)| info.room_name == room)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get metadata for a tracked session.
    pub fn session_info(&self, id: SessionId) -> Option<&SessionInfo> {
        self.tracked.get(&id)
    }

    /// Total sessions across both tracking layers.
    pub fn total_count(&self) -> usize {
        self.sessions.len() + self.tracked.len()
    }
}

/// Generate a random 16-byte session identifier.
fn rand_session_id() -> SessionId {
    let mut id = [0u8; 16];
    // Use a simple monotonic + random source to avoid pulling in `rand` crate.
    // Hash the instant + a counter for uniqueness.
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(1);
    let ctr = CTR.fetch_add(1, Ordering::Relaxed);
    let bytes = ctr.to_le_bytes();
    id[..8].copy_from_slice(&bytes);
    // Mix in some time-based entropy for the upper half.
    let t = Instant::now().elapsed().as_nanos() as u64;
    id[8..16].copy_from_slice(&t.to_le_bytes());
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Pipeline session tests (pre-existing, adapted to renamed API) ───

    #[test]
    fn create_and_get_pipeline_session() {
        let mut mgr = SessionManager::new(10);
        let id = [1u8; 16];
        mgr.create_pipeline_session(id, PipelineConfig::default());
        assert!(mgr.get_session(&id).is_some());
    }

    #[test]
    fn respects_max_pipeline_sessions() {
        let mut mgr = SessionManager::new(1);
        mgr.create_pipeline_session([1u8; 16], PipelineConfig::default());
        let result = mgr.create_pipeline_session([2u8; 16], PipelineConfig::default());
        assert!(result.is_none());
    }

    #[test]
    fn expire_idle_removes_old() {
        let mut mgr = SessionManager::new(10);
        let id = [1u8; 16];
        mgr.create_pipeline_session(id, PipelineConfig::default());
        let expired = mgr.expire_idle(60_000, 30_000);
        assert_eq!(expired, 1);
        assert_eq!(mgr.pipeline_total_count(), 0);
    }

    // ── Concurrent session (room-mode) tests ────────────────────────────

    #[test]
    fn create_and_remove() {
        let mut mgr = SessionManager::new(10);
        let id = mgr.create_session("room-a", Some("fp123".into())).unwrap();
        assert_eq!(mgr.active_count(), 1);
        mgr.remove_session(id);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn max_sessions_enforced() {
        let mut mgr = SessionManager::new(2);
        mgr.create_session("r1", None).unwrap();
        mgr.create_session("r2", None).unwrap();
        let err = mgr.create_session("r3", None);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("max sessions"));
    }

    #[test]
    fn sessions_in_room_tracking() {
        let mut mgr = SessionManager::new(10);
        let a1 = mgr.create_session("alpha", None).unwrap();
        let _a2 = mgr.create_session("alpha", None).unwrap();
        let _b1 = mgr.create_session("beta", None).unwrap();

        let alpha_ids = mgr.sessions_in_room("alpha");
        assert_eq!(alpha_ids.len(), 2);
        assert!(alpha_ids.contains(&a1));

        let beta_ids = mgr.sessions_in_room("beta");
        assert_eq!(beta_ids.len(), 1);

        let empty = mgr.sessions_in_room("gamma");
        assert!(empty.is_empty());
    }

    #[test]
    fn session_info_returns_correct_data() {
        let mut mgr = SessionManager::new(10);
        let id = mgr.create_session("room-x", Some("alice-fp".into())).unwrap();

        let info = mgr.session_info(id).expect("session should exist");
        assert_eq!(info.room_name, "room-x");
        assert_eq!(info.fingerprint.as_deref(), Some("alice-fp"));
        assert_eq!(info.state, SessionState::Active);

        // Non-existent session returns None
        assert!(mgr.session_info([0xFFu8; 16]).is_none());
    }

    #[test]
    fn max_sessions_shared_across_both_layers() {
        let mut mgr = SessionManager::new(2);
        // One pipeline session + one tracked session = 2 = at capacity
        mgr.create_pipeline_session([1u8; 16], PipelineConfig::default());
        mgr.create_session("room", None).unwrap();
        // Both layers should now reject
        assert!(mgr.create_session("room", None).is_err());
        assert!(mgr.create_pipeline_session([2u8; 16], PipelineConfig::default()).is_none());
    }
}
