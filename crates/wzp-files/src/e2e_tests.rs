//! End-to-end tests: real wzp-relay room + splicer code in-process, and
//! direct QUIC sessions. Lives in the lib (not tests/) so CI's
//! `cargo test --lib` runs it.

#![allow(dead_code)]

mod common {

    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use rand::RngCore;
    use wzp_relay::file_relay::{self, StreamRelayLimits};
    use wzp_relay::metrics::RelayMetrics;
    use wzp_relay::room::{self, RoomManager};

    pub fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
    }

    pub struct TestRelay {
        pub addr: SocketAddr,
        pub rooms: Arc<RoomManager>,
        _task: tokio::task::JoinHandle<()>,
    }

    pub async fn start_relay() -> TestRelay {
        start_relay_with(StreamRelayLimits::default()).await
    }

    pub async fn start_relay_with(limits: StreamRelayLimits) -> TestRelay {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let seed = [0x42u8; 32];
        let (sc, _) = wzp_transport::server_config_from_seed(&seed);
        let ep = wzp_transport::create_endpoint("127.0.0.1:0".parse().unwrap(), Some(sc)).unwrap();
        let addr = ep.local_addr().unwrap();
        let rooms = Arc::new(RoomManager::new());
        let metrics = Arc::new(RelayMetrics::new());
        let rm = rooms.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let rm = rm.clone();
                let metrics = metrics.clone();
                let limits = limits.clone();
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
                    let (pid, update, senders, _) = match rm.join(
                        &room_name,
                        addr,
                        room::ParticipantSender::Quic(transport.clone()),
                        Some(&fp),
                        alias.as_deref(),
                    ) {
                        Ok(v) => v,
                        Err(_) => return,
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
                        limits,
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
        TestRelay {
            addr,
            rooms,
            _task: task,
        }
    }

    pub fn random_file(path: &Path, len: usize) -> Vec<u8> {
        let mut data = vec![0u8; len];
        rand::thread_rng().fill_bytes(&mut data);
        std::fs::write(path, &data).unwrap();
        data
    }

    pub fn seed(b: u8) -> [u8; 32] {
        [b; 32]
    }

    pub async fn timeout<T>(secs: u64, f: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(secs), f)
            .await
            .expect("test step timed out")
    }
}

mod transfer {
    use std::path::Path;
    use std::time::Duration;

    use super::common::*;
    use crate::*;

    fn cfg(dir: &Path) -> FileConfig {
        FileConfig {
            download_dir: dir.to_path_buf(),
            chunk_size: 64 * 1024,
            ..Default::default()
        }
    }

    async fn relay_pair(
        relay: std::net::SocketAddr,
        a: [u8; 32],
        b: [u8; 32],
    ) -> (Session, Session) {
        let room = random_room();
        let opts_a = ConnectOptions {
            expected_peer: Some(fingerprint_of_seed(&b)),
            ..Default::default()
        };
        let opts_b = ConnectOptions {
            expected_peer: Some(fingerprint_of_seed(&a)),
            ..Default::default()
        };
        let (sa, sb) = timeout(30, async {
            tokio::join!(
                connect_via_relay_with(relay, &room, &a, &opts_a),
                connect_via_relay_with(relay, &room, &b, &opts_b),
            )
        })
        .await;
        (sa.unwrap(), sb.unwrap())
    }

