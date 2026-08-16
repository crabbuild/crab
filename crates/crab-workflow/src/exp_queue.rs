//! Filesystem-backed experiment queue.
//!
//! Queue entries are JSON files stored in `.crab/exp-queue/`. Each
//! experiment gets a unique ID, and status transitions flow:
//! `pending → running → done/failed`.
//!
//! The queue is intentionally simple — no database, no locking beyond
//! atomic file writes. Concurrent readers see a consistent snapshot
//! because each entry is a single JSON file written atomically via
//! tempfile-rename.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ExperimentId, Result, WorkflowError};

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive fields by reference"
)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Status of an experiment in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExpStatus {
    Pending,
    Running,
    Done,
    Failed,
}

/// A single experiment queue entry, serialized as JSON on disk.
///
/// Field layout uses [`BTreeMap`] for `param_overrides` so the JSON
/// output is key-sorted and byte-stable across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpQueueEntry {
    /// Unique UUIDv7 experiment ID.
    pub id: String,
    /// ISO 8601 timestamp of when the experiment was queued.
    pub queued_at: String,
    /// Git commit hash at queue time.
    pub base_commit: String,
    /// Optional experiment label to persist when this entry runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional experiment message to persist when this entry runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Parameter overrides to apply before running.
    pub param_overrides: BTreeMap<String, String>,
    /// Stage targets to pass to the workflow runner when this
    /// queued experiment starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// Discover nested workflow files when this entry runs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub recursive: bool,
    /// Run only target stage(s), without upstream dependencies.
    #[serde(default, skip_serializing_if = "is_false")]
    pub single_item: bool,
    /// Run target stage(s) and downstream consumers.
    #[serde(default, skip_serializing_if = "is_false")]
    pub downstream: bool,
    /// Force descendants of executed stages to execute.
    #[serde(default, skip_serializing_if = "is_false")]
    pub force_downstream: bool,
    /// Run the connected pipeline component containing target stage(s).
    #[serde(default, skip_serializing_if = "is_false")]
    pub pipeline: bool,
    /// Discover and run all pipelines under the repo root.
    #[serde(default, skip_serializing_if = "is_false")]
    pub all_pipelines: bool,
    /// Treat `targets` as glob patterns over stage names.
    #[serde(default, skip_serializing_if = "is_false")]
    pub glob: bool,
    /// Ignored or untracked repo-relative paths to overlay into the
    /// experiment worktree before execution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_paths: Vec<PathBuf>,
    /// Current status of this entry.
    pub status: ExpStatus,
}

/// Filesystem-backed experiment queue.
///
/// Each entry is stored as `<queue_dir>/<id>.json`. Operations are
/// non-transactional but atomic at the single-file level (tempfile +
/// rename).
pub struct ExpQueue {
    queue_dir: PathBuf,
}

impl ExpQueue {
    /// Create a queue handle pointing at the given directory.
    ///
    /// The directory is created lazily on the first write operation,
    /// not at construction time.
    pub fn new(queue_dir: PathBuf) -> Self {
        Self { queue_dir }
    }

    /// Write a new entry to the queue as a JSON file.
    ///
    /// Creates the queue directory if it does not exist. Uses atomic
    /// file writes (tempfile in the same directory + rename) so
    /// concurrent readers never see a partial file.
    pub fn enqueue(&self, entry: &ExpQueueEntry) -> Result<()> {
        std::fs::create_dir_all(&self.queue_dir)?;

        let json = serde_json::to_vec_pretty(entry).map_err(|source| {
            WorkflowError::QueueEntrySerialize {
                id: entry.id.clone(),
                source,
            }
        })?;

        let dest = self.entry_path(&entry.id);
        atomic_write(&dest, &json)
    }

    /// List all entries with `status == Pending`, sorted by ID.
    pub fn list_pending(&self) -> Result<Vec<ExpQueueEntry>> {
        let all = self.list_all()?;
        Ok(all
            .into_iter()
            .filter(|e| e.status == ExpStatus::Pending)
            .collect())
    }

    /// List all entries in the queue, sorted by ID.
    ///
    /// Returns an empty vec if the queue directory does not exist.
    pub fn list_all(&self) -> Result<Vec<ExpQueueEntry>> {
        if !self.queue_dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        for dir_entry in std::fs::read_dir(&self.queue_dir)? {
            let dir_entry = dir_entry?;
            let path = dir_entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            match Self::read_entry(&path) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!(?path, %e, "skipping malformed queue entry");
                }
            }
        }

        entries.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(entries)
    }

    /// Update the status of an existing entry.
    ///
    /// Reads the entry, changes its status field, and writes it back
    /// atomically. Returns `NotFound` if the entry does not exist.
    pub fn update_status(&self, id: &str, status: ExpStatus) -> Result<()> {
        let path = self.entry_path(id);
        let mut entry = Self::read_entry(&path).map_err(|_| WorkflowError::QueueEntryNotFound {
            path: path.display().to_string(),
        })?;

        entry.status = status;

        let json = serde_json::to_vec_pretty(&entry).map_err(|source| {
            WorkflowError::QueueEntrySerialize {
                id: id.to_owned(),
                source,
            }
        })?;

        atomic_write(&path, &json)
    }

    /// Remove a queue entry file from disk.
    ///
    /// Returns `Ok(())` if the file was removed or did not exist
    /// (idempotent delete).
    pub fn remove(&self, id: &str) -> Result<()> {
        let path = self.entry_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Generate a unique experiment ID.
    ///
    /// Queue entries use the same UUIDv7 identifier as direct
    /// experiments so `exp start` can write metadata under the queued
    /// id and `exp show` / `exp diff` can read it without translation.
    pub fn generate_id() -> String {
        ExperimentId::new_v7().to_string()
    }

    /// Resolve the filesystem path for an entry by ID.
    fn entry_path(&self, id: &str) -> PathBuf {
        self.queue_dir.join(format!("{id}.json"))
    }

    /// Read and deserialize a single entry from its JSON file.
    fn read_entry(path: &Path) -> Result<ExpQueueEntry> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(|source| WorkflowError::QueueEntryMalformed {
            path: path.display().to_string(),
            source,
        })
    }
}

