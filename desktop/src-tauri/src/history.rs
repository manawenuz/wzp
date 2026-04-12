//! Call history store.
//!
//! Keeps a rolling JSON file of the last N direct-call events so the UI can
//! show "recent contacts" + "call history with callback buttons" on the
//! direct-call screen. Storage lives in `<APP_DATA_DIR>/call_history.json`
//! alongside the identity file. The file is read lazily on first access and
//! cached in an RwLock behind a OnceLock.
//!
//! This is a v1 — no duration tracking yet, entries are logged at the
//! moment the direction is decided (placed / received / missed).

use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Maximum number of history entries we keep. Older ones are pruned FIFO.
const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallDirection {
    /// Local user placed the call.
    Placed,
    /// Remote user called and local user answered.
    Received,
    /// Remote user called but local user did not answer (rejected or
    /// missed entirely — the UI treats these identically).
    Missed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallHistoryEntry {
    pub call_id: String,
    pub peer_fp: String,
    pub peer_alias: Option<String>,
    pub direction: CallDirection,
    /// Seconds since UNIX epoch, UTC.
    pub timestamp_unix: u64,
}

// ─── In-process store (loaded from disk once) ─────────────────────────────

static STORE: OnceLock<RwLock<Vec<CallHistoryEntry>>> = OnceLock::new();

fn store() -> &'static RwLock<Vec<CallHistoryEntry>> {
    STORE.get_or_init(|| RwLock::new(load_from_disk()))
}

fn history_path() -> PathBuf {
    crate::APP_DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".wzp")
        })
        .join("call_history.json")
}

fn load_from_disk() -> Vec<CallHistoryEntry> {
    let path = history_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<CallHistoryEntry>>(&bytes)
        .inspect_err(|e| tracing::warn!(path = %path.display(), error = %e, "call_history.json parse failed"))
        .unwrap_or_default()
}

fn save_to_disk(entries: &[CallHistoryEntry]) {
    let path = history_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_vec_pretty(entries) else { return };
    // Atomic write via temp file + rename so a crash mid-write doesn't
    // leave us with a half-file on disk.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Public API ───────────────────────────────────────────────────────────

/// Append a new entry to the store and persist to disk. Trims the store to
/// `MAX_ENTRIES` after insertion.
pub fn log(
    call_id: String,
    peer_fp: String,
    peer_alias: Option<String>,
    direction: CallDirection,
) {
    tracing::info!(
        %call_id, %peer_fp, ?direction,
        alias = ?peer_alias,
        "history::log"
    );
    let entry = CallHistoryEntry {
        call_id: call_id.clone(),
        peer_fp,
        peer_alias,
        direction,
        timestamp_unix: now_unix(),
    };
    let mut guard = store().write().unwrap();
    // If an entry for this call_id already exists, update it in-place
    // rather than appending a duplicate. Protects against the caller
    // side adding a second Missed row when the callee's DirectCallOffer
    // bounces back through federation / loopback, or when some future
    // relay routing edge case double-emits a signal. The dedup keeps
    // history tidy and matches what the user intuitively expects (one
    // history row per call, not one per signal event).
    if let Some(existing) = guard.iter_mut().rev().find(|e| e.call_id == call_id) {
        tracing::info!(%call_id, from = ?existing.direction, to = ?direction, "history::log replacing existing entry");
        existing.direction = direction;
        existing.timestamp_unix = entry.timestamp_unix;
        save_to_disk(&guard);
        return;
    }
    guard.push(entry);
    if guard.len() > MAX_ENTRIES {
        let drop_n = guard.len() - MAX_ENTRIES;
        guard.drain(0..drop_n);
    }
    save_to_disk(&guard);
}

/// Return a copy of all entries in reverse-chronological order
/// (most recent first).
pub fn all() -> Vec<CallHistoryEntry> {
    let guard = store().read().unwrap();
    guard.iter().rev().cloned().collect()
}

/// Unique peer contacts sorted by most recent interaction. Each contact
/// is represented by the newest history entry for that fingerprint.
pub fn contacts() -> Vec<CallHistoryEntry> {
    let guard = store().read().unwrap();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
    // iterate newest → oldest
    for entry in guard.iter().rev() {
        if seen.insert(entry.peer_fp.clone()) {
            out.push(entry.clone());
        }
    }
    out
}

/// Clear the entire history and persist the empty file.
pub fn clear() {
    let mut guard = store().write().unwrap();
    guard.clear();
    save_to_disk(&guard);
}

/// Find a Missed-candidate entry that matches `call_id` and hasn't been
/// answered yet. Used by the signal loop to turn "pending incoming" into
/// "Received" when the user accepts.
pub fn mark_received_if_pending(call_id: &str) -> bool {
    let mut guard = store().write().unwrap();
    for entry in guard.iter_mut().rev() {
        if entry.call_id == call_id && entry.direction == CallDirection::Missed {
            entry.direction = CallDirection::Received;
            save_to_disk(&guard);
            return true;
        }
    }
    false
}
