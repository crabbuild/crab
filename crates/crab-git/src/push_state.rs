//! Persistent push state tracking last-pushed SHA per (remote URL, ref) pair.
//!
//! Stored at `.crab/push-state` as JSON. Updated atomically
//! (write to temp file, then rename) after a successful push.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tracing::warn;

/// Tracks the last-pushed commit SHA per (remote URL, ref) pair.
///
/// Loaded from `.crab/push-state` on push start, updated only after
/// all refs in a batch are successfully pushed. A failed or partial
/// push leaves the file unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushState {
    pub destinations: HashMap<String, HashMap<String, String>>,
}

/// Relative path within the repo root where push state is persisted.
const PUSH_STATE_PATH: &str = ".crab/push-state";

impl PushState {
    /// Load from `.crab/push-state`, or return default if missing/corrupt.
    pub fn load(repo_root: &Path) -> Self {
        let path = repo_root.join(PUSH_STATE_PATH);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read push-state, using default");
                return Self::default();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(state) => state,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "corrupt push-state JSON, using default");
                Self::default()
            }
        }
    }

    /// Get the last-pushed SHA for a (remote URL, ref) pair.
    pub fn last_pushed(&self, remote_url: &str, ref_name: &str) -> Option<&str> {
        self.destinations
            .get(remote_url)
            .and_then(|refs| refs.get(ref_name))
            .map(String::as_str)
    }

    /// Update the SHA for a (remote URL, ref) pair.
    pub fn set(&mut self, remote_url: &str, ref_name: &str, sha: &str) {
        self.destinations
            .entry(remote_url.to_owned())
            .or_default()
            .insert(ref_name.to_owned(), sha.to_owned());
    }

    /// Remove the entry for a (remote, ref) pair. No-op if the entry
    /// doesn't exist. Used after a successful delete-ref push so the
    /// next incremental walk doesn't try to hide against a SHA that
    /// no longer exists on the remote.
    pub fn remove(&mut self, remote_url: &str, ref_name: &str) {
        if let Some(refs) = self.destinations.get_mut(remote_url) {
            refs.remove(ref_name);
            if refs.is_empty() {
                self.destinations.remove(remote_url);
            }
        }
    }

    /// Atomically save to `.crab/push-state` (temp file + rename).
    pub fn save(&self, repo_root: &Path) -> std::io::Result<()> {
        let dest = repo_root.join(PUSH_STATE_PATH);
        let parent = repo_root.join(".crab");
        std::fs::create_dir_all(&parent)?;

        let temp = NamedTempFile::new_in(&parent)?;
        serde_json::to_writer_pretty(temp.as_file(), self).map_err(std::io::Error::other)?;
        temp.persist(&dest).map_err(|e| e.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let state = PushState::load(dir.path());
        assert!(state.destinations.is_empty());
    }

    #[test]
    fn load_corrupt_json_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let crab_dir = dir.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();
        std::fs::write(crab_dir.join("push-state"), b"not json{{{").unwrap();

        let state = PushState::load(dir.path());
        assert!(state.destinations.is_empty());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = PushState::default();
        state.set("crab://bucket/repo", "refs/heads/main", "abc123");
        state.set("crab://bucket/repo", "refs/heads/feature", "def456");
        state.set("crab://bucket/upstream", "refs/heads/main", "789012");

        state.save(dir.path()).unwrap();

        let loaded = PushState::load(dir.path());
        assert_eq!(
            loaded.last_pushed("crab://bucket/repo", "refs/heads/main"),
            Some("abc123")
        );
        assert_eq!(
            loaded.last_pushed("crab://bucket/repo", "refs/heads/feature"),
            Some("def456")
        );
        assert_eq!(
            loaded.last_pushed("crab://bucket/upstream", "refs/heads/main"),
            Some("789012")
        );
    }

    #[test]
    fn last_pushed_unknown_remote_returns_none() {
        let state = PushState::default();
        assert_eq!(
            state.last_pushed("crab://bucket/repo", "refs/heads/main"),
            None
        );
    }

    #[test]
    fn last_pushed_unknown_ref_returns_none() {
        let mut state = PushState::default();
        state.set("crab://bucket/repo", "refs/heads/main", "abc123");
        assert_eq!(
            state.last_pushed("crab://bucket/repo", "refs/heads/other"),
            None
        );
    }

    #[test]
    fn set_overwrites_existing_entry() {
        let mut state = PushState::default();
        state.set("crab://bucket/repo", "refs/heads/main", "abc123");
        state.set("crab://bucket/repo", "refs/heads/main", "def456");
        assert_eq!(
            state.last_pushed("crab://bucket/repo", "refs/heads/main"),
            Some("def456")
        );
    }

    #[test]
    fn retargeted_remote_url_has_independent_push_boundary() {
        let mut state = PushState::default();
        state.set("crab://bucket/old", "refs/heads/main", "abc123");

        assert_eq!(
            state.last_pushed("crab://bucket/new", "refs/heads/main"),
            None
        );
    }

    #[test]
    fn legacy_alias_keyed_state_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let crab_dir = dir.path().join(".crab");
        std::fs::create_dir_all(&crab_dir).unwrap();
        std::fs::write(
            crab_dir.join("push-state"),
            br#"{"remotes":{"origin":{"refs/heads/main":"abc123"}}}"#,
        )
        .unwrap();

        let state = PushState::load(dir.path());
        assert!(state.destinations.is_empty());
    }

    #[test]
    fn remove_deletes_entry_and_looks_unfetched() {
        let mut state = PushState::default();
        state.set("crab://bucket/repo", "refs/heads/main", "abc123");
        state.set("crab://bucket/repo", "refs/heads/feature", "def456");

        state.remove("crab://bucket/repo", "refs/heads/main");
        assert_eq!(
            state.last_pushed("crab://bucket/repo", "refs/heads/main"),
            None
        );
        assert_eq!(
            state.last_pushed("crab://bucket/repo", "refs/heads/feature"),
            Some("def456")
        );
    }

    #[test]
    fn remove_missing_entry_is_noop() {
        let mut state = PushState::default();
        // Removing from an empty state must not panic or error.
        state.remove("crab://bucket/repo", "refs/heads/never-existed");
        assert!(state.destinations.is_empty());
    }

    #[test]
    fn remove_last_ref_drops_empty_remote() {
        let mut state = PushState::default();
        state.set("crab://bucket/repo", "refs/heads/main", "abc123");
        state.remove("crab://bucket/repo", "refs/heads/main");
        // Removing the only ref for a remote cleans up the remote entry
        // too so the on-disk file doesn't accumulate stale keys.
        assert!(state.destinations.is_empty());
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        // .crab/ doesn't exist yet — save should create it.
        let state = PushState::default();
        state.save(dir.path()).unwrap();

        let loaded = PushState::load(dir.path());
        assert!(loaded.destinations.is_empty());
    }

    #[test]
    fn atomic_save_no_temp_file_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = PushState::default();
        state.set("crab://bucket/repo", "refs/heads/main", "abc123");
        state.save(dir.path()).unwrap();

        let crab_dir = dir.path().join(".crab");
        let entries: Vec<_> = std::fs::read_dir(&crab_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        // Only the push-state file should exist — no temp files.
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), "push-state");
    }
}