/// Atomic file write: write to a tempfile in the same directory, then
/// rename over the destination. This ensures readers never see a
/// partial file.
fn atomic_write(dest: &Path, data: &[u8]) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| WorkflowError::QueueEntryPathNoParent {
            path: dest.display().to_string(),
        })?;

    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(tmp.path(), data)?;
    tmp.persist(dest)
        .map_err(|source| WorkflowError::QueueEntryPersist {
            path: dest.display().to_string(),
            source,
        })?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn make_entry(id: &str) -> ExpQueueEntry {
        ExpQueueEntry {
            id: id.to_owned(),
            queued_at: "2026-05-05T12:00:00Z".to_owned(),
            base_commit: "abc123".to_owned(),
            name: None,
            message: None,
            param_overrides: BTreeMap::from([("train.lr".to_owned(), "0.01".to_owned())]),
            targets: Vec::new(),
            recursive: false,
            single_item: false,
            downstream: false,
            force_downstream: false,
            pipeline: false,
            all_pipelines: false,
            glob: false,
            copy_paths: Vec::new(),
            status: ExpStatus::Pending,
        }
    }

    #[test]
    fn enqueue_and_list_all() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("exp-queue"));

        queue.enqueue(&make_entry("exp-001")).unwrap();
        queue.enqueue(&make_entry("exp-002")).unwrap();

        let all = queue.list_all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "exp-001");
        assert_eq!(all[1].id, "exp-002");
    }

    #[test]
    fn list_pending_filters_by_status() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("exp-queue"));

        let mut running = make_entry("exp-001");
        running.status = ExpStatus::Running;
        queue.enqueue(&running).unwrap();
        queue.enqueue(&make_entry("exp-002")).unwrap();

        let pending = queue.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "exp-002");
    }

    #[test]
    fn update_status_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("exp-queue"));

        queue.enqueue(&make_entry("exp-001")).unwrap();
        queue.update_status("exp-001", ExpStatus::Running).unwrap();

        let all = queue.list_all().unwrap();
        assert_eq!(all[0].status, ExpStatus::Running);
    }

    #[test]
    fn update_status_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("exp-queue"));
        std::fs::create_dir_all(tmp.path().join("exp-queue")).unwrap();

        let result = queue.update_status("nonexistent", ExpStatus::Done);
        assert!(result.is_err());
    }

    #[test]
    fn remove_deletes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("exp-queue"));

        queue.enqueue(&make_entry("exp-001")).unwrap();
        queue.remove("exp-001").unwrap();

        let all = queue.list_all().unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn remove_idempotent_for_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("exp-queue"));
        std::fs::create_dir_all(tmp.path().join("exp-queue")).unwrap();

        // Removing a non-existent entry should succeed silently.
        queue.remove("nonexistent").unwrap();
    }

    #[test]
    fn list_all_returns_empty_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = ExpQueue::new(tmp.path().join("does-not-exist"));

        let all = queue.list_all().unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn generate_id_is_uuidv7_experiment_id() {
        let id = ExpQueue::generate_id();
        let parsed: ExperimentId = id.parse().unwrap();
        assert_eq!(parsed.to_string(), id);
    }

    #[test]
    fn generate_id_is_unique() {
        let ids: Vec<String> = (0..100).map(|_| ExpQueue::generate_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "generated duplicate IDs");
    }

    #[test]
    fn serde_round_trip() {
        let entry = make_entry("exp-test-001");
        let json = serde_json::to_string_pretty(&entry).unwrap();
        let parsed: ExpQueueEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn serde_accepts_entry_without_target_fields() {
        let json = r#"{
  "id": "exp-test-001",
  "queued_at": "2026-05-05T12:00:00Z",
  "base_commit": "abc123",
  "name": null,
  "param_overrides": {
    "train.lr": "0.01"
  },
  "status": "pending"
}"#;

        let parsed: ExpQueueEntry = serde_json::from_str(json).unwrap();
        assert!(parsed.message.is_none());
        assert!(parsed.targets.is_empty());
        assert!(!parsed.recursive);
        assert!(!parsed.single_item);
        assert!(!parsed.downstream);
        assert!(!parsed.force_downstream);
        assert!(!parsed.pipeline);
        assert!(!parsed.all_pipelines);
        assert!(!parsed.glob);
        assert!(parsed.copy_paths.is_empty());
    }

    #[test]
    fn status_serializes_lowercase() {
        let json = serde_json::to_string(&ExpStatus::Pending).unwrap();
        assert_eq!(json, "\"pending\"");

        let json = serde_json::to_string(&ExpStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");

        let json = serde_json::to_string(&ExpStatus::Done).unwrap();
        assert_eq!(json, "\"done\"");

        let json = serde_json::to_string(&ExpStatus::Failed).unwrap();
        assert_eq!(json, "\"failed\"");
    }
}
