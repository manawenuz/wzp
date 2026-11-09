//! [`ShareNode`]: iroh contacts/signaling + one wzp-files session per transfer.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::{broadcast, watch};
use tracing::{debug, info, warn};
use wzp_files::{
    ConnectOptions, DirectListener, FileEndpoint, FileSessionInvite, Fingerprint, IncomingOffer,
    Progress, TransferId,
};
use wzp_iroh::{Contact, EndpointId, Envelope, IrohNode, NodeEvent};

use crate::types::{
    Direction, InvitePayload, Result, ShareConfig, ShareError, ShareEvent, ShareId, ShareInvite,
    TransferInfo, TransferState,
};

type Outcome = Option<std::result::Result<PathBuf, String>>;

struct Entry {
    info: TransferInfo,
    offer: Option<IncomingOffer>,
    ep: Option<Arc<FileEndpoint>>,
    wzp_id: Option<TransferId>,
    task: Option<tokio::task::AbortHandle>,
    done: watch::Sender<Outcome>,
}

struct Inner {
    seed: [u8; 32],
    cfg: ShareConfig,
    iroh: IrohNode,
    events: broadcast::Sender<ShareEvent>,
    transfers: Mutex<HashMap<ShareId, Entry>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.seed.fill(0);
    }
}

/// Handle returned by [`ShareNode::send_file`].
#[derive(Debug, Clone)]
pub struct TransferHandle {
    id: ShareId,
    done: watch::Receiver<Outcome>,
}

impl TransferHandle {
    pub fn id(&self) -> ShareId {
        self.id
    }

    /// Wait until the transfer finishes. `Ok(source path)` on completion.
    pub async fn wait(mut self) -> Result<PathBuf> {
        let v = self
            .done
            .wait_for(|o| o.is_some())
            .await
            .map_err(|_| ShareError::Failed("node shut down".into()))?
            .clone();
        match v {
            Some(Ok(p)) => Ok(p),
            Some(Err(e)) if e.starts_with("rejected: ") => {
                Err(ShareError::Rejected(e["rejected: ".len()..].to_string()))
            }
            Some(Err(e)) if e == "cancelled" => Err(ShareError::Cancelled),
            Some(Err(e)) => Err(ShareError::Failed(e)),
            None => unreachable!(),
        }
    }
}

/// App-level "send a file to a contact" node.
pub struct ShareNode {
    inner: Arc<Inner>,
    pump: tokio::task::JoinHandle<()>,
}

impl ShareNode {
    /// Start the iroh node and the invite handler.
    pub async fn start(seed: &[u8; 32], mut cfg: ShareConfig) -> Result<Self> {
        if cfg.iroh.data_dir.is_none() {
            cfg.iroh.data_dir = Some(cfg.data_dir.clone());
        }
        std::fs::create_dir_all(&cfg.data_dir)?;
        std::fs::create_dir_all(&cfg.files.download_dir)?;
        let iroh = IrohNode::start(seed, cfg.iroh.clone()).await?;
        let (events, _) = broadcast::channel(1024);
        let node_events = iroh.events();
        let inner = Arc::new(Inner {
            seed: *seed,
            cfg,
            iroh,
            events,
            transfers: Mutex::new(HashMap::new()),
        });
        let pump = tokio::spawn(pump(inner.clone(), node_events));
        info!(id = %inner.iroh.id(), "wzp-share node started");
        Ok(Self { inner, pump })
    }

    /// The underlying iroh node (contacts, presence, raw envelopes).
    pub fn iroh(&self) -> &IrohNode {
        &self.inner.iroh
    }

    pub fn id(&self) -> EndpointId {
        self.inner.iroh.id()
    }

    pub fn fingerprint(&self) -> String {
        self.inner.iroh.fingerprint()
    }

    pub fn contact_string(&self, name: &str) -> String {
        self.inner.iroh.contact_string(name)
    }

