//! # wzp-files — file transfer over the WZP protocol
//!
//! Sends files of up to 10 GiB (configurable) between two peers over a WZP
//! QUIC connection, either relayed through a `wzp-relay` room or direct.
//!
//! * **E2E encrypted**: the peers run a signed X25519 handshake bound to
//!   their WZP identities (see [`session`]); every stream is sealed with
//!   per-direction XChaCha20-Poly1305 keys. A relay only splices ciphertext.
//! * **Verified per chunk**: files are split into chunks hashed with BLAKE3;
//!   the receiver checks the chunk-hash manifest against the offered root
//!   and verifies each chunk on arrival, NACKing bad ones.
//! * **Consent first**: nothing touches disk until the application accepts
//!   an [`IncomingOffer`].
//! * **Resumable**: partial data lives in `<name>.wzpart` with a persisted
//!   chunk bitmap; re-offering the same file resumes where it stopped.
//! * **Bounded memory**: files are streamed from/to disk chunk by chunk.
//!
//! Data path: reliable QUIC unidirectional streams (path "B"). A RaptorQ
//! datagram path ("A") is a documented follow-up, see docs/FILE-TRANSFER.md.
//!
//! ```no_run
//! # async fn demo() -> wzp_files::Result<()> {
//! use wzp_files::*;
//! let seed = [7u8; 32];
//! let session = connect_via_relay("203.0.113.1:4433".parse().unwrap(), "file-abc", &seed).await?;
//! let ep = FileEndpoint::new(session, FileConfig::default());
//! let id = ep.send(std::path::Path::new("movie.mkv")).await?;
//! ep.wait(id).await?;
//! # Ok(()) }
//! ```

mod endpoint;
pub mod manifest;
pub mod session;
mod store;
mod wire;

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use endpoint::{FileEndpoint, IncomingOffer};
pub use session::{
    ConnectOptions, DIRECT_CONTEXT, DirectListener, FILE_ROOM_PREFIX, FileSessionInvite,
    RelayRendezvous, Session, bulk_transport_config, connect_direct, connect_via_relay,
    connect_via_relay_with, fingerprint_of_seed, host_invite, join_invite, listen_direct,
    random_room, relay_room_name,
};
pub use wzp_crypto::Fingerprint;
pub use wzp_proto::sanitize_file_name;

/// Default per-file size limit: 10 GiB.
pub const DEFAULT_MAX_SIZE: u64 = 10 * 1024 * 1024 * 1024;
/// Default chunk size: 1 MiB.
pub const DEFAULT_CHUNK_SIZE: u32 = 1024 * 1024;
/// Smallest chunk size accepted from a peer.
pub const MIN_CHUNK_SIZE: u32 = 16 * 1024;
/// Largest chunk size accepted from a peer (bounds receiver memory).
pub const MAX_CHUNK_SIZE: u32 = 4 * 1024 * 1024;
/// Max chunks per file (bounds the manifest to 8 MiB).
pub const MAX_CHUNKS: u64 = 262_144;

/// 128-bit random transfer identifier.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TransferId(pub [u8; 16]);

impl TransferId {
    pub fn random() -> Self {
        use rand::RngCore;
        let mut b = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut b);
        Self(b)
    }
}

impl std::fmt::Display for TransferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl std::fmt::Debug for TransferId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TransferId({self})")
    }
}

impl std::str::FromStr for TransferId {
    type Err = FileError;
    fn from_str(s: &str) -> Result<Self> {
        let v = hex::decode(s).map_err(|e| FileError::Invalid(format!("transfer id: {e}")))?;
        let b: [u8; 16] = v
            .try_into()
            .map_err(|_| FileError::Invalid("transfer id must be 16 bytes".into()))?;
        Ok(Self(b))
    }
}

/// Which way a transfer goes, from this endpoint's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Outgoing,
    Incoming,
}

/// Transfer lifecycle events (see [`FileEndpoint::events`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Progress {
    /// Offer sent (outgoing) or received and awaiting a decision (incoming).
    Offered {
        transfer_id: TransferId,
        direction: Direction,
        name: String,
        size: u64,
    },
    /// Peer accepted / we accepted; bytes start flowing.
    Started {
        transfer_id: TransferId,
        direction: Direction,
        /// Bytes already present at the receiver from an earlier attempt.
        resumed_bytes: u64,
    },
    /// Verified bytes at the receiver.
    Bytes {
        transfer_id: TransferId,
        done: u64,
        total: u64,
    },
    /// Done. Outgoing: the source path; incoming: the final file path.
    Completed {
        transfer_id: TransferId,
        path: PathBuf,
    },
    /// The receiver declined the offer.
    Rejected {
        transfer_id: TransferId,
        reason: String,
    },
    Failed {
        transfer_id: TransferId,
        error: String,
    },
    Cancelled {
        transfer_id: TransferId,
        by_peer: bool,
    },
}

impl Progress {
    pub fn transfer_id(&self) -> TransferId {
        match self {
            Progress::Offered { transfer_id, .. }
            | Progress::Started { transfer_id, .. }
            | Progress::Bytes { transfer_id, .. }
            | Progress::Completed { transfer_id, .. }
            | Progress::Rejected { transfer_id, .. }
            | Progress::Failed { transfer_id, .. }
            | Progress::Cancelled { transfer_id, .. } => *transfer_id,
        }
    }
}

/// File-transfer configuration.
#[derive(Clone, Debug)]
pub struct FileConfig {
    /// Max size of a single file, both for sending and accepting.
    pub max_size: u64,
    /// Chunk size for files we send.
    pub chunk_size: u32,
    /// Where accepted files are written.
    pub download_dir: PathBuf,
    /// Parallel QUIC streams per outgoing transfer.
    pub streams_per_transfer: usize,
    /// Optional send-rate cap in bytes/second (per transfer).
    pub max_send_rate: Option<u64>,
    /// How long an offer may wait for a decision (both ends).
    pub offer_timeout: Duration,
    /// Max offers waiting for a decision; further offers are rejected.
    pub max_pending_offers: usize,
    /// Max concurrently active incoming transfers.
    pub max_active_incoming: usize,
    /// Receiver NACKs missing chunks after this much silence.
    pub nack_idle: Duration,
    /// Give up after a chunk failed verification this many times.
    pub max_chunk_retries: u32,
    /// Test hook: chunks the *sender* corrupts once after its own check.
    #[doc(hidden)]
    pub test_corrupt_chunks: Vec<u32>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_SIZE,
            chunk_size: DEFAULT_CHUNK_SIZE,
            download_dir: PathBuf::from("."),
            streams_per_transfer: 4,
            max_send_rate: None,
            offer_timeout: Duration::from_secs(300),
            max_pending_offers: 8,
            max_active_incoming: 8,
            nack_idle: Duration::from_secs(2),
            max_chunk_retries: 5,
            test_corrupt_chunks: Vec::new(),
        }
    }
}

/// wzp-files error type.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("authentication: {0}")]
    Auth(String),
    #[error("timeout: {0}")]
    Timeout(&'static str),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("file too large: {size} > {max}")]
    TooLarge { size: u64, max: u64 },
    #[error("rejected by peer: {0}")]
    Rejected(String),
    #[error("transfer failed: {0}")]
    Failed(String),
    #[error("transfer cancelled")]
    Cancelled,
    #[error("unknown transfer")]
    UnknownTransfer,
    #[error("session closed")]
    Closed,
}

impl From<wzp_proto::TransportError> for FileError {
    fn from(e: wzp_proto::TransportError) -> Self {
        FileError::Transport(e.to_string())
    }
}

pub type Result<T, E = FileError> = std::result::Result<T, E>;
