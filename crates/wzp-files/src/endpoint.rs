//! [`FileEndpoint`]: offer / accept / send / receive files over a [`Session`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use wzp_crypto::Fingerprint;
use wzp_proto::{SignalMessage, default_signal_version, sanitize_file_name};

use crate::manifest::{self, Bitmap};
use crate::session::{IncomingData, Session};
use crate::store::{self, PartFile};
use crate::wire::{self, OpenReader, SealWriter, codes};
use crate::{
    Direction, FileConfig, FileError, MAX_CHUNK_SIZE, MAX_CHUNKS, MIN_CHUNK_SIZE, Progress,
    Result, TransferId,
};

const CTL_SIGNAL: u8 = 0;
const CTL_MANIFEST: u8 = 1;
const DATA_CHUNK_START: u8 = 1;
const DATA_CHUNK_BYTES: u8 = 2;
const MAX_NACK_ENTRIES: usize = 4096;

/// Final state of a transfer.
#[derive(Clone, Debug)]
enum Outcome {
    Completed(PathBuf),
    Rejected(String),
    Failed(String),
    Cancelled,
}

// ── outgoing ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OutState {
    Offered,
    Sending,
}

struct Outgoing {
    id: TransferId,
    path: PathBuf,
    size: u64,
    chunk_size: u32,
    n: u64,
    hashes: Arc<Vec<[u8; 32]>>,
    root: [u8; 32],
    state: Mutex<OutState>,
    queue: Mutex<(VecDeque<u32>, HashSet<u32>)>,
    notify: Notify,
    stop: watch::Sender<Option<bool>>, // Some(true)=done ok, Some(false)=abort
    corrupt_once: Mutex<HashSet<u32>>,
    pacer: Option<Pacer>,
    done: watch::Sender<Option<Outcome>>,
}

impl Outgoing {
    fn push(&self, idx: u32) {
        let mut q = self.queue.lock().unwrap();
        if idx as u64 >= self.n || !q.1.insert(idx) {
            return;
        }
        q.0.push_back(idx);
        drop(q);
        self.notify.notify_waiters();
    }

    fn pop(&self) -> Option<u32> {
        let mut q = self.queue.lock().unwrap();
        let i = q.0.pop_front()?;
        q.1.remove(&i);
        Some(i)
    }
}

struct Pacer {
    rate: u64,
    next: Mutex<tokio::time::Instant>,
}

impl Pacer {
    async fn wait(&self, bytes: usize) {
        let at = {
            let mut n = self.next.lock().unwrap();
            let now = tokio::time::Instant::now();
            if *n < now {
                *n = now;
            }
            let at = *n;
            *n += Duration::from_secs_f64(bytes as f64 / self.rate as f64);
            at
        };
        tokio::time::sleep_until(at).await;
    }
}

// ── incoming ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum IncState {
    AwaitingDecision,
    Receiving,
    Finishing,
}

struct Incoming {
    id: TransferId,
    name: String,
    size: u64,
    chunk_size: u32,
    n: u64,
    root: [u8; 32],
    state: Mutex<IncState>,
    part: Mutex<Option<PartFile>>,
    hashes: watch::Sender<Option<Arc<Vec<[u8; 32]>>>>,
    last_activity: Mutex<Instant>,
    marks: Mutex<Marks>,
    bad: Mutex<HashMap<u32, u32>>,
    done: watch::Sender<Option<Outcome>>,
}

struct Marks {
    last_progress_sent: Instant,
    last_event: Instant,
    last_persist: Instant,
}

/// An offer from the peer, waiting for the application's decision.
/// Dropping it without deciding rejects the offer.
pub struct IncomingOffer {
    pub transfer_id: TransferId,
    /// Sanitized file name.
    pub name: String,
    pub size: u64,
    pub mime: Option<String>,
    /// Identity of the sending peer.
    pub peer: Fingerprint,
    decision: oneshot::Sender<std::result::Result<(), String>>,
}

impl std::fmt::Debug for IncomingOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncomingOffer")
            .field("transfer_id", &self.transfer_id)
            .field("name", &self.name)
            .field("size", &self.size)
            .finish()
    }
}

impl IncomingOffer {
    pub fn accept(self) {
        let _ = self.decision.send(Ok(()));
    }

    pub fn reject(self, reason: impl Into<String>) {
        let _ = self.decision.send(Err(reason.into()));
    }
}

// ── endpoint ────────────────────────────────────────────────────────────

struct Inner {
    cfg: FileConfig,
    conn: quinn::Connection,
    send_key: [u8; 32],
    recv_key: [u8; 32],
    peer: Fingerprint,
    ctl: tokio::sync::Mutex<SealWriter>,
    events: broadcast::Sender<Progress>,
    out: Mutex<HashMap<TransferId, Arc<Outgoing>>>,
    inc: Mutex<HashMap<TransferId, Arc<Incoming>>>,
    finished: Mutex<HashMap<TransferId, Outcome>>,
    pending_offers: Mutex<usize>,
    offers_tx: mpsc::Sender<IncomingOffer>,
    closed: watch::Sender<bool>,
}

/// File transfer endpoint on top of an established [`Session`]. Both peers
/// can send and receive at the same time.
pub struct FileEndpoint {
    inner: Arc<Inner>,
    offers_rx: tokio::sync::Mutex<mpsc::Receiver<IncomingOffer>>,
    session: Session,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for FileEndpoint {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        let _ = self.inner.closed.send_replace(true);
    }
}