    pub async fn add_contact(&self, contact: &str) -> Result<Contact> {
        Ok(self.inner.iroh.add_contact(contact).await?)
    }

    pub async fn remove_contact(&self, id: EndpointId) -> Result<()> {
        Ok(self.inner.iroh.remove_contact(id).await?)
    }

    pub fn contacts(&self) -> Vec<Contact> {
        self.inner.iroh.contacts()
    }

    /// Unified event stream (contacts, presence, text, transfers).
    pub fn events(&self) -> broadcast::Receiver<ShareEvent> {
        self.inner.events.subscribe()
    }

    /// Send `path` to `contact`: host a WZP file session (direct listener
    /// and/or relay room), invite the contact over iroh, then offer the file.
    /// Returns once the invite is under way; follow progress via
    /// [`events`](Self::events) or [`TransferHandle::wait`].
    pub async fn send_file(
        &self,
        contact: EndpointId,
        path: impl AsRef<Path>,
    ) -> Result<TransferHandle> {
        let inner = &self.inner;
        let path = path.as_ref().to_path_buf();
        let c = inner
            .iroh
            .contacts()
            .into_iter()
            .find(|c| c.id == contact)
            .ok_or(wzp_iroh::Error::UnknownContact(contact))?;
        let meta = tokio::fs::metadata(&path).await?;
        if !meta.is_file() {
            return Err(ShareError::Failed(format!(
                "{} is not a file",
                path.display()
            )));
        }
        if meta.len() > inner.cfg.files.max_size {
            return Err(wzp_files::FileError::TooLarge {
                size: meta.len(),
                max: inner.cfg.files.max_size,
            }
            .into());
        }

        let listener = match inner.cfg.direct_bind {
            Some(bind) => Some(wzp_files::listen_direct(bind, &inner.seed).await?),
            None => None,
        };
        let direct = match (&inner.cfg.test_direct_override, &listener) {
            (Some(v), _) => v.clone(),
            (None, Some(l)) => direct_candidates(l.local_addr()?, &inner.iroh),
            (None, None) => Vec::new(),
        };
        let plan = plan_relay(&inner.cfg);
        if direct.is_empty() && plan.is_none() {
            return Err(ShareError::Failed(NO_RELAY.into()));
        }
        let host_auth = plan.as_ref().and_then(|p| p.host_auth.clone());
        let invite = build_invite(direct, plan, &inner.seed);
        let id = ShareId::random();
        let info = TransferInfo {
            id,
            contact,
            contact_name: c.name.clone(),
            direction: Direction::Outgoing,
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size: meta.len(),
            path: Some(path.clone()),
            state: TransferState::Connecting,
            done: 0,
            relayed: None,
            sas: None,
            error: None,
        };
        let (done_tx, done_rx) = watch::channel(None);
        inner.transfers.lock().unwrap().insert(
            id,
            Entry {
                info: info.clone(),
                offer: None,
                ep: None,
                wzp_id: None,
                task: None,
                done: done_tx,
            },
        );
        inner.emit(ShareEvent::TransferOffered { transfer: info });

        let task = tokio::spawn(run_outgoing(
            inner.clone(),
            id,
            contact,
            path,
            invite,
            listener,
            host_auth,
        ));
        inner.set_task(id, task.abort_handle());
        Ok(TransferHandle { id, done: done_rx })
    }

    /// Accept an incoming offer.
    pub async fn accept(&self, id: ShareId) -> Result<()> {
        let offer = self.inner.take_offer(id)?;
        offer.accept();
        Ok(())
    }

    /// Reject an incoming offer.
    pub async fn reject(&self, id: ShareId, reason: &str) -> Result<()> {
        let offer = self.inner.take_offer(id)?;
        offer.reject(reason);
        self.inner.finish(
            id,
            TransferState::Rejected,
            Err(format!("rejected: {reason}")),
            false,
        );
        Ok(())
    }

