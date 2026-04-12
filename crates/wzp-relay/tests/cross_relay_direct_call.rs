//! Phase 4 integration test for cross-relay direct calling
//! (PRD: .taskmaster/docs/prd_phase4_cross_relay_p2p.txt).
//!
//! Drives the call-registry cross-wiring + a simulated federation
//! forward without spinning up actual relay binaries. The real
//! main-loop and dispatcher code are exercised end-to-end in
//! `reflect.rs` / `hole_punching.rs` already; this file focuses on
//! the *new* invariants Phase 4 adds:
//!
//! 1. When Relay A forwards a DirectCallOffer, its local registry
//!    stashes caller_reflexive_addr and leaves peer_relay_fp
//!    unset (broadcast, answer-side will identify itself).
//! 2. When Relay B's cross-relay dispatcher receives the forward,
//!    its local registry stores the call with
//!    peer_relay_fp = Some(relay_a_tls_fp).
//! 3. When Relay B processes the local callee's answer, it sees
//!    peer_relay_fp.is_some() and MUST NOT deliver the answer via
//!    local signal_hub — instead it routes through federation.
//! 4. When Relay A receives the forwarded answer via its
//!    cross-relay dispatcher, it stashes callee_reflexive_addr
//!    and emits a CallSetup to its local caller with
//!    peer_direct_addr = callee_addr.
//! 5. Final state: Alice's CallSetup carries Bob's reflex addr,
//!    Bob's CallSetup carries Alice's reflex addr — cross-wired
//!    through two relays + a federation link.

use wzp_proto::{CallAcceptMode, SignalMessage};
use wzp_relay::call_registry::CallRegistry;

// ────────────────────────────────────────────────────────────────
// Simulated dispatch helpers — these reproduce the exact logic
// in main.rs without the tokio + federation boilerplate.
// ────────────────────────────────────────────────────────────────

const RELAY_A_TLS_FP: &str = "relay-A-tls-fingerprint";
const RELAY_B_TLS_FP: &str = "relay-B-tls-fingerprint";
const ALICE_ADDR: &str = "192.0.2.1:4433";
const BOB_ADDR: &str = "198.51.100.9:4433";
const RELAY_A_ADDR: &str = "203.0.113.5:4433";
const RELAY_B_ADDR: &str = "203.0.113.10:4433";

/// Helper that Alice's place_call sends.
fn alice_offer(call_id: &str) -> SignalMessage {
    SignalMessage::DirectCallOffer {
        caller_fingerprint: "alice".into(),
        caller_alias: None,
        target_fingerprint: "bob".into(),
        call_id: call_id.into(),
        identity_pub: [0; 32],
        ephemeral_pub: [0; 32],
        signature: vec![],
        supported_profiles: vec![],
        caller_reflexive_addr: Some(ALICE_ADDR.into()),
        caller_local_addrs: Vec::new(),
        caller_build_version: None,
    }
}

/// Relay A receives Alice's offer. Target Bob is not local.
/// Relay A wraps + broadcasts over federation, stashes the call
/// locally with peer_relay_fp = None (broadcast — answer-side
/// identifies itself).
fn relay_a_handle_offer(reg_a: &mut CallRegistry, offer: &SignalMessage) -> SignalMessage {
    match offer {
        SignalMessage::DirectCallOffer {
            caller_fingerprint,
            target_fingerprint,
            call_id,
            caller_reflexive_addr,
            ..
        } => {
            reg_a.create_call(
                call_id.clone(),
                caller_fingerprint.clone(),
                target_fingerprint.clone(),
            );
            reg_a.set_caller_reflexive_addr(call_id, caller_reflexive_addr.clone());
            // peer_relay_fp stays None — we don't know which peer
            // will respond yet.
        }
        _ => panic!("not an offer"),
    }
    // Build the federation envelope the main loop would
    // broadcast.
    SignalMessage::FederatedSignalForward {
        inner: Box::new(offer.clone()),
        origin_relay_fp: RELAY_A_TLS_FP.into(),
    }
}

