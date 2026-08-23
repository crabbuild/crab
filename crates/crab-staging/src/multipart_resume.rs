//! SQLite-backed registry for multipart upload resume.
//!
//! Tracks in-progress multipart uploads so that an interrupted push can
//! resume from the last completed part rather than re-uploading the
//! entire xorb or pack. The database lives at `staging/multipart.db`
//! alongside the staging index. The registry implements
//! [`crab_storage::MultipartJournal`], the persistence boundary
//! `crab_storage::Store` consults during resumable multipart uploads.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use tracing::{debug, warn};

use crate::error::{Result, StagingError};

/// Information needed to resume an interrupted multipart upload.
#[derive(Debug, Clone)]
pub struct ResumeInfo {
    /// The backend-assigned multipart upload ID.
    pub upload_id: String,
    /// Bucket where the upload is targeted.
    pub bucket: String,
    /// Object key for the upload.
    pub key: String,
    /// Parts that have already been successfully uploaded.
    pub completed_parts: Vec<CompletedPart>,
}

/// A single completed part within a multipart upload.
#[derive(Debug, Clone)]
pub struct CompletedPart {
    pub part_number: i64,
    pub etag: String,
    pub size: i64,
}

/// Metadata for an abandoned multipart upload detected by fsck.
#[derive(Debug, Clone)]
pub struct AbandonedUpload {
    pub upload_id: String,
    pub payload_hash: Vec<u8>,
    pub bucket: String,
    pub key: String,
    pub started_at: i64,
}

/// SQLite-backed registry tracking in-progress multipart uploads.
///
/// Each registry instance owns a single `Connection` to
/// `staging/multipart.db`. The schema uses WAL mode for safe
/// concurrent reads.
pub struct MultipartRegistry {
    conn: Connection,
}

