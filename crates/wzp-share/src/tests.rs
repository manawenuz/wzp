//! Two in-process ShareNodes (iroh relays/DNS/mDNS off, loopback only) plus
//! an in-process wzp-relay for the fallback path. In the lib so CI's
//! `cargo test --lib` runs them.

use std::{net::SocketAddr, path::Path, sync::Arc, time::Duration};

use tokio::sync::broadcast::Receiver;
use wzp_files::FileSessionInvite;
use wzp_iroh::{Envelope, RelayChoice};

use crate::*;

const T: Duration = Duration::from_secs(30);

// ── in-process wzp-relay (same wiring as wzp-files' e2e tests) ──────────

struct TestRelay {
    addr: SocketAddr,
    _task: tokio::task::JoinHandle<()>,
}

async fn start_relay() -> TestRelay {
    use wzp_relay::file_relay::{self, StreamRelayLimits};
    use wzp_relay::metrics::RelayMetrics;
    use wzp_relay::room::{self, RoomManager};

    let _ = rustls::crypto::ring::default_provider().install_default();
    let seed = [0x42u8; 32];
    let (sc, _) = wzp_transport::server_config_from_seed(&seed);
    let ep = wzp_transport::create_endpoint("127.0.0.1:0".parse().unwrap(), Some(sc)).unwrap();
    let addr = ep.local_addr().unwrap();
    let rooms = Arc::new(RoomManager::new());
    let metrics = Arc::new(RelayMetrics::new());
    let task = tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let rm = rooms.clone();
            let metrics = metrics.clone();
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                let room_name = conn
                    .handshake_data()
                    .and_then(|hd| hd.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
                    .and_then(|hd| hd.server_name.clone())
                    .unwrap_or_else(|| "default".into());
                let addr = conn.remote_address();
                let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));
                let Ok((_s, _p, fp, alias)) =
                    wzp_relay::handshake::accept_handshake(&*transport, &seed).await
                else {
                    return;
                };
                if file_relay::is_two_party_room(&room_name) && rm.room_size(&room_name) >= 2 {
                    transport.close_now();
                    return;
                }
                let Ok((pid, update, senders, _)) = rm.join(
                    &room_name,
                    addr,
                    room::ParticipantSender::Quic(transport.clone()),
                    Some(&fp),
                    alias.as_deref(),
                ) else {
                    return;
                };
                room::broadcast_signal(&senders, &update).await;
                let media = tokio::spawn(room::run_participant(
                    rm.clone(),
                    room_name.clone(),
                    pid,
                    transport.clone(),
                    metrics,
                    format!("{pid}"),
                    false,
                    None,
                    None,
                    None,
                    false,
                ));
                let sig = tokio::spawn(room::run_participant_signals(
                    rm.clone(),
                    room_name.clone(),
                    pid,
                    transport.clone(),
                ));
                let splice = tokio::spawn(file_relay::run_stream_splicer(
                    rm.clone(),
                    room_name.clone(),
                    pid,
                    transport.clone(),
                    StreamRelayLimits::default(),
                ));
                transport.connection().closed().await;
                media.abort();
                sig.abort();
                splice.abort();
                if let Some((update, senders)) = rm.leave(&room_name, pid) {
                    room::broadcast_signal(&senders, &update).await;
                }
            });
        }
    });
    TestRelay { addr, _task: task }
}

// ── helpers ─────────────────────────────────────────────────────────────

fn cfg(dir: &Path, relay: Option<SocketAddr>) -> ShareConfig {
    let mut c = ShareConfig::new(dir);
    c.iroh = IrohConfig {
        relay: RelayChoice::Disabled,
        local_discovery: false,
        dns_discovery: false,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        presence_interval: Duration::from_secs(1),
        presence_max_backoff: Duration::from_secs(2),
        request_timeout: Duration::from_secs(3),
        ..IrohConfig::default()
    };
    c.relay = relay;
    c.direct_bind = Some("127.0.0.1:0".parse().unwrap());
    c.connect_timeout = Duration::from_secs(2);
    c.peer_wait = Duration::from_secs(15);
    c.files.offer_timeout = Duration::from_secs(15);
    c
}

