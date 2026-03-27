//! Session manager — tracks active call sessions on the relay.

use std::collections::HashMap;

use wzp_proto::{QualityProfile, Session};

use crate::pipeline::{PipelineConfig, RelayPipeline};

/// Unique identifier for a relay session.
pub type SessionId = [u8; 16];

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
pub struct SessionManager {
    sessions: HashMap<SessionId, RelaySession>,
    max_sessions: usize,
}

impl SessionManager {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: HashMap::new(),
            max_sessions,
        }
    }

    /// Create a new session. Returns None if at capacity.
    pub fn create_session(
        &mut self,
        session_id: SessionId,
        config: PipelineConfig,
    ) -> Option<&mut RelaySession> {
        if self.sessions.len() >= self.max_sessions {
            return None;
        }
        self.sessions
            .entry(session_id)
            .or_insert_with(|| RelaySession::new(session_id, config));
        self.sessions.get_mut(&session_id)
    }

    /// Get a session by ID.
    pub fn get_session(&mut self, id: &SessionId) -> Option<&mut RelaySession> {
        self.sessions.get_mut(id)
    }

    /// Remove a session.
    pub fn remove_session(&mut self, id: &SessionId) -> Option<RelaySession> {
        self.sessions.remove(id)
    }

    /// Number of active sessions.
    pub fn active_count(&self) -> usize {
        self.sessions.values().filter(|s| s.is_active()).count()
    }

    /// Total sessions (including inactive/closing).
    pub fn total_count(&self) -> usize {
        self.sessions.len()
    }

    /// Remove sessions idle for longer than `timeout_ms`.
    pub fn expire_idle(&mut self, now_ms: u64, timeout_ms: u64) -> usize {
        let before = self.sessions.len();
        self.sessions
            .retain(|_, s| now_ms.saturating_sub(s.last_activity_ms) < timeout_ms);
        before - self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_get_session() {
        let mut mgr = SessionManager::new(10);
        let id = [1u8; 16];
        mgr.create_session(id, PipelineConfig::default());
        assert_eq!(mgr.total_count(), 1);
        assert!(mgr.get_session(&id).is_some());
    }

    #[test]
    fn respects_max_sessions() {
        let mut mgr = SessionManager::new(1);
        mgr.create_session([1u8; 16], PipelineConfig::default());
        let result = mgr.create_session([2u8; 16], PipelineConfig::default());
        assert!(result.is_none());
    }

    #[test]
    fn expire_idle_removes_old() {
        let mut mgr = SessionManager::new(10);
        let id = [1u8; 16];
        mgr.create_session(id, PipelineConfig::default());
        // Session has last_activity_ms = 0, current time = 60000, timeout = 30000
        let expired = mgr.expire_idle(60_000, 30_000);
        assert_eq!(expired, 1);
        assert_eq!(mgr.total_count(), 0);
    }
}