impl MultipartRegistry {
    /// Open (or create) the multipart registry at `path`.
    ///
    /// Enables WAL mode and runs idempotent schema migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| {
            StagingError::Internal(format!(
                "failed to open multipart.db at {}: {e}",
                path.display()
            ))
        })?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StagingError::Internal(format!("failed to set WAL mode: {e}")))?;

        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| {
                StagingError::Internal(format!("failed to set synchronous = NORMAL: {e}"))
            })?;

        // Enable foreign-key enforcement. Without this, SQLite silently
        // ignores the `FOREIGN KEY` clause below and orphaned rows in
        // `multipart_parts` accumulate when `begin()` replaces an entry
        // in `multipart_uploads`. See finding CR10-F2.
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| StagingError::Internal(format!("failed to enable foreign_keys: {e}")))?;

        let mut registry = Self { conn };
        registry.run_migrations()?;
        Ok(registry)
    }

    /// Run schema migrations (idempotent via `IF NOT EXISTS`).
    fn run_migrations(&mut self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS multipart_uploads (
                    upload_id   TEXT PRIMARY KEY,
                    payload_hash BLOB NOT NULL,
                    bucket      TEXT NOT NULL,
                    key         TEXT NOT NULL,
                    started_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                    completed   INTEGER NOT NULL DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS multipart_uploads_by_payload
                    ON multipart_uploads(payload_hash);

                CREATE TABLE IF NOT EXISTS multipart_parts (
                    upload_id   TEXT NOT NULL,
                    part_number INTEGER NOT NULL,
                    etag        TEXT NOT NULL,
                    size        INTEGER NOT NULL,
                    PRIMARY KEY (upload_id, part_number),
                    FOREIGN KEY (upload_id) REFERENCES multipart_uploads(upload_id)
                        ON DELETE CASCADE
                );",
            )
            .map_err(|e| {
                StagingError::Internal(format!("multipart schema migration failed: {e}"))
            })?;

        debug!("multipart registry schema ready");
        Ok(())
    }

    /// Claim journal ownership for a new upload before its first part PUT.
    ///
    /// Returns `false` when a different active upload already owns the
    /// payload hash (a concurrent uploader); the caller must proceed
    /// unjournaled instead of clobbering that row.
    pub fn begin(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<bool> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT upload_id FROM multipart_uploads
                 WHERE payload_hash = ?1 AND completed = 0",
                params![payload_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to check existing multipart upload: {e}"))
            })?;
        if let Some(owner) = existing
            && owner != upload_id
        {
            return Ok(false);
        }

        self.conn
            .execute(
                "INSERT OR REPLACE INTO multipart_uploads
                    (upload_id, payload_hash, bucket, key)
                 VALUES (?1, ?2, ?3, ?4)",
                params![upload_id, payload_hash, bucket, key],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to insert multipart upload: {e}"))
            })?;

        debug!(upload_id, "registered multipart upload");
        Ok(true)
    }

    /// Record a successfully uploaded part.
    pub fn record_part(
        &self,
        upload_id: &str,
        part_number: i64,
        etag: &str,
        size: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO multipart_parts
                    (upload_id, part_number, etag, size)
                 VALUES (?1, ?2, ?3, ?4)",
                params![upload_id, part_number, etag, size],
            )
            .map_err(|e| StagingError::Internal(format!("failed to record multipart part: {e}")))?;

        Ok(())
    }

    /// Mark an upload as completed and remove its parts.
    ///
    /// Called after a successful `CompleteMultipartUpload`.
    pub fn complete(&self, upload_id: &str) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin transaction: {e}")))?;

        tx.execute(
            "DELETE FROM multipart_parts WHERE upload_id = ?1",
            params![upload_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete parts for {upload_id}: {e}"))
        })?;

        tx.execute(
            "DELETE FROM multipart_uploads WHERE upload_id = ?1",
            params![upload_id],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete upload {upload_id}: {e}")))?;

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit complete for {upload_id}: {e}"))
        })?;

        debug!(upload_id, "completed multipart upload");
        Ok(())
    }

    /// Abort a stale upload: remove both the upload row and its parts.
    ///
    /// Called when the upload is cancelled, errors out, or the backend
    /// reports `NoSuchUpload`.
    pub fn abort_stale(&self, upload_id: &str) -> Result<()> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin transaction: {e}")))?;

        tx.execute(
            "DELETE FROM multipart_parts WHERE upload_id = ?1",
            params![upload_id],
        )
        .map_err(|e| {
            StagingError::Internal(format!("failed to delete parts for {upload_id}: {e}"))
        })?;

        tx.execute(
            "DELETE FROM multipart_uploads WHERE upload_id = ?1",
            params![upload_id],
        )
        .map_err(|e| StagingError::Internal(format!("failed to delete upload {upload_id}: {e}")))?;

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit abort for {upload_id}: {e}"))
        })?;

        debug!(upload_id, "aborted stale multipart upload");
        Ok(())
    }

    /// Look up a resumable upload for the given xorb hash.
    ///
    /// Returns `Some(ResumeInfo)` if an incomplete upload exists with at
    /// least one recorded part. Returns `None` if no resumable upload is
    /// found.
    pub fn resumable(&self, payload_hash: &[u8]) -> Result<Option<ResumeInfo>> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT upload_id, bucket, key
                 FROM multipart_uploads
                 WHERE payload_hash = ?1 AND completed = 0",
                params![payload_hash],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| {
                StagingError::Internal(format!("failed to query resumable upload: {e}"))
            })?;

        let Some((upload_id, bucket, key)) = row else {
            return Ok(None);
        };

        let mut stmt = self
            .conn
            .prepare(
                "SELECT part_number, etag, size
                 FROM multipart_parts
                 WHERE upload_id = ?1
                 ORDER BY part_number",
            )
            .map_err(|e| StagingError::Internal(format!("failed to prepare parts query: {e}")))?;

        let parts: Vec<CompletedPart> = stmt
            .query_map(params![&upload_id], |row| {
                Ok(CompletedPart {
                    part_number: row.get(0)?,
                    etag: row.get(1)?,
                    size: row.get(2)?,
                })
            })
            .map_err(|e| StagingError::Internal(format!("failed to query parts: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| StagingError::Internal(format!("failed to collect parts: {e}")))?;

        Ok(Some(ResumeInfo {
            upload_id,
            bucket,
            key,
            completed_parts: parts,
        }))
    }

    /// Handle a `NoSuchUpload` error from the backend.
    ///
    /// Deletes the registry row so the next attempt starts fresh.
    pub fn handle_no_such_upload(&self, upload_id: &str) -> Result<()> {
        warn!(
            upload_id,
            "backend reported NoSuchUpload, clearing registry"
        );
        self.abort_stale(upload_id)
    }

    /// Record an upload with an explicit start timestamp.
    ///
    /// Recovery and fsck tooling seed rows whose age must be exact; the
    /// normal [`Self::begin`] path always stamps the current time.
    pub fn register_at(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
        upload_id: &str,
        started_at: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO multipart_uploads
                    (upload_id, payload_hash, bucket, key, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![upload_id, payload_hash, bucket, key, started_at],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to insert multipart upload: {e}"))
            })?;
        Ok(())
    }

    /// Drop the row for `upload_id`, reporting whether it was tracked.
    ///
    /// Used by fsck repair to clean abandoned uploads.
    pub fn abort_if_tracked(&self, upload_id: &str) -> Result<bool> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| StagingError::Internal(format!("failed to begin transaction: {e}")))?;

        let parts = tx
            .execute(
                "DELETE FROM multipart_parts WHERE upload_id = ?1",
                params![upload_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete parts for {upload_id}: {e}"))
            })?;
        let removed = tx
            .execute(
                "DELETE FROM multipart_uploads WHERE upload_id = ?1",
                params![upload_id],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to delete upload {upload_id}: {e}"))
            })?;

        tx.commit().map_err(|e| {
            StagingError::Internal(format!("failed to commit abort for {upload_id}: {e}"))
        })?;

        if removed > 0 || parts > 0 {
            debug!(upload_id, "aborted tracked multipart upload");
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Find multipart uploads older than `grace_period` that are not
    /// marked completed.
    ///
    /// Used by fsck to detect and report abandoned uploads.
    pub fn find_abandoned(&self, grace_period: Duration) -> Result<Vec<AbandonedUpload>> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - grace_period.as_secs() as i64;

        let mut stmt = self
            .conn
            .prepare(
                "SELECT upload_id, payload_hash, bucket, key, started_at
                 FROM multipart_uploads
                 WHERE completed = 0 AND started_at < ?1",
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to prepare abandoned query: {e}"))
            })?;

        let uploads: Vec<AbandonedUpload> = stmt
            .query_map(params![cutoff], |row| {
                Ok(AbandonedUpload {
                    upload_id: row.get(0)?,
                    payload_hash: row.get(1)?,
                    bucket: row.get(2)?,
                    key: row.get(3)?,
                    started_at: row.get(4)?,
                })
            })
            .map_err(|e| StagingError::Internal(format!("failed to query abandoned uploads: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                StagingError::Internal(format!("failed to collect abandoned uploads: {e}"))
            })?;

        Ok(uploads)
    }
}

