//! [`IrohNode`]: iroh endpoint + contact book + presence + signaling.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointId, RelayMode, RelayUrl,
    address_lookup::{DnsAddressLookup, PkarrPublisher, PkarrResolver},
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use serde::Serialize;
use tokio::sync::{Notify, broadcast};
use tracing::{debug, info, warn};

use crate::{
    config::{IrohConfig, RelayChoice},
    contact::{Contact, ContactBook, ContactCard, Trust, now_secs},
    error::{Error, Result},
    identity,
    wire::{
        Envelope, MAX_FRAME, MAX_UNKNOWN_FRAME, PRESENCE_ALPN, PresenceMsg, Reply, SIGNAL_ALPN,
        read_frame, write_frame,
    },
};

/// Online state of a contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    /// Not probed yet.
    Unknown,
    Online,
    Offline,
}

/// Events emitted by the node. Subscribe with [`IrohNode::events`].
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)] // mirrors wzp_proto::SignalMessage
pub enum NodeEvent {
    /// An envelope from a known contact.
    Message { from: EndpointId, env: Envelope },
    /// A contact's presence changed.
    Presence {
        id: EndpointId,
        state: PresenceState,
        /// Unix seconds of the last successful exchange.
        last_seen: Option<u64>,
    },
    /// A peer that is not in the contact book tried to message us. Nothing
    /// was delivered. Accept by calling `add_contact` with `contact` (if the
    /// peer sent a `Hello`) or with the id.
    ContactRequest {
        id: EndpointId,
        fingerprint: String,
        name: Option<String>,
        contact: Option<String>,
    },
}

const CLOSE_NOT_CONTACT: u32 = 403;
const CLOSE_BYE: u32 = 0;
/// Repeated contact requests from the same peer within this window are
/// suppressed.
const REQUEST_DEDUP: Duration = Duration::from_secs(60);
/// Cached outgoing connections unused for this long are closed.
const CONN_IDLE: Duration = Duration::from_secs(120);

#[derive(Debug)]
struct PeerPresence {
    state: PresenceState,
    next_due: Instant,
    backoff: Duration,
    in_flight: bool,
}

#[derive(Debug)]
struct CachedConn {
    conn: Connection,
    last_used: Instant,
}

