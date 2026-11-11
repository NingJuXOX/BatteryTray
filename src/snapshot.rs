//! Cross-process snapshot of the tray's latest readings.
//!
//! The tray is the only process that talks to HID hardware. It publishes a
//! small JSON snapshot after every refresh, and the panel (or any other window)
//! only reads that file. This keeps a second process from opening the same
//! receiver at the same time, and lets an open panel keep showing fresh data.

use crate::error::Result;
use crate::protocol::Reading;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Snapshot file name inside the application data directory.
pub const SNAPSHOT_FILE: &str = "readings.json";
/// Refresh-request marker file name inside the same directory.
pub const REQUEST_FILE: &str = "refresh.request";

/// Latest readings published by the tray process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Monotonic counter incremented by the tray on every publish.
    pub sequence: u64,
    /// Local wall-clock time of the publish, in milliseconds.
    pub published_ms: i64,
    /// One reading per enabled profile, in configuration order.
    pub readings: Vec<Reading>,
}

/// Returns the snapshot path for an application data directory.
pub fn path(directory: &Path) -> PathBuf {
    directory.join(SNAPSHOT_FILE)
}

/// Returns the refresh-request path for an application data directory.
pub fn request_path(directory: &Path) -> PathBuf {
    directory.join(REQUEST_FILE)
}

/// Atomically publishes a snapshot for other processes to read.
pub fn publish(directory: &Path, snapshot: &Snapshot) -> Result<()> {
    let bytes = serde_json::to_vec(snapshot)?;
    fs::create_dir_all(directory)?;
    let final_path = path(directory);
    let temporary = directory.join(format!("{SNAPSHOT_FILE}.{}.tmp", std::process::id()));
    fs::write(&temporary, &bytes)?;
    match fs::rename(&temporary, &final_path) {
        Ok(()) => Ok(()),
        Err(_) => {
            // A reader can hold the file open on Windows. The temp copy is
            // complete, so writing the same bytes in place is still safe: a
            // reader that catches a torn write simply retries on the next tick.
            fs::write(&final_path, &bytes)?;
            cleanup_temp(&temporary)
        }
    }
}

fn cleanup_temp(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Loads the published snapshot, ignoring missing or partially written files.
pub fn load(directory: &Path) -> Option<Snapshot> {
    let bytes = fs::read(path(directory)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Returns whether a snapshot is recent enough to display.
pub fn is_fresh(snapshot: &Snapshot, max_age: Duration) -> bool {
    let age = Local::now().timestamp_millis() - snapshot.published_ms;
    age >= 0 && age <= max_age.as_millis() as i64
}

/// Removes the published snapshot when the tray exits.
pub fn clear(directory: &Path) {
    for file in [path(directory), request_path(directory)] {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("清理读数快照失败（{}）：{error}", file.display()),
        }
    }
}

/// Asks the tray to perform a manual refresh.
///
/// The value is a nanosecond timestamp so two successive requests never look
/// identical to the tray's poller.
pub fn request(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    fs::write(request_path(directory), stamp.to_string())?;
    Ok(())
}

/// Reads the current refresh-request token, if one has been posted.
pub fn take_request(directory: &Path) -> Option<u128> {
    let text = fs::read_to_string(request_path(directory)).ok()?;
    text.trim().parse::<u128>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::sample_profiles;
    use crate::protocol::ReadingStatus;

    fn sample(sequence: u64) -> Snapshot {
        let profile = sample_profiles()
            .into_iter()
            .next()
            .unwrap_or_else(|| unreachable!());
        Snapshot {
            sequence,
            published_ms: Local::now().timestamp_millis(),
            readings: vec![Reading::new(&profile, Some(77), ReadingStatus::Ok, "")],
        }
    }

    #[test]
    fn snapshot_round_trips_and_can_be_cleared() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok(), "temporary directory must be created");
        let Some(root) = temporary.ok().map(|dir| dir.path().to_path_buf()) else {
            return;
        };
        let original = sample(7);
        assert!(publish(&root, &original).is_ok());
        let loaded = load(&root);
        assert!(loaded.is_some(), "published snapshot must be readable");
        let Some(loaded) = loaded else { return };
        assert_eq!(loaded.sequence, 7);
        assert_eq!(loaded.readings.len(), 1);
        assert_eq!(loaded.readings[0].percent, Some(77));
        assert!(is_fresh(&loaded, Duration::from_secs(60)));
        clear(&root);
        assert!(load(&root).is_none());
    }

    #[test]
    fn request_token_is_unique_and_absent_by_default() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok(), "temporary directory must be created");
        let Some(root) = temporary.ok().map(|dir| dir.path().to_path_buf()) else {
            return;
        };
        assert!(take_request(&root).is_none());
        assert!(request(&root).is_ok());
        let first = take_request(&root);
        assert!(first.is_some());
        assert!(request(&root).is_ok());
        assert_ne!(take_request(&root), first, "each request must be distinct");
    }
}
