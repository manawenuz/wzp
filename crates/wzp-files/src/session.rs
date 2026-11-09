//! Establishing an end-to-end authenticated wzp-files session over a WZP
//! QUIC connection — either through a relay room or directly peer-to-peer.
//!
//! Whatever carries the QUIC connection, the two file peers run their own
//! signed X25519 handshake ([`crate::wire::Hello`]) on a dedicated control
//! stream, so a relay only ever sees ciphertext.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wzp_crypto::{Fingerprint, KeyExchange, WarzoneKeyExchange};
use wzp_proto::{MediaTransport, QualityProfile, SignalMessage, default_signal_version};
use wzp_transport::QuinnTransport;

use crate::wire::{self, Hello, OpenReader, SealWriter};
use crate::{FileError, Result};

/// Room-name prefix used for relay file sessions. The relay limits such
/// rooms to two participants and splices QUIC streams between them.
pub const FILE_ROOM_PREFIX: &str = "file-";
/// Signing context used for direct (relay-less) sessions.
pub const DIRECT_CONTEXT: &str = "wzp-files-direct";

/// Options for establishing a session.
#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Abort unless the peer proves this identity.
    pub expected_peer: Option<Fingerprint>,
    /// Relay auth token (`--auth-url` relays expect it before the handshake).
    pub auth_token: Option<String>,
    /// Display alias sent to the relay.
    pub alias: Option<String>,
    /// How long to wait for the peer to show up in the relay room.
    pub peer_wait: Duration,
    /// Timeout for the QUIC connect and each handshake step.
    pub handshake_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            expected_peer: None,
            auth_token: None,
            alias: None,
            peer_wait: Duration::from_secs(300),
            handshake_timeout: Duration::from_secs(20),
        }
    }
}

/// What the sender hands the receiver (e.g. over iroh) so it can join.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSessionInvite {
    #[serde(default = "default_signal_version")]
    pub version: u8,
    /// Relay rendezvous, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayRendezvous>,
    /// Direct candidates of the inviter (try these first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub direct: Vec<SocketAddr>,
    /// Fingerprint (hex, colons optional) the joiner must see on the other
    /// end, i.e. the inviter's identity.
    pub peer_fingerprint: String,
}

/// Relay half of a [`FileSessionInvite`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayRendezvous {
    pub addr: SocketAddr,
    /// Room name (`file-<random>`); acts as the rendezvous token.
    pub room: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

impl FileSessionInvite {
    /// New invite for `inviter_seed`'s identity with a fresh random relay room.
    pub fn new(
        relay: Option<SocketAddr>,
        direct: Vec<SocketAddr>,
        inviter_seed: &[u8; 32],
    ) -> Self {
        Self {
            version: 1,
            relay: relay.map(|addr| RelayRendezvous {
                addr,
                room: random_room(),
                auth_token: None,
            }),
            direct,
            peer_fingerprint: fingerprint_of_seed(inviter_seed).to_hex(),
        }
    }

    pub fn expected_peer(&self) -> Result<Fingerprint> {
        Fingerprint::from_hex(&self.peer_fingerprint).map_err(FileError::Invalid)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("invite serializes")
    }

    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| FileError::Invalid(format!("invite: {e}")))
    }
}

/// `file-` + 32 random hex chars.
pub fn random_room() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    format!("{FILE_ROOM_PREFIX}{}", hex::encode(b))
}

/// Fingerprint of the identity derived from `seed`.
pub fn fingerprint_of_seed(seed: &[u8; 32]) -> Fingerprint {
    let kx = WarzoneKeyExchange::from_identity_seed(seed);
    Fingerprint(kx.fingerprint())
}

/// Map a token or room to the relay room name: names that already carry a
/// two-party prefix (`file-`, `call-`) are used as-is, anything else gets
/// `file-` prepended.
pub fn relay_room_name(room_or_token: &str) -> String {
    if room_or_token.starts_with(FILE_ROOM_PREFIX) || room_or_token.starts_with("call-") {
        room_or_token.to_string()
    } else {
        format!("{FILE_ROOM_PREFIX}{room_or_token}")
    }
}