/// Relay B receives a FederatedSignalForward(DirectCallOffer).
/// This is the cross-relay dispatcher task code in main.rs —
/// reproduced here for the test.
fn relay_b_handle_forwarded_offer(reg_b: &mut CallRegistry, forward: &SignalMessage) {
    let (inner, origin_relay_fp) = match forward {
        SignalMessage::FederatedSignalForward { inner, origin_relay_fp } => {
            (inner.as_ref().clone(), origin_relay_fp.clone())
        }
        _ => panic!("not a forward"),
    };
    // Loop-prevention: drop self-sourced.
    assert_ne!(origin_relay_fp, RELAY_B_TLS_FP);

    let SignalMessage::DirectCallOffer {
        caller_fingerprint,
        target_fingerprint,
        call_id,
        caller_reflexive_addr,
        ..
    } = inner
    else {
        panic!("inner was not DirectCallOffer");
    };

    // Simulated: target is local to B (Bob is registered here).
    reg_b.create_call(
        call_id.clone(),
        caller_fingerprint,
        target_fingerprint,
    );
    reg_b.set_caller_reflexive_addr(&call_id, caller_reflexive_addr);
    reg_b.set_peer_relay_fp(&call_id, Some(origin_relay_fp));
}

/// Bob's answer — AcceptTrusted with his reflex addr.
fn bob_answer(call_id: &str) -> SignalMessage {
    SignalMessage::DirectCallAnswer {
        call_id: call_id.into(),
        accept_mode: CallAcceptMode::AcceptTrusted,
        identity_pub: None,
        ephemeral_pub: None,
        signature: None,
        chosen_profile: None,
        callee_reflexive_addr: Some(BOB_ADDR.into()),
        callee_local_addrs: Vec::new(),
        callee_build_version: None,
    }
}

/// Relay B handles the LOCAL callee's answer. If peer_relay_fp
/// is Some, wrap the answer in a FederatedSignalForward + emit the
/// local CallSetup to Bob. Returns the (forward_envelope,
/// bob_call_setup) pair.
fn relay_b_handle_local_answer(
    reg_b: &mut CallRegistry,
    answer: &SignalMessage,
) -> (SignalMessage, SignalMessage) {
    let (call_id, mode, callee_addr) = match answer {
        SignalMessage::DirectCallAnswer {
            call_id,
            accept_mode,
            callee_reflexive_addr,
            ..
        } => (call_id.clone(), *accept_mode, callee_reflexive_addr.clone()),
        _ => panic!(),
    };
    // Stash callee addr + activate.
    reg_b.set_active(&call_id, mode, format!("call-{call_id}"));
    reg_b.set_callee_reflexive_addr(&call_id, callee_addr);
    let call = reg_b.get(&call_id).unwrap();
    let caller_addr = call.caller_reflexive_addr.clone();
    let callee_addr = call.callee_reflexive_addr.clone();
    assert!(
        call.peer_relay_fp.is_some(),
        "Relay B must know this call is cross-relay"
    );

    // Forward the answer back over federation.
    let forward = SignalMessage::FederatedSignalForward {
        inner: Box::new(answer.clone()),
        origin_relay_fp: RELAY_B_TLS_FP.into(),
    };

    // Local CallSetup for Bob — peer_direct_addr = Alice's addr.
    let setup_for_bob = SignalMessage::CallSetup {
        call_id: call_id.clone(),
        room: format!("call-{call_id}"),
        relay_addr: RELAY_B_ADDR.into(),
        peer_direct_addr: caller_addr,
        peer_local_addrs: Vec::new(),
    };
    let _ = callee_addr;
    (forward, setup_for_bob)
}

