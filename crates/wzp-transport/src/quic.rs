//! `QuinnTransport` — implements `MediaTransport` trait from wzp-proto.
//!
//! Wraps a `quinn::Connection` and provides unreliable media (DATAGRAM frames)
//! and reliable signaling (QUIC streams).

use async_trait::async_trait;
use std::sync::Mutex;

use wzp_proto::{MediaPacket, MediaTransport, PathQuality, SignalMessage, TransportError};

use crate::datagram;
use crate::path_monitor::PathMonitor;
use crate::reliable;

/// QUIC-based transport implementing the `MediaTransport` trait.
pub struct QuinnTransport {
    connection: quinn::Connection,
    path_monitor: Mutex<PathMonitor>,
}

impl QuinnTransport {
    /// Create a new transport wrapping an established QUIC connection.
    pub fn new(connection: quinn::Connection) -> Self {
        Self {
            connection,
            path_monitor: Mutex::new(PathMonitor::new()),
        }
    }

    /// Get a reference to the underlying QUIC connection.
    pub fn connection(&self) -> &quinn::Connection {
        &self.connection
    }

    /// Get the maximum datagram payload size, if datagrams are supported.
    pub fn max_datagram_size(&self) -> Option<usize> {
        datagram::max_datagram_payload(&self.connection)
    }
}

#[async_trait]
impl MediaTransport for QuinnTransport {
    async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError> {
        let data = datagram::serialize_media(packet);

        // Check MTU
        if let Some(max_size) = self.connection.max_datagram_size() {
            if data.len() > max_size {
                return Err(TransportError::DatagramTooLarge {
                    size: data.len(),
                    max: max_size,
                });
            }
        }

        // Record send observation
        {
            let mut monitor = self.path_monitor.lock().unwrap();
            monitor.observe_sent(packet.header.seq, packet.header.timestamp as u64);
        }

        self.connection.send_datagram(data).map_err(|e| {
            TransportError::Internal(format!("send datagram error: {e}"))
        })?;

        Ok(())
    }

    async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError> {
        let data = match self.connection.read_datagram().await {
            Ok(data) => data,
            Err(quinn::ConnectionError::ApplicationClosed(_)) => return Ok(None),
            Err(quinn::ConnectionError::LocallyClosed) => return Ok(None),
            Err(e) => {
                return Err(TransportError::Internal(format!(
                    "recv datagram error: {e}"
                )))
            }
        };

        match datagram::deserialize_media(data) {
            Some(packet) => {
                // Record receive observation
                {
                    let mut monitor = self.path_monitor.lock().unwrap();
                    monitor.observe_received(
                        packet.header.seq,
                        packet.header.timestamp as u64,
                    );
                }
                Ok(Some(packet))
            }
            None => {
                tracing::warn!("received malformed media datagram");
                Ok(None)
            }
        }
    }

    async fn send_signal(&self, msg: &SignalMessage) -> Result<(), TransportError> {
        reliable::send_signal(&self.connection, msg).await
    }

    async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError> {
        match self.connection.accept_bi().await {
            Ok((_send, mut recv)) => {
                let msg = reliable::recv_signal(&mut recv).await?;
                Ok(Some(msg))
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => Ok(None),
            Err(quinn::ConnectionError::LocallyClosed) => Ok(None),
            Err(e) => Err(TransportError::Internal(format!(
                "accept stream error: {e}"
            ))),
        }
    }

    fn path_quality(&self) -> PathQuality {
        let monitor = self.path_monitor.lock().unwrap();
        monitor.quality()
    }

    async fn close(&self) -> Result<(), TransportError> {
        self.connection.close(
            quinn::VarInt::from_u32(0),
            b"normal close",
        );
        Ok(())
    }
}