/// [`Sync`] wrapper implementing
/// [`crab_storage::multipart::MultipartJournal`].
///
/// `rusqlite::Connection` is `Send` but not `Sync`, while the journal
/// trait requires `Sync` so push tasks can hold the reference across
/// awaits. All journal calls are single fast SQLite statements, so a
/// plain mutex adds no meaningful contention.
pub struct SharedMultipartJournal(Mutex<MultipartRegistry>);

impl SharedMultipartJournal {
    /// Wraps a registry for use as a resumable-upload journal.
    #[must_use]
    pub fn new(registry: MultipartRegistry) -> Self {
        Self(Mutex::new(registry))
    }

    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, MultipartRegistry>> {
        self.0.lock()
    }

    /// See [`MultipartRegistry::find_abandoned`].
    pub fn find_abandoned(&self, grace_period: Duration) -> Result<Vec<AbandonedUpload>> {
        let registry = self
            .lock()
            .map_err(|_| StagingError::Internal("journal lock poisoned".into()))?;
        registry.find_abandoned(grace_period)
    }

    /// See [`MultipartRegistry::abort_if_tracked`].
    pub fn abort_if_tracked(&self, upload_id: &str) -> Result<bool> {
        let registry = self
            .lock()
            .map_err(|_| StagingError::Internal("journal lock poisoned".into()))?;
        registry.abort_if_tracked(upload_id)
    }
}

use std::sync::Mutex;

