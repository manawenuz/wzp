//! In-process relay built from the real wzp-relay room/splicer code.

#![allow(dead_code)]

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