/// Incoming data stream after its preamble was consumed.
pub(crate) struct IncomingData {
    pub stream: quinn::RecvStream,
    pub salt: [u8; 16],
}

/// An end-to-end authenticated wzp-files session.
pub struct Session {
    pub(crate) conn: quinn::Connection,
    endpoint: Option<quinn::Endpoint>,
    pub(crate) send_key: [u8; 32],
    pub(crate) recv_key: [u8; 32],
    pub(crate) ctl_tx: Option<SealWriter>,
    pub(crate) ctl_rx: Option<OpenReader>,
    pub(crate) data_rx: Option<mpsc::Receiver<IncomingData>>,
    peer_identity: [u8; 32],
    sas: u32,
    relayed: bool,
    peer_present: Option<watch::Receiver<bool>>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("peer", &self.peer_fingerprint())
            .field("relayed", &self.relayed)
            .finish()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Session {
    /// Run the wzp-files handshake over an already established WZP
    /// connection (e.g. the QUIC connection of an ongoing call).
    ///
    /// `context` must be identical on both ends (use the relay room name, or
    /// [`DIRECT_CONTEXT`]); it is bound into the handshake signatures.
    /// The caller keeps owning the transport's bidirectional signal streams;
    /// wzp-files only uses unidirectional streams.
    pub async fn over_transport(
        transport: &QuinnTransport,
        seed: &[u8; 32],
        context: &str,
        relayed: bool,
        opts: &ConnectOptions,
    ) -> Result<Session> {
        handshake(
            transport.connection().clone(),
            None,
            seed,
            context,
            relayed,
            opts,
            None,
            Vec::new(),
        )
        .await
    }

    pub fn peer_fingerprint(&self) -> Fingerprint {
        Fingerprint(wire::fingerprint_of(&self.peer_identity))
    }

    pub fn peer_identity_pub(&self) -> [u8; 32] {
        self.peer_identity
    }

    /// 4-digit short authentication string; equal on both ends unless
    /// there is a man in the middle.
    pub fn sas(&self) -> u32 {
        self.sas
    }

    /// True when the session runs through a relay.
    pub fn is_relayed(&self) -> bool {
        self.relayed
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Resolves when the peer is gone (connection closed, or for relayed
    /// sessions the peer left the room).
    pub(crate) fn peer_present(&self) -> Option<watch::Receiver<bool>> {
        self.peer_present.clone()
    }

    /// Close the QUIC connection and flush the close frame.
    pub async fn close(&self) {
        self.conn.close(quinn::VarInt::from_u32(0), b"done");
        if let Some(ep) = &self.endpoint {
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
        }
    }
}

/// QUIC transport parameters for bulk transfer (bigger flow-control
/// windows than the VoIP defaults, more concurrent uni streams).
pub fn bulk_transport_config() -> quinn::TransportConfig {
    let mut c = quinn::TransportConfig::default();
    c.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).unwrap(),
    ));
    c.keep_alive_interval(Some(Duration::from_secs(5)));
    c.receive_window(quinn::VarInt::from_u32(32 * 1024 * 1024));
    c.stream_receive_window(quinn::VarInt::from_u32(8 * 1024 * 1024));
    c.send_window(32 * 1024 * 1024);
    c.max_concurrent_uni_streams(quinn::VarInt::from_u32(64));
    c.datagram_receive_buffer_size(Some(1024 * 1024));
    c.initial_rtt(Duration::from_millis(100));
    c
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client_config() -> quinn::ClientConfig {
    install_crypto_provider();
    let mut cfg = wzp_transport::client_config();
    cfg.transport_config(Arc::new(bulk_transport_config()));
    cfg
}

fn client_endpoint(remote: SocketAddr) -> Result<quinn::Endpoint> {
    let bind: SocketAddr = if remote.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    Ok(wzp_transport::create_endpoint(bind, None)?)
}

/// Join relay room `room_or_token` and run the E2E handshake with whoever
/// else joins it.
pub async fn connect_via_relay(
    relay: SocketAddr,
    room_or_token: &str,
    seed: &[u8; 32],
) -> Result<Session> {
    connect_via_relay_with(relay, room_or_token, seed, &ConnectOptions::default()).await
}

/// [`connect_via_relay`] with explicit options (expected peer, auth token, ...).
pub async fn connect_via_relay_with(
    relay: SocketAddr,
    room_or_token: &str,
    seed: &[u8; 32],
    opts: &ConnectOptions,
) -> Result<Session> {
    let room = relay_room_name(room_or_token);
    let endpoint = client_endpoint(relay)?;
    let conn = tokio::time::timeout(
        opts.handshake_timeout,
        wzp_transport::connect(&endpoint, relay, &room, client_config()),
    )
    .await
    .map_err(|_| FileError::Timeout("relay connect"))??;
    let transport = Arc::new(QuinnTransport::new(conn.clone()));

    if let Some(token) = &opts.auth_token {
        transport
            .send_signal(&SignalMessage::AuthToken {
                version: default_signal_version(),
                token: token.clone(),
            })
            .await?;
    }
    tokio::time::timeout(
        opts.handshake_timeout,
        relay_handshake(&*transport, seed, opts.alias.as_deref()),
    )
    .await
    .map_err(|_| FileError::Timeout("relay handshake"))??;
    info!(%relay, room = %room, "joined relay room, waiting for peer");

    // Watch RoomUpdate to learn when the peer is present / gone. Draining
    // the relay's signal streams also keeps its open_bi() from blocking.
    let (count_tx, mut count_rx) = watch::channel(0u32);
    let t2 = transport.clone();
    let watcher = tokio::spawn(async move {
        loop {
            match t2.recv_signal().await {
                Ok(Some(SignalMessage::RoomUpdate { count, .. })) => {
                    let _ = count_tx.send(count);
                }
                Ok(Some(other)) => {
                    debug!("relay signal ignored: {:?}", std::mem::discriminant(&other))
                }
                Ok(None) => break,
                Err(wzp_proto::TransportError::Deserialize(e)) => debug!("relay signal: {e}"),
                Err(_) => break,
            }
        }
        let _ = count_tx.send(0);
    });

    let waited = tokio::time::timeout(opts.peer_wait, async {
        loop {
            if *count_rx.borrow_and_update() >= 2 {
                return true;
            }
            if count_rx.changed().await.is_err() {
                return false;
            }
        }
    })
    .await;
    match waited {
        Ok(true) => {}
        Ok(false) => {
            watcher.abort();
            return Err(FileError::Transport(
                "relay closed while waiting for peer".into(),
            ));
        }
        Err(_) => {
            watcher.abort();
            conn.close(quinn::VarInt::from_u32(0), b"no peer");
            return Err(FileError::Timeout("waiting for peer in relay room"));
        }
    }

    // Peer presence for the endpoint: true while count >= 2.
    let (present_tx, present_rx) = watch::channel(true);
    let mut crx = count_rx.clone();
    let presence = tokio::spawn(async move {
        loop {
            if crx.changed().await.is_err() {
                let _ = present_tx.send(false);
                break;
            }
            let c = *crx.borrow();
            let _ = present_tx.send(c >= 2);
        }
    });

    handshake(
        conn,
        Some(endpoint),
        seed,
        &room,
        true,
        opts,
        Some(present_rx),
        vec![watcher, presence],
    )
    .await
}

/// Client side of the WZP relay handshake (CallOffer → CallAnswer). The
/// derived hop-by-hop key is discarded: wzp-files does its own E2E crypto.
async fn relay_handshake(
    transport: &dyn MediaTransport,
    seed: &[u8; 32],
    alias: Option<&str>,
) -> Result<()> {
    let mut kx = WarzoneKeyExchange::from_identity_seed(seed);
    let identity_pub = kx.identity_public_key();
    let ephemeral_pub = kx.generate_ephemeral();
    let mut sign_data = ephemeral_pub.to_vec();
    sign_data.extend_from_slice(b"call-offer");
    let signature = kx.sign(&sign_data);
    transport
        .send_signal(&SignalMessage::CallOffer {
            version: default_signal_version(),
            identity_pub,
            ephemeral_pub,
            signature,
            supported_profiles: vec![QualityProfile::GOOD],
            alias: alias.map(str::to_string),
            protocol_version: 2,
            supported_versions: vec![2],
            video_codecs: Vec::new(),
        })
        .await?;
    match transport.recv_signal().await? {
        Some(SignalMessage::CallAnswer {
            identity_pub,
            ephemeral_pub,
            signature,
            ..
        }) => {
            let mut v = ephemeral_pub.to_vec();
            v.extend_from_slice(b"call-answer");
            if !WarzoneKeyExchange::verify(&identity_pub, &v, &signature) {
                return Err(FileError::Auth("relay signature invalid".into()));
            }
            Ok(())
        }
        Some(SignalMessage::Hangup { reason, .. }) => {
            Err(FileError::Transport(format!("relay refused: {reason:?}")))
        }
        Some(_) => Err(FileError::Protocol("expected CallAnswer from relay".into())),
        None => Err(FileError::Transport("relay closed during handshake".into())),
    }
}

/// Listener for direct (relay-less) sessions.
pub struct DirectListener {
    endpoint: quinn::Endpoint,
    seed: [u8; 32],
}

/// Listen for direct sessions on `bind`. The QUIC certificate is derived
/// from `seed` like the relay's.
pub async fn listen_direct(bind: SocketAddr, seed: &[u8; 32]) -> Result<DirectListener> {
    install_crypto_provider();
    let (mut sc, _cert) = wzp_transport::server_config_from_seed(seed);
    sc.transport_config(Arc::new(bulk_transport_config()));
    let endpoint = wzp_transport::create_endpoint(bind, Some(sc))?;
    Ok(DirectListener {
        endpoint,
        seed: *seed,
    })
}

impl DirectListener {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Accept the next direct session.
    pub async fn accept(&self, opts: &ConnectOptions) -> Result<Session> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or_else(|| FileError::Transport("listener closed".into()))?;
        let conn = tokio::time::timeout(opts.handshake_timeout, incoming)
            .await
            .map_err(|_| FileError::Timeout("direct accept"))?
            .map_err(|e| FileError::Transport(format!("accept: {e}")))?;
        handshake(
            conn,
            Some(self.endpoint.clone()),
            &self.seed,
            DIRECT_CONTEXT,
            false,
            opts,
            None,
            Vec::new(),
        )
        .await
    }