impl FileEndpoint {
    /// Take over `session` (its control and data streams) and start
    /// serving. Must be called inside a tokio runtime.
    pub fn new(mut session: Session, cfg: FileConfig) -> Self {
        let ctl_tx = session.ctl_tx.take().expect("session control stream");
        let ctl_rx = session.ctl_rx.take().expect("session control stream");
        let data_rx = session.data_rx.take().expect("session data streams");
        let (events, _) = broadcast::channel(1024);
        let (offers_tx, offers_rx) = mpsc::channel(cfg.max_pending_offers.max(1));
        let (closed, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            cfg,
            conn: session.conn.clone(),
            send_key: session.send_key,
            recv_key: session.recv_key,
            peer: session.peer_fingerprint(),
            ctl: tokio::sync::Mutex::new(ctl_tx),
            events,
            out: Mutex::new(HashMap::new()),
            inc: Mutex::new(HashMap::new()),
            finished: Mutex::new(HashMap::new()),
            pending_offers: Mutex::new(0),
            offers_tx,
            closed,
        });
        let mut tasks = vec![
            tokio::spawn(control_loop(inner.clone(), ctl_rx)),
            tokio::spawn(data_accept_loop(inner.clone(), data_rx)),
        ];
        let i2 = inner.clone();
        let present = session.peer_present();
        tasks.push(tokio::spawn(async move {
            let conn = i2.conn.clone();
            let mut closed = i2.closed.subscribe();
            let gone = async {
                match present {
                    Some(mut p) => loop {
                        if !*p.borrow_and_update() {
                            break;
                        }
                        if p.changed().await.is_err() {
                            break;
                        }
                    },
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = conn.closed() => {}
                _ = gone => {}
                _ = closed.wait_for(|c| *c) => {}
            }
            session_lost(&i2, "session closed").await;
        }));
        Self {
            inner,
            offers_rx: tokio::sync::Mutex::new(offers_rx),
            session,
            tasks,
        }
    }

    /// The underlying session (peer fingerprint, SAS, relayed?).
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn peer(&self) -> Fingerprint {
        self.inner.peer
    }

    /// Subscribe to transfer events.
    pub fn events(&self) -> broadcast::Receiver<Progress> {
        self.inner.events.subscribe()
    }

    /// Offer `path` to the peer. Hashes the file (streaming), sends the
    /// offer and returns; progress arrives via [`events`](Self::events) and
    /// [`wait`](Self::wait).
    pub async fn send(&self, path: &Path) -> Result<TransferId> {
        let inner = &self.inner;
        if *inner.closed.borrow() {
            return Err(FileError::Closed);
        }
        let meta = tokio::fs::metadata(path).await?;
        if !meta.is_file() {
            return Err(FileError::Invalid(format!("{} is not a file", path.display())));
        }
        if meta.len() > inner.cfg.max_size {
            return Err(FileError::TooLarge {
                size: meta.len(),
                max: inner.cfg.max_size,
            });
        }
        let chunk_size = inner.cfg.chunk_size.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE);
        if manifest::chunk_count(meta.len(), chunk_size) > MAX_CHUNKS {
            return Err(FileError::Invalid("too many chunks; raise chunk_size".into()));
        }
        let p = path.to_path_buf();
        let (size, hashes) =
            tokio::task::spawn_blocking(move || manifest::hash_file(&p, chunk_size))
                .await
                .map_err(|e| FileError::Failed(format!("hash task: {e}")))??;
        if size > inner.cfg.max_size {
            return Err(FileError::TooLarge {
                size,
                max: inner.cfg.max_size,
            });
        }
        let root = manifest::manifest_root(size, chunk_size, &hashes);
        let name = sanitize_file_name(
            &path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let id = TransferId::random();
        let (stop, _) = watch::channel(None);
        let (done, _) = watch::channel(None);
        let og = Arc::new(Outgoing {
            id,
            path: path.to_path_buf(),
            size,
            chunk_size,
            n: hashes.len() as u64,
            hashes: Arc::new(hashes),
            root,
            state: Mutex::new(OutState::Offered),
            queue: Mutex::new((VecDeque::new(), HashSet::new())),
            notify: Notify::new(),
            stop,
            corrupt_once: Mutex::new(inner.cfg.test_corrupt_chunks.iter().copied().collect()),
            pacer: inner.cfg.max_send_rate.filter(|r| *r > 0).map(|rate| Pacer {
                rate,
                next: Mutex::new(tokio::time::Instant::now()),
            }),
            done,
        });
        inner.out.lock().unwrap().insert(id, og.clone());
        let offer = SignalMessage::FileOffer {
            version: default_signal_version(),
            transfer_id: id.0,
            name: name.clone(),
            size,
            blake3_root: root,
            mime: guess_mime(&name),
            chunk_size,
        };
        if let Err(e) = send_signal(inner, &offer).await {
            finish(inner, id, Outcome::Failed(e.to_string()));
            return Err(e);
        }
        emit(
            inner,
            Progress::Offered {
                transfer_id: id,
                direction: Direction::Outgoing,
                name,
                size,
            },
        );
        // Offer timeout.
        let i2 = self.inner.clone();
        let timeout = inner.cfg.offer_timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let still_offered = i2
                .out
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|o| *o.state.lock().unwrap() == OutState::Offered);
            if still_offered {
                let _ = send_signal(&i2, &cancel_msg(id)).await;
                finish(&i2, id, Outcome::Failed("offer timed out".into()));
            }
        });
        Ok(id)
    }

    /// Next offer from the peer awaiting a decision. `None` once the
    /// session is closed.
    pub async fn next_offer(&self) -> Option<IncomingOffer> {
        let mut rx = self.offers_rx.lock().await;
        let mut closed = self.inner.closed.subscribe();
        tokio::select! {
            o = rx.recv() => o,
            _ = closed.wait_for(|c| *c) => rx.try_recv().ok(),
        }
    }

    /// Cancel a transfer in either direction.
    pub async fn cancel(&self, id: TransferId) {
        let known = self.inner.out.lock().unwrap().contains_key(&id)
            || self.inner.inc.lock().unwrap().contains_key(&id);
        if known {
            let _ = send_signal(&self.inner, &cancel_msg(id)).await;
            cancel_local(&self.inner, id, false);
        }
    }

    /// Wait for a transfer to finish. Returns the received file path
    /// (incoming) or the source path (outgoing).
    pub async fn wait(&self, id: TransferId) -> Result<PathBuf> {
        let rx = {
            let f = self.inner.finished.lock().unwrap();
            if let Some(o) = f.get(&id) {
                return outcome_result(o.clone());
            }
            let out = self.inner.out.lock().unwrap().get(&id).map(|o| o.done.subscribe());
            out.or_else(|| self.inner.inc.lock().unwrap().get(&id).map(|i| i.done.subscribe()))
        };
        let outcome = match rx {
            Some(mut rx) => {
                let r = rx.wait_for(|o| o.is_some()).await;
                match r {
                    Ok(v) => v.clone(),
                    Err(_) => None,
                }
            }
            None => None,
        };
        let outcome = outcome
            .or_else(|| self.inner.finished.lock().unwrap().get(&id).cloned())
            .ok_or(FileError::UnknownTransfer)?;
        outcome_result(outcome)
    }

    /// True once the session is gone.
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Resolves when the session is gone.
    pub async fn closed(&self) {
        let mut c = self.inner.closed.subscribe();
        let _ = c.wait_for(|c| *c).await;
    }

    /// Close the session gracefully.
    pub async fn close(&self) {
        {
            let mut ctl = self.inner.ctl.lock().await;
            ctl.finish();
        }
        self.session.close().await;
        let _ = self.inner.closed.send_replace(true);
    }
}