    /// Accept every offer on `ep` and return the first completed path.
    async fn accept_one(ep: &FileEndpoint) -> (TransferId, std::path::PathBuf) {
        let offer = timeout(30, ep.next_offer()).await.expect("offer");
        let id = offer.transfer_id;
        offer.accept();
        let p = timeout(60, ep.wait(id)).await.expect("receive ok");
        (id, p)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_loopback_transfer_and_collision_safe_name() {
        init_tracing();
        let relay = start_relay().await;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data = random_file(&src.path().join("payload.bin"), 3 * 1024 * 1024 + 12345);
        std::fs::write(dst.path().join("payload.bin"), b"pre-existing").unwrap();

        let (sa, sb) = relay_pair(relay.addr, seed(1), seed(2)).await;
        assert!(sa.is_relayed());
        assert_eq!(sa.sas(), sb.sas());
        assert_eq!(sa.peer_fingerprint(), fingerprint_of_seed(&seed(2)));
        let a = FileEndpoint::new(sa, cfg(src.path()));
        let b = FileEndpoint::new(sb, cfg(dst.path()));
        let mut ev = a.events();

        let id = a.send(&src.path().join("payload.bin")).await.unwrap();
        let ((rid, path), sent) = tokio::join!(accept_one(&b), timeout(60, a.wait(id)));
        assert_eq!(rid, id);
        sent.unwrap();
        assert_eq!(path.file_name().unwrap(), "payload (1).bin");
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(
            std::fs::read(dst.path().join("payload.bin")).unwrap(),
            b"pre-existing"
        );
        // no leftovers
        let names: Vec<_> = std::fs::read_dir(dst.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.contains("wzpart")), "{names:?}");
        // sender saw progress and completion
        let mut saw_started = false;
        let mut saw_completed = false;
        while let Ok(e) = ev.try_recv() {
            match e {
                Progress::Started { .. } => saw_started = true,
                Progress::Completed { .. } => saw_completed = true,
                _ => {}
            }
        }
        assert!(saw_started && saw_completed);

        // Second transfer in the other direction on the same session.
        let back = random_file(&dst.path().join("reply.txt"), 1000);
        let id2 = b.send(&dst.path().join("reply.txt")).await.unwrap();
        let ((_, p2), r2) = tokio::join!(accept_one(&a), timeout(30, b.wait(id2)));
        r2.unwrap();
        assert_eq!(std::fs::read(p2).unwrap(), back);
        a.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_transfer_and_fingerprint_enforced() {
        init_tracing();
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data = random_file(&src.path().join("d.bin"), 2 * 1024 * 1024);
        let empty = random_file(&src.path().join("empty"), 0);

        let listener = listen_direct("127.0.0.1:0".parse().unwrap(), &seed(3))
            .await
            .unwrap();
        let laddr = listener.local_addr().unwrap();

        // Wrong expected fingerprint is refused by the connecting side.
        let bad = ConnectOptions {
            expected_peer: Some(fingerprint_of_seed(&seed(9))),
            ..Default::default()
        };
        let s4 = seed(4);
        let dflt = ConnectOptions::default();
        let (r_bad, _l) = tokio::join!(connect_direct(laddr, &s4, &bad), listener.accept(&dflt));
        assert!(matches!(r_bad, Err(FileError::Auth(_))), "{r_bad:?}");

        let good = ConnectOptions {
            expected_peer: Some(fingerprint_of_seed(&seed(3))),
            ..Default::default()
        };
        let (sa, sb) = tokio::join!(connect_direct(laddr, &s4, &good), listener.accept(&dflt));
        let (sa, sb) = (sa.unwrap(), sb.unwrap());
        assert!(!sa.is_relayed());
        let a = FileEndpoint::new(sa, cfg(src.path()));
        let b = FileEndpoint::new(sb, cfg(dst.path()));
        let id = a.send(&src.path().join("d.bin")).await.unwrap();
        let ((_, p), r) = tokio::join!(accept_one(&b), timeout(60, a.wait(id)));
        r.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), data);