    /// Cancel a transfer in any non-finished state.
    pub async fn cancel(&self, id: ShareId) -> Result<()> {
        let (offer, ep, wzp_id, task, contact, connecting) = {
            let mut m = self.inner.transfers.lock().unwrap();
            let e = m.get_mut(&id).ok_or(ShareError::UnknownTransfer(id))?;
            if e.info.state.is_finished() {
                return Ok(());
            }
            (
                e.offer.take(),
                e.ep.clone(),
                e.wzp_id,
                e.task.clone(),
                e.info.contact,
                e.info.state == TransferState::Connecting,
            )
        };
        // Settle the local state first so racing session errors can't turn
        // a cancel into a failure.
        self.inner
            .finish(id, TransferState::Cancelled, Err("cancelled".into()), false);
        if let Some(o) = offer {
            o.reject("cancelled");
        }
        if let (Some(ep), Some(wid)) = (&ep, wzp_id) {
            ep.cancel(wid).await;
        }
        if let Some(t) = task {
            t.abort();
        }
        if connecting {
            // The peer may not have a WZP session with us yet: tell it over iroh.
            send_abort(&self.inner, contact, id, "cancelled").await;
        }
        if let Some(ep) = ep {
            ep.close().await;
        }
        Ok(())
    }

    /// Snapshot of all known transfers (active and finished).
    pub fn transfers(&self) -> Vec<TransferInfo> {
        let mut v: Vec<_> = self
            .inner
            .transfers
            .lock()
            .unwrap()
            .values()
            .map(|e| e.info.clone())
            .collect();
        v.sort_by_key(|t| t.id);
        v
    }

    /// Drop finished transfers from the list. Returns how many were removed.
    pub fn remove_finished(&self) -> usize {
        let mut m = self.inner.transfers.lock().unwrap();
        let before = m.len();
        m.retain(|_, e| !e.info.state.is_finished());
        before - m.len()
    }

    /// Cancel active transfers and stop the iroh node.
    pub async fn shutdown(self) {
        self.pump.abort();
        let active: Vec<ShareId> = self
            .inner
            .transfers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| !e.info.state.is_finished())
            .map(|(id, _)| *id)
            .collect();
        for id in active {
            let _ = self.cancel(id).await;
        }
        self.inner.iroh.close().await;
    }
}

/// Direct candidates for a listener: its own address if bound to a specific
/// IP, else the local interface IPs iroh discovered, with the listener port.
fn direct_candidates(local: SocketAddr, iroh: &IrohNode) -> Vec<SocketAddr> {
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let mut v: Vec<SocketAddr> = iroh
        .endpoint()
        .addr()
        .ip_addrs()
        .filter(|a| a.is_ipv4() == local.is_ipv4())
        .map(|a| SocketAddr::new(a.ip(), local.port()))
        .collect();
    v.sort();
    v.dedup();
    v.truncate(8);
    v
}

/// Error text when neither path is available.
pub(crate) const NO_RELAY: &str = "direct connection failed and no relay configured/authorized";

/// How this node may use its WZP relay for one hosted transfer.
pub(crate) struct RelayPlan {
    addr: SocketAddr,
    room: String,
    /// Secret credential the *host* presents (the relay token). Never sent
    /// to the peer.
    host_auth: Option<String>,
    /// Room-scoped ticket for the guest, carried in the invite.
    guest_ticket: Option<String>,
}

/// Decide whether the relay fallback is available: it needs a relay address
/// and a relay token (or the test-only open-relay flag).
pub(crate) fn plan_relay(cfg: &ShareConfig) -> Option<RelayPlan> {
    let addr = cfg.relay?;
    let room = wzp_files::random_room();
    match &cfg.relay_token {
        Some(token) => Some(RelayPlan {
            guest_ticket: issue_guest_ticket(addr, token, &room),
            addr,
            room,
            host_auth: Some(token.clone()),
        }),
        None if cfg.test_open_relay => Some(RelayPlan {
            addr,
            room,
            host_auth: None,
            guest_ticket: None,
        }),
        None => None,
    }
}