fn outcome_result(o: Outcome) -> Result<PathBuf> {
    match o {
        Outcome::Completed(p) => Ok(p),
        Outcome::Rejected(r) => Err(FileError::Rejected(r)),
        Outcome::Failed(e) => Err(FileError::Failed(e)),
        Outcome::Cancelled => Err(FileError::Cancelled),
    }
}

fn guess_mime(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let m = match ext.as_str() {
        "txt" => "text/plain",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "zip" => "application/zip",
        "json" => "application/json",
        _ => return None,
    };
    Some(m.to_string())
}

fn cancel_msg(id: TransferId) -> SignalMessage {
    SignalMessage::FileCancel {
        version: default_signal_version(),
        transfer_id: id.0,
    }
}

fn emit(inner: &Inner, p: Progress) {
    let _ = inner.events.send(p);
}

async fn send_signal(inner: &Inner, msg: &SignalMessage) -> Result<()> {
    let mut buf = vec![CTL_SIGNAL];
    serde_json::to_writer(&mut buf, msg).map_err(|e| FileError::Protocol(e.to_string()))?;
    inner.ctl.lock().await.send(&buf).await
}

/// Record a final outcome, emit the matching event, stop workers.
fn finish(inner: &Inner, id: TransferId, outcome: Outcome) {
    let Some((og, inc)) = take_transfer(inner, id, &outcome) else {
        return;
    };
    let ev = match &outcome {
        Outcome::Completed(p) => Progress::Completed {
            transfer_id: id,
            path: p.clone(),
        },
        Outcome::Rejected(r) => Progress::Rejected {
            transfer_id: id,
            reason: r.clone(),
        },
        Outcome::Failed(e) => Progress::Failed {
            transfer_id: id,
            error: e.clone(),
        },
        Outcome::Cancelled => Progress::Cancelled {
            transfer_id: id,
            by_peer: false,
        },
    };
    finish_with_event(inner, id, og, inc, outcome, ev);
}

fn finish_with_event(
    inner: &Inner,
    _id: TransferId,
    og: Option<Arc<Outgoing>>,
    inc: Option<Arc<Incoming>>,
    outcome: Outcome,
    ev: Progress,
) {
    if let Some(og) = &og {
        let ok = matches!(outcome, Outcome::Completed(_));
        let _ = og.stop.send_replace(Some(ok));
        og.notify.notify_waiters();
        let _ = og.done.send_replace(Some(outcome.clone()));
    }
    if let Some(inc) = &inc {
        let _ = inc.done.send_replace(Some(outcome.clone()));
    }
    emit(inner, ev);
}

/// Atomically move a transfer from the active maps to `finished`, so that
/// [`FileEndpoint::wait`] always finds it in one place or the other.
fn take_transfer(
    inner: &Inner,
    id: TransferId,
    outcome: &Outcome,
) -> Option<(Option<Arc<Outgoing>>, Option<Arc<Incoming>>)> {
    let mut f = inner.finished.lock().unwrap();
    let og = inner.out.lock().unwrap().remove(&id);
    let inc = inner.inc.lock().unwrap().remove(&id);
    if og.is_none() && inc.is_none() {
        return None;
    }
    if f.len() > 4096 {
        f.clear();
    }
    f.insert(id, outcome.clone());
    Some((og, inc))
}

fn cancel_local(inner: &Inner, id: TransferId, by_peer: bool) {
    let Some((og, inc)) = take_transfer(inner, id, &Outcome::Cancelled) else {
        return;
    };
    if let Some(inc) = &inc {
        if let Some(part) = inc.part.lock().unwrap().take() {
            part.discard();
        }
    }
    finish_with_event(
        inner,
        id,
        og,
        inc,
        Outcome::Cancelled,
        Progress::Cancelled {
            transfer_id: id,
            by_peer,
        },
    );
}

async fn session_lost(inner: &Arc<Inner>, why: &str) {
    let _ = inner.closed.send_replace(true);
    let outs: Vec<TransferId> = inner.out.lock().unwrap().keys().copied().collect();
    for id in outs {
        finish(inner, id, Outcome::Failed(why.to_string()));
    }
    let incs: Vec<Arc<Incoming>> = inner.inc.lock().unwrap().values().cloned().collect();
    for inc in incs {
        let part = inc.part.lock().unwrap().take();
        if let Some(part) = part {
            let _ = tokio::task::spawn_blocking(move || part.persist()).await;
        }
        finish(
            inner,
            inc.id,
            Outcome::Failed(format!("{why}; partial data kept for resume")),
        );
    }
}

