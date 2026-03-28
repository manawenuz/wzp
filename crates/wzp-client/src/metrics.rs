//! Client-side JSONL metrics export.
//!
//! When `--metrics-file <path>` is passed, the client writes one JSON object
//! per second to the specified file. Each line is a self-contained JSON object
//! (JSONL format) containing jitter buffer stats, loss, and quality profile.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::time::{Duration, Instant};

use serde::Serialize;

use wzp_proto::jitter::JitterStats;

/// A single metrics snapshot written as one JSONL line.
#[derive(Serialize)]
pub struct ClientMetricsSnapshot {
    pub ts: String,
    pub buffer_depth: usize,
    pub underruns: u64,
    pub overruns: u64,
    pub loss_pct: f64,
    pub rtt_ms: u64,
    pub jitter_ms: u64,
    pub frames_sent: u64,
    pub frames_received: u64,
    pub quality_profile: String,
}

/// Periodic JSONL writer that respects a configurable interval.
pub struct MetricsWriter {
    file: File,
    interval: Duration,
    last_write: Instant,
}

impl MetricsWriter {
    /// Create a new `MetricsWriter` that appends JSONL to the given path.
    ///
    /// The file is created (or truncated) immediately.
    pub fn new(path: &str, interval_secs: u64) -> Result<Self, anyhow::Error> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            file,
            interval: Duration::from_secs(interval_secs),
            // Set last_write far in the past so the first call writes immediately.
            last_write: Instant::now() - Duration::from_secs(interval_secs + 1),
        })
    }

    /// Write a JSONL line if the interval has elapsed since the last write.
    ///
    /// Returns `Ok(true)` when a line was written, `Ok(false)` when skipped.
    pub fn maybe_write(&mut self, snapshot: &ClientMetricsSnapshot) -> Result<bool, anyhow::Error> {
        let now = Instant::now();
        if now.duration_since(self.last_write) >= self.interval {
            let line = serde_json::to_string(snapshot)?;
            writeln!(self.file, "{}", line)?;
            self.file.flush()?;
            self.last_write = now;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Build a `ClientMetricsSnapshot` from jitter buffer stats and a quality profile name.
///
/// Fields not available from `JitterStats` alone (rtt_ms, jitter_ms, frames_sent)
/// are set to zero — the caller can override them if the data is available.
pub fn snapshot_from_stats(stats: &JitterStats, profile: &str) -> ClientMetricsSnapshot {
    let loss_pct = if stats.packets_received > 0 {
        (stats.packets_lost as f64 / stats.packets_received as f64) * 100.0
    } else {
        0.0
    };
    ClientMetricsSnapshot {
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        buffer_depth: stats.current_depth,
        underruns: stats.underruns,
        overruns: stats.overruns,
        loss_pct,
        rtt_ms: 0,
        jitter_ms: 0,
        frames_sent: 0,
        frames_received: stats.total_decoded,
        quality_profile: profile.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_stats() -> JitterStats {
        JitterStats {
            packets_received: 100,
            packets_played: 95,
            packets_lost: 5,
            packets_late: 2,
            packets_duplicate: 0,
            current_depth: 8,
            total_decoded: 93,
            underruns: 1,
            overruns: 0,
            max_depth_seen: 12,
        }
    }

    #[test]
    fn snapshot_serializes_to_json() {
        let stats = make_test_stats();
        let snap = snapshot_from_stats(&stats, "GOOD");
        let json = serde_json::to_string(&snap).unwrap();

        // Verify expected fields are present in the JSON string.
        assert!(json.contains("\"ts\""));
        assert!(json.contains("\"buffer_depth\":8"));
        assert!(json.contains("\"underruns\":1"));
        assert!(json.contains("\"overruns\":0"));
        assert!(json.contains("\"loss_pct\":5."));
        assert!(json.contains("\"rtt_ms\":0"));
        assert!(json.contains("\"jitter_ms\":0"));
        assert!(json.contains("\"frames_sent\":0"));
        assert!(json.contains("\"frames_received\":93"));
        assert!(json.contains("\"quality_profile\":\"GOOD\""));

        // Verify it round-trips as valid JSON.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["buffer_depth"], 8);
        assert_eq!(value["quality_profile"], "GOOD");
    }

    #[test]
    fn metrics_writer_creates_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("wzp_metrics_test.jsonl");
        let path_str = path.to_str().unwrap();

        let mut writer = MetricsWriter::new(path_str, 1).unwrap();
        let stats = make_test_stats();
        let snap = snapshot_from_stats(&stats, "DEGRADED");

        let wrote = writer.maybe_write(&snap).unwrap();
        assert!(wrote, "first write should succeed immediately");

        // Read the file back and verify it contains valid JSONL.
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1, "should have exactly one JSONL line");

        let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(value["quality_profile"], "DEGRADED");
        assert_eq!(value["buffer_depth"], 8);

        // Clean up.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn metrics_writer_respects_interval() {
        let dir = std::env::temp_dir();
        let path = dir.join("wzp_metrics_interval_test.jsonl");
        let path_str = path.to_str().unwrap();

        let mut writer = MetricsWriter::new(path_str, 60).unwrap();
        let stats = make_test_stats();
        let snap = snapshot_from_stats(&stats, "GOOD");

        // First write succeeds (last_write is set far in the past).
        let first = writer.maybe_write(&snap).unwrap();
        assert!(first, "first write should succeed");

        // Immediate second write should be skipped (60s interval).
        let second = writer.maybe_write(&snap).unwrap();
        assert!(!second, "second write should be skipped — interval not elapsed");

        // Clean up.
        let _ = std::fs::remove_file(&path);
    }
}