/// Room-scoped, expiring ticket (HMAC, one `file-*` room, 2 participants)
/// that lets a recipient without a relay token join our relayed transfer.
///
/// TODO(relay-tickets): wire to the wzp-files ticket API from
/// feat/relay-throughput once it is on main. Until then no ticket is issued,
/// so a token-protected relay only works for recipients that hold their own
/// token for the same relay (see `run_incoming`).
fn issue_guest_ticket(_relay: SocketAddr, _token: &str, _room: &str) -> Option<String> {
    None
}

/// Build the invite. The relay token never goes in here, only the ticket.
pub(crate) fn build_invite(
    direct: Vec<SocketAddr>,
    plan: Option<RelayPlan>,
    seed: &[u8; 32],
) -> FileSessionInvite {
    let mut invite = FileSessionInvite::new(None, direct, seed);
    invite.relay = plan.map(|p| wzp_files::RelayRendezvous {
        addr: p.addr,
        room: p.room,
        auth_token: p.guest_ticket,
    });
    invite
}

/// Best-effort abort notice over iroh.
async fn send_abort(inner: &Inner, to: EndpointId, share_id: ShareId, reason: &str) {
    let msg = crate::types::ShareAbort {
        share_id,
        abort: reason.to_string(),
    };
    let v = serde_json::to_value(&msg).expect("abort serializes");
    if let Err(e) = inner.iroh.send(to, Envelope::FileInvite(v)).await {
        debug!(%share_id, "abort notice not delivered: {e}");
    }
}

fn fingerprint_of(id: &EndpointId) -> Fingerprint {
    Fingerprint(wzp_iroh::identity::fingerprint_bytes(id))
}

impl Inner {
    fn emit(&self, ev: ShareEvent) {
        let _ = self.events.send(ev);
    }

    fn connect_opts(&self, peer: &EndpointId) -> ConnectOptions {
        ConnectOptions {
            expected_peer: Some(fingerprint_of(peer)),
            auth_token: None,
            alias: None,
            peer_wait: self.cfg.peer_wait,
            handshake_timeout: self.cfg.connect_timeout,
        }
    }

    fn contact_name(&self, id: &EndpointId) -> String {
        self.iroh
            .contacts()
            .into_iter()
            .find(|c| c.id == *id)
            .map(|c| c.name)
            .unwrap_or_default()
    }

    fn set_task(&self, id: ShareId, h: tokio::task::AbortHandle) {
        if let Some(e) = self.transfers.lock().unwrap().get_mut(&id) {
            e.task = Some(h);
        }
    }

    /// Mutate a transfer's info; returns the updated snapshot unless finished.
    fn update(&self, id: ShareId, f: impl FnOnce(&mut Entry)) -> Option<TransferInfo> {
        let mut m = self.transfers.lock().unwrap();
        let e = m.get_mut(&id)?;
        if e.info.state.is_finished() {
            return None;
        }
        f(e);
        Some(e.info.clone())
    }

    fn take_offer(&self, id: ShareId) -> Result<IncomingOffer> {
        let mut m = self.transfers.lock().unwrap();
        let e = m.get_mut(&id).ok_or(ShareError::UnknownTransfer(id))?;
        e.offer.take().ok_or(ShareError::NotPending(id))
    }

    fn is_finished(&self, id: ShareId) -> bool {
        self.transfers
            .lock()
            .unwrap()
            .get(&id)
            .is_none_or(|e| e.info.state.is_finished())
    }

