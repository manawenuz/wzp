//! Reliable stream transport for signaling messages.
//!
//! Uses length-prefixed framing (4-byte big-endian length + serde_json) over QUIC streams.

use bytes::{BufMut, BytesMut};
use quinn::Connection;
use wzp_proto::{SignalMessage, TransportError};

/// Send a signaling message over a new bidirectional QUIC stream.
///
/// Opens a new bidi stream, writes a length-prefixed JSON frame, then finishes the send side.
pub async fn send_signal(connection: &Connection, msg: &SignalMessage) -> Result<(), TransportError> {
    let (mut send, _recv) = connection.open_bi().await.map_err(|e| {
        TransportError::Internal(format!("failed to open bidi stream: {e}"))
    })?;

    let json = serde_json::to_vec(msg)
        .map_err(|e| TransportError::Internal(format!("signal serialize error: {e}")))?;

    let mut frame = BytesMut::with_capacity(4 + json.len());
    frame.put_u32(json.len() as u32);
    frame.put_slice(&json);

    send.write_all(&frame)
        .await
        .map_err(|e| TransportError::Internal(format!("stream write error: {e}")))?;

    send.finish()
        .map_err(|e| TransportError::Internal(format!("stream finish error: {e}")))?;

    Ok(())
}

/// Receive a signaling message from a QUIC receive stream.
///
/// Reads a 4-byte big-endian length prefix, then the JSON payload.
pub async fn recv_signal(recv: &mut quinn::RecvStream) -> Result<SignalMessage, TransportError> {
    // Read 4-byte length prefix
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::Internal(format!("stream read length error: {e}")))?;

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1_048_576 {
        return Err(TransportError::Internal(format!(
            "signal message too large: {len} bytes"
        )));
    }

    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload)
        .await
        .map_err(|e| TransportError::Internal(format!("stream read payload error: {e}")))?;

    serde_json::from_slice(&payload)
        .map_err(|e| TransportError::Internal(format!("signal deserialize error: {e}")))
}