    pub fn close(&self) {
        self.endpoint.close(quinn::VarInt::from_u32(0), b"bye");
    }
}

/// Connect directly to a peer's [`DirectListener`].
pub async fn connect_direct(
    addr: SocketAddr,
    seed: &[u8; 32],
    opts: &ConnectOptions,
) -> Result<Session> {
    let endpoint = client_endpoint(addr)?;
    let conn = tokio::time::timeout(
        opts.handshake_timeout,
        wzp_transport::connect(&endpoint, addr, "wzp-files", client_config()),
    )
    .await
    .map_err(|_| FileError::Timeout("direct connect"))??;
    handshake(
        conn,
        Some(endpoint),
        seed,
        DIRECT_CONTEXT,
        false,
        opts,
        None,
        Vec::new(),
    )
    .await
}

/// Joiner side of an invite: try each direct candidate (short timeout),
/// then fall back to the relay. The inviter's fingerprint is enforced.
pub async fn join_invite(
    invite: &FileSessionInvite,
    seed: &[u8; 32],
    opts: &ConnectOptions,
) -> Result<Session> {
    let mut opts = opts.clone();
    opts.expected_peer = Some(invite.expected_peer()?);
    let mut last_err = None;
    for addr in &invite.direct {
        let mut o = opts.clone();
        o.handshake_timeout = o.handshake_timeout.min(Duration::from_secs(5));
        match connect_direct(*addr, seed, &o).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                warn!(%addr, "direct candidate failed: {e}");
                last_err = Some(e);
            }
        }
    }
    if let Some(r) = &invite.relay {
        if opts.auth_token.is_none() {
            opts.auth_token = r.auth_token.clone();
        }
        return connect_via_relay_with(r.addr, &r.room, seed, &opts).await;
    }
    Err(last_err.unwrap_or_else(|| FileError::Invalid("invite has no candidates".into())))
}

