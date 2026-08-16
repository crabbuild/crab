//! SQLite-backed registry for multipart upload resume.
//!
//! Tracks in-progress multipart uploads so that an interrupted push can
//! resume from the last completed part rather than re-uploading the
//! entire xorb. The database lives at `staging/multipart.db` alongside
//! the staging index.
//!
//! The uploader hooks in as follows:
//! - Before issuing the first part PUT, call [`MultipartRegistry::begin`]
//!   to persist the upload metadata.
//! - After each successful part PUT, call [`MultipartRegistry::record_part`].
//! - On successful `CompleteMultipartUpload`, call [`MultipartRegistry::complete`].
//! - On abort (cancel, error), call [`MultipartRegistry::abort_stale`].
//!
//! The resume path queries [`MultipartRegistry::resumable`] for each
//! planned xorb before starting a new upload. If a resumable upload
//! exists, only the missing parts are reissued.

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
    pub xorb_hash: Vec<u8>,
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
                    xorb_hash   BLOB NOT NULL,
                    bucket      TEXT NOT NULL,
                    key         TEXT NOT NULL,
                    started_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                    completed   INTEGER NOT NULL DEFAULT 0
                );

                CREATE INDEX IF NOT EXISTS multipart_uploads_by_xorb
                    ON multipart_uploads(xorb_hash);

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

    /// Record a new multipart upload before issuing the first part PUT.
    pub fn begin(&self, xorb_hash: &[u8], bucket: &str, key: &str, upload_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO multipart_uploads
                    (upload_id, xorb_hash, bucket, key)
                 VALUES (?1, ?2, ?3, ?4)",
                params![upload_id, xorb_hash, bucket, key],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to insert multipart upload: {e}"))
            })?;

        debug!(upload_id, "registered multipart upload");
        Ok(())
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
    pub fn resumable(&self, xorb_hash: &[u8]) -> Result<Option<ResumeInfo>> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT upload_id, bucket, key
                 FROM multipart_uploads
                 WHERE xorb_hash = ?1 AND completed = 0",
                params![xorb_hash],
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
                "SELECT upload_id, xorb_hash, bucket, key, started_at
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
                    xorb_hash: row.get(1)?,
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
                    (upload_id, xorb_hash, bucket, key, started_at)
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
                    (upload_id, xorb_hash, bucket, key, started_at, completed)
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
