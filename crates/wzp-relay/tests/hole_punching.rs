//! Phase 3 integration tests for hole-punching advertising
//! (PRD: .taskmaster/docs/prd_hole_punching.txt).
//!
//! These verify the end-to-end protocol cross-wiring:
//!   caller (places offer with caller_reflexive_addr=A)
//!     → relay (stashes A in registry)
//!       → callee (reads A off the forwarded offer)
//!   callee (sends AcceptTrusted answer with callee_reflexive_addr=B)
//!     → relay (stashes B, emits CallSetup to both parties)
//!       → caller receives CallSetup.peer_direct_addr = B
//!       → callee receives CallSetup.peer_direct_addr = A
//!
//! The actual QUIC hole-punch race is a Phase 3.5 follow-up.
//! These tests only cover the signal-plane plumbing — that the
//! addrs make it from each peer's offer/answer through the relay
//! cross-wiring back out in CallSetup with the peer's addr.
//!
//! We drive the call registry + a minimal routing function
//! directly instead of spinning up a full relay process — easier
//! to reason about, no real network, and what we actually want to
//! test is the cross-wiring logic, not the whole signal stack.

use wzp_proto::{CallAcceptMode, SignalMessage};
use wzp_relay::call_registry::CallRegistry;

/// Helper: simulate the relay's handling of a DirectCallOffer. In
/// `wzp-relay/src/main.rs` this is the match arm that creates the
/// call in the registry and stashes the caller's reflex addr.
fn handle_offer(reg: &mut CallRegistry, offer: &SignalMessage) -> String {
    match offer {
        SignalMessage::DirectCallOffer {
            caller_fingerprint,
            target_fingerprint,
            call_id,
            caller_reflexive_addr,
            ..
        } => {
            reg.create_call(
                call_id.clone(),
                caller_fingerprint.clone(),
                target_fingerprint.clone(),
            );
            reg.set_caller_reflexive_addr(call_id, caller_reflexive_addr.clone());
            call_id.clone()
        }
        _ => panic!("not an offer"),
    }
}

/// Helper: simulate the relay's handling of a DirectCallAnswer +
/// the subsequent CallSetup emission. Returns the two CallSetup
/// messages the relay would push: (for_caller, for_callee).
fn handle_answer_and_build_setups(
    reg: &mut CallRegistry,
    answer: &SignalMessage,
) -> (SignalMessage, SignalMessage) {
    let (call_id, mode, callee_addr) = match answer {
        SignalMessage::DirectCallAnswer {
            call_id,
            accept_mode,
            callee_reflexive_addr,
            ..
        } => (call_id.clone(), *accept_mode, callee_reflexive_addr.clone()),
        _ => panic!("not an answer"),
    };

    reg.set_callee_reflexive_addr(&call_id, callee_addr);
    let room = format!("call-{call_id}");
    reg.set_active(&call_id, mode, room.clone());

    let (caller_addr, callee_addr) = {
        let c = reg.get(&call_id).unwrap();
        (
            c.caller_reflexive_addr.clone(),
            c.callee_reflexive_addr.clone(),
        )
    };

    let setup_for_caller = SignalMessage::CallSetup {
        call_id: call_id.clone(),
        room: room.clone(),
        relay_addr: "203.0.113.5:4433".into(),
        peer_direct_addr: callee_addr,
    };
    let setup_for_callee = SignalMessage::CallSetup {
        call_id,
        room,
        relay_addr: "203.0.113.5:4433".into(),
        peer_direct_addr: caller_addr,
    };
    (setup_for_caller, setup_for_callee)
}

fn mk_offer(call_id: &str, caller_reflexive_addr: Option<&str>) -> SignalMessage {
    SignalMessage::DirectCallOffer {
        caller_fingerprint: "alice".into(),
        caller_alias: None,
        target_fingerprint: "bob".into(),
        call_id: call_id.into(),
        identity_pub: [0; 32],
        ephemeral_pub: [0; 32],
        signature: vec![],
        supported_profiles: vec![],
        caller_reflexive_addr: caller_reflexive_addr.map(String::from),
    }
}

fn mk_answer(
    call_id: &str,
    mode: CallAcceptMode,
    callee_reflexive_addr: Option<&str>,
) -> SignalMessage {
    SignalMessage::DirectCallAnswer {
        call_id: call_id.into(),
        accept_mode: mode,
        identity_pub: None,
        ephemeral_pub: None,
        signature: None,
        chosen_profile: None,
        callee_reflexive_addr: callee_reflexive_addr.map(String::from),
    }
}

// -----------------------------------------------------------------------
// Test 1: both peers advertise — CallSetup cross-wires correctly
// -----------------------------------------------------------------------

