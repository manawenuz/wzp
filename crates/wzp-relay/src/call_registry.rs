//! Direct call state tracking.
//!
//! Manages the lifecycle of 1:1 direct calls placed via the `_signal` channel.
//! Each call goes through: Pending → Ringing → Active → Ended.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// State of a direct call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectCallState {
    /// Offer sent to callee, waiting for response.
    Pending,
    /// Callee acknowledged, ringing.
    Ringing,
    /// Call accepted, media room active.
    Active,
    /// Call ended (hangup, reject, timeout, or error).
    Ended,
}

/// A tracked direct call between two users.
pub struct DirectCall {
    pub call_id: String,
    pub caller_fingerprint: String,
    pub callee_fingerprint: String,
    pub state: DirectCallState,
    pub accept_mode: Option<wzp_proto::CallAcceptMode>,
    /// Private room name (set when accepted).
    pub room_name: Option<String>,
    pub created_at: Instant,
    pub answered_at: Option<Instant>,
    pub ended_at: Option<Instant>,
    /// Phase 3 (hole-punching): caller's server-reflexive address
    /// as carried in the `DirectCallOffer`. The relay stashes it
    /// here when the offer arrives so it can later inject it as
    /// `peer_direct_addr` into the callee's `CallSetup`.
    pub caller_reflexive_addr: Option<String>,
    /// Phase 3 (hole-punching): callee's server-reflexive address
    /// as carried in the `DirectCallAnswer`. Only populated for
    /// `AcceptTrusted` answers — privacy-mode answers leave this
    /// `None`. Fed into the caller's `CallSetup.peer_direct_addr`.
    pub callee_reflexive_addr: Option<String>,
}

/// Registry of active direct calls.
pub struct CallRegistry {
    calls: HashMap<String, DirectCall>,
}

impl CallRegistry {
    pub fn new() -> Self {
        Self {
            calls: HashMap::new(),
        }
    }

    /// Create a new pending call. Returns the call_id.
    pub fn create_call(&mut self, call_id: String, caller_fp: String, callee_fp: String) -> &DirectCall {
        let call = DirectCall {
            call_id: call_id.clone(),
            caller_fingerprint: caller_fp,
            callee_fingerprint: callee_fp,
            state: DirectCallState::Pending,
            accept_mode: None,
            room_name: None,
            created_at: Instant::now(),
            answered_at: None,
            ended_at: None,
            caller_reflexive_addr: None,
            callee_reflexive_addr: None,
        };
        self.calls.insert(call_id.clone(), call);
        self.calls.get(&call_id).unwrap()
    }

    /// Phase 3: stash the caller's server-reflexive address read
    /// off a `DirectCallOffer`. Safe to call on any call state;
    /// a no-op if the call doesn't exist.
    pub fn set_caller_reflexive_addr(&mut self, call_id: &str, addr: Option<String>) {
        if let Some(call) = self.calls.get_mut(call_id) {
            call.caller_reflexive_addr = addr;
        }
    }

    /// Phase 3: stash the callee's server-reflexive address read
    /// off a `DirectCallAnswer`. Safe to call on any call state;
    /// a no-op if the call doesn't exist.
    pub fn set_callee_reflexive_addr(&mut self, call_id: &str, addr: Option<String>) {
        if let Some(call) = self.calls.get_mut(call_id) {
            call.callee_reflexive_addr = addr;
        }
    }

    /// Get a call by ID.
    pub fn get(&self, call_id: &str) -> Option<&DirectCall> {
        self.calls.get(call_id)
    }

    /// Get a mutable call by ID.
    pub fn get_mut(&mut self, call_id: &str) -> Option<&mut DirectCall> {
        self.calls.get_mut(call_id)
    }

    /// Transition to Ringing state.
    pub fn set_ringing(&mut self, call_id: &str) -> bool {
        if let Some(call) = self.calls.get_mut(call_id) {
            if call.state == DirectCallState::Pending {
                call.state = DirectCallState::Ringing;
                return true;
            }
        }
        false
    }

    /// Transition to Active state.
    pub fn set_active(&mut self, call_id: &str, mode: wzp_proto::CallAcceptMode, room: String) -> bool {
        if let Some(call) = self.calls.get_mut(call_id) {
            if call.state == DirectCallState::Pending || call.state == DirectCallState::Ringing {
                call.state = DirectCallState::Active;
                call.accept_mode = Some(mode);
                call.room_name = Some(room);
                call.answered_at = Some(Instant::now());
                return true;
            }
        }
        false
    }

    /// End a call.
    pub fn end_call(&mut self, call_id: &str) -> Option<DirectCall> {
        if let Some(call) = self.calls.get_mut(call_id) {
            call.state = DirectCallState::Ended;
            call.ended_at = Some(Instant::now());
        }
        self.calls.remove(call_id)
    }