// ── control channel ────────────────────────────────────────────────────

async fn control_loop(inner: Arc<Inner>, mut rx: OpenReader) {
    loop {
        let frame = match rx.recv().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                debug!("control stream finished by peer");
                break;
            }
            Err(e) => {
                debug!("control stream ended: {e}");
                break;
            }
        };
        if frame.is_empty() {
            continue;
        }
        match frame[0] {
            CTL_SIGNAL => match serde_json::from_slice::<SignalMessage>(&frame[1..]) {
                Ok(msg) => handle_signal(&inner, msg).await,
                Err(e) => debug!("ignoring unknown control message: {e}"),
            },
            CTL_MANIFEST => handle_manifest(&inner, &frame[1..]),
            k => debug!("ignoring control frame kind {k}"),
        }
    }
    session_lost(&inner, "peer closed the session").await;
}

async fn handle_signal(inner: &Arc<Inner>, msg: SignalMessage) {
    match msg {
        SignalMessage::FileOffer {
            transfer_id,
            name,
            size,
            blake3_root,
            mime,
            chunk_size,
            ..
        } => handle_offer(inner, TransferId(transfer_id), name, size, blake3_root, mime, chunk_size).await,
        SignalMessage::FileAccept {
            transfer_id,
            resume_bitmap,
            ..
        } => handle_accept(inner, TransferId(transfer_id), resume_bitmap).await,
        SignalMessage::FileReject {
            transfer_id,
            reason,
            ..
        } => {
            let id = TransferId(transfer_id);
            if inner.out.lock().unwrap().contains_key(&id) {
                finish(inner, id, Outcome::Rejected(reason));
            }
        }
        SignalMessage::FileProgress {
            transfer_id,
            chunks_done,
            ..
        } => {
            let id = TransferId(transfer_id);
            let og = inner.out.lock().unwrap().get(&id).cloned();
            if let Some(og) = og {
                let done = (chunks_done.min(og.n) * og.chunk_size as u64).min(og.size);
                emit(
                    inner,
                    Progress::Bytes {
                        transfer_id: id,
                        done,
                        total: og.size,
                    },
                );
            }
        }
        SignalMessage::FileComplete {
            transfer_id,
            blake3_root,
            ..
        } => {
            let id = TransferId(transfer_id);
            let og = inner.out.lock().unwrap().get(&id).cloned();
            if let Some(og) = og {
                if *og.state.lock().unwrap() != OutState::Sending {
                    return;
                }
                if blake3_root == og.root {
                    emit(
                        inner,
                        Progress::Bytes {
                            transfer_id: id,
                            done: og.size,
                            total: og.size,
                        },
                    );
                    info!(transfer = %id, "transfer complete (peer verified)");
                    finish(inner, id, Outcome::Completed(og.path.clone()));
                } else {
                    finish(inner, id, Outcome::Failed("peer reported wrong root".into()));
                }
            }
        }
        SignalMessage::FileCancel { transfer_id, .. } => {
            cancel_local(inner, TransferId(transfer_id), true);
        }
        SignalMessage::ChunkNack {
            transfer_id,
            chunks,
            ..
        } => {
            let id = TransferId(transfer_id);
            let og = inner.out.lock().unwrap().get(&id).cloned();
            if let Some(og) = og {
                if *og.state.lock().unwrap() == OutState::Sending {
                    debug!(transfer = %id, n = chunks.len(), "NACK");
                    for c in chunks.into_iter().take(MAX_NACK_ENTRIES) {
                        og.push(c);
                    }
                }
            }
        }
        other => debug!("ignoring non-file signal {:?}", std::mem::discriminant(&other)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_offer(
    inner: &Arc<Inner>,
    id: TransferId,
    raw_name: String,
    size: u64,
    root: [u8; 32],
    mime: Option<String>,
    chunk_size: u32,
) {
    let reject = |reason: String| {
        let inner = inner.clone();
        async move {
            info!(transfer = %id, %reason, "rejecting offer");
            let _ = send_signal(
                &inner,
                &SignalMessage::FileReject {
                    version: default_signal_version(),
                    transfer_id: id.0,
                    reason,
                },
            )
            .await;
        }
    };
    if inner.inc.lock().unwrap().contains_key(&id) || inner.finished.lock().unwrap().contains_key(&id) {
        return reject("duplicate transfer id".into()).await;
    }
    if size > inner.cfg.max_size {
        return reject(format!("file too large ({size} > {} bytes)", inner.cfg.max_size)).await;
    }
    if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_size) {
        return reject(format!("unsupported chunk size {chunk_size}")).await;
    }
    let n = manifest::chunk_count(size, chunk_size);
    if n > MAX_CHUNKS {
        return reject("too many chunks".into()).await;
    }
    let active = inner
        .inc
        .lock()
        .unwrap()
        .values()
        .filter(|i| *i.state.lock().unwrap() != IncState::AwaitingDecision)
        .count();
    if active >= inner.cfg.max_active_incoming {
        return reject("too many active transfers".into()).await;
    }
    let too_many = {
        let mut p = inner.pending_offers.lock().unwrap();
        if *p >= inner.cfg.max_pending_offers {
            true
        } else {
            *p += 1;
            false
        }
    };
    if too_many {
        return reject("too many pending offers".into()).await;
    }
    let name = sanitize_file_name(&raw_name);
    let mime = mime.filter(|m| m.len() <= 128 && m.is_ascii());
    let (hashes, _) = watch::channel(None);
    let (done, _) = watch::channel(None);
    let now = Instant::now();
    let inc = Arc::new(Incoming {
        id,
        name: name.clone(),
        size,
        chunk_size,
        n,
        root,
        state: Mutex::new(IncState::AwaitingDecision),
        part: Mutex::new(None),
        hashes,
        last_activity: Mutex::new(now),
        marks: Mutex::new(Marks {
            last_progress_sent: now,
            last_event: now,
            last_persist: now,
        }),
        bad: Mutex::new(HashMap::new()),
        done,
    });
    inner.inc.lock().unwrap().insert(id, inc.clone());
    let (dtx, drx) = oneshot::channel();
    let offer = IncomingOffer {
        transfer_id: id,
        name: name.clone(),
        size,
        mime,
        peer: inner.peer,
        decision: dtx,
    };
    if inner.offers_tx.try_send(offer).is_err() {
        *inner.pending_offers.lock().unwrap() -= 1;
        inner.inc.lock().unwrap().remove(&id);
        return reject("receiver busy".into()).await;
    }
    emit(
        inner,
        Progress::Offered {
            transfer_id: id,
            direction: Direction::Incoming,
            name,
            size,
        },
    );
    let inner = inner.clone();
    tokio::spawn(async move {
        let decision = tokio::time::timeout(inner.cfg.offer_timeout, drx).await;
        *inner.pending_offers.lock().unwrap() -= 1;
        if !inner.inc.lock().unwrap().contains_key(&id) {
            return; // cancelled meanwhile
        }
        match decision {
            Ok(Ok(Ok(()))) => accept_incoming(&inner, inc).await,
            Ok(Ok(Err(reason))) => {
                let _ = send_signal(
                    &inner,
                    &SignalMessage::FileReject {
                        version: default_signal_version(),
                        transfer_id: id.0,
                        reason: reason.clone(),
                    },
                )
                .await;
                finish(&inner, id, Outcome::Rejected(reason));
            }
            Ok(Err(_)) | Err(_) => {
                let reason = "no decision".to_string();
                let _ = send_signal(
                    &inner,
                    &SignalMessage::FileReject {
                        version: default_signal_version(),
                        transfer_id: id.0,
                        reason: reason.clone(),
                    },
                )
                .await;
                finish(&inner, id, Outcome::Rejected(reason));
            }
        }
    });
}

async fn accept_incoming(inner: &Arc<Inner>, inc: Arc<Incoming>) {
    let id = inc.id;
    let dir = inner.cfg.download_dir.clone();
    let (name, size, cs, root, n) = (inc.name.clone(), inc.size, inc.chunk_size, inc.root, inc.n);
    let part = tokio::task::spawn_blocking(move || PartFile::open_or_create(&dir, &name, size, cs, root, n))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
        .and_then(|r| r);
    let part = match part {
        Ok(p) => p,
        Err(e) => {
            warn!(transfer = %id, "cannot create partial file: {e}");
            let _ = send_signal(
                inner,
                &SignalMessage::FileReject {
                    version: default_signal_version(),
                    transfer_id: id.0,
                    reason: "receiver storage error".into(),
                },
            )
            .await;
            finish(inner, id, Outcome::Failed(format!("storage: {e}")));
            return;
        }
    };
    let have = part.bitmap.count_ones();
    let resume = (have > 0).then(|| part.bitmap.as_bytes().to_vec());
    let resumed_bytes = (0..n)
        .filter(|i| part.bitmap.get(*i))
        .map(|i| manifest::chunk_len(size, cs, i as u32) as u64)
        .sum();
    *inc.part.lock().unwrap() = Some(part);
    *inc.state.lock().unwrap() = IncState::Receiving;
    *inc.last_activity.lock().unwrap() = Instant::now();
    if let Err(e) = send_signal(
        inner,
        &SignalMessage::FileAccept {
            version: default_signal_version(),
            transfer_id: id.0,
            resume_bitmap: resume,
        },
    )
    .await
    {
        warn!(transfer = %id, "send accept: {e}");
        return;
    }
    info!(transfer = %id, resumed_chunks = have, "accepted offer");
    emit(
        inner,
        Progress::Started {
            transfer_id: id,
            direction: Direction::Incoming,
            resumed_bytes,
        },
    );
    // Idle NACK timer.
    let inner = inner.clone();
    tokio::spawn(async move {
        let mut done = inc.done.subscribe();
        let mut tick = tokio::time::interval(inner.cfg.nack_idle);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = done.wait_for(|d| d.is_some()) => break,
            }
            if *inc.state.lock().unwrap() != IncState::Receiving
                || inc.hashes.borrow().is_none()
                || inc.last_activity.lock().unwrap().elapsed() < inner.cfg.nack_idle
            {
                continue;
            }
            let missing: Vec<u32> = {
                let p = inc.part.lock().unwrap();
                match p.as_ref() {
                    Some(p) => p.bitmap.missing().take(MAX_NACK_ENTRIES).map(|i| i as u32).collect(),
                    None => break,
                }
            };
            if missing.is_empty() {
                continue;
            }
            debug!(transfer = %inc.id, n = missing.len(), "idle: NACKing missing chunks");
            *inc.last_activity.lock().unwrap() = Instant::now();
            let _ = send_signal(
                &inner,
                &SignalMessage::ChunkNack {
                    version: default_signal_version(),
                    transfer_id: inc.id.0,
                    chunks: missing,
                },
            )
            .await;
        }
    });
}

