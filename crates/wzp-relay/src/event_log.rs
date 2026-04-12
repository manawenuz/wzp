//! JSONL event log for protocol analysis.
//!
//! When `--event-log <path>` is set, every media packet emits a structured
//! event at each decision point (recv, forward, drop, deliver).
//! Use `wzp-analyzer` to correlate events across multiple relays.

use std::path::PathBuf;

use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{error, info};

/// A single protocol event for JSONL output.
#[derive(Debug, Serialize)]
pub struct Event {
    /// ISO 8601 timestamp with microseconds.
    pub ts: String,
    /// Event type.
    pub event: &'static str,
    /// Room name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Source address or peer label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// Packet sequence number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u16>,
    /// Codec identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// FEC block ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fec_block: Option<u8>,
    /// FEC symbol index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fec_sym: Option<u8>,
    /// Is FEC repair packet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<bool>,
    /// Payload length in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub len: Option<usize>,
    /// Number of recipients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_count: Option<usize>,
    /// Peer label (for federation events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Drop/error reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Presence action (active/inactive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Participant count (presence events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub participants: Option<usize>,
}

impl Event {
    fn now() -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
    }

    /// Create a minimal event with just type and timestamp.
    pub fn new(event: &'static str) -> Self {
        Self {
            ts: Self::now(),
            event,
            room: None,
            src: None,
            seq: None,
            codec: None,
            fec_block: None,
            fec_sym: None,
            repair: None,
            len: None,
            to_count: None,
            peer: None,
            reason: None,
            action: None,
            participants: None,
        }
    }

    /// Set room.
    pub fn room(mut self, room: &str) -> Self { self.room = Some(room.to_string()); self }
    /// Set source.
    pub fn src(mut self, src: &str) -> Self { self.src = Some(src.to_string()); self }
    /// Set packet header fields from a MediaPacket.
    pub fn packet(mut self, pkt: &wzp_proto::MediaPacket) -> Self {
        self.seq = Some(pkt.header.seq);
        self.codec = Some(format!("{:?}", pkt.header.codec_id));
        self.fec_block = Some(pkt.header.fec_block);
        self.fec_sym = Some(pkt.header.fec_symbol);
        self.repair = Some(pkt.header.is_repair);
        self.len = Some(pkt.payload.len());
        self
    }
    /// Set seq only (when full packet not available).
    pub fn seq(mut self, seq: u16) -> Self { self.seq = Some(seq); self }
    /// Set payload length.
    pub fn len(mut self, len: usize) -> Self { self.len = Some(len); self }
    /// Set recipient count.
    pub fn to_count(mut self, n: usize) -> Self { self.to_count = Some(n); self }
    /// Set peer label.
    pub fn peer(mut self, peer: &str) -> Self { self.peer = Some(peer.to_string()); self }
    /// Set drop reason.
    pub fn reason(mut self, reason: &str) -> Self { self.reason = Some(reason.to_string()); self }
    /// Set presence action.
    pub fn action(mut self, action: &str) -> Self { self.action = Some(action.to_string()); self }
    /// Set participant count.
    pub fn participants(mut self, n: usize) -> Self { self.participants = Some(n); self }
}

/// Handle for emitting events. Cheap to clone.
#[derive(Clone)]
pub struct EventLog {
    tx: mpsc::UnboundedSender<Event>,
}

impl EventLog {
    /// Emit an event (non-blocking, drops if channel is full).
    pub fn emit(&self, event: Event) {
        let _ = self.tx.send(event);
    }
}

/// No-op event log for when `--event-log` is not set.
/// All methods are no-ops that compile to nothing.
#[derive(Clone)]
pub struct NoopEventLog;

/// Unified event log handle — either real or no-op.
#[derive(Clone)]
pub enum EventLogger {
    Active(EventLog),
    Noop,
}

impl EventLogger {
    pub fn emit(&self, event: Event) {
        if let EventLogger::Active(log) = self {
            log.emit(event);
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, EventLogger::Active(_))
    }
}

/// Start the event log writer. Returns an `EventLogger` handle.
pub fn start_event_log(path: Option<PathBuf>) -> EventLogger {
    match path {
        Some(path) => {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(writer_task(path, rx));
            info!("event log enabled");
            EventLogger::Active(EventLog { tx })
        }
        None => EventLogger::Noop,
    }
}

/// Background task that writes events to a JSONL file.
async fn writer_task(path: PathBuf, mut rx: mpsc::UnboundedReceiver<Event>) {
    use tokio::io::AsyncWriteExt;

    let file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            error!("failed to create event log {}: {e}", path.display());
            return;
        }
    };
    let mut writer = tokio::io::BufWriter::new(file);
    let mut count: u64 = 0;

    while let Some(event) = rx.recv().await {
        match serde_json::to_string(&event) {
            Ok(json) => {
                if writer.write_all(json.as_bytes()).await.is_err() { break; }
                if writer.write_all(b"\n").await.is_err() { break; }
                count += 1;
                // Flush every 100 events
                if count % 100 == 0 {
                    let _ = writer.flush().await;
                }
            }
            Err(e) => {
                error!("event log serialize error: {e}");
            }
        }
    }

    let _ = writer.flush().await;
    info!(events = count, "event log closed");
}