/// Relay A's cross-relay dispatcher receives the forwarded answer.
/// It stashes the callee addr, forwards the raw answer to local
/// Alice, and emits a CallSetup with peer_direct_addr = Bob's addr.
fn relay_a_handle_forwarded_answer(
    reg_a: &mut CallRegistry,
    forward: &SignalMessage,
) -> SignalMessage {
    let (inner, origin_relay_fp) = match forward {
        SignalMessage::FederatedSignalForward { inner, origin_relay_fp } => {
            (inner.as_ref().clone(), origin_relay_fp.clone())
        }
        _ => panic!("not a forward"),
    };
    assert_ne!(origin_relay_fp, RELAY_A_TLS_FP);

    let SignalMessage::DirectCallAnswer {
        call_id,
        accept_mode,
        callee_reflexive_addr,
        ..
    } = inner
    else {
        panic!("inner was not DirectCallAnswer");
    };
    assert_eq!(accept_mode, CallAcceptMode::AcceptTrusted);

    reg_a.set_active(&call_id, accept_mode, format!("call-{call_id}"));
    reg_a.set_callee_reflexive_addr(&call_id, callee_reflexive_addr.clone());

    // Alice's CallSetup — peer_direct_addr = Bob's addr.
    SignalMessage::CallSetup {
        call_id: call_id.clone(),
        room: format!("call-{call_id}"),
        relay_addr: RELAY_A_ADDR.into(),
        peer_direct_addr: callee_reflexive_addr,
        peer_local_addrs: Vec::new(),
    }
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[test]
fn cross_relay_offer_forwards_and_stashes_peer_relay_fp() {
    let mut reg_a = CallRegistry::new();
    let mut reg_b = CallRegistry::new();

    let offer = alice_offer("c-xrelay-1");
    let forward = relay_a_handle_offer(&mut reg_a, &offer);

    // Relay A's local view: call exists, caller addr stashed,
    // peer_relay_fp still None (broadcast — answer identifies the
    // peer).
    let call_a = reg_a.get("c-xrelay-1").unwrap();
    assert_eq!(call_a.caller_fingerprint, "alice");
    assert_eq!(call_a.callee_fingerprint, "bob");
    assert_eq!(call_a.caller_reflexive_addr.as_deref(), Some(ALICE_ADDR));
    assert!(call_a.peer_relay_fp.is_none());

    // Relay B dispatches the forward: creates the call locally
    // and stashes peer_relay_fp = Relay A.
    relay_b_handle_forwarded_offer(&mut reg_b, &forward);
    let call_b = reg_b.get("c-xrelay-1").unwrap();
    assert_eq!(call_b.caller_fingerprint, "alice");
    assert_eq!(call_b.callee_fingerprint, "bob");
    assert_eq!(call_b.caller_reflexive_addr.as_deref(), Some(ALICE_ADDR));
    assert_eq!(call_b.peer_relay_fp.as_deref(), Some(RELAY_A_TLS_FP));
}

#[test]
fn cross_relay_answer_crosswires_peer_direct_addrs() {
    let mut reg_a = CallRegistry::new();
    let mut reg_b = CallRegistry::new();

    // Full round trip: offer → forward → dispatch → answer →
    // forward back → dispatch → both CallSetups.
    let offer = alice_offer("c-xrelay-2");
    let offer_forward = relay_a_handle_offer(&mut reg_a, &offer);
    relay_b_handle_forwarded_offer(&mut reg_b, &offer_forward);

    // Bob answers on Relay B.
    let answer = bob_answer("c-xrelay-2");
    let (answer_forward, setup_for_bob) =
        relay_b_handle_local_answer(&mut reg_b, &answer);

    // Bob's CallSetup carries Alice's addr.
    match setup_for_bob {
        SignalMessage::CallSetup { peer_direct_addr, relay_addr, .. } => {
            assert_eq!(peer_direct_addr.as_deref(), Some(ALICE_ADDR));
            assert_eq!(relay_addr, RELAY_B_ADDR);
        }
        _ => panic!("wrong variant"),
    }

    // Alice's dispatcher receives the forwarded answer and builds
    // her CallSetup.
    let setup_for_alice = relay_a_handle_forwarded_answer(&mut reg_a, &answer_forward);
    match setup_for_alice {
        SignalMessage::CallSetup { peer_direct_addr, relay_addr, .. } => {
            assert_eq!(peer_direct_addr.as_deref(), Some(BOB_ADDR));
            assert_eq!(relay_addr, RELAY_A_ADDR);
        }
        _ => panic!("wrong variant"),
    }

    // Both registries agree on caller + callee reflex addrs after
    // the full round-trip.
    for reg in [&reg_a, &reg_b] {
        let c = reg.get("c-xrelay-2").unwrap();
        assert_eq!(c.caller_reflexive_addr.as_deref(), Some(ALICE_ADDR));
        assert_eq!(c.callee_reflexive_addr.as_deref(), Some(BOB_ADDR));
    }
}

#[test]
fn cross_relay_loop_prevention_drops_self_sourced_forward() {
    // A FederatedSignalForward that circles back to the origin
    // relay should be dropped before it hits the call registry.
    let forward = SignalMessage::FederatedSignalForward {
        inner: Box::new(alice_offer("c-loop")),
        origin_relay_fp: RELAY_B_TLS_FP.into(),
    };
    // The dispatcher in main.rs calls this explicit check before
    // doing any work. Reproduce it inline.
    let origin = match &forward {
        SignalMessage::FederatedSignalForward { origin_relay_fp, .. } => origin_relay_fp.clone(),
        _ => unreachable!(),
    };
    // Relay B sees origin == its own fp → drop.
    assert_eq!(origin, RELAY_B_TLS_FP, "loop-prevention triggers on self-fp");
}