async fn handle_accept(inner: &Arc<Inner>, id: TransferId, resume: Option<Vec<u8>>) {
    let og = inner.out.lock().unwrap().get(&id).cloned();
    let Some(og) = og else {
        debug!(transfer = %id, "ignoring unsolicited FileAccept");
        return;
    };
    {
        let mut st = og.state.lock().unwrap();
        if *st != OutState::Offered {
            return;
        }
        *st = OutState::Sending;
    }
    let have = match resume {
        Some(b) => match Bitmap::from_bytes(&b, og.n) {
            Some(bm) => bm,
            None => {
                let _ = send_signal(inner, &cancel_msg(id)).await;
                finish(inner, id, Outcome::Failed("peer sent malformed resume bitmap".into()));
                return;
            }
        },
        None => Bitmap::new(og.n),
    };
    // Manifest first, then chunks.
    let mut buf = Vec::with_capacity(1 + 16 + 32 * og.hashes.len());
    buf.push(CTL_MANIFEST);
    buf.extend_from_slice(&id.0);
    for h in og.hashes.iter() {
        buf.extend_from_slice(h);
    }
    if let Err(e) = inner.ctl.lock().await.send(&buf).await {
        finish(inner, id, Outcome::Failed(e.to_string()));
        return;
    }
    let mut resumed = 0u64;
    for i in 0..og.n {
        if have.get(i) {
            resumed += manifest::chunk_len(og.size, og.chunk_size, i as u32) as u64;
        } else {
            og.push(i as u32);
        }
    }
    info!(transfer = %id, chunks = og.n, resumed_bytes = resumed, "peer accepted, sending");
    emit(
        inner,
        Progress::Started {
            transfer_id: id,
            direction: Direction::Outgoing,
            resumed_bytes: resumed,
        },
    );
    let workers = inner.cfg.streams_per_transfer.clamp(1, 16);
    for _ in 0..workers {
        tokio::spawn(send_worker(inner.clone(), og.clone()));
    }
}