struct Pair {
    a: ShareNode,
    b: ShareNode,
    _da: tempfile::TempDir,
    db: tempfile::TempDir,
}

async fn pair(
    seed_a: u8,
    seed_b: u8,
    mutual: bool,
    tweak_a: impl FnOnce(&mut ShareConfig),
    tweak_b: impl FnOnce(&mut ShareConfig),
) -> Pair {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    pair_inner(seed_a, seed_b, mutual, tweak_a, tweak_b).await
}

async fn pair_inner(
    seed_a: u8,
    seed_b: u8,
    mutual: bool,
    tweak_a: impl FnOnce(&mut ShareConfig),
    tweak_b: impl FnOnce(&mut ShareConfig),
) -> Pair {
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut ca = cfg(da.path(), None);
    tweak_a(&mut ca);
    let mut cb = cfg(db.path(), None);
    tweak_b(&mut cb);
    let a = ShareNode::start(&[seed_a; 32], ca).await.unwrap();
    let b = ShareNode::start(&[seed_b; 32], cb).await.unwrap();
    a.add_contact(&b.contact_string("bob")).await.unwrap();
    if mutual {
        b.add_contact(&a.contact_string("alice")).await.unwrap();
    }
    Pair { a, b, _da: da, db }
}

fn random_file(dir: &Path, name: &str, len: usize) -> std::path::PathBuf {
    use rand::RngCore;
    let mut data = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut data);
    let p = dir.join(name);
    std::fs::write(&p, &data).unwrap();
    p
}

