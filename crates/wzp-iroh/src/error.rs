//! Error type for `wzp-iroh`.

use iroh::EndpointId;

/// Errors returned by `wzp-iroh`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid contact string: {0}")]
    InvalidContact(String),
    #[error("unknown contact {0}")]
    UnknownContact(EndpointId),
    #[error("peer {0} rejected the message: {1}")]
    Rejected(EndpointId, String),
    #[error("cannot send to self")]
    SelfContact,
    #[error("invalid config: {0}")]
    Config(String),
    #[error("bind failed: {0}")]
    Bind(String),
    #[error("connect to {peer} failed: {reason}")]
    Connect { peer: EndpointId, reason: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("timed out")]
    Timeout,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