fn handle_manifest(inner: &Arc<Inner>, body: &[u8]) {
    if body.len() < 16 || (body.len() - 16) % 32 != 0 {
        return;
    }
    let id = TransferId(body[..16].try_into().unwrap());
    let inc = inner.inc.lock().unwrap().get(&id).cloned();
    let Some(inc) = inc else { return };
    if *inc.state.lock().unwrap() != IncState::Receiving || inc.hashes.borrow().is_some() {
        return;
    }
    let hashes: Vec<[u8; 32]> = body[16..].chunks_exact(32).map(|c| c.try_into().unwrap()).collect();
    if hashes.len() as u64 != inc.n || manifest::manifest_root(inc.size, inc.chunk_size, &hashes) != inc.root {
        warn!(transfer = %id, "manifest does not match offered root");
        let inner = inner.clone();
        tokio::spawn(async move {
            let _ = send_signal(&inner, &cancel_msg(id)).await;
            finish(&inner, id, Outcome::Failed("manifest does not match offered root".into()));
        });
        return;
    }
    let _ = inc.hashes.send_replace(Some(Arc::new(hashes)));
    *inc.last_activity.lock().unwrap() = Instant::now();
    let inner = inner.clone();
    tokio::spawn(async move { maybe_complete(&inner, &inc).await });
}

// ── data path (reliable QUIC streams) ──────────────────────────────────

async fn send_worker(inner: Arc<Inner>, og: Arc<Outgoing>) {
    let id = og.id;
    let mut stop = og.stop.subscribe();
    let stream = match inner.conn.open_uni().await {
        Ok(s) => s,
        Err(e) => {
            debug!(transfer = %id, "open data stream: {e}");
            return;
        }
    };
    let salt = wire::random_salt();
    let mut stream = stream;
    let mut pre = Vec::with_capacity(21);
    pre.extend_from_slice(&wire::MAGIC_DATA);
    pre.push(wire::WIRE_VERSION);
    pre.extend_from_slice(&salt);
    if stream.write_all(&pre).await.is_err() {
        return;
    }
    let mut w = SealWriter::new(stream, &inner.send_key, salt, wire::MAGIC_DATA);
    let file = match std::fs::File::open(&og.path) {
        Ok(f) => Arc::new(f),
        Err(e) => {
            fail_out(&inner, &og, format!("open source: {e}")).await;
            return;
        }
    };
    loop {
        let notified = og.notify.notified();
        if let Some(s) = *stop.borrow() {
            if s {
                w.finish();
            } else {
                w.reset(codes::CANCELLED);
            }
            return;
        }
        let Some(idx) = og.pop() else {
            tokio::select! {
                _ = notified => {}
                _ = stop.changed() => {}
            }
            continue;
        };
        let len = manifest::chunk_len(og.size, og.chunk_size, idx);
        let off = idx as u64 * og.chunk_size as u64;
        let f2 = file.clone();
        let read = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            store::read_at(&f2, &mut buf, off).map(|_| buf)
        })
        .await;
        let mut data = match read {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => return fail_out(&inner, &og, format!("read source: {e}")).await,
            Err(e) => return fail_out(&inner, &og, format!("read task: {e}")).await,
        };
        if manifest::hash_chunk(&data) != og.hashes[idx as usize] {
            return fail_out(&inner, &og, "source file changed while sending".into()).await;
        }
        if og.corrupt_once.lock().unwrap().remove(&idx) {
            data[0] ^= 0xff; // test hook
        }
        let mut start = Vec::with_capacity(21);
        start.push(DATA_CHUNK_START);
        start.extend_from_slice(&id.0);
        start.extend_from_slice(&idx.to_be_bytes());
        if w.send(&start).await.is_err() {
            og.push(idx);
            return;
        }
        let mut frame = Vec::with_capacity(wire::DATA_FRAME_MAX);
        for piece in data.chunks(wire::DATA_FRAME_MAX - 1) {
            if let Some(p) = &og.pacer {
                p.wait(piece.len()).await;
            }
            frame.clear();
            frame.push(DATA_CHUNK_BYTES);
            frame.extend_from_slice(piece);
            if w.send(&frame).await.is_err() {
                og.push(idx);
                return;
            }
        }
    }
}

async fn fail_out(inner: &Arc<Inner>, og: &Outgoing, err: String) {
    warn!(transfer = %og.id, "{err}");
    let _ = send_signal(inner, &cancel_msg(og.id)).await;
    finish(inner, og.id, Outcome::Failed(err));
}

