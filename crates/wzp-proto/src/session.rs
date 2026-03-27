use serde::{Deserialize, Serialize};

/// Session state machine for a call.
///
/// ```text
/// Idle → Connecting → Handshaking → Active ⇄ Rekeying → Active
///                                      ↓
///                                    Closed
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionState {
    /// No active call. Waiting for initiation.
    Idle,
    /// Transport connection being established (QUIC handshake).
    Connecting,
    /// Crypto handshake in progress (X25519 key exchange, identity verification).
    Handshaking,
    /// Call is active — media flowing.
    Active,
    /// Rekeying in progress (forward secrecy rotation). Media continues flowing.
    Rekeying,
    /// Call has ended.
    Closed,
}

/// Events that drive session state transitions.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// User initiates a call.
    Initiate,
    /// Transport connection established.
    Connected,
    /// Crypto handshake completed successfully.
    HandshakeComplete,
    /// Rekey initiated (local or remote).
    RekeyStart,
    /// Rekey completed successfully.
    RekeyComplete,
    /// Call ended (local hangup, remote hangup, or error).
    Terminate { reason: TerminateReason },
    /// Transport connection lost.
    ConnectionLost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminateReason {
    LocalHangup,
    RemoteHangup,
    Timeout,
    Error,
}

/// Session state machine.
pub struct Session {
    state: SessionState,
    /// Unique session identifier (random, generated at call initiation).
    session_id: [u8; 16],
    /// Timestamp of the last state transition (ms since epoch).
    last_transition_ms: u64,
    /// Number of successful rekeys in this session.
    rekey_count: u32,
}

/// Error when a state transition is invalid.
#[derive(Debug, thiserror::Error)]
#[error("invalid transition from {from:?} on event {event}")]
pub struct TransitionError {
    pub from: SessionState,
    pub event: String,
}

impl Session {
    pub fn new(session_id: [u8; 16]) -> Self {
        Self {
            state: SessionState::Idle,
            session_id,
            last_transition_ms: 0,
            rekey_count: 0,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn session_id(&self) -> &[u8; 16] {
        &self.session_id
    }

    pub fn rekey_count(&self) -> u32 {
        self.rekey_count
    }

    /// Process an event and transition state.
    pub fn transition(
        &mut self,
        event: SessionEvent,
        now_ms: u64,
    ) -> Result<SessionState, TransitionError> {
        let new_state = match (&self.state, &event) {
            (SessionState::Idle, SessionEvent::Initiate) => SessionState::Connecting,

            (SessionState::Connecting, SessionEvent::Connected) => SessionState::Handshaking,
            (SessionState::Connecting, SessionEvent::Terminate { .. })
            | (SessionState::Connecting, SessionEvent::ConnectionLost) => SessionState::Closed,

            (SessionState::Handshaking, SessionEvent::HandshakeComplete) => SessionState::Active,
            (SessionState::Handshaking, SessionEvent::Terminate { .. })
            | (SessionState::Handshaking, SessionEvent::ConnectionLost) => SessionState::Closed,

            (SessionState::Active, SessionEvent::RekeyStart) => SessionState::Rekeying,
            (SessionState::Active, SessionEvent::Terminate { .. }) => SessionState::Closed,
            (SessionState::Active, SessionEvent::ConnectionLost) => SessionState::Closed,

            (SessionState::Rekeying, SessionEvent::RekeyComplete) => {
                self.rekey_count += 1;
                SessionState::Active
            }
            (SessionState::Rekeying, SessionEvent::Terminate { .. })
            | (SessionState::Rekeying, SessionEvent::ConnectionLost) => SessionState::Closed,

            _ => {
                return Err(TransitionError {
                    from: self.state,
                    event: format!("{event:?}"),
                });
            }
        };

        self.state = new_state;
        self.last_transition_ms = now_ms;
        Ok(new_state)
    }

    /// Whether the session is in a state where media can flow.
    pub fn is_media_active(&self) -> bool {
        matches!(self.state, SessionState::Active | SessionState::Rekeying)
    }

    /// Duration since last state transition.
    pub fn time_in_state_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_transition_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session() -> Session {
        Session::new([0u8; 16])
    }

    #[test]
    fn happy_path() {
        let mut s = make_session();
        assert_eq!(s.state(), SessionState::Idle);

        s.transition(SessionEvent::Initiate, 0).unwrap();
        assert_eq!(s.state(), SessionState::Connecting);

        s.transition(SessionEvent::Connected, 100).unwrap();
        assert_eq!(s.state(), SessionState::Handshaking);

        s.transition(SessionEvent::HandshakeComplete, 200).unwrap();
        assert_eq!(s.state(), SessionState::Active);
        assert!(s.is_media_active());

        s.transition(SessionEvent::RekeyStart, 60_000).unwrap();
        assert_eq!(s.state(), SessionState::Rekeying);
        assert!(s.is_media_active()); // media continues during rekey

        s.transition(SessionEvent::RekeyComplete, 60_100).unwrap();
        assert_eq!(s.state(), SessionState::Active);
        assert_eq!(s.rekey_count(), 1);

        s.transition(
            SessionEvent::Terminate {
                reason: TerminateReason::LocalHangup,
            },
            120_000,
        )
        .unwrap();
        assert_eq!(s.state(), SessionState::Closed);
    }

    #[test]
    fn invalid_transition() {
        let mut s = make_session();
        let result = s.transition(SessionEvent::Connected, 0);
        assert!(result.is_err());
    }

    #[test]
    fn connection_lost_from_active() {
        let mut s = make_session();
        s.transition(SessionEvent::Initiate, 0).unwrap();
        s.transition(SessionEvent::Connected, 100).unwrap();
        s.transition(SessionEvent::HandshakeComplete, 200).unwrap();

        s.transition(SessionEvent::ConnectionLost, 5000).unwrap();
        assert_eq!(s.state(), SessionState::Closed);
    }
}