#[derive(Debug)]
struct Inner {
    endpoint: Endpoint,
    book: ContactBook,
    events: broadcast::Sender<NodeEvent>,
    cfg: IrohConfig,
    conns: Mutex<HashMap<(EndpointId, &'static [u8]), CachedConn>>,
    presence: Mutex<HashMap<EndpointId, PeerPresence>>,
    wake: Notify,
    seen_requests: Mutex<HashMap<(EndpointId, bool), Instant>>,
}

/// A running WZP iroh node.
#[derive(Debug)]
pub struct IrohNode {
    inner: Arc<Inner>,
    router: Router,
    presence_task: tokio::task::JoinHandle<()>,
}

impl IrohNode {
    /// Start a node whose identity is derived from the WZP seed.
    pub async fn start(seed: &[u8; 32], cfg: IrohConfig) -> Result<Self> {
        let secret = identity::secret_key_from_seed(seed);
        let relay_mode = match &cfg.relay {
            RelayChoice::N0 => RelayMode::Default,
            RelayChoice::Disabled => RelayMode::Disabled,
            RelayChoice::Custom(urls) => {
                if urls.is_empty() {
                    return Err(Error::Config(
                        "custom relay mode needs at least one URL".into(),
                    ));
                }
                let parsed = urls
                    .iter()
                    .map(|u| {
                        u.parse::<RelayUrl>()
                            .map_err(|e| Error::Config(format!("relay URL {u}: {e}")))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let map = iroh::RelayMap::from_iter(parsed);
                RelayMode::Custom(match &cfg.relay_auth_token {
                    Some(tok) => map.with_auth_token(tok.clone()),
                    None => map,
                })
            }
        };

        let mut b = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .relay_mode(relay_mode);
        if cfg.dns_discovery {
            b = b
                .address_lookup(PkarrPublisher::n0_dns())
                .address_lookup(PkarrResolver::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns());
        }
        if cfg.local_discovery {
            b = b.address_lookup(
                iroh_mdns_address_lookup::MdnsAddressLookup::builder().service_name("wzp-iroh"),
            );
        }
        if let Some(addr) = cfg.bind_addr {
            b = b
                .clear_ip_transports()
                .bind_addr(addr)
                .map_err(|e| Error::Bind(e.to_string()))?;
        }
        let endpoint = b.bind().await.map_err(|e| Error::Bind(e.to_string()))?;

        let book = match &cfg.data_dir {
            Some(dir) => ContactBook::open(dir)?,
            None => ContactBook::in_memory(),
        };
        let (events, _) = broadcast::channel(256);
        let inner = Arc::new(Inner {
            endpoint: endpoint.clone(),
            book,
            events,
            cfg,
            conns: Mutex::new(HashMap::new()),
            presence: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            seen_requests: Mutex::new(HashMap::new()),
        });
        for c in inner.book.list() {
            inner.track(c.id);
        }

        let router = Router::builder(endpoint)
            .accept(SIGNAL_ALPN, SignalHandler(inner.clone()))
            .accept(PRESENCE_ALPN, PresenceHandler(inner.clone()))
            .spawn();
        let presence_task = tokio::spawn(presence_loop(inner.clone()));
        info!(id = %inner.endpoint.id(), "wzp-iroh node started");
        Ok(Self {
            inner,
            router,
            presence_task,
        })
    }

    /// Our endpoint id (== WZP identity public key).
    pub fn id(&self) -> EndpointId {
        self.inner.endpoint.id()
    }

    /// Our WZP fingerprint (`xxxx:…`).
    pub fn fingerprint(&self) -> String {
        identity::fingerprint(&self.id())
    }

    /// The underlying iroh endpoint (e.g. to wait for `online()`).
    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    /// Our contact card with current addressing info.
    ///
    /// Includes the home relay URL when known. Direct IP addresses are
    /// included only when relays are disabled or no relay is connected yet,
    /// to avoid leaking LAN addresses in shared strings unnecessarily.
    pub fn contact_card(&self, name: &str) -> ContactCard {
        let addr = self.inner.endpoint.addr();
        let relay_url = addr.relay_urls().next().cloned();
        let direct_addrs = if self.inner.cfg.relay == RelayChoice::Disabled || relay_url.is_none() {
            addr.ip_addrs().copied().collect()
        } else {
            Vec::new()
        };
        ContactCard {
            id: self.id(),
            name: name.to_string(),
            relay_url,
            direct_addrs,
        }
    }

    /// Shareable `wzp1…` contact string.
    pub fn contact_string(&self, name: &str) -> String {
        self.contact_card(name).encode()
    }

    /// Add (or refresh) a contact from a `wzp1…` string, or from a bare
    /// endpoint id (64 hex chars or z-base-32). New contacts are TOFU-pinned.
    pub async fn add_contact(&self, contact: &str) -> Result<Contact> {
        let card = match ContactCard::parse(contact) {
            Ok(c) => c,
            Err(e) => match identity::parse_endpoint_id(contact) {
                Some(id) => ContactCard {
                    id,
                    name: String::new(),
                    relay_url: None,
                    direct_addrs: Vec::new(),
                },
                None => return Err(e),
            },
        };
        if card.id == self.id() {
            return Err(Error::SelfContact);
        }
        let stored = self.inner.book.upsert(Contact::from_card(&card))?;
        self.inner.track(stored.id);
        Ok(stored)
    }

    /// All contacts.
    pub fn contacts(&self) -> Vec<Contact> {
        self.inner.book.list()
    }

    /// Current presence of a contact.
    pub fn presence(&self, id: EndpointId) -> PresenceState {
        self.inner
            .presence
            .lock()
            .unwrap()
            .get(&id)
            .map(|p| p.state)
            .unwrap_or(PresenceState::Unknown)
    }

    /// Mark a contact's fingerprint as verified out of band.
    pub fn verify_contact(&self, id: EndpointId) -> Result<()> {
        if self.inner.book.set_trust(&id, Trust::Verified)? {
            Ok(())
        } else {
            Err(Error::UnknownContact(id))
        }
    }

    /// Remove a contact; closes cached connections to it.
    pub async fn remove_contact(&self, id: EndpointId) -> Result<()> {
        if self.inner.book.remove(&id)?.is_none() {
            return Err(Error::UnknownContact(id));
        }
        self.inner.presence.lock().unwrap().remove(&id);
        self.inner.evict(id, None);
        Ok(())
    }

    /// Send an envelope to a contact and wait for its acknowledgement.
    pub async fn send(&self, to: EndpointId, env: Envelope) -> Result<()> {
        if to == self.id() {
            return Err(Error::SelfContact);
        }
        if !self.inner.book.contains(&to) {
            return Err(Error::UnknownContact(to));
        }
        let inner = &self.inner;
        let had_cached = inner.conns.lock().unwrap().contains_key(&(to, SIGNAL_ALPN));
        let res = inner.request(to, &env).await;
        let res = match res {
            // A cached connection may have silently died; retry once fresh.
            Err(Error::Protocol(_) | Error::Timeout) if had_cached => inner.request(to, &env).await,
            other => other,
        };
        match &res {
            Ok(()) => inner.set_presence(to, PresenceState::Online),
            Err(Error::Rejected(..)) => {}
            Err(e) => debug!(peer = %to.fmt_short(), "send failed: {e}"),
        }
        res
    }

    /// Subscribe to node events.
    pub fn events(&self) -> broadcast::Receiver<NodeEvent> {
        self.inner.events.subscribe()
    }

    /// Say goodbye to online contacts, stop the router and close the endpoint.
    pub async fn shutdown(self) {
        self.close().await;
    }

    /// Like [`shutdown`](Self::shutdown) but by reference, for owners that
    /// share the node (e.g. behind an `Arc`). The node is unusable afterwards.
    pub async fn close(&self) {
        if self.inner.endpoint.is_closed() {
            return;
        }
        self.presence_task.abort();
        let online: Vec<EndpointId> = self
            .inner
            .presence
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.state == PresenceState::Online)
            .map(|(id, _)| *id)
            .collect();
        let byes = online.into_iter().map(|id| {
            let inner = self.inner.clone();
            async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), inner.bye(id)).await;
            }
        });
        futures_join_all(byes).await;
        let _ = self.inner.book.save();
        if let Err(e) = self.router.shutdown().await {
            warn!("router shutdown: {e}");
        }
        self.inner.endpoint.close().await;
    }
}