async fn wait_for<T>(
    rx: &mut Receiver<ShareEvent>,
    mut f: impl FnMut(&ShareEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(T, async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Some(t) = f(&ev) {
                        return t;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("event stream closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn incoming_offer(ev: &ShareEvent) -> Option<TransferInfo> {
    match ev {
        ShareEvent::TransferOffered { transfer } if transfer.direction == Direction::Incoming => {
            Some(transfer.clone())
        }
        _ => None,
    }
}

// ── tests ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_transfer_with_consent() {
    let p = pair(1, 2, true, |_| {}, |_| {}).await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = random_file(src_dir.path(), "photo.bin", 3 * 1024 * 1024 + 123);
    let mut ev_a = p.a.events();
    let mut ev_b = p.b.events();

    let h = p.a.send_file(p.b.id(), &src).await.unwrap();
    let offer = wait_for(&mut ev_b, incoming_offer).await;
    assert_eq!(offer.name, "photo.bin");
    assert_eq!(offer.size, 3 * 1024 * 1024 + 123);
    assert_eq!(offer.contact, p.a.id());
    assert_eq!(offer.contact_name, "alice");
    assert_eq!(offer.relayed, Some(false), "direct path expected");
    assert_eq!(offer.state, TransferState::AwaitingDecision);
    let snapshot = p.b.transfers();
    assert_eq!(snapshot.len(), 1);
    p.b.accept(offer.id).await.unwrap();

    assert_eq!(h.clone().wait().await.unwrap(), src);
    let done = wait_for(&mut ev_b, |e| match e {
        ShareEvent::TransferCompleted { transfer } => Some(transfer.clone()),
        _ => None,
    })
    .await;
    let out = done.path.clone().unwrap();
    assert!(out.starts_with(p.db.path().join("downloads")));
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&src).unwrap());

    // Sender saw progress and completion with the same id.
    let sender_done = wait_for(&mut ev_a, |e| match e {
        ShareEvent::TransferCompleted { transfer } if transfer.id == h.id() => {
            Some(transfer.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(sender_done.direction, Direction::Outgoing);
    assert_eq!(
        sender_done.sas, done.sas,
        "both ends see the same verify code"
    );

    // Event JSON shape for the UI.
    let js = serde_json::to_value(ShareEvent::TransferCompleted { transfer: done }).unwrap();
    assert_eq!(js["type"], "transfer_completed");
    assert_eq!(js["transfer"]["direction"], "incoming");
    assert!(js["transfer"]["id"].is_string());

    assert_eq!(p.b.remove_finished(), 1);
    assert!(p.b.transfers().is_empty());
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn falls_back_to_relay_when_direct_unusable() {
    let relay = start_relay().await;
    let p = pair(
        3,
        4,
        true,
        |c| {
            c.relay = Some(relay.addr);
            c.test_open_relay = true; // in-process relay has no tokens
            // Advertise a dead candidate only.
            c.test_direct_override = Some(vec!["127.0.0.1:9".parse().unwrap()]);
        },
        |c| c.auto_accept = true,
    )
    .await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = random_file(src_dir.path(), "doc.pdf", 1024 * 1024 + 7);
    let mut ev_b = p.b.events();

    let h = p.a.send_file(p.b.id(), &src).await.unwrap();
    let offer = wait_for(&mut ev_b, incoming_offer).await;
    assert_eq!(offer.relayed, Some(true), "relay fallback expected");
    h.wait().await.unwrap();
    let done = wait_for(&mut ev_b, |e| match e {
        ShareEvent::TransferCompleted { transfer } => Some(transfer.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        std::fs::read(done.path.unwrap()).unwrap(),
        std::fs::read(&src).unwrap()
    );
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invite_from_non_contact_rejected() {
    // B does not have A as a contact.
    let p = pair(5, 6, false, |_| {}, |c| c.auto_accept = true).await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = random_file(src_dir.path(), "x.bin", 1000);
    let mut ev_b = p.b.events();

    let h = p.a.send_file(p.b.id(), &src).await.unwrap();
    let err = h.wait().await.unwrap_err();
    assert!(
        matches!(err, ShareError::Failed(ref e) if e.contains("invite not delivered")),
        "{err}"
    );
    let aid = p.a.id();
    wait_for(&mut ev_b, |e| match e {
        ShareEvent::ContactRequest { id, .. } if *id == aid => Some(()),
        ShareEvent::TransferOffered { .. } => panic!("offer from non-contact surfaced"),
        _ => None,
    })
    .await;
    assert!(p.b.transfers().is_empty());
    assert_eq!(p.a.transfers()[0].state, TransferState::Failed);
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receiver_rejects() {
    let p = pair(7, 8, true, |_| {}, |_| {}).await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = random_file(src_dir.path(), "nope.bin", 50_000);
    let mut ev_a = p.a.events();
    let mut ev_b = p.b.events();

    let h = p.a.send_file(p.b.id(), &src).await.unwrap();
    let offer = wait_for(&mut ev_b, incoming_offer).await;
    p.b.reject(offer.id, "not now").await.unwrap();
    assert!(matches!(
        p.b.accept(offer.id).await,
        Err(ShareError::NotPending(_))
    ));

    let err = h.clone().wait().await.unwrap_err();
    assert!(
        matches!(err, ShareError::Rejected(ref r) if r.contains("not now")),
        "{err}"
    );
    wait_for(&mut ev_a, |e| match e {
        ShareEvent::TransferRejected { transfer } if transfer.id == h.id() => Some(()),
        _ => None,
    })
    .await;
    assert!(
        std::fs::read_dir(p.db.path().join("downloads"))
            .unwrap()
            .next()
            .is_none()
    );
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fingerprint_mismatch_refused() {
    let p = pair(9, 10, true, |_| {}, |c| c.auto_accept = true).await;
    let mut ev_b = p.b.events();
    // A sends an invite claiming someone else's identity.
    let invite = FileSessionInvite::new(None, vec!["127.0.0.1:9".parse().unwrap()], &[99u8; 32]);
    let payload = ShareInvite {
        invite,
        share_id: ShareId::random(),
    };
    p.a.iroh()
        .send(
            p.b.id(),
            Envelope::FileInvite(serde_json::to_value(&payload).unwrap()),
        )
        .await
        .unwrap();
    let aid = p.a.id();
    let reason = wait_for(&mut ev_b, |e| match e {
        ShareEvent::InviteRefused { from, reason } if *from == aid => Some(reason.clone()),
        ShareEvent::TransferOffered { .. } => panic!("mismatched invite accepted"),
        _ => None,
    })
    .await;
    assert!(reason.contains("fingerprint mismatch"), "{reason}");
    // Garbage payload is refused too.
    p.a.iroh()
        .send(
            p.b.id(),
            Envelope::FileInvite(serde_json::json!({"hello": 1})),
        )
        .await
        .unwrap();
    wait_for(&mut ev_b, |e| match e {
        ShareEvent::InviteRefused { reason, .. } if reason.contains("bad invite") => Some(()),
        _ => None,
    })
    .await;
    assert!(p.b.transfers().is_empty());
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_pending_offer() {
    let p = pair(11, 12, true, |_| {}, |_| {}).await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = random_file(src_dir.path(), "c.bin", 10_000);
    let mut ev_b = p.b.events();
    let h = p.a.send_file(p.b.id(), &src).await.unwrap();
    let offer = wait_for(&mut ev_b, incoming_offer).await;
    p.a.cancel(h.id()).await.unwrap();
    let r = h.wait().await;
    assert!(matches!(r, Err(ShareError::Cancelled)), "{r:?}");
    // Receiver's offer ends (cancelled or failed), never completes.
    let ev = wait_for(&mut ev_b, |e| match e {
        ShareEvent::TransferCancelled { transfer, .. }
        | ShareEvent::TransferFailed { transfer }
            if transfer.id == offer.id =>
        {
            Some(e.clone())
        }
        ShareEvent::TransferCompleted { .. } => panic!("cancelled transfer completed"),
        _ => None,
    })
    .await;
    let _ = ev;
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_authorized_relay_fails_clearly() {
    let relay = start_relay().await;
    // Relay configured but no token -> relay fallback unavailable.
    let p = pair(
        13,
        14,
        true,
        |c| {
            c.relay = Some(relay.addr);
            c.test_direct_override = Some(vec!["127.0.0.1:9".parse().unwrap()]);
        },
        |c| c.auto_accept = true,
    )
    .await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = random_file(src_dir.path(), "y.bin", 1000);
    let mut ev_b = p.b.events();
    let h = p.a.send_file(p.b.id(), &src).await.unwrap();
    // Receiver gives up and tells the sender over iroh.
    let err = h.wait().await.unwrap_err();
    assert!(
        matches!(err, ShareError::Failed(ref e) if e.contains("no relay configured/authorized")),
        "{err}"
    );
    let t = wait_for(&mut ev_b, |e| match e {
        ShareEvent::TransferFailed { transfer } => Some(transfer.clone()),
        _ => None,
    })
    .await;
    assert!(t.error.unwrap().contains("no relay configured/authorized"));

    // With neither direct candidates nor an authorized relay, send_file refuses up front.
    let mut c = cfg(src_dir.path(), Some(relay.addr));
    c.direct_bind = None;
    assert!(crate::node::plan_relay(&c).is_none());
    p.a.shutdown().await;
    p.b.shutdown().await;
}

#[test]
fn relay_token_never_in_invite_or_debug() {
    let mut c = ShareConfig::new("/tmp/x");
    c.relay = Some("203.0.113.1:4433".parse().unwrap());
    c.relay_token = Some("SUPER-SECRET-TOKEN".into());
    c.iroh.relay_auth_token = Some("IROH-SECRET".into());
    let plan = crate::node::plan_relay(&c).expect("relay usable with token");
    let invite = crate::node::build_invite(vec![], Some(plan), &[1u8; 32]);
    let payload = ShareInvite {
        invite: invite.clone(),
        share_id: ShareId::random(),
    };
    let js = serde_json::to_string(&payload).unwrap();
    assert!(!js.contains("SUPER-SECRET-TOKEN"), "{js}");
    assert!(invite.relay.as_ref().unwrap().room.starts_with("file-"));
    let dbg = format!("{c:?}");
    assert!(
        !dbg.contains("SUPER-SECRET-TOKEN") && !dbg.contains("IROH-SECRET"),
        "{dbg}"
    );
    // Without a token (and without the test hook) the relay is not used.
    c.relay_token = None;
    assert!(crate::node::plan_relay(&c).is_none());
}