async fn data_accept_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<IncomingData>) {
    while let Some(d) = rx.recv().await {
        tokio::spawn(recv_stream(inner.clone(), d));
    }
}

async fn recv_stream(inner: Arc<Inner>, d: IncomingData) {
    let mut r = OpenReader::new(d.stream, &inner.recv_key, d.salt, wire::MAGIC_DATA, wire::DATA_FRAME_LIMIT);
    loop {
        let frame = match r.recv().await {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) => {
                debug!("data stream ended: {e}");
                return;
            }
        };
        if frame.len() != 21 || frame[0] != DATA_CHUNK_START {
            r.stop(codes::PROTOCOL);
            return;
        }
        let id = TransferId(frame[1..17].try_into().unwrap());
        let idx = u32::from_be_bytes(frame[17..21].try_into().unwrap());
        let inc = inner.inc.lock().unwrap().get(&id).cloned();
        let Some(inc) = inc.filter(|i| *i.state.lock().unwrap() == IncState::Receiving) else {
            warn!(transfer = %id, "unsolicited chunk data, stopping stream");
            r.stop(codes::UNSOLICITED);
            return;
        };
        if idx as u64 >= inc.n {
            r.stop(codes::PROTOCOL);
            return;
        }
        // Manifest must be in before we can verify anything.
        let hashes = {
            let mut hrx = inc.hashes.subscribe();
            let mut done = inc.done.subscribe();
            let got = tokio::select! {
                h = tokio::time::timeout(Duration::from_secs(30), hrx.wait_for(|h| h.is_some())) => {
                    h.ok().and_then(|r| r.ok()).and_then(|h| h.clone())
                }
                _ = done.wait_for(|d| d.is_some()) => None,
            };
            match got {
                Some(h) => h,
                None => {
                    r.stop(codes::CANCELLED);
                    return;
                }
            }
        };
        let len = manifest::chunk_len(inc.size, inc.chunk_size, idx);
        let mut buf = Vec::with_capacity(len);
        while buf.len() < len {
            let f = match r.recv().await {
                Ok(Some(f)) => f,
                _ => return,
            };
            if f.is_empty() || f[0] != DATA_CHUNK_BYTES || buf.len() + f.len() - 1 > len {
                r.stop(codes::PROTOCOL);
                return;
            }
            buf.extend_from_slice(&f[1..]);
            *inc.last_activity.lock().unwrap() = Instant::now();
        }
        if inc.done.borrow().is_some() {
            r.stop(codes::CANCELLED);
            return;
        }
        if manifest::hash_chunk(&buf) != hashes[idx as usize] {
            let tries = {
                let mut b = inc.bad.lock().unwrap();
                let e = b.entry(idx).or_insert(0);
                *e += 1;
                *e
            };
            warn!(transfer = %id, chunk = idx, tries, "chunk failed BLAKE3 verification, NACKing");
            if tries > inner.cfg.max_chunk_retries {
                let _ = send_signal(&inner, &cancel_msg(id)).await;
                finish(&inner, id, Outcome::Failed(format!("chunk {idx} kept failing verification")));
                return;
            }
            let _ = send_signal(
                &inner,
                &SignalMessage::ChunkNack {
                    version: default_signal_version(),
                    transfer_id: id.0,
                    chunks: vec![idx],
                },
            )
            .await;
            continue;
        }
        if let Err(e) = store_chunk(&inc, idx, buf).await {
            let _ = send_signal(&inner, &cancel_msg(id)).await;
            finish(&inner, id, Outcome::Failed(format!("write: {e}")));
            return;
        }
        report_progress(&inner, &inc).await;
        maybe_complete(&inner, &inc).await;
    }
}