async fn futures_join_all<F: std::future::Future<Output = ()> + Send + 'static>(
    futs: impl IntoIterator<Item = F>,
) {
    let mut set = tokio::task::JoinSet::new();
    for f in futs {
        set.spawn(f);
    }
    while set.join_next().await.is_some() {}
}

impl Inner {
    fn emit(&self, ev: NodeEvent) {
        let _ = self.events.send(ev);
    }

    /// Start tracking presence for a contact and wake the presence loop.
    fn track(&self, id: EndpointId) {
        self.presence
            .lock()
            .unwrap()
            .entry(id)
            .or_insert(PeerPresence {
                state: PresenceState::Unknown,
                next_due: Instant::now(),
                backoff: self.cfg.presence_interval,
                in_flight: false,
            });
        self.wake.notify_one();
    }

    fn set_presence(&self, id: EndpointId, state: PresenceState) {
        let now = Instant::now();
        let changed = {
            let mut m = self.presence.lock().unwrap();
            let Some(p) = m.get_mut(&id) else { return };
            let changed = p.state != state;
            p.state = state;
            match state {
                PresenceState::Online => {
                    p.backoff = self.cfg.presence_interval;
                    p.next_due = now + self.cfg.presence_interval;
                }
                PresenceState::Offline => {
                    p.next_due = now + p.backoff;
                    p.backoff = (p.backoff * 2).min(self.cfg.presence_max_backoff);
                }
                PresenceState::Unknown => {}
            }
            changed
        };
        let last_seen = if state == PresenceState::Online {
            let ts = now_secs();
            self.book.touch(&id, ts);
            Some(ts)
        } else {
            self.book.get(&id).and_then(|c| c.last_seen)
        };
        if changed {
            self.emit(NodeEvent::Presence {
                id,
                state,
                last_seen,
            });
        }
    }

    /// Drop cached connections to `id` (for one ALPN or all).
    fn evict(&self, id: EndpointId, alpn: Option<&'static [u8]>) {
        let mut m = self.conns.lock().unwrap();
        m.retain(|(peer, a), c| {
            let hit = *peer == id && alpn.is_none_or(|x| x == *a);
            if hit {
                c.conn.close(CLOSE_BYE.into(), b"");
            }
            !hit
        });
    }

    /// Get a cached connection or dial a new one.
    async fn conn(&self, id: EndpointId, alpn: &'static [u8]) -> Result<Connection> {
        {
            let mut m = self.conns.lock().unwrap();
            if let Some(c) = m.get_mut(&(id, alpn)) {
                if c.conn.close_reason().is_none() {
                    c.last_used = Instant::now();
                    return Ok(c.conn.clone());
                }
                m.remove(&(id, alpn));
            }
        }
        let addr = self
            .book
            .get(&id)
            .map(|c| c.endpoint_addr())
            .unwrap_or_else(|| iroh::EndpointAddr::new(id));
        let conn = self
            .endpoint
            .connect(addr, alpn)
            .await
            .map_err(|e| Error::Connect {
                peer: id,
                reason: e.to_string(),
            })?;
        self.conns.lock().unwrap().insert(
            (id, alpn),
            CachedConn {
                conn: conn.clone(),
                last_used: Instant::now(),
            },
        );
        Ok(conn)
    }