impl crab_storage::multipart::MultipartJournal for SharedMultipartJournal {
    fn begin(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> crab_storage::Result<bool> {
        let registry = self
            .lock()
            .map_err(|_| crab_storage::StorageError::Internal("journal lock poisoned".into()))?;
        registry
            .begin(payload_hash, bucket, key, upload_id)
            .map_err(|err| crab_storage::StorageError::Internal(err.to_string()))
    }

    fn record_part(
        &self,
        upload_id: &str,
        part_idx: usize,
        content_id: &str,
        size: u64,
    ) -> crab_storage::Result<()> {
        let size = i64::try_from(size)
            .map_err(|_| crab_storage::StorageError::Internal("part size overflow".into()))?;
        let registry = self
            .lock()
            .map_err(|_| crab_storage::StorageError::Internal("journal lock poisoned".into()))?;
        registry
            .record_part(upload_id, part_idx as i64, content_id, size)
            .map_err(|err| crab_storage::StorageError::Internal(err.to_string()))
    }

    fn complete(&self, upload_id: &str) -> crab_storage::Result<()> {
        let registry = self
            .lock()
            .map_err(|_| crab_storage::StorageError::Internal("journal lock poisoned".into()))?;
        registry
            .complete(upload_id)
            .map_err(|err| crab_storage::StorageError::Internal(err.to_string()))
    }

    fn abort_stale(&self, upload_id: &str) -> crab_storage::Result<()> {
        let registry = self
            .lock()
            .map_err(|_| crab_storage::StorageError::Internal("journal lock poisoned".into()))?;
        registry
            .abort_stale(upload_id)
            .map_err(|err| crab_storage::StorageError::Internal(err.to_string()))
    }

    fn resumable(
        &self,
        payload_hash: &[u8],
    ) -> crab_storage::Result<Option<crab_storage::multipart::ResumeInfo>> {
        let registry = self
            .lock()
            .map_err(|_| crab_storage::StorageError::Internal("journal lock poisoned".into()))?;
        let info = registry
            .resumable(payload_hash)
            .map_err(|err| crab_storage::StorageError::Internal(err.to_string()))?;
        let Some(info) = info else {
            return Ok(None);
        };
        let parts = info
            .completed_parts
            .iter()
            .map(|part| {
                Ok(crab_storage::multipart::JournalPart {
                    part_idx: usize::try_from(part.part_number).map_err(|_| {
                        crab_storage::StorageError::Internal("part index overflow".into())
                    })?,
                    content_id: part.etag.clone(),
                    size: part.size.max(0) as u64,
                })
            })
            .collect::<crab_storage::Result<Vec<_>>>()?;
        Ok(Some(crab_storage::multipart::ResumeInfo {
            upload_id: info.upload_id,
            parts,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn open_in_memory() -> MultipartRegistry {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
        let mut reg = MultipartRegistry { conn };
        reg.run_migrations().unwrap();
        reg
    }

    #[test]
    fn schema_migration_is_idempotent() {
        let mut reg = open_in_memory();
        reg.run_migrations().unwrap();
        reg.run_migrations().unwrap();
    }

    #[test]
    fn begin_and_resumable_round_trip() {
        let reg = open_in_memory();
        let hash = b"deadbeef01234567deadbeef01234567";

        reg.begin(hash, "my-bucket", "xet/xorbs/ab/abcd", "upload-1")
            .unwrap();
        reg.record_part("upload-1", 1, "\"etag-1\"", 5_000_000)
            .unwrap();
        reg.record_part("upload-1", 2, "\"etag-2\"", 5_000_000)
            .unwrap();

        let info = reg.resumable(hash).unwrap().expect("should be resumable");
        assert_eq!(info.upload_id, "upload-1");
        assert_eq!(info.bucket, "my-bucket");
        assert_eq!(info.key, "xet/xorbs/ab/abcd");
        assert_eq!(info.completed_parts.len(), 2);
        assert_eq!(info.completed_parts[0].part_number, 1);
        assert_eq!(info.completed_parts[0].etag, "\"etag-1\"");
        assert_eq!(info.completed_parts[1].part_number, 2);
    }

    #[test]
    fn resumable_returns_none_for_unknown_hash() {
        let reg = open_in_memory();
        assert!(reg.resumable(b"unknown").unwrap().is_none());
    }

    #[test]
    fn complete_removes_upload_and_parts() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();
        reg.record_part("u1", 1, "e1", 100).unwrap();
        reg.complete("u1").unwrap();

        assert!(reg.resumable(hash).unwrap().is_none());
    }

    #[test]
    fn abort_stale_removes_upload_and_parts() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();
        reg.record_part("u1", 1, "e1", 100).unwrap();
        reg.abort_stale("u1").unwrap();

        assert!(reg.resumable(hash).unwrap().is_none());
    }

    #[test]
    fn handle_no_such_upload_clears_row() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();
        reg.record_part("u1", 1, "e1", 100).unwrap();
        reg.handle_no_such_upload("u1").unwrap();

        assert!(reg.resumable(hash).unwrap().is_none());
    }

    #[test]
    fn find_abandoned_returns_old_uploads() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        // Insert with a manually backdated started_at.
        reg.conn
            .execute(
                "INSERT INTO multipart_uploads
                    (upload_id, payload_hash, bucket, key, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["old-upload", hash.as_slice(), "b", "k", 1_000_000i64],
            )
            .unwrap();

        // Recent upload should not appear.
        reg.begin(hash, "b", "k2", "new-upload").unwrap();

        let abandoned = reg.find_abandoned(Duration::from_secs(3600)).unwrap();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].upload_id, "old-upload");
    }

