//! Two in-process nodes on loopback, relays/DNS/mDNS disabled. No internet.
//! Kept inside the lib (not `tests/`) so CI's `cargo test --lib` runs them.

use std::time::Duration;

use tokio::sync::broadcast::Receiver;
use crate::{
    ContactCard, Envelope, Error, IrohConfig, IrohNode, NodeEvent, PresenceState, RelayChoice,
};

const T: Duration = Duration::from_secs(15);

fn cfg(dir: &std::path::Path) -> IrohConfig {
    IrohConfig {
        data_dir: Some(dir.to_path_buf()),
        relay: RelayChoice::Disabled,
        local_discovery: false,
        dns_discovery: false,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        presence_interval: Duration::from_millis(300),
        presence_max_backoff: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
    }
}

async fn node(seed: u8, dir: &std::path::Path) -> IrohNode {
    IrohNode::start(&[seed; 32], cfg(dir)).await.unwrap()
}

/// Wait for the first event matching `f`.
async fn wait_for<T>(rx: &mut Receiver<NodeEvent>, mut f: impl FnMut(&NodeEvent) -> Option<T>) -> T {
    tokio::time::timeout(T, async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Some(t) = f(&ev) {
                        return t;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("event channel closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_determinism_and_fingerprint() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let seed = [42u8; 32];
    let a = IrohNode::start(&seed, cfg(d1.path())).await.unwrap();
    let id = a.id();
    a.shutdown().await;
    let b = IrohNode::start(&seed, cfg(d2.path())).await.unwrap();
    assert_eq!(b.id(), id, "same seed -> same EndpointId");

    let wzp = wzp_crypto::Seed::from_bytes(seed).derive_identity().public_identity();
    assert_eq!(id.as_bytes(), wzp.signing.as_bytes(), "EndpointId == WZP identity key");
    assert_eq!(b.fingerprint(), wzp.fingerprint.to_string());

    let card = ContactCard::parse(&b.contact_string("bob")).unwrap();
    assert_eq!(card.id, id);
    assert_eq!(card.name, "bob");
    assert!(!card.direct_addrs.is_empty(), "relay disabled -> direct addrs");
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contacts_presence_and_envelopes() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = node(1, da.path()).await;
    let b = node(2, db.path()).await;
    let mut ev_a = a.events();
    let mut ev_b = b.events();

    // Contact exchange via contact strings.
    let ca = a.add_contact(&b.contact_string("bob")).await.unwrap();
    let cb = b.add_contact(&a.contact_string("alice")).await.unwrap();
    assert_eq!(ca.id, b.id());
    assert_eq!(ca.name, "bob");
    assert_eq!(ca.fingerprint, b.fingerprint());
    assert_eq!(cb.id, a.id());
    assert_eq!(a.contacts().len(), 1);
    assert!(matches!(a.add_contact(&a.contact_string("me")).await, Err(Error::SelfContact)));

    // Presence: both come online.
    let bid = b.id();
    wait_for(&mut ev_a, |e| match e {
        NodeEvent::Presence { id, state: PresenceState::Online, last_seen } if *id == bid => {
            assert!(last_seen.is_some());
            Some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(a.presence(bid), PresenceState::Online);

    // Envelope round trip: Text, FileInvite, Signal.
    a.send(bid, Envelope::text("hello bob")).await.unwrap();
    let invite = serde_json::json!({"relay": "203.0.113.5:4433", "token": "t0k", "candidates": ["192.0.2.1:9000"]});
    a.send(bid, Envelope::FileInvite(invite.clone())).await.unwrap();
    a.send(bid, Envelope::Signal(wzp_proto::SignalMessage::Reflect)).await.unwrap();
    a.send(bid, Envelope::Ping).await.unwrap(); // acked, not surfaced

    let aid = a.id();
    let mut got = Vec::new();
    while got.len() < 3 {
        let env = wait_for(&mut ev_b, |e| match e {
            NodeEvent::Message { from, env } => {
                assert_eq!(*from, aid);
                Some(env.clone())
            }
            _ => None,
        })
        .await;
        got.push(env);
    }
    assert!(got.iter().any(|e| matches!(e, Envelope::Text { text, .. } if text == "hello bob")));
    assert!(got.iter().any(|e| matches!(e, Envelope::FileInvite(v) if *v == invite)));
    assert!(got.iter().any(|e| matches!(e, Envelope::Signal(wzp_proto::SignalMessage::Reflect))));
    assert!(!got.iter().any(|e| matches!(e, Envelope::Ping)));

    // Reply direction.
    b.send(aid, Envelope::text("hi alice")).await.unwrap();
    wait_for(&mut ev_a, |e| match e {
        NodeEvent::Message { from, env: Envelope::Text { text, .. } } if *from == bid && text == "hi alice" => Some(()),
        _ => None,
    })
    .await;

    // Contacts persisted.
    let saved = std::fs::read_to_string(da.path().join("contacts.json")).unwrap();
    assert!(saved.contains(&b.fingerprint()));

    // Presence: B goes away -> A sees offline.
    b.shutdown().await;
    wait_for(&mut ev_a, |e| match e {
        NodeEvent::Presence { id, state: PresenceState::Offline, .. } if *id == bid => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(a.presence(bid), PresenceState::Offline);
    assert!(a.send(bid, Envelope::text("anyone?")).await.is_err());

    a.remove_contact(bid).await.unwrap();
    assert!(a.contacts().is_empty());
    assert!(matches!(a.send(bid, Envelope::Ping).await, Err(Error::UnknownContact(_))));
    a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_sender_rejected() {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = node(3, da.path()).await;
    let b = node(4, db.path()).await;
    let mut ev_b = b.events();

    // A knows B, but B does not know A.
    a.add_contact(&b.contact_string("bob")).await.unwrap();
    let res = a.send(b.id(), Envelope::text("let me in")).await;
    assert!(matches!(res, Err(Error::Rejected(..))), "{res:?}");

    let aid = a.id();
    let fp_a = a.fingerprint();
    wait_for(&mut ev_b, |e| match e {
        NodeEvent::ContactRequest { id, fingerprint, name, .. } if *id == aid => {
            assert_eq!(*fingerprint, fp_a);
            assert!(name.is_none());
            Some(())
        }
        NodeEvent::Message { .. } => panic!("message from unknown sender delivered"),
        _ => None,
    })
    .await;

    // A Hello carries a contact string B can accept.
    assert!(matches!(a.send_hello(b.id(), "alice").await, Err(Error::Rejected(..))));
    let contact = wait_for(&mut ev_b, |e| match e {
        NodeEvent::ContactRequest { id, name: Some(n), contact: Some(c), .. } if *id == aid => {
            assert_eq!(n, "alice");
            Some(c.clone())
        }
        NodeEvent::Message { .. } => panic!("message from unknown sender delivered"),
        _ => None,
    })
    .await;

    // B accepts -> messages now flow.
    let c = b.add_contact(&contact).await.unwrap();
    assert_eq!(c.id, aid);
    a.send(b.id(), Envelope::text("thanks")).await.unwrap();
    wait_for(&mut ev_b, |e| match e {
        NodeEvent::Message { from, env: Envelope::Text { text, .. } } if *from == aid && text == "thanks" => Some(()),
        _ => None,
    })
    .await;

    a.shutdown().await;
    b.shutdown().await;
}