async fn store_chunk(inc: &Arc<Incoming>, idx: u32, buf: Vec<u8>) -> std::io::Result<()> {
    let inc2 = inc.clone();
    tokio::task::spawn_blocking(move || {
        let mut g = inc2.part.lock().unwrap();
        let Some(part) = g.as_mut() else {
            return Ok(()); // cancelled / session lost meanwhile
        };
        if part.bitmap.get(idx as u64) {
            return Ok(());
        }
        part.write_chunk(idx, &buf)?;
        part.bitmap.set(idx as u64);
        let mut m = inc2.marks.lock().unwrap();
        if m.last_persist.elapsed() >= Duration::from_secs(1) {
            m.last_persist = Instant::now();
            drop(m);
            part.persist()?;
        }
        Ok(())
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

fn chunks_done(inc: &Incoming) -> (u64, u64) {
    let g = inc.part.lock().unwrap();
    match g.as_ref() {
        Some(p) => {
            let n = p.bitmap.count_ones();
            let mut bytes = n * inc.chunk_size as u64;
            if p.bitmap.get(inc.n.saturating_sub(1)) {
                bytes = bytes - inc.chunk_size as u64
                    + manifest::chunk_len(inc.size, inc.chunk_size, (inc.n - 1) as u32) as u64;
            }
            (n, bytes)
        }
        None => (0, 0),
    }
}

async fn report_progress(inner: &Arc<Inner>, inc: &Arc<Incoming>) {
    let (n, bytes) = chunks_done(inc);
    let (ev, sig) = {
        let mut m = inc.marks.lock().unwrap();
        let full = n == inc.n;
        let ev = full || m.last_event.elapsed() >= Duration::from_millis(100);
        if ev {
            m.last_event = Instant::now();
        }
        let sig = !full && m.last_progress_sent.elapsed() >= Duration::from_millis(250);
        if sig {
            m.last_progress_sent = Instant::now();
        }
        (ev, sig)
    };
    if ev {
        emit(
            inner,
            Progress::Bytes {
                transfer_id: inc.id,
                done: bytes,
                total: inc.size,
            },
        );
    }
    if sig {
        let _ = send_signal(
            inner,
            &SignalMessage::FileProgress {
                version: default_signal_version(),
                transfer_id: inc.id.0,
                chunks_done: n,
            },
        )
        .await;
    }
}

async fn maybe_complete(inner: &Arc<Inner>, inc: &Arc<Incoming>) {
    if inc.hashes.borrow().is_none() {
        return;
    }
    {
        let full = inc
            .part
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|p| p.bitmap.is_full());
        if !full {
            return;
        }
        let mut st = inc.state.lock().unwrap();
        if *st != IncState::Receiving {
            return;
        }
        *st = IncState::Finishing;
    }
    let Some(part) = inc.part.lock().unwrap().take() else {
        return;
    };
    let dir = inner.cfg.download_dir.clone();
    let res = tokio::task::spawn_blocking(move || part.finalize(&dir))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
        .and_then(|r| r);
    match res {
        Ok(path) => {
            let _ = send_signal(
                inner,
                &SignalMessage::FileComplete {
                    version: default_signal_version(),
                    transfer_id: inc.id.0,
                    blake3_root: inc.root,
                },
            )
            .await;
            info!(transfer = %inc.id, path = %path.display(), "file received and verified");
            finish(inner, inc.id, Outcome::Completed(path));
        }
        Err(e) => {
            let _ = send_signal(inner, &cancel_msg(inc.id)).await;
            finish(inner, inc.id, Outcome::Failed(format!("finalize: {e}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ConnectOptions, connect_direct, listen_direct};

    /// B is a real endpoint; A is a hand-driven (possibly malicious) peer.
    async fn raw_pair(dir: &Path) -> (Session, FileEndpoint) {
        let l = listen_direct("127.0.0.1:0".parse().unwrap(), &[1u8; 32]).await.unwrap();
        let addr = l.local_addr().unwrap();
        let o = ConnectOptions::default();
        let (a, b) = tokio::join!(connect_direct(addr, &[2u8; 32], &o), l.accept(&o));
        let cfg = FileConfig {
            download_dir: dir.to_path_buf(),
            chunk_size: MIN_CHUNK_SIZE,
            ..Default::default()
        };
        (a.unwrap(), FileEndpoint::new(b.unwrap(), cfg))
    }

    async fn raw_signal(a: &mut Session, msg: &SignalMessage) {
        let mut buf = vec![CTL_SIGNAL];
        serde_json::to_writer(&mut buf, msg).unwrap();
        a.ctl_tx.as_mut().unwrap().send(&buf).await.unwrap();
    }

    async fn raw_data_stream(a: &Session) -> SealWriter {
        let mut s = a.conn.open_uni().await.unwrap();
        let salt = wire::random_salt();
        let mut pre = wire::MAGIC_DATA.to_vec();
        pre.push(wire::WIRE_VERSION);
        pre.extend_from_slice(&salt);
        s.write_all(&pre).await.unwrap();
        SealWriter::new(s, &a.send_key, salt, wire::MAGIC_DATA)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsolicited_chunks_and_accepts_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, b) = raw_pair(dir.path()).await;

        // Unsolicited FileAccept / ChunkNack / FileComplete: ignored.
        let bogus = [9u8; 16];
        raw_signal(&mut a, &SignalMessage::FileAccept { version: 1, transfer_id: bogus, resume_bitmap: None }).await;
        raw_signal(&mut a, &SignalMessage::ChunkNack { version: 1, transfer_id: bogus, chunks: vec![1] }).await;
        raw_signal(&mut a, &SignalMessage::FileComplete { version: 1, transfer_id: bogus, blake3_root: [0; 32] }).await;

        // Chunk data for a transfer that was never offered/accepted.
        let mut w = raw_data_stream(&a).await;
        let mut start = vec![DATA_CHUNK_START];
        start.extend_from_slice(&bogus);
        start.extend_from_slice(&0u32.to_be_bytes());
        w.send(&start).await.unwrap();
        let mut d = vec![DATA_CHUNK_BYTES];
        d.extend_from_slice(&[0xAA; 1000]);
        let _ = w.send(&d).await;
        let code = tokio::time::timeout(Duration::from_secs(5), w.stopped()).await.unwrap();
        assert_eq!(code, Some(codes::UNSOLICITED));

        // Offered but not yet accepted: still refused.
        let id = [7u8; 16];
        raw_signal(&mut a, &SignalMessage::FileOffer {
            version: 1, transfer_id: id, name: "../../evil.txt".into(), size: 10,
            blake3_root: [0; 32], mime: None, chunk_size: MIN_CHUNK_SIZE,
        }).await;
        let offer = tokio::time::timeout(Duration::from_secs(5), b.next_offer()).await.unwrap().unwrap();
        assert_eq!(offer.name, "evil.txt", "peer-supplied path components stripped");
        let mut w = raw_data_stream(&a).await;
        let mut start = vec![DATA_CHUNK_START];
        start.extend_from_slice(&id);
        start.extend_from_slice(&0u32.to_be_bytes());
        w.send(&start).await.unwrap();
        let code = tokio::time::timeout(Duration::from_secs(5), w.stopped()).await.unwrap();
        assert_eq!(code, Some(codes::UNSOLICITED));
        offer.reject("no");

        // Bad chunk size / duplicate id are rejected without prompting.
        raw_signal(&mut a, &SignalMessage::FileOffer {
            version: 1, transfer_id: [8u8; 16], name: "x".into(), size: 10,
            blake3_root: [0; 32], mime: None, chunk_size: 3,
        }).await;
        let mut rx = a.ctl_rx.take().unwrap();
        let mut rejected = 0;
        while rejected < 2 {
            let f = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap().unwrap();
            if let Ok(SignalMessage::FileReject { reason, .. }) = serde_json::from_slice::<SignalMessage>(&f[1..]) {
                rejected += 1;
                assert!(reason == "no" || reason.contains("chunk size"), "{reason}");
            }
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing written");
    }
}