    #[test]
    fn find_abandoned_excludes_completed() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.conn
            .execute(
                "INSERT INTO multipart_uploads
                    (upload_id, payload_hash, bucket, key, started_at, completed)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
                params!["done-upload", hash.as_slice(), "b", "k", 1_000_000i64],
            )
            .unwrap();

        let abandoned = reg.find_abandoned(Duration::from_secs(3600)).unwrap();
        assert!(abandoned.is_empty());
    }

    #[test]
    fn resumable_returns_empty_parts_when_none_recorded() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();

        let info = reg.resumable(hash).unwrap().expect("should be resumable");
        assert_eq!(info.upload_id, "u1");
        assert!(info.completed_parts.is_empty());
    }

    #[test]
    fn begin_replaces_existing_upload_for_same_id() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "bucket-1", "key-1", "u1").unwrap();
        reg.begin(hash, "bucket-2", "key-2", "u1").unwrap();

        let info = reg.resumable(hash).unwrap().expect("should be resumable");
        assert_eq!(info.bucket, "bucket-2");
        assert_eq!(info.key, "key-2");
    }
}

#[cfg(test)]
mod journal_adapter_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn open_temp() -> (tempfile::TempDir, MultipartRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = MultipartRegistry::open(&dir.path().join("multipart.db")).unwrap();
        (dir, registry)
    }

    #[test]
    fn begin_rejects_concurrent_owner_of_same_payload() {
        let (_dir, reg) = open_temp();
        let hash = b"payload-hash";

        assert!(reg.begin(hash, "bucket", "key", "uploader-a").unwrap());
        assert!(
            !reg.begin(hash, "bucket", "key", "uploader-b").unwrap(),
            "second uploader must not clobber the active row"
        );
        // The original row survives untouched.
        let info = reg.resumable(hash).unwrap().expect("row intact");
        assert_eq!(info.upload_id, "uploader-a");
    }

    #[test]
    fn begin_allows_same_id_to_refresh_its_row() {
        let (_dir, reg) = open_temp();
        assert!(reg.begin(b"h", "b1", "k1", "u").unwrap());
        assert!(reg.begin(b"h", "b2", "k2", "u").unwrap());
        assert_eq!(reg.resumable(b"h").unwrap().unwrap().key, "k2");
    }

    #[test]
    fn shared_journal_adapter_round_trips_parts() {
        let (_dir, registry) = open_temp();
        let journal = SharedMultipartJournal::new(registry);
        use crab_storage::multipart::MultipartJournal;

        assert!(
            journal
                .begin(b"hash-bytes", "bucket", "some/key", "upload-9")
                .unwrap()
        );
        journal.record_part("upload-9", 0, "etag-0", 4096).unwrap();
        journal.record_part("upload-9", 1, "etag-1", 4096).unwrap();

        let resumed = journal
            .resumable(b"hash-bytes")
            .unwrap()
            .expect("resumable");
        assert_eq!(resumed.upload_id, "upload-9");
        assert_eq!(resumed.parts.len(), 2);
        assert_eq!(resumed.parts[0].content_id, "etag-0");

        journal.complete("upload-9").unwrap();
        assert!(journal.resumable(b"hash-bytes").unwrap().is_none());
    }
}