    /// Move to a terminal state (first call wins) and emit the event.
    fn finish(
        &self,
        id: ShareId,
        state: TransferState,
        result: std::result::Result<PathBuf, String>,
        by_peer: bool,
    ) -> bool {
        let info = {
            let mut m = self.transfers.lock().unwrap();
            let Some(e) = m.get_mut(&id) else {
                return false;
            };
            if e.info.state.is_finished() {
                return false;
            }
            e.info.state = state;
            e.offer = None;
            e.ep = None;
            match &result {
                Ok(p) => {
                    e.info.path = Some(p.clone());
                    e.info.done = e.info.size;
                }
                Err(err) => e.info.error = Some(err.clone()),
            }
            let _ = e.done.send(Some(result));
            e.info.clone()
        };
        self.emit(match state {
            TransferState::Completed => ShareEvent::TransferCompleted { transfer: info },
            TransferState::Rejected => ShareEvent::TransferRejected { transfer: info },
            TransferState::Cancelled => ShareEvent::TransferCancelled {
                transfer: info,
                by_peer,
            },
            _ => ShareEvent::TransferFailed { transfer: info },
        });
        true
    }

    fn fail(&self, id: ShareId, err: impl std::fmt::Display) {
        self.finish(id, TransferState::Failed, Err(err.to_string()), false);
    }
}

/// Sender side of one transfer.
async fn run_outgoing(
    inner: Arc<Inner>,
    id: ShareId,
    contact: EndpointId,
    path: PathBuf,
    invite: FileSessionInvite,
    listener: Option<DirectListener>,
    host_auth: Option<String>,
) {
    let payload = ShareInvite {
        invite: invite.clone(),
        share_id: id,
    };
    let value = serde_json::to_value(&payload).expect("invite serializes");
    if let Err(e) = inner.iroh.send(contact, Envelope::FileInvite(value)).await {
        inner.fail(id, format!("invite not delivered: {e}"));
        return;
    }
    let mut opts = inner.connect_opts(&contact);
    opts.auth_token = host_auth;
    let host = wzp_files::host_invite(&invite, listener.as_ref(), &inner.seed, &opts);
    let session =
        match tokio::time::timeout(inner.cfg.peer_wait + Duration::from_secs(5), host).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return inner.fail(id, format!("session: {e}")),
            Err(_) => return inner.fail(id, "peer did not join in time"),
        };
    // Note: don't `close()` the listener — that would close its quinn
    // endpoint and with it the session. It is dropped with this task.
    let relayed = session.is_relayed();
    let sas = session.sas();
    debug!(%id, relayed, "outgoing session up");
    let ep = Arc::new(FileEndpoint::new(session, inner.cfg.files.clone()));
    let rx = ep.events();
    inner.update(id, |e| {
        e.info.relayed = Some(relayed);
        e.info.sas = Some(sas);
        e.ep = Some(ep.clone());
    });
    let wid = match ep.send(&path).await {
        Ok(w) => w,
        Err(e) => {
            inner.fail(id, e);
            ep.close().await;
            return;
        }
    };
    inner.update(id, |e| {
        e.wzp_id = Some(wid);
        e.info.state = TransferState::AwaitingDecision;
    });
    drive(&inner, id, &ep, rx, Some(path)).await;
    linger_close(&ep).await;
}

/// Receiver side: called for every `FileInvite` from a known contact.
fn handle_invite(inner: &Arc<Inner>, from: EndpointId, value: serde_json::Value) {
    let refuse = |reason: String| {
        warn!(from = %from.fmt_short(), "file invite refused: {reason}");
        inner.emit(ShareEvent::InviteRefused { from, reason });
    };
    let payload: ShareInvite = match serde_json::from_value(value) {
        Ok(InvitePayload::Invite(p)) => p,
        Ok(InvitePayload::Abort(a)) => return handle_abort(inner, from, a),
        Err(e) => return refuse(format!("bad invite: {e}")),
    };
    match payload.invite.expected_peer() {
        Ok(fp) if fp == fingerprint_of(&from) => {}
        Ok(_) => {
            return refuse("fingerprint mismatch: invite is not for the sending contact".into());
        }
        Err(e) => return refuse(format!("bad fingerprint: {e}")),
    }
    let id = payload.share_id;
    {
        let mut m = inner.transfers.lock().unwrap();
        if m.contains_key(&id) {
            return refuse("duplicate invite".into());
        }
        let joining = m
            .values()
            .filter(|e| {
                e.info.direction == Direction::Incoming
                    && matches!(
                        e.info.state,
                        TransferState::Connecting | TransferState::AwaitingDecision
                    )
            })
            .count();
        if joining >= inner.cfg.max_incoming_sessions {
            drop(m);
            return refuse("too many pending incoming transfers".into());
        }
        let (done, _) = watch::channel(None);
        m.insert(
            id,
            Entry {
                info: TransferInfo {
                    id,
                    contact: from,
                    contact_name: inner.contact_name(&from),
                    direction: Direction::Incoming,
                    name: String::new(),
                    size: 0,
                    path: None,
                    state: TransferState::Connecting,
                    done: 0,
                    relayed: None,
                    sas: None,
                    error: None,
                },
                offer: None,
                ep: None,
                wzp_id: None,
                task: None,
                done,
            },
        );
    }
    let task = tokio::spawn(run_incoming(inner.clone(), id, from, payload.invite));
    inner.set_task(id, task.abort_handle());
}

