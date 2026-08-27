/// Daemon state snapshot (`~/.codex-switch/daemon-state.json`).
///
/// The daemon has no control socket; this file is its observability surface.
/// The loop overwrites it atomically after every event, `daemon status` (and
/// anything else) reads it. Writes are best-effort — a failing snapshot must
/// never take the daemon down.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub last_poll_at: Option<i64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
    /// Unix seconds until which polling is suspended after failures.
    pub backoff_until: Option<i64>,
    pub last_switch: Option<SwitchRecord>,
    pub pending_switch: Option<PendingSwitch>,
    pub last_cache_refresh_at: Option<i64>,
    /// Last completed warmup slot identity (`YYYY-MM-DD HH:MM`), not the fire time.
    #[serde(default)]
    pub last_warmup_slot: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchRecord {
    pub from: String,
    pub to: String,
    pub at: i64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSwitch {
    pub to: String,
    pub since: i64,
}

pub fn state_path() -> anyhow::Result<PathBuf> {
    Ok(crate::auth::app_home()?.join("daemon-state.json"))
}

/// Best-effort atomic write; failures are logged at debug level only.
pub fn write(state: &mut DaemonState) {
    state.updated_at = crate::auth::now_unix_secs();
    let Ok(path) = state_path() else {
        return;
    };
    write_at(&path, state);
}

fn write_at(path: &Path, state: &DaemonState) {
    let Ok(bytes) = serde_json::to_vec_pretty(state) else {
        return;
    };
    if let Err(e) = crate::auth::atomic_write_private(path, &bytes) {
        tracing::debug!("daemon state snapshot write failed: {e}");
    }
}

pub fn read() -> Option<DaemonState> {
    read_at(&state_path().ok()?)
}

fn read_at(path: &Path) -> Option<DaemonState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon-state.json");

        let state = DaemonState {
            pid: 4242,
            started_at: 100,
            updated_at: 200,
            last_poll_at: Some(190),
            last_error: Some("boom".to_string()),
            consecutive_failures: 2,
            backoff_until: Some(400),
            last_switch: Some(SwitchRecord {
                from: "alice".to_string(),
                to: "bob".to_string(),
                at: 150,
                score: 87.5,
            }),
            pending_switch: Some(PendingSwitch {
                to: "carol".to_string(),
                since: 195,
            }),
            last_cache_refresh_at: Some(180),
            last_warmup_slot: Some("2026-08-26 13:10".to_string()),
        };

        write_at(&path, &state);
        let loaded = read_at(&path).expect("snapshot should parse");

        assert_eq!(loaded.pid, 4242);
        assert_eq!(loaded.consecutive_failures, 2);
        assert_eq!(loaded.last_switch.as_ref().unwrap().to, "bob");
        assert_eq!(loaded.pending_switch.as_ref().unwrap().to, "carol");
        assert_eq!(loaded.backoff_until, Some(400));
        assert_eq!(loaded.last_warmup_slot.as_deref(), Some("2026-08-26 13:10"));
    }

    #[test]
    fn unreadable_snapshot_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon-state.json");
        assert!(read_at(&path).is_none());
        std::fs::write(&path, b"not json").unwrap();
        assert!(read_at(&path).is_none());
    }
}
