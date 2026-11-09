//! Wire format: ALPNs, [`Envelope`] and length-prefixed JSON framing
//! (4-byte big-endian length + `serde_json`, same as
//! `wzp-transport::reliable`).

use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

/// Signaling ALPN: one bidi stream per [`Envelope`], answered by a [`Reply`].
pub const SIGNAL_ALPN: &[u8] = b"wzp/signal/1";
/// Presence ALPN: one bidi stream per [`PresenceMsg`] exchange.
pub const PRESENCE_ALPN: &[u8] = b"wzp/presence/1";

/// Maximum frame size accepted from a known contact (1 MiB, as in
/// `wzp-transport::reliable`).
pub const MAX_FRAME: usize = 1 << 20;
/// Maximum frame size read from an unknown peer (contact request).
pub const MAX_UNKNOWN_FRAME: usize = 4 * 1024;

/// Application payload carried over [`SIGNAL_ALPN`].
///
/// JSON-encoded, adjacently tagged: `{"type":"Text","body":{...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum Envelope {
    /// A WZP call-signaling message (offer/answer/hangup/...).
    Signal(wzp_proto::SignalMessage),
    /// A file-session invite. Opaque here: the `wzp-files` crate defines the
    /// struct (`FileSessionInvite`); the app layer (de)serializes it.
    FileInvite(serde_json::Value),
    /// A short text message.
    Text {
        text: String,
        /// Sender's clock, Unix milliseconds.
        sent_at_ms: u64,
    },
    /// Introduce yourself to a peer that may not know you yet. Delivered as
    /// a `ContactRequest` event if you are not their contact.
    Hello { name: String, contact: String },
    /// Application-level liveness probe (acknowledged, not surfaced).
    Ping,
}

impl Envelope {
    /// Convenience constructor for [`Envelope::Text`] stamped with now.
    pub fn text(text: impl Into<String>) -> Self {
        Envelope::Text {
            text: text.into(),
            sent_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

/// Receiver's answer to an [`Envelope`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Reply {
    Ack,
    Rejected { reason: String },
}

/// Presence protocol frames.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum PresenceMsg {
    Ping,
    Pong,
    /// Sent on graceful shutdown so contacts flip to offline immediately.
    Bye,
}

/// Write one length-prefixed JSON frame.
pub async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let json = serde_json::to_vec(msg)?;
    if json.len() > MAX_FRAME {
        return Err(Error::Protocol(format!("frame too large: {}", json.len())));
    }
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&(json.len() as u32).to_be_bytes());
    buf.extend_from_slice(&json);
    send.write_all(&buf)
        .await
        .map_err(|e| Error::Protocol(format!("write: {e}")))
}

/// Read one length-prefixed JSON frame of at most `max` bytes.
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream, max: usize) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len)
        .await
        .map_err(|e| Error::Protocol(format!("read length: {e}")))?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err(Error::Protocol(format!("frame too large: {len} > {max}")));
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf)
        .await
        .map_err(|e| Error::Protocol(format!("read payload: {e}")))?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_json_shape() {
        let e = Envelope::FileInvite(serde_json::json!({"relay":"1.2.3.4:4433","token":"abc"}));
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.starts_with(r#"{"type":"FileInvite","body":"#), "{s}");
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Envelope::FileInvite(v) if v["token"] == "abc"));
        let s = serde_json::to_string(&Envelope::Ping).unwrap();
        assert_eq!(s, r#"{"type":"Ping"}"#);
        let s = serde_json::to_string(&Envelope::Signal(wzp_proto::SignalMessage::Reflect)).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Envelope::Signal(wzp_proto::SignalMessage::Reflect)));
    }
}