/// Inviter side: accept a direct connection on `listener` (if any) and/or
/// join the invite's relay room, whichever completes first.
pub async fn host_invite(
    invite: &FileSessionInvite,
    listener: Option<&DirectListener>,
    seed: &[u8; 32],
    opts: &ConnectOptions,
) -> Result<Session> {
    let relay_fut = async {
        match &invite.relay {
            Some(r) => {
                let mut o = opts.clone();
                if o.auth_token.is_none() {
                    o.auth_token = r.auth_token.clone();
                }
                connect_via_relay_with(r.addr, &r.room, seed, &o).await
            }
            None => std::future::pending().await,
        }
    };
    let direct_fut = async {
        match listener {
            Some(l) => loop {
                match l.accept(opts).await {
                    Ok(s) => break Ok(s),
                    Err(FileError::Auth(e)) => warn!("direct: rejected peer: {e}"),
                    Err(e) => break Err(e),
                }
            },
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        r = relay_fut => r,
        d = direct_fut => d,
    }
}

/// Wire up the uni-stream dispatcher and run the Hello exchange.
#[allow(clippy::too_many_arguments)]
async fn handshake(
    conn: quinn::Connection,
    endpoint: Option<quinn::Endpoint>,
    seed: &[u8; 32],
    context: &str,
    relayed: bool,
    opts: &ConnectOptions,
    peer_present: Option<watch::Receiver<bool>>,
    mut tasks: Vec<JoinHandle<()>>,
) -> Result<Session> {
    let kx = WarzoneKeyExchange::from_identity_seed(seed);
    let eph_secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
    let eph_pub = x25519_dalek::PublicKey::from(&eph_secret).to_bytes();
    let ctx = wire::context_hash(context);
    let hello = Hello::new(&kx, eph_pub, &ctx);

    let (ctl_tx, mut ctl_rx) = mpsc::channel::<quinn::RecvStream>(1);
    let (data_tx, data_rx) = mpsc::channel::<IncomingData>(64);
    tasks.push(tokio::spawn(dispatch_uni(conn.clone(), ctl_tx, data_tx)));

    let fail = |tasks: &Vec<JoinHandle<()>>| {
        for t in tasks {
            t.abort();
        }
    };

    let mut send = match conn.open_uni().await {
        Ok(s) => s,
        Err(e) => {
            fail(&tasks);
            return Err(FileError::Transport(format!("open control stream: {e}")));
        }
    };
    let mut pre = Vec::with_capacity(5 + wire::HELLO_LEN);
    pre.extend_from_slice(&wire::MAGIC_CONTROL);
    pre.push(wire::WIRE_VERSION);
    pre.extend_from_slice(&hello.encode());
    if let Err(e) = send.write_all(&pre).await {
        fail(&tasks);
        return Err(FileError::Transport(format!("write hello: {e}")));
    }

    let peer = async {
        let mut recv = ctl_rx
            .recv()
            .await
            .ok_or_else(|| FileError::Transport("connection closed before hello".into()))?;
        let mut hb = [0u8; wire::HELLO_LEN];
        recv.read_exact(&mut hb)
            .await
            .map_err(|e| FileError::Transport(format!("read hello: {e}")))?;
        Ok::<_, FileError>((recv, Hello::decode(&hb)))
    };
    let (peer_recv, peer_hello) = match tokio::time::timeout(opts.handshake_timeout, peer).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            fail(&tasks);
            return Err(e);
        }
        Err(_) => {
            fail(&tasks);
            return Err(FileError::Timeout("peer hello"));
        }
    };
    if !peer_hello.verify(&ctx) {
        fail(&tasks);
        conn.close(quinn::VarInt::from_u32(wire::codes::PROTOCOL), b"bad hello");
        return Err(FileError::Auth("peer hello signature invalid".into()));
    }
    let peer_fp = Fingerprint(wire::fingerprint_of(&peer_hello.identity_pub));
    if let Some(exp) = opts.expected_peer {
        if exp != peer_fp {
            fail(&tasks);
            conn.close(
                quinn::VarInt::from_u32(wire::codes::PROTOCOL),
                b"wrong peer",
            );
            return Err(FileError::Auth(format!(
                "peer fingerprint {peer_fp} != expected {exp}"
            )));
        }
    }
    let shared =
        eph_secret.diffie_hellman(&x25519_dalek::PublicKey::from(peer_hello.ephemeral_pub));
    if !shared.was_contributory() {
        fail(&tasks);
        return Err(FileError::Auth("non-contributory key exchange".into()));
    }
    let keys = wire::derive_keys(shared.as_bytes(), &ctx, &eph_pub, &peer_hello.ephemeral_pub);
    info!(peer = %peer_fp, sas = keys.sas, relayed, "wzp-files session established");

    Ok(Session {
        ctl_tx: Some(SealWriter::new(
            send,
            &keys.send,
            hello.salt,
            wire::MAGIC_CONTROL,
        )),
        ctl_rx: Some(OpenReader::new(
            peer_recv,
            &keys.recv,
            peer_hello.salt,
            wire::MAGIC_CONTROL,
            wire::CONTROL_FRAME_LIMIT,
        )),
        data_rx: Some(data_rx),
        conn,
        endpoint,
        send_key: keys.send,
        recv_key: keys.recv,
        peer_identity: peer_hello.identity_pub,
        sas: keys.sas,
        relayed,
        peer_present,
        tasks,
    })
}

