//! Public data types: ids, transfer snapshots, events, config, errors.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use wzp_files::{FileConfig, FileSessionInvite};
use wzp_iroh::{EndpointId, IrohConfig, PresenceState};

/// App-level transfer id (16 random bytes, hex on the wire / in JSON).
///
/// Independent of the wzp-files `TransferId`, because it exists before
/// the WZP session does (it is in the invite).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShareId(pub [u8; 16]);

impl ShareId {
    pub fn random() -> Self {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        Self(b)
    }
}

impl std::fmt::Display for ShareId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl std::fmt::Debug for ShareId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShareId({self})")
    }
}

impl std::str::FromStr for ShareId {
    type Err = ShareError;
    fn from_str(s: &str) -> Result<Self> {
        let v = hex::decode(s.trim()).map_err(|_| ShareError::InvalidId(s.to_string()))?;
        let b: [u8; 16] = v
            .try_into()
            .map_err(|_| ShareError::InvalidId(s.to_string()))?;
        Ok(Self(b))
    }
}

impl Serialize for ShareId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ShareId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Which way a transfer goes, from this node's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Outgoing,
    Incoming,
}

/// Lifecycle of a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    /// Invite sent / received; WZP session being set up.
    Connecting,
    /// Outgoing: offer made, waiting for the peer's decision.
    /// Incoming: waiting for *our* `accept` / `reject`.
    AwaitingDecision,
    /// Bytes flowing.
    Transferring,
    Completed,
    Rejected,
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            TransferState::Completed
                | TransferState::Rejected
                | TransferState::Failed
                | TransferState::Cancelled
        )
    }
}

/// Snapshot of one transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransferInfo {
    pub id: ShareId,
    pub contact: EndpointId,
    pub contact_name: String,
    pub direction: Direction,
    /// File name (outgoing: source file name; incoming: sanitized offer
    /// name, empty until the offer arrived).
    pub name: String,
    pub size: u64,
    /// Outgoing: source path. Incoming: final path once completed.
    pub path: Option<PathBuf>,
    pub state: TransferState,
    /// Verified bytes at the receiver.
    pub done: u64,
    /// `Some(true)` if the WZP session goes through the relay.
    pub relayed: Option<bool>,
    /// 4-digit verification code of the WZP session (both sides see the same).
    pub sas: Option<u32>,
    /// Reject reason or error text.
    pub error: Option<String>,
}

/// Unified event stream for the UI (`#[serde(tag = "type")]`, snake_case).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShareEvent {
    Presence {
        id: EndpointId,
        state: PresenceState,
        last_seen: Option<u64>,
    },
    ContactRequest {
        id: EndpointId,
        fingerprint: String,
        name: Option<String>,
        contact: Option<String>,
    },
    Text {
        from: EndpointId,
        text: String,
        sent_at_ms: u64,
    },
    /// New transfer (outgoing: just started; incoming: offer arrived and
    /// awaits `accept`/`reject`, unless auto-accept is on).
    TransferOffered {
        transfer: TransferInfo,
    },
    /// Bytes started flowing.
    TransferStarted {
        transfer: TransferInfo,
    },
    TransferProgress {
        id: ShareId,
        done: u64,
        total: u64,
    },
    TransferCompleted {
        transfer: TransferInfo,
    },
    TransferRejected {
        transfer: TransferInfo,
    },
    TransferFailed {
        transfer: TransferInfo,
    },
    TransferCancelled {
        transfer: TransferInfo,
        by_peer: bool,
    },
    /// A file invite from a contact was refused before any connection
    /// (bad payload, fingerprint mismatch, too many pending).
    InviteRefused {
        from: EndpointId,
        reason: String,
    },
}

/// What goes into `Envelope::FileInvite`: a [`FileSessionInvite`] (flattened,
/// so the JSON is a valid `FileSessionInvite`) plus the app-level id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareInvite {
    #[serde(flatten)]
    pub invite: FileSessionInvite,
    pub share_id: ShareId,
}

/// Sent back over iroh (`Envelope::FileInvite`) to abort an invite: by the
/// receiver when it can't connect, by the sender when it cancels early.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShareAbort {
    pub share_id: ShareId,
    pub abort: String,
}