/// An abort notice from `from` for one of our transfers with them.
fn handle_abort(inner: &Inner, from: EndpointId, a: crate::types::ShareAbort) {
    let task = {
        let m = inner.transfers.lock().unwrap();
        match m.get(&a.share_id) {
            Some(e) if e.info.contact == from && !e.info.state.is_finished() => e.task.clone(),
            _ => return,
        }
    };
    if a.abort == "cancelled" {
        inner.finish(
            a.share_id,
            TransferState::Cancelled,
            Err("cancelled".into()),
            true,
        );
    } else {
        inner.fail(a.share_id, format!("peer: {}", a.abort));
    }
    if let Some(t) = task {
        t.abort();
    }
}

async fn run_incoming(inner: Arc<Inner>, id: ShareId, from: EndpointId, invite: FileSessionInvite) {
    let mut opts = inner.connect_opts(&from);
    // The invite's ticket is used for the relay. If there is none and the
    // relay is our own configured one, our own token authorizes us.
    if let Some(r) = &invite.relay
        && r.auth_token.is_none()
        && Some(r.addr) == inner.cfg.relay
    {
        opts.auth_token = inner.cfg.relay_token.clone();
    }
    let join = wzp_files::join_invite(&invite, &inner.seed, &opts);
    let res = tokio::time::timeout(inner.cfg.peer_wait + Duration::from_secs(5), join).await;
    let session = match res {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let msg = if invite.relay.is_none() {
                format!("{NO_RELAY} ({e})")
            } else {
                format!("session: {e}")
            };
            send_abort(&inner, from, id, &msg).await;
            return inner.fail(id, msg);
        }
        Err(_) => {
            send_abort(&inner, from, id, "could not reach sender in time").await;
            return inner.fail(id, "could not reach sender in time");
        }
    };
    let relayed = session.is_relayed();
    let sas = session.sas();
    debug!(%id, relayed, "incoming session up");
    let ep = Arc::new(FileEndpoint::new(session, inner.cfg.files.clone()));
    let rx = ep.events();
    inner.update(id, |e| {
        e.info.relayed = Some(relayed);
        e.info.sas = Some(sas);
        e.ep = Some(ep.clone());
    });
    let offer = match tokio::time::timeout(inner.cfg.files.offer_timeout, ep.next_offer()).await {
        Ok(Some(o)) => o,
        Ok(None) => {
            inner.fail(id, "session closed before the offer");
            ep.close().await;
            return;
        }
        Err(_) => {
            inner.fail(id, "no offer received");
            ep.close().await;
            return;
        }
    };
    if offer.peer != fingerprint_of(&from) {
        offer.reject("identity mismatch");
        inner.fail(id, "offer from unexpected identity");
        ep.close().await;
        return;
    }
    let auto = inner.cfg.auto_accept;
    let wid = offer.transfer_id;
    let mut offer = Some(offer);
    let info = inner.update(id, |e| {
        e.info.name = offer.as_ref().unwrap().name.clone();
        e.info.size = offer.as_ref().unwrap().size;
        e.info.state = TransferState::AwaitingDecision;
        e.wzp_id = Some(wid);
        if !auto {
            e.offer = offer.take();
        }
    });
    let Some(info) = info else {
        // Cancelled meanwhile.
        ep.close().await;
        return;
    };
    inner.emit(ShareEvent::TransferOffered { transfer: info });
    if let Some(o) = offer {
        o.accept();
    }
    drive(&inner, id, &ep, rx, None).await;
    linger_close(&ep).await;
}

