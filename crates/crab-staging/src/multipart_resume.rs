//! SQLite-backed registry for multipart upload resume.
//!
//! Tracks in-progress multipart uploads so that an interrupted push can
//! resume from the last completed part rather than re-uploading the
//! entire xorb or pack. The database lives at `staging/multipart.db`
//! alongside the staging index. Product composition adapts this registry
//! to the storage transport's journal contract without coupling staging
//! to the storage crate.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use tracing::debug;

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

        conn.busy_timeout(Duration::from_secs(5)).map_err(|e| {
            StagingError::Internal(format!("failed to set multipart.db busy timeout: {e}"))
        })?;

        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| StagingError::Internal(format!("failed to set WAL mode: {e}")))?;

        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| {
                StagingError::Internal(format!("failed to set synchronous = NORMAL: {e}"))
            })?;

        // Enable foreign-key enforcement. Without this, SQLite silently
        // ignores the `FOREIGN KEY` clause below and orphaned rows in
        // `multipart_parts` can outlive their upload.
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
                    started_at  INTEGER NOT NULL DEFAULT (unixepoch())
                );

                CREATE UNIQUE INDEX IF NOT EXISTS multipart_uploads_active_object
                    ON multipart_uploads(payload_hash, bucket, key);

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
    /// payload and destination (a concurrent uploader); the caller must proceed
    /// unjournaled instead of clobbering that row.
    pub fn begin(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> Result<bool> {
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO multipart_uploads
                    (upload_id, payload_hash, bucket, key)
                 VALUES (?1, ?2, ?3, ?4)",
                params![upload_id, payload_hash, bucket, key],
            )
            .map_err(|e| {
                StagingError::Internal(format!("failed to insert multipart upload: {e}"))
            })?;

        if inserted == 0 {
            let owner: Option<String> = self
                .conn
                .query_row(
                    "SELECT upload_id FROM multipart_uploads
                     WHERE payload_hash = ?1 AND bucket = ?2 AND key = ?3",
                    params![payload_hash, bucket, key],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| {
                    StagingError::Internal(format!(
                        "failed to identify existing multipart upload: {e}"
                    ))
                })?;
            return Ok(owner.as_deref() == Some(upload_id));
        }

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
    /// Called after a hard upload error or when a recorded session is
    /// incompatible with the current upload plan.
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
    /// Returns `Some(ResumeInfo)` if an incomplete upload exists, including
    /// a session interrupted before its first part completed. Returns `None`
    /// if no resumable upload is found.
    pub fn resumable(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
    ) -> Result<Option<ResumeInfo>> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT upload_id, bucket, key
                 FROM multipart_uploads
                 WHERE payload_hash = ?1 AND bucket = ?2 AND key = ?3",
                params![payload_hash, bucket, key],
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
    pub fn find_abandoned(
        &self,
        now: SystemTime,
        grace_period: Duration,
    ) -> Result<Vec<AbandonedUpload>> {
        let now = i64::try_from(now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs())
            .unwrap_or(i64::MAX);
        let grace = i64::try_from(grace_period.as_secs()).unwrap_or(i64::MAX);
        let cutoff = now.saturating_sub(grace);

        let mut stmt = self
            .conn
            .prepare(
                "SELECT upload_id, payload_hash, bucket, key, started_at
                 FROM multipart_uploads
                 WHERE started_at < ?1",
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

        let info = reg
            .resumable(hash, "my-bucket", "xet/xorbs/ab/abcd")
            .unwrap()
            .expect("should be resumable");
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
        assert!(reg.resumable(b"unknown", "b", "k").unwrap().is_none());
    }

    #[test]
    fn complete_removes_upload_and_parts() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();
        reg.record_part("u1", 1, "e1", 100).unwrap();
        reg.complete("u1").unwrap();

        assert!(reg.resumable(hash, "b", "k").unwrap().is_none());
    }

    #[test]
    fn abort_stale_removes_upload_and_parts() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();
        reg.record_part("u1", 1, "e1", 100).unwrap();
        reg.abort_stale("u1").unwrap();

        assert!(reg.resumable(hash, "b", "k").unwrap().is_none());
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

        let abandoned = reg
            .find_abandoned(
                UNIX_EPOCH + Duration::from_secs(2_000_000),
                Duration::from_secs(3600),
            )
            .unwrap();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].upload_id, "old-upload");
    }

    #[test]
    fn resumable_returns_empty_parts_when_none_recorded() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        reg.begin(hash, "b", "k", "u1").unwrap();

        let info = reg
            .resumable(hash, "b", "k")
            .unwrap()
            .expect("should be resumable");
        assert_eq!(info.upload_id, "u1");
        assert!(info.completed_parts.is_empty());
    }

    #[test]
    fn begin_preserves_the_existing_object_owner() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        assert!(reg.begin(hash, "bucket", "key", "u1").unwrap());
        assert!(!reg.begin(hash, "bucket", "key", "u2").unwrap());

        let info = reg
            .resumable(hash, "bucket", "key")
            .unwrap()
            .expect("should be resumable");
        assert_eq!(info.upload_id, "u1");
    }

    #[test]
    fn same_payload_on_a_different_target_has_an_independent_row() {
        let reg = open_in_memory();
        let hash = b"deadbeef";

        assert!(reg.begin(hash, "bucket-1", "key", "u1").unwrap());
        assert!(reg.begin(hash, "bucket-2", "key", "u2").unwrap());

        assert_eq!(
            reg.resumable(hash, "bucket-1", "key")
                .unwrap()
                .unwrap()
                .upload_id,
            "u1"
        );
        assert_eq!(
            reg.resumable(hash, "bucket-2", "key")
                .unwrap()
                .unwrap()
                .upload_id,
            "u2"
        );
    }

    #[test]
    fn concurrent_connections_claim_one_active_upload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.db");
        let first = MultipartRegistry::open(&path).unwrap();
        let second = MultipartRegistry::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let first_barrier = barrier.clone();
        let first_claim = std::thread::spawn(move || {
            first_barrier.wait();
            first.begin(b"hash", "bucket", "key", "u1").unwrap()
        });
        let second_claim = std::thread::spawn(move || {
            barrier.wait();
            second.begin(b"hash", "bucket", "key", "u2").unwrap()
        });

        let claims = [first_claim.join().unwrap(), second_claim.join().unwrap()];
        assert_eq!(claims.into_iter().filter(|claimed| *claimed).count(), 1);

        let registry = MultipartRegistry::open(&path).unwrap();
        let owner = registry
            .resumable(b"hash", "bucket", "key")
            .unwrap()
            .unwrap()
            .upload_id;
        assert!(owner == "u1" || owner == "u2");
    }
}