/// Accept uni streams, classify them by preamble, hand them out.
async fn dispatch_uni(
    conn: quinn::Connection,
    ctl_tx: mpsc::Sender<quinn::RecvStream>,
    data_tx: mpsc::Sender<IncomingData>,
) {
    let mut control_taken = false;
    let seen = Arc::new(std::sync::Mutex::new(
        std::collections::HashSet::<[u8; 16]>::new(),
    ));
    loop {
        let mut recv = match conn.accept_uni().await {
            Ok(r) => r,
            Err(_) => break,
        };
        let mut pre = [0u8; 5];
        let ok = tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut pre)).await;
        if !matches!(ok, Ok(Ok(()))) || pre[4] != wire::WIRE_VERSION {
            let _ = recv.stop(quinn::VarInt::from_u32(wire::codes::PROTOCOL));
            continue;
        }
        let magic = [pre[0], pre[1], pre[2], pre[3]];
        if magic == wire::MAGIC_CONTROL && !control_taken {
            control_taken = true;
            let _ = ctl_tx.send(recv).await;
        } else if magic == wire::MAGIC_DATA {
            // Read the salt without blocking the accept loop.
            let data_tx = data_tx.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut salt = [0u8; 16];
                let r =
                    tokio::time::timeout(Duration::from_secs(10), recv.read_exact(&mut salt)).await;
                if !matches!(r, Ok(Ok(()))) || !seen.lock().unwrap().insert(salt) {
                    let _ = recv.stop(quinn::VarInt::from_u32(wire::codes::PROTOCOL));
                    return;
                }
                if data_tx
                    .send(IncomingData { stream: recv, salt })
                    .await
                    .is_err()
                {
                    // endpoint gone
                }
            });
        } else {
            let _ = recv.stop(quinn::VarInt::from_u32(wire::codes::PROTOCOL));
        }
    }
}