    /// Find active/pending calls involving a fingerprint.
    pub fn calls_for_fingerprint(&self, fp: &str) -> Vec<&DirectCall> {
        self.calls.values()
            .filter(|c| {
                c.state != DirectCallState::Ended
                    && (c.caller_fingerprint == fp || c.callee_fingerprint == fp)
            })
            .collect()
    }

    /// Find the peer's fingerprint in a call.
    pub fn peer_fingerprint(&self, call_id: &str, my_fp: &str) -> Option<&str> {
        self.calls.get(call_id).map(|c| {
            if c.caller_fingerprint == my_fp {
                c.callee_fingerprint.as_str()
            } else {
                c.caller_fingerprint.as_str()
            }
        })
    }

    /// Remove calls that have been pending longer than the timeout.
    /// Returns call IDs of expired calls.
    pub fn expire_stale(&mut self, timeout: Duration) -> Vec<DirectCall> {
        let now = Instant::now();
        let expired: Vec<String> = self.calls.iter()
            .filter(|(_, c)| {
                c.state == DirectCallState::Pending
                    && now.duration_since(c.created_at) > timeout
            })
            .map(|(id, _)| id.clone())
            .collect();

        expired.into_iter()
            .filter_map(|id| self.calls.remove(&id))
            .collect()
    }

    /// Number of active (non-ended) calls.
    pub fn active_count(&self) -> usize {
        self.calls.values()
            .filter(|c| c.state != DirectCallState::Ended)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_lifecycle() {
        let mut reg = CallRegistry::new();
        reg.create_call("c1".into(), "alice".into(), "bob".into());

        assert_eq!(reg.get("c1").unwrap().state, DirectCallState::Pending);
        assert!(reg.set_ringing("c1"));
        assert_eq!(reg.get("c1").unwrap().state, DirectCallState::Ringing);

        assert!(reg.set_active("c1", wzp_proto::CallAcceptMode::AcceptGeneric, "_call:c1".into()));
        assert_eq!(reg.get("c1").unwrap().state, DirectCallState::Active);
        assert_eq!(reg.get("c1").unwrap().room_name.as_deref(), Some("_call:c1"));

        let ended = reg.end_call("c1").unwrap();
        assert_eq!(ended.state, DirectCallState::Ended);
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn expire_stale_calls() {
        let mut reg = CallRegistry::new();
        reg.create_call("c1".into(), "alice".into(), "bob".into());

        // Not expired yet
        let expired = reg.expire_stale(Duration::from_secs(30));
        assert!(expired.is_empty());

        // Force expiry with 0 timeout
        let expired = reg.expire_stale(Duration::from_secs(0));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].call_id, "c1");
    }

    #[test]
    fn peer_lookup() {
        let mut reg = CallRegistry::new();
        reg.create_call("c1".into(), "alice".into(), "bob".into());
        assert_eq!(reg.peer_fingerprint("c1", "alice"), Some("bob"));
        assert_eq!(reg.peer_fingerprint("c1", "bob"), Some("alice"));
    }

    #[test]
    fn call_registry_stores_reflexive_addrs() {
        let mut reg = CallRegistry::new();
        reg.create_call("c1".into(), "alice".into(), "bob".into());

        // Default: both addrs are None.
        let c = reg.get("c1").unwrap();
        assert!(c.caller_reflexive_addr.is_none());
        assert!(c.callee_reflexive_addr.is_none());

        // Caller advertises its reflex addr via DirectCallOffer.
        reg.set_caller_reflexive_addr("c1", Some("192.0.2.1:4433".into()));
        assert_eq!(
            reg.get("c1").unwrap().caller_reflexive_addr.as_deref(),
            Some("192.0.2.1:4433")
        );

        // Callee responds with AcceptTrusted + its own reflex addr.
        reg.set_callee_reflexive_addr("c1", Some("198.51.100.9:4433".into()));
        assert_eq!(
            reg.get("c1").unwrap().callee_reflexive_addr.as_deref(),
            Some("198.51.100.9:4433")
        );

        // Both addrs are independently readable — the relay uses
        // them to cross-wire peer_direct_addr in CallSetup.
        let c = reg.get("c1").unwrap();
        assert_eq!(
            c.caller_reflexive_addr.as_deref(),
            Some("192.0.2.1:4433")
        );
        assert_eq!(
            c.callee_reflexive_addr.as_deref(),
            Some("198.51.100.9:4433")
        );

        // Setter on an unknown call is a no-op, not a panic.
        reg.set_caller_reflexive_addr("does-not-exist", Some("x".into()));
    }

    #[test]
    fn call_registry_clearing_reflex_addr_works() {
        // Passing None to the setter must clear a previously-set value
        // so callers that downgrade to privacy mode mid-flow don't
        // leak a stale addr into CallSetup.
        let mut reg = CallRegistry::new();
        reg.create_call("c1".into(), "alice".into(), "bob".into());
        reg.set_caller_reflexive_addr("c1", Some("192.0.2.1:4433".into()));
        reg.set_caller_reflexive_addr("c1", None);
        assert!(reg.get("c1").unwrap().caller_reflexive_addr.is_none());
    }
}