    /// One envelope exchange on the signal ALPN.
    async fn request(&self, to: EndpointId, env: &Envelope) -> Result<()> {
        let fut = async {
            let conn = self.conn(to, SIGNAL_ALPN).await?;
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| Error::Protocol(format!("open stream: {e}")))?;
            write_frame(&mut send, env).await?;
            send.finish()
                .map_err(|e| Error::Protocol(format!("finish: {e}")))?;
            let reply: Reply = read_frame(&mut recv, MAX_FRAME).await?;
            Ok::<_, Error>((conn, reply))
        };
        match tokio::time::timeout(self.cfg.request_timeout, fut).await {
            Ok(Ok((_, Reply::Ack))) => Ok(()),
            Ok(Ok((conn, Reply::Rejected { reason }))) => {
                self.evict(to, Some(SIGNAL_ALPN));
                conn.close(CLOSE_NOT_CONTACT.into(), b"rejected");
                Err(Error::Rejected(to, reason))
            }
            Ok(Err(e)) => {
                self.evict(to, Some(SIGNAL_ALPN));
                Err(e)
            }
            Err(_) => {
                self.evict(to, Some(SIGNAL_ALPN));
                Err(Error::Timeout)
            }
        }
    }

    /// One presence exchange.
    async fn presence_exchange(&self, to: EndpointId, msg: PresenceMsg) -> Result<PresenceMsg> {
        let fut = async {
            let conn = self.conn(to, PRESENCE_ALPN).await?;
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| Error::Protocol(format!("open stream: {e}")))?;
            write_frame(&mut send, &msg).await?;
            send.finish()
                .map_err(|e| Error::Protocol(format!("finish: {e}")))?;
            read_frame::<PresenceMsg>(&mut recv, 1024).await
        };
        match tokio::time::timeout(self.cfg.request_timeout, fut).await {
            Ok(Ok(m)) => Ok(m),
            Ok(Err(e)) => {
                self.evict(to, Some(PRESENCE_ALPN));
                Err(e)
            }
            Err(_) => {
                self.evict(to, Some(PRESENCE_ALPN));
                Err(Error::Timeout)
            }
        }
    }

    async fn ping(&self, to: EndpointId) -> Result<()> {
        match self.presence_exchange(to, PresenceMsg::Ping).await? {
            PresenceMsg::Pong => Ok(()),
            other => Err(Error::Protocol(format!(
                "unexpected presence reply {other:?}"
            ))),
        }
    }

    async fn bye(&self, to: EndpointId) {
        let _ = self.presence_exchange(to, PresenceMsg::Bye).await;
        self.evict(to, None);
    }

    /// Surface a contact request (deduplicated).
    fn contact_request(&self, id: EndpointId, hello: Option<(String, String)>) {
        let key = (id, hello.is_some());
        {
            let mut seen = self.seen_requests.lock().unwrap();
            let now = Instant::now();
            seen.retain(|_, t| now.duration_since(*t) < REQUEST_DEDUP);
            if seen.contains_key(&key) {
                return;
            }
            seen.insert(key, now);
        }
        // Only keep a contact string that matches the authenticated peer id.
        let (name, contact) = match hello {
            Some((name, contact)) => {
                let ok = ContactCard::parse(&contact).is_ok_and(|c| c.id == id);
                (Some(name), ok.then_some(contact))
            }
            None => (None, None),
        };
        self.emit(NodeEvent::ContactRequest {
            id,
            fingerprint: identity::fingerprint(&id),
            name,
            contact,
        });
    }
}

/// Presence scheduler: pings due contacts with exponential backoff and
/// closes idle cached connections.
async fn presence_loop(inner: Arc<Inner>) {
    loop {
        let now = Instant::now();
        let mut due = Vec::new();
        let mut next_wake = now + Duration::from_secs(5);
        {
            let mut m = inner.presence.lock().unwrap();
            for (id, p) in m.iter_mut() {
                if p.in_flight {
                    continue;
                }
                if p.next_due <= now {
                    p.in_flight = true;
                    due.push(*id);
                } else {
                    next_wake = next_wake.min(p.next_due);
                }
            }
        }
        for id in due {
            let inner = inner.clone();
            tokio::spawn(async move {
                let res = inner.ping(id).await;
                if let Some(p) = inner.presence.lock().unwrap().get_mut(&id) {
                    p.in_flight = false;
                }
                match res {
                    Ok(()) => inner.set_presence(id, PresenceState::Online),
                    Err(e) => {
                        debug!(peer = %id.fmt_short(), "presence ping failed: {e}");
                        inner.set_presence(id, PresenceState::Offline);
                    }
                }
                inner.wake.notify_one();
            });
        }
        // Close idle outgoing connections.
        inner.conns.lock().unwrap().retain(|_, c| {
            let keep =
                now.duration_since(c.last_used) < CONN_IDLE && c.conn.close_reason().is_none();
            if !keep {
                c.conn.close(CLOSE_BYE.into(), b"idle");
            }
            keep
        });
        let sleep = next_wake.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = inner.wake.notified() => {}
        }
    }
}