        // zero-byte file
        let id = a.send(&src.path().join("empty")).await.unwrap();
        let ((_, p), r) = tokio::join!(accept_one(&b), timeout(30, a.wait(id)));
        r.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), empty);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn corrupted_chunk_is_nacked_and_recovered() {
        init_tracing();
        let relay = start_relay().await;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let data = random_file(&src.path().join("c.bin"), 1024 * 1024);

        let (sa, sb) = relay_pair(relay.addr, seed(5), seed(6)).await;
        let mut ca = cfg(src.path());
        ca.test_corrupt_chunks = vec![0, 3, 15];
        let a = FileEndpoint::new(sa, ca);
        let b = FileEndpoint::new(sb, cfg(dst.path()));
        let id = a.send(&src.path().join("c.bin")).await.unwrap();
        let ((_, p), r) = tokio::join!(accept_one(&b), timeout(60, a.wait(id)));
        r.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), data);

        // With zero retries allowed the same corruption fails the transfer,
        // proving the corrupt chunks were detected (not silently accepted).
        let mut ca = cfg(src.path());
        ca.test_corrupt_chunks = vec![2];
        let mut cb = cfg(dst.path());
        cb.max_chunk_retries = 0;
        let (sa, sb) = relay_pair(relay.addr, seed(5), seed(6)).await;
        let a = FileEndpoint::new(sa, ca);
        let b = FileEndpoint::new(sb, cb);
        let id = a.send(&src.path().join("c.bin")).await.unwrap();
        let offer = timeout(30, b.next_offer()).await.unwrap();
        offer.accept();
        let rb = timeout(30, b.wait(id)).await;
        assert!(matches!(rb, Err(FileError::Failed(_))), "{rb:?}");
        let ra = timeout(30, a.wait(id)).await;
        assert!(ra.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resume_after_interruption() {
        init_tracing();
        let relay = start_relay().await;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let total = 4 * 1024 * 1024;
        let data = random_file(&src.path().join("big.bin"), total);

        // First attempt: slow sender, cut the session at ~40%.
        let (sa, sb) = relay_pair(relay.addr, seed(7), seed(8)).await;
        let mut ca = cfg(src.path());
        ca.max_send_rate = Some(2 * 1024 * 1024);
        let a = FileEndpoint::new(sa, ca);
        let b = FileEndpoint::new(sb, cfg(dst.path()));
        let mut evb = b.events();
        let id = a.send(&src.path().join("big.bin")).await.unwrap();
        let offer = timeout(30, b.next_offer()).await.unwrap();
        offer.accept();
        timeout(30, async {
            loop {
                if let Ok(Progress::Bytes { done, .. }) = evb.recv().await {
                    if done >= total as u64 * 4 / 10 {
                        break;
                    }
                }
            }
        })
        .await;
        a.close().await;
        drop(a);
        let rb = timeout(30, b.wait(id)).await;
        assert!(matches!(rb, Err(FileError::Failed(_))), "{rb:?}");
        assert!(dst.path().join("big.bin.wzpart").exists());
        assert!(dst.path().join("big.bin.wzpart.state").exists());
        drop(b);

        // Second attempt on a new session resumes.
        let (sa, sb) = relay_pair(relay.addr, seed(7), seed(8)).await;
        let a = FileEndpoint::new(sa, cfg(src.path()));
        let b = FileEndpoint::new(sb, cfg(dst.path()));
        let mut eva = a.events();
        let id = a.send(&src.path().join("big.bin")).await.unwrap();
        let ((_, p), r) = tokio::join!(accept_one(&b), timeout(60, a.wait(id)));
        r.unwrap();
        assert_eq!(p.file_name().unwrap(), "big.bin");
        assert_eq!(std::fs::read(&p).unwrap(), data);
        let mut resumed = None;
        while let Ok(e) = eva.try_recv() {
            if let Progress::Started { resumed_bytes, .. } = e {
                resumed = Some(resumed_bytes);
            }
        }
        let resumed = resumed.expect("started event");
        assert!(resumed >= total as u64 / 4, "resumed only {resumed} bytes");
        assert!(!dst.path().join("big.bin.wzpart").exists());
        assert!(!dst.path().join("big.bin.wzpart.state").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_too_large_unanswered_and_declined_offers() {
        init_tracing();
        let relay = start_relay().await;
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        random_file(&src.path().join("two-mb.bin"), 2 * 1024 * 1024);

        // Sender-side limit.
        let (sa, sb) = relay_pair(relay.addr, seed(10), seed(11)).await;
        let mut small = cfg(src.path());
        small.max_size = 1024 * 1024;
        let a = FileEndpoint::new(sa, small);
        let r = a.send(&src.path().join("two-mb.bin")).await;
        assert!(matches!(r, Err(FileError::TooLarge { .. })), "{r:?}");
        drop(a);
        drop(sb);

        // Receiver-side limit: rejected automatically, no prompt.
        let (sa, sb) = relay_pair(relay.addr, seed(10), seed(11)).await;
        let a = FileEndpoint::new(sa, cfg(src.path()));
        let mut cb = cfg(dst.path());
        cb.max_size = 1024 * 1024;
        cb.offer_timeout = Duration::from_secs(1);
        let b = FileEndpoint::new(sb, cb);
        let id = a.send(&src.path().join("two-mb.bin")).await.unwrap();
        let r = timeout(10, a.wait(id)).await;
        match r {
            Err(FileError::Rejected(reason)) => assert!(reason.contains("too large"), "{reason}"),
            other => panic!("{other:?}"),
        }

        // Nobody answers: rejected after offer_timeout.
        random_file(&src.path().join("small.bin"), 1000);
        let id = a.send(&src.path().join("small.bin")).await.unwrap();
        let r = timeout(10, a.wait(id)).await;
        assert!(matches!(r, Err(FileError::Rejected(_))), "{r:?}");

        // Explicit decline.
        let id = a.send(&src.path().join("small.bin")).await.unwrap();
        // drain the stale unanswered offer first
        loop {
            let o = timeout(10, b.next_offer()).await.unwrap();
            if o.transfer_id == id {
                o.reject("not now");
                break;
            }
        }
        let r = timeout(10, a.wait(id)).await;
        match r {
            Err(FileError::Rejected(reason)) => assert_eq!(reason, "not now"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_dir(dst.path()).unwrap().count(),
            0,
            "nothing written"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invite_roundtrip_and_join_via_relay() {
        init_tracing();
        let relay = start_relay().await;
        let inviter = seed(20);
        let joiner = seed(21);
        let invite = FileSessionInvite::new(Some(relay.addr), vec![], &inviter);
        let json = invite.to_json();
        let parsed = FileSessionInvite::from_json(&json).unwrap();
        assert_eq!(parsed, invite);
        let host_opts = ConnectOptions {
            expected_peer: Some(fingerprint_of_seed(&joiner)),
            ..Default::default()
        };
        let dflt = ConnectOptions::default();
        let (h, j) = timeout(30, async {
            tokio::join!(
                host_invite(&invite, None, &inviter, &host_opts),
                join_invite(&parsed, &joiner, &dflt)
            )
        })
        .await;
        let (h, j) = (h.unwrap(), j.unwrap());
        assert_eq!(h.peer_fingerprint(), fingerprint_of_seed(&joiner));
        assert_eq!(j.peer_fingerprint(), fingerprint_of_seed(&inviter));
        // A third party with the token cannot join the full two-party room.
        let third = tokio::time::timeout(
            Duration::from_secs(3),
            connect_via_relay_with(
                relay.addr,
                &invite.relay.as_ref().unwrap().room,
                &seed(22),
                &ConnectOptions {
                    peer_wait: Duration::from_secs(2),
                    ..Default::default()
                },
            ),
        )
        .await;
        assert!(!matches!(third, Ok(Ok(_))));
    }
}