/// Any `FileInvite` payload wzp-share understands.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum InvitePayload {
    Abort(ShareAbort),
    Invite(ShareInvite),
}

/// Configuration for [`crate::ShareNode::start`].
///
/// `Debug` redacts secrets (`relay_token`, `iroh.relay_auth_token`).
#[derive(Clone)]
pub struct ShareConfig {
    /// iroh node config. `iroh.data_dir` defaults to `data_dir` if `None`.
    pub iroh: IrohConfig,
    /// wzp-files config (download dir, limits).
    pub files: FileConfig,
    /// WZP relay used as fallback rendezvous (user-configured). `None` =
    /// direct only.
    pub relay: Option<SocketAddr>,
    /// **Secret** access token for `relay`. Only the host side uses it to
    /// authenticate; it is never logged and never put into an envelope or
    /// invite (the invite carries at most a room-scoped ticket). Without it
    /// the relay fallback is unavailable.
    pub relay_token: Option<String>,
    /// Directory for contacts etc.
    pub data_dir: PathBuf,
    /// Bind address for per-transfer direct listeners (`None` = no direct
    /// path, relay only). Default `0.0.0.0:0`.
    pub direct_bind: Option<SocketAddr>,
    /// Accept every offer from a contact without asking.
    pub auto_accept: bool,
    /// QUIC connect + handshake timeout (direct candidates are capped at 5 s).
    pub connect_timeout: Duration,
    /// How long either side waits for the peer in the relay room / for the
    /// direct connection.
    pub peer_wait: Duration,
    /// Max invites being joined at once; more are refused.
    pub max_incoming_sessions: usize,
    /// Test hook: replace the advertised direct candidates.
    #[doc(hidden)]
    pub test_direct_override: Option<Vec<SocketAddr>>,
    /// Test hook: use `relay` without a token (in-process test relays).
    #[doc(hidden)]
    pub test_open_relay: bool,
}

impl std::fmt::Debug for ShareConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |o: &Option<String>| o.as_ref().map(|_| "<redacted>");
        let mut iroh = self.iroh.clone();
        if iroh.relay_auth_token.is_some() {
            iroh.relay_auth_token = Some("<redacted>".into());
        }
        f.debug_struct("ShareConfig")
            .field("iroh", &iroh)
            .field("files", &self.files)
            .field("relay", &self.relay)
            .field("relay_token", &redact(&self.relay_token))
            .field("data_dir", &self.data_dir)
            .field("direct_bind", &self.direct_bind)
            .field("auto_accept", &self.auto_accept)
            .field("connect_timeout", &self.connect_timeout)
            .field("peer_wait", &self.peer_wait)
            .field("max_incoming_sessions", &self.max_incoming_sessions)
            .finish_non_exhaustive()
    }
}

impl ShareConfig {
    /// Defaults with everything under `data_dir` (downloads in
    /// `data_dir/downloads`).
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            iroh: IrohConfig::default(),
            files: FileConfig {
                download_dir: data_dir.join("downloads"),
                ..FileConfig::default()
            },
            relay: None,
            relay_token: None,
            data_dir,
            direct_bind: Some("0.0.0.0:0".parse().unwrap()),
            auto_accept: false,
            connect_timeout: Duration::from_secs(20),
            peer_wait: Duration::from_secs(120),
            max_incoming_sessions: 8,
            test_direct_override: None,
            test_open_relay: false,
        }
    }
}

/// wzp-share errors.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error(transparent)]
    Iroh(#[from] wzp_iroh::Error),
    #[error(transparent)]
    Files(#[from] wzp_files::FileError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown transfer {0}")]
    UnknownTransfer(ShareId),
    #[error("invalid transfer id {0:?}")]
    InvalidId(String),
    #[error("transfer {0} is not awaiting a decision")]
    NotPending(ShareId),
    #[error("transfer failed: {0}")]
    Failed(String),
    #[error("transfer rejected: {0}")]
    Rejected(String),
    #[error("transfer cancelled")]
    Cancelled,
}

pub type Result<T, E = ShareError> = std::result::Result<T, E>;