#[derive(Debug, Clone)]
struct SignalHandler(Arc<Inner>);

impl ProtocolHandler for SignalHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let inner = &self.0;
        let peer = conn.remote_id();
        if !inner.book.contains(&peer) {
            handle_unknown(inner, &conn, peer).await;
            return Ok(());
        }
        loop {
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let inner = inner.clone();
            tokio::spawn(async move {
                if !inner.book.contains(&peer) {
                    let _ = write_frame(
                        &mut send,
                        &Reply::Rejected {
                            reason: "not a contact".into(),
                        },
                    )
                    .await;
                    let _ = send.finish();
                    return;
                }
                let env: Envelope = match read_frame(&mut recv, MAX_FRAME).await {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = write_frame(
                            &mut send,
                            &Reply::Rejected {
                                reason: format!("bad frame: {e}"),
                            },
                        )
                        .await;
                        let _ = send.finish();
                        return;
                    }
                };
                let _ = write_frame(&mut send, &Reply::Ack).await;
                let _ = send.finish();
                inner.set_presence(peer, PresenceState::Online);
                if !matches!(env, Envelope::Ping) {
                    inner.emit(NodeEvent::Message { from: peer, env });
                }
            });
        }
        Ok(())
    }
}

/// Read at most one small frame from a stranger, surface a contact request,
/// reply `Rejected` and close.
async fn handle_unknown(inner: &Inner, conn: &Connection, peer: EndpointId) {
    let fut = async {
        let (mut send, mut recv) = conn.accept_bi().await.ok()?;
        let env = read_frame::<Envelope>(&mut recv, MAX_UNKNOWN_FRAME)
            .await
            .ok();
        let _ = write_frame(
            &mut send,
            &Reply::Rejected {
                reason: "not a contact".into(),
            },
        )
        .await;
        let _ = send.finish();
        Some(env)
    };
    if let Ok(Some(env)) = tokio::time::timeout(Duration::from_secs(5), fut).await {
        let hello = match env {
            Some(Envelope::Hello { name, contact }) => Some((name, contact)),
            _ => None,
        };
        inner.contact_request(peer, hello);
        // Give the peer a moment to read the reply, then close.
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    }
    conn.close(CLOSE_NOT_CONTACT.into(), b"not a contact");
}

#[derive(Debug, Clone)]
struct PresenceHandler(Arc<Inner>);

impl ProtocolHandler for PresenceHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let inner = &self.0;
        let peer = conn.remote_id();
        if !inner.book.contains(&peer) {
            // Don't reveal presence to strangers.
            conn.close(CLOSE_NOT_CONTACT.into(), b"");
            return Ok(());
        }
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            if !inner.book.contains(&peer) {
                break;
            }
            let Ok(msg) = read_frame::<PresenceMsg>(&mut recv, 1024).await else {
                continue;
            };
            match msg {
                PresenceMsg::Ping => {
                    let _ = write_frame(&mut send, &PresenceMsg::Pong).await;
                    let _ = send.finish();
                    inner.set_presence(peer, PresenceState::Online);
                }
                PresenceMsg::Bye => {
                    let _ = write_frame(&mut send, &PresenceMsg::Bye).await;
                    let _ = send.finish();
                    inner.evict(peer, None);
                    inner.set_presence(peer, PresenceState::Offline);
                }
                PresenceMsg::Pong => {}
            }
        }
        Ok(())
    }
}

impl IrohNode {
    /// Helper for the CLI/tests: our `Hello` envelope.
    pub fn hello(&self, name: &str) -> Envelope {
        Envelope::Hello {
            name: name.to_string(),
            contact: self.contact_string(name),
        }
    }

    /// Introduce ourselves to someone who may not have us as a contact.
    /// Expected to return `Err(Rejected)` if they don't know us yet; they get
    /// a `ContactRequest` event either way.
    pub async fn send_hello(&self, to: EndpointId, name: &str) -> Result<()> {
        self.send(to, self.hello(name)).await
    }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn is_send<T: Send + Sync>() {}
    is_send::<IrohNode>();
}
