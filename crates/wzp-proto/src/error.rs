use thiserror::Error;

/// Errors from audio codec operations.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("encode failed: {0}")]
    EncodeFailed(String),
    #[error("decode failed: {0}")]
    DecodeFailed(String),
    #[error("unsupported profile transition from {from:?} to {to:?}")]
    UnsupportedTransition {
        from: crate::CodecId,
        to: crate::CodecId,
    },
}

/// Errors from FEC operations.
#[derive(Debug, Error)]
pub enum FecError {
    #[error("source block is full (max {max} symbols)")]
    BlockFull { max: usize },
    #[error("decode impossible: need {needed} symbols, have {have}")]
    InsufficientSymbols { needed: usize, have: usize },
    #[error("invalid block id {0}")]
    InvalidBlock(u8),
    #[error("internal FEC error: {0}")]
    Internal(String),
}

/// Errors from cryptographic operations.
#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("decryption failed (bad key or tampered data)")]
    DecryptionFailed,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("rekey failed: {0}")]
    RekeyFailed(String),
    #[error("anti-replay: duplicate or old packet (seq={seq})")]
    ReplayDetected { seq: u16 },
    #[error("internal crypto error: {0}")]
    Internal(String),
}

/// Errors from transport operations.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("connection lost")]
    ConnectionLost,
    #[error("datagram too large: {size} bytes (max {max})")]
    DatagramTooLarge { size: usize, max: usize },
    #[error("connection timeout after {ms}ms")]
    Timeout { ms: u64 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Parsed wire bytes successfully but the payload didn't
    /// deserialize into a known `SignalMessage` variant. Usually
    /// means the peer is running a newer build with a variant we
    /// don't know yet. Callers should **log and continue** rather
    /// than tearing down the connection, so that forward-compat
    /// additions to `SignalMessage` don't silently kill old
    /// clients/relays.
    #[error("signal deserialize: {0}")]
    Deserialize(String),
    #[error("internal transport error: {0}")]
    Internal(String),
}

/// Errors from obfuscation layer.
#[derive(Debug, Error)]
pub enum ObfuscationError {
    #[error("obfuscation failed: {0}")]
    Failed(String),
    #[error("deobfuscation failed: invalid framing")]
    InvalidFraming,
}
