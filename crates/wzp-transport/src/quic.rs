//! `QuinnTransport` — implements `MediaTransport` trait from wzp-proto.
//!
//! Wraps a `quinn::Connection` and provides unreliable media (DATAGRAM frames)
//! and reliable signaling (QUIC streams).

use async_trait::async_trait;
use std::sync::Mutex;

use wzp_proto::packet::TrunkFrame;
use wzp_proto::{MediaPacket, MediaTransport, PathQuality, SignalMessage, TransportError};

use crate::datagram;
use crate::path_monitor::PathMonitor;
use crate::reliable;

/// Snapshot of quinn's QUIC-level path statistics.
///
/// Provides more accurate loss/RTT data than `PathMonitor`'s sequence-gap
/// heuristic because quinn sees ACK frames and congestion signals directly.
#[derive(Clone, Copy, Debug)]
pub struct QuinnPathSnapshot {
    /// Smoothed RTT in milliseconds (from quinn's congestion controller).
    pub rtt_ms: u32,
    /// Cumulative loss percentage (lost_packets / sent_packets × 100).
    pub loss_pct: f32,
    /// Total congestion events observed by the QUIC stack.
    pub congestion_events: u64,
    /// Current congestion window in bytes.
    pub cwnd: u64,
    /// Total packets sent on this path.
    pub sent_packets: u64,
    /// Total packets lost on this path.
    pub lost_packets: u64,
    /// Current PMTUD-discovered maximum datagram payload size (bytes).
    /// Starts at `initial_mtu` (1200) and grows as PMTUD probes succeed.
    pub current_mtu: usize,
}

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

    /// Remote address of the peer on this connection.
    pub fn remote_address(&self) -> std::net::SocketAddr {
        self.connection.remote_address()
    }

    /// Send raw bytes as a QUIC datagram (no MediaPacket framing).
    pub fn send_raw_datagram(&self, data: &[u8]) -> Result<(), TransportError> {
        self.connection
            .send_datagram(bytes::Bytes::copy_from_slice(data))
            .map_err(|e| TransportError::Internal(format!("datagram: {e}")))
    }

    /// Close the QUIC connection immediately (synchronous, no async needed).
    /// The relay will detect the close and remove this participant from the room.
    pub fn close_now(&self) {
        self.connection.close(quinn::VarInt::from_u32(0), b"hangup");
    }

    /// Feed an external RTT observation (e.g. from QUIC path stats) into the path monitor.
    pub fn feed_rtt(&self, rtt_ms: u32) {
        self.path_monitor.lock().unwrap().observe_rtt(rtt_ms);
    }

    /// Get raw packet counts from path monitor (sent, received).
    pub fn monitor_counts(&self) -> (u64, u64) {
        self.path_monitor.lock().unwrap().counts()
    }

    /// Get the maximum datagram payload size, if datagrams are supported.
    pub fn max_datagram_size(&self) -> Option<usize> {
        datagram::max_datagram_payload(&self.connection)
    }

    /// Snapshot of QUIC-level path stats from quinn, useful for DRED tuning.
    ///
    /// Returns `(rtt_ms, loss_pct, congestion_events)` derived from quinn's
    /// internal congestion controller — more accurate than our own sequence-gap
    /// heuristic in `PathMonitor` because quinn sees ACK frames directly.
    pub fn quinn_path_stats(&self) -> QuinnPathSnapshot {
        let stats = self.connection.stats();
        let rtt_ms = stats.path.rtt.as_millis() as u32;
        let loss_pct = if stats.path.sent_packets > 0 {
            (stats.path.lost_packets as f32 / stats.path.sent_packets as f32) * 100.0
        } else {
            0.0
        };
        let current_mtu = self.connection.max_datagram_size().unwrap_or(1200);
        QuinnPathSnapshot {
            rtt_ms,
            loss_pct,
            congestion_events: stats.path.congestion_events,
            cwnd: stats.path.cwnd,
            sent_packets: stats.path.sent_packets,
            lost_packets: stats.path.lost_packets,
            current_mtu,
        }
    }

    /// Send an encoded [`TrunkFrame`] as a single QUIC datagram.
    pub fn send_trunk(&self, frame: &TrunkFrame) -> Result<(), TransportError> {
        let data = frame.encode();

        if let Some(max_size) = self.connection.max_datagram_size() {
            if data.len() > max_size {
                return Err(TransportError::DatagramTooLarge {
                    size: data.len(),
                    max: max_size,
                });
            }
        }

        self.connection.send_datagram(data).map_err(|e| {
            TransportError::Internal(format!("send trunk datagram error: {e}"))
        })?;

        Ok(())
    }

    /// Receive a single QUIC datagram and decode it as a [`TrunkFrame`].
    ///
    /// Returns `Ok(None)` on connection close, `Ok(Some(frame))` on success,
    /// or an error on malformed data / transport failure.
    pub async fn recv_trunk(&self) -> Result<Option<TrunkFrame>, TransportError> {
        let data = match self.connection.read_datagram().await {
            Ok(data) => data,
            Err(quinn::ConnectionError::ApplicationClosed(_)) => return Ok(None),
            Err(quinn::ConnectionError::LocallyClosed) => return Ok(None),
            Err(e) => {
                return Err(TransportError::Internal(format!(
                    "recv trunk datagram error: {e}"
                )))
            }
        };

        TrunkFrame::decode(&data)
            .map(Some)
            .ok_or_else(|| TransportError::Internal("malformed trunk frame".into()))
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

        match datagram::deserialize_media(data.clone()) {
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
                tracing::warn!(len = data.len(), "skipping malformed media datagram, continuing");
                // Don't return Ok(None) — that signals connection closed.
                // Recurse to read the next datagram instead.
                Box::pin(self.recv_media()).await
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