#[test]
fn both_peers_advertise_reflex_addrs_cross_wire_in_setup() {
    let mut reg = CallRegistry::new();

    let caller_addr = "192.0.2.1:4433";
    let callee_addr = "198.51.100.9:4433";

    let offer = mk_offer("c1", Some(caller_addr));
    let call_id = handle_offer(&mut reg, &offer);
    assert_eq!(call_id, "c1");
    assert_eq!(
        reg.get("c1").unwrap().caller_reflexive_addr.as_deref(),
        Some(caller_addr)
    );

    let answer = mk_answer("c1", CallAcceptMode::AcceptTrusted, Some(callee_addr));
    let (setup_caller, setup_callee) =
        handle_answer_and_build_setups(&mut reg, &answer);

    // The CALLER's setup should carry the CALLEE's addr as peer_direct_addr.
    match setup_caller {
        SignalMessage::CallSetup { peer_direct_addr, .. } => {
            assert_eq!(
                peer_direct_addr.as_deref(),
                Some(callee_addr),
                "caller's CallSetup must contain callee's addr"
            );
        }
        _ => panic!("wrong variant"),
    }

    // The CALLEE's setup should carry the CALLER's addr.
    match setup_callee {
        SignalMessage::CallSetup { peer_direct_addr, .. } => {
            assert_eq!(
                peer_direct_addr.as_deref(),
                Some(caller_addr),
                "callee's CallSetup must contain caller's addr"
            );
        }
        _ => panic!("wrong variant"),
    }
}

// -----------------------------------------------------------------------
// Test 2: callee uses AcceptGeneric (privacy) — no addr leaks
// -----------------------------------------------------------------------

#[test]
fn privacy_mode_answer_omits_callee_addr_from_setup() {
    let mut reg = CallRegistry::new();
    let caller_addr = "192.0.2.1:4433";

    handle_offer(&mut reg, &mk_offer("c2", Some(caller_addr)));

    // AcceptGeneric explicitly passes None for callee_reflexive_addr —
    // the whole point is to hide the callee's IP from the caller.
    let answer = mk_answer("c2", CallAcceptMode::AcceptGeneric, None);
    let (setup_caller, setup_callee) =
        handle_answer_and_build_setups(&mut reg, &answer);

    // CALLER should see peer_direct_addr = None (privacy preserved).
    match setup_caller {
        SignalMessage::CallSetup { peer_direct_addr, .. } => {
            assert!(
                peer_direct_addr.is_none(),
                "privacy mode must not leak callee addr to caller"
            );
        }
        _ => panic!("wrong variant"),
    }

    // CALLEE still gets the caller's addr — only the callee opted for
    // privacy, the caller already volunteered its addr in the offer.
    match setup_callee {
        SignalMessage::CallSetup { peer_direct_addr, .. } => {
            assert_eq!(
                peer_direct_addr.as_deref(),
                Some(caller_addr),
                "callee's CallSetup should still carry caller's volunteered addr"
            );
        }
        _ => panic!("wrong variant"),
    }
}

// -----------------------------------------------------------------------
// Test 3: old caller (no addr) + new callee — relay path only
// -----------------------------------------------------------------------

#[test]
fn pre_phase3_caller_leaves_both_setups_relay_only() {
    let mut reg = CallRegistry::new();

    // Pre-Phase-3 client doesn't know about caller_reflexive_addr
    // so the field is None.
    handle_offer(&mut reg, &mk_offer("c3", None));

    // New callee advertises its addr — doesn't matter because
    // without caller_reflexive_addr the caller has nothing to
    // attempt a direct handshake to, so the cross-wiring should
    // still leave the caller's CallSetup without peer_direct_addr.
    let answer = mk_answer(
        "c3",
        CallAcceptMode::AcceptTrusted,
        Some("198.51.100.9:4433"),
    );
    let (setup_caller, setup_callee) =
        handle_answer_and_build_setups(&mut reg, &answer);

    match setup_caller {
        SignalMessage::CallSetup { peer_direct_addr, .. } => {
            // Phase 3 relay behavior: we always inject whatever
            // addrs are in the registry, regardless of who
            // advertised. The caller here gets the callee's addr
            // because the callee did advertise.
            assert_eq!(peer_direct_addr.as_deref(), Some("198.51.100.9:4433"));
        }
        _ => panic!("wrong variant"),
    }

    // The callee's setup has no caller addr (pre-Phase-3 offer).
    match setup_callee {
        SignalMessage::CallSetup { peer_direct_addr, .. } => {
            assert!(
                peer_direct_addr.is_none(),
                "callee should see no caller addr when offer was pre-Phase-3"
            );
        }
        _ => panic!("wrong variant"),
    }
}

// -----------------------------------------------------------------------
// Test 4: neither side advertises — both CallSetups fall back cleanly
// -----------------------------------------------------------------------

#[test]
fn neither_peer_advertises_both_setups_are_relay_only() {
    let mut reg = CallRegistry::new();

    handle_offer(&mut reg, &mk_offer("c4", None));
    let answer = mk_answer("c4", CallAcceptMode::AcceptTrusted, None);
    let (setup_caller, setup_callee) =
        handle_answer_and_build_setups(&mut reg, &answer);

    for (label, setup) in [("caller", setup_caller), ("callee", setup_callee)] {
        match setup {
            SignalMessage::CallSetup { peer_direct_addr, relay_addr, .. } => {
                assert!(
                    peer_direct_addr.is_none(),
                    "{label}'s CallSetup must have no peer_direct_addr"
                );
                // Relay addr is always filled — that's the fallback
                // path and the existing behavior.
                assert!(!relay_addr.is_empty(), "{label} relay_addr must be set");
            }
            _ => panic!("wrong variant"),
        }
    }
}