/// Give the peer a moment to read our last control message (e.g. a
/// `FileReject` or `FileComplete`) and close first, then close.
async fn linger_close(ep: &FileEndpoint) {
    let _ = tokio::time::timeout(Duration::from_secs(2), ep.closed()).await;
    ep.close().await;
}

/// Map wzp-files progress for the (single) transfer on `ep` to share events.
async fn drive(
    inner: &Inner,
    id: ShareId,
    ep: &FileEndpoint,
    mut rx: broadcast::Receiver<Progress>,
    source: Option<PathBuf>,
) {
    let mut closed = false;
    loop {
        if inner.is_finished(id) {
            return;
        }
        let ev = if closed {
            match rx.try_recv() {
                Ok(ev) => ev,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => {
                    inner.fail(id, "session closed");
                    return;
                }
            }
        } else {
            tokio::select! {
                r = rx.recv() => match r {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => { inner.fail(id, "session closed"); return; }
                },
                _ = ep.closed() => { closed = true; continue; }
            }
        };
        match ev {
            Progress::Offered { .. } => {}
            Progress::Started { resumed_bytes, .. } => {
                if let Some(info) = inner.update(id, |e| {
                    e.info.state = TransferState::Transferring;
                    e.info.done = resumed_bytes;
                }) {
                    inner.emit(ShareEvent::TransferStarted { transfer: info });
                }
            }
            Progress::Bytes { done, total, .. } => {
                if inner
                    .update(id, |e| {
                        e.info.state = TransferState::Transferring;
                        e.info.done = done;
                    })
                    .is_some()
                {
                    inner.emit(ShareEvent::TransferProgress { id, done, total });
                }
            }
            Progress::Completed { path, .. } => {
                let p = source.clone().unwrap_or(path);
                inner.finish(id, TransferState::Completed, Ok(p), false);
                return;
            }
            Progress::Rejected { reason, .. } => {
                inner.finish(
                    id,
                    TransferState::Rejected,
                    Err(format!("rejected: {reason}")),
                    true,
                );
                return;
            }
            Progress::Failed { error, .. } => {
                inner.fail(id, error);
                return;
            }
            Progress::Cancelled { by_peer, .. } => {
                inner.finish(
                    id,
                    TransferState::Cancelled,
                    Err("cancelled".into()),
                    by_peer,
                );
                return;
            }
        }
    }
}

/// Forward iroh events and dispatch file invites.
async fn pump(inner: Arc<Inner>, mut rx: broadcast::Receiver<NodeEvent>) {
    loop {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("dropped {n} iroh events");
                continue;
            }
            Err(_) => return,
        };
        match ev {
            NodeEvent::Presence {
                id,
                state,
                last_seen,
            } => inner.emit(ShareEvent::Presence {
                id,
                state,
                last_seen,
            }),
            NodeEvent::ContactRequest {
                id,
                fingerprint,
                name,
                contact,
            } => inner.emit(ShareEvent::ContactRequest {
                id,
                fingerprint,
                name,
                contact,
            }),
            NodeEvent::Message { from, env } => match env {
                Envelope::FileInvite(v) => handle_invite(&inner, from, v),
                Envelope::Text { text, sent_at_ms } => inner.emit(ShareEvent::Text {
                    from,
                    text,
                    sent_at_ms,
                }),
                _ => {}
            },
        }
    }
}
