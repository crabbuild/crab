//! SQLite-backed ownership journal for resumable multipart uploads.
//!
//! One row owns one exact provider destination. A process must atomically
//! acquire its expiring lease before it may create, upload, complete, abort,
//! or delete the corresponding provider session. Part records and lease
//! renewal commit in the same SQLite transaction.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tracing::debug;

use crate::error::{Result, StagingError};

const SCHEMA_VERSION: i64 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartTarget {
    pub provider: String,
    pub host: String,
    pub container: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedPart {
    pub part_number: i64,
    pub etag: String,
    pub size: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartLease {
    pub entry_id: String,
    pub owner_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartClaim {
    pub lease: MultipartLease,
    pub upload_id: Option<String>,
    pub payload_hash: Vec<u8>,
    pub expected_hash: [u8; 32],
    pub size: u64,
    pub part_size: usize,
    pub completed_parts: Vec<CompletedPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Acquired(MultipartClaim),
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedUpload {
    pub entry_id: String,
    pub upload_id: Option<String>,
    pub target: MultipartTarget,
    pub started_at: i64,
    pub updated_at: i64,
    /// Monotonic row revision captured by fsck for compare-and-swap repair.
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbandonedClaim {
    pub lease: MultipartLease,
    pub upload_id: Option<String>,
    pub target: MultipartTarget,
}

pub struct MultipartRegistry {
    conn: Connection,
}

impl MultipartRegistry {
    /// Opens the registry and migrates the pre-wiring prototype schema.
    ///
    /// The retired schema was never called by production code and cannot
    /// express leases or exact provider identity, so migration discards only
    /// those unusable prototype rows.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let mut registry = Self { conn };
        registry.run_migrations()?;
        Ok(registry)
    }

    fn run_migrations(&mut self) -> Result<()> {
        // Inspect the schema only after serializing initializers; an earlier
        // read could retire the canonical journal another process just created.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StagingError::from)?;
        let version = tx
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .map_err(StagingError::from)?;
        if version != 0 && version != SCHEMA_VERSION {
            return Err(StagingError::Internal(format!(
                "unsupported multipart journal schema version {version}"
            )));
        }
        if version == SCHEMA_VERSION {
            tx.commit().map_err(StagingError::from)?;
            debug!(version = SCHEMA_VERSION, "multipart journal schema ready");
            return Ok(());
        }

        let retire_prototype: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master
                WHERE type = 'table' AND name = 'multipart_uploads'
            )",
            [],
            |row| row.get(0),
        )?;
        if retire_prototype {
            tx.execute_batch(
                "DROP TABLE IF EXISTS multipart_parts;
                 DROP TABLE IF EXISTS multipart_uploads;",
            )?;
        }

        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS multipart_uploads (
                    entry_id          TEXT PRIMARY KEY,
                    upload_id         TEXT,
                    payload_hash      BLOB NOT NULL,
                    expected_hash     BLOB NOT NULL,
                    provider          TEXT NOT NULL,
                    host              TEXT NOT NULL,
                    container         TEXT NOT NULL,
                    key               TEXT NOT NULL,
                    size              INTEGER NOT NULL,
                    part_size         INTEGER NOT NULL,
                    owner_token       TEXT NOT NULL,
                    lease_expires_at  INTEGER NOT NULL,
                    started_at        INTEGER NOT NULL,
                    updated_at        INTEGER NOT NULL,
                    revision          INTEGER NOT NULL DEFAULT 0,
                    UNIQUE(provider, host, container, key)
                );

                CREATE TABLE IF NOT EXISTS multipart_parts (
                    entry_id     TEXT NOT NULL,
                    part_number  INTEGER NOT NULL,
                    etag         TEXT NOT NULL,
                    size         INTEGER NOT NULL,
                    PRIMARY KEY (entry_id, part_number),
                    FOREIGN KEY (entry_id) REFERENCES multipart_uploads(entry_id)
                        ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS multipart_uploads_abandoned
                    ON multipart_uploads(lease_expires_at, updated_at);",
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit().map_err(StagingError::from)?;
        debug!(version = SCHEMA_VERSION, "multipart journal schema ready");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim(
        &mut self,
        target: &MultipartTarget,
        payload_hash: &[u8],
        expected_hash: &[u8; 32],
        size: u64,
        part_size: usize,
        owner_token: &str,
        now: i64,
        lease_duration: Duration,
    ) -> Result<ClaimOutcome> {
        let size = to_sql_u64(size, "multipart size")?;
        let part_size = to_sql_usize(part_size, "multipart part size")?;
        let lease_expires_at = lease_deadline(now, lease_duration);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StagingError::from)?;

        let existing = tx
            .query_row(
                "SELECT entry_id, owner_token, lease_expires_at
                 FROM multipart_uploads
                 WHERE provider = ?1 AND host = ?2 AND container = ?3 AND key = ?4",
                params![target.provider, target.host, target.container, target.key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(StagingError::from)?;

        let entry_id = match existing {
            Some((entry_id, current_owner, expires_at)) => {
                if current_owner != owner_token && expires_at > now {
                    tx.commit().map_err(StagingError::from)?;
                    return Ok(ClaimOutcome::Busy);
                }
                let updated = tx
                    .execute(
                        "UPDATE multipart_uploads
                         SET owner_token = ?1, lease_expires_at = ?2, updated_at = ?3,
                             revision = revision + 1
                         WHERE entry_id = ?4
                           AND (owner_token = ?1 OR lease_expires_at <= ?3)",
                        params![owner_token, lease_expires_at, now, entry_id],
                    )
                    .map_err(StagingError::from)?;
                if updated == 0 {
                    tx.commit().map_err(StagingError::from)?;
                    return Ok(ClaimOutcome::Busy);
                }
                entry_id
            }
            None => {
                let entry_id = owner_token.to_owned();
                let inserted = tx
                    .execute(
                        "INSERT OR IGNORE INTO multipart_uploads (
                            entry_id, upload_id, payload_hash, expected_hash,
                            provider, host, container, key, size, part_size,
                            owner_token, lease_expires_at, started_at, updated_at
                         ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
                        params![
                            entry_id,
                            payload_hash,
                            expected_hash.as_slice(),
                            target.provider,
                            target.host,
                            target.container,
                            target.key,
                            size,
                            part_size,
                            owner_token,
                            lease_expires_at,
                            now,
                        ],
                    )
                    .map_err(StagingError::from)?;
                if inserted == 0 {
                    tx.commit().map_err(StagingError::from)?;
                    return Ok(ClaimOutcome::Busy);
                }
                entry_id
            }
        };

        let claim = load_claim(&tx, &entry_id, owner_token)?;
        tx.commit().map_err(StagingError::from)?;
        Ok(ClaimOutcome::Acquired(claim))
    }

    pub fn bind_upload(
        &self,
        lease: &MultipartLease,
        upload_id: &str,
        now: i64,
        lease_duration: Duration,
    ) -> Result<bool> {
        self.renewing_update(
            "UPDATE multipart_uploads
             SET upload_id = ?1, lease_expires_at = ?2, updated_at = ?3,
                 revision = revision + 1
             WHERE entry_id = ?4 AND owner_token = ?5 AND lease_expires_at > ?3",
            params![
                upload_id,
                lease_deadline(now, lease_duration),
                now,
                lease.entry_id,
                lease.owner_token,
            ],
        )
    }

    pub fn renew(
        &self,
        lease: &MultipartLease,
        now: i64,
        lease_duration: Duration,
    ) -> Result<bool> {
        self.renewing_update(
            "UPDATE multipart_uploads
             SET lease_expires_at = ?1, updated_at = ?2, revision = revision + 1
             WHERE entry_id = ?3 AND owner_token = ?4 AND lease_expires_at > ?2",
            params![
                lease_deadline(now, lease_duration),
                now,
                lease.entry_id,
                lease.owner_token,
            ],
        )
    }

    pub fn record_part(
        &mut self,
        lease: &MultipartLease,
        part: &CompletedPart,
        now: i64,
        lease_duration: Duration,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StagingError::from)?;
        let renewed = tx
            .execute(
                "UPDATE multipart_uploads
                 SET lease_expires_at = ?1, updated_at = ?2, revision = revision + 1
                 WHERE entry_id = ?3 AND owner_token = ?4 AND lease_expires_at > ?2",
                params![
                    lease_deadline(now, lease_duration),
                    now,
                    lease.entry_id,
                    lease.owner_token,
                ],
            )
            .map_err(StagingError::from)?;
        if renewed == 0 {
            tx.commit().map_err(StagingError::from)?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO multipart_parts(entry_id, part_number, etag, size)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(entry_id, part_number) DO UPDATE SET
                etag = excluded.etag, size = excluded.size",
            params![lease.entry_id, part.part_number, part.etag, part.size],
        )
        .map_err(StagingError::from)?;
        tx.commit().map_err(StagingError::from)?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reset_owned(
        &mut self,
        lease: &MultipartLease,
        payload_hash: &[u8],
        expected_hash: &[u8; 32],
        size: u64,
        part_size: usize,
        now: i64,
        lease_duration: Duration,
    ) -> Result<bool> {
        let size = to_sql_u64(size, "multipart size")?;
        let part_size = to_sql_usize(part_size, "multipart part size")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StagingError::from)?;
        let updated = tx
            .execute(
                "UPDATE multipart_uploads
                 SET upload_id = NULL, payload_hash = ?1, expected_hash = ?2,
                     size = ?3, part_size = ?4, lease_expires_at = ?5, updated_at = ?6,
                     revision = revision + 1
                 WHERE entry_id = ?7 AND owner_token = ?8 AND lease_expires_at > ?6",
                params![
                    payload_hash,
                    expected_hash.as_slice(),
                    size,
                    part_size,
                    lease_deadline(now, lease_duration),
                    now,
                    lease.entry_id,
                    lease.owner_token,
                ],
            )
            .map_err(StagingError::from)?;
        if updated > 0 {
            tx.execute(
                "DELETE FROM multipart_parts WHERE entry_id = ?1",
                params![lease.entry_id],
            )
            .map_err(StagingError::from)?;
        }
        tx.commit().map_err(StagingError::from)?;
        Ok(updated > 0)
    }

    pub fn complete_owned(&self, lease: &MultipartLease, now: i64) -> Result<bool> {
        self.delete_owned(lease, now)
    }

    pub fn abandon_owned(&self, lease: &MultipartLease, now: i64) -> Result<bool> {
        self.delete_owned(lease, now)
    }

    pub fn release_owned(&self, lease: &MultipartLease, now: i64) -> Result<bool> {
        self.renewing_update(
            "UPDATE multipart_uploads
             SET lease_expires_at = ?1, updated_at = ?1, revision = revision + 1
             WHERE entry_id = ?2 AND owner_token = ?3",
            params![now, lease.entry_id, lease.owner_token],
        )
    }

    pub fn find_abandoned(
        &self,
        now: SystemTime,
        grace_period: Duration,
    ) -> Result<Vec<AbandonedUpload>> {
        let now = unix_seconds(now);
        let cutoff = now.saturating_sub(duration_seconds(grace_period));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT entry_id, upload_id, provider, host, container, key,
                        started_at, updated_at, revision
                 FROM multipart_uploads
                 WHERE lease_expires_at <= ?1 AND updated_at < ?2
                 ORDER BY started_at, entry_id",
            )
            .map_err(StagingError::from)?;
        let rows = stmt
            .query_map(params![now, cutoff], |row| {
                Ok(AbandonedUpload {
                    entry_id: row.get(0)?,
                    upload_id: row.get(1)?,
                    target: MultipartTarget {
                        provider: row.get(2)?,
                        host: row.get(3)?,
                        container: row.get(4)?,
                        key: row.get(5)?,
                    },
                    started_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    revision: row.get(8)?,
                })
            })
            .map_err(StagingError::from)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StagingError::from)?;
        Ok(rows)
    }

    /// Claims an expired row for fsck immediately before provider cleanup.
    pub fn claim_abandoned(
        &mut self,
        entry_id: &str,
        expected_revision: i64,
        owner_token: &str,
        now: i64,
        lease_duration: Duration,
    ) -> Result<Option<AbandonedClaim>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StagingError::from)?;
        let updated = tx
            .execute(
                "UPDATE multipart_uploads
                 SET owner_token = ?1, lease_expires_at = ?2, updated_at = ?3,
                     revision = revision + 1
                 WHERE entry_id = ?4 AND revision = ?5 AND lease_expires_at <= ?3",
                params![
                    owner_token,
                    lease_deadline(now, lease_duration),
                    now,
                    entry_id,
                    expected_revision,
                ],
            )
            .map_err(StagingError::from)?;
        if updated == 0 {
            tx.commit().map_err(StagingError::from)?;
            return Ok(None);
        }
        let claim = tx
            .query_row(
                "SELECT upload_id, provider, host, container, key
                 FROM multipart_uploads
                 WHERE entry_id = ?1 AND owner_token = ?2",
                params![entry_id, owner_token],
                |row| {
                    Ok(AbandonedClaim {
                        lease: MultipartLease {
                            entry_id: entry_id.to_owned(),
                            owner_token: owner_token.to_owned(),
                        },
                        upload_id: row.get(0)?,
                        target: MultipartTarget {
                            provider: row.get(1)?,
                            host: row.get(2)?,
                            container: row.get(3)?,
                            key: row.get(4)?,
                        },
                    })
                },
            )
            .map_err(StagingError::from)?;
        tx.commit().map_err(StagingError::from)?;
        Ok(Some(claim))
    }

    fn renewing_update<P>(&self, sql: &str, params: P) -> Result<bool>
    where
        P: rusqlite::Params,
    {
        self.conn
            .execute(sql, params)
            .map(|updated| updated > 0)
            .map_err(StagingError::from)
    }

    fn delete_owned(&self, lease: &MultipartLease, now: i64) -> Result<bool> {
        self.conn
            .execute(
                "DELETE FROM multipart_uploads
                 WHERE entry_id = ?1 AND owner_token = ?2 AND lease_expires_at > ?3",
                params![lease.entry_id, lease.owner_token, now],
            )
            .map(|deleted| deleted > 0)
            .map_err(StagingError::from)
    }
}

fn load_claim(conn: &Connection, entry_id: &str, owner_token: &str) -> Result<MultipartClaim> {
    let row = conn
        .query_row(
            "SELECT upload_id, payload_hash, expected_hash, size, part_size
             FROM multipart_uploads
             WHERE entry_id = ?1 AND owner_token = ?2",
            params![entry_id, owner_token],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .map_err(StagingError::from)?;
    let expected_hash: [u8; 32] = row.2.try_into().map_err(|hash: Vec<u8>| {
        StagingError::Internal(format!(
            "multipart expected hash has {} bytes instead of 32",
            hash.len()
        ))
    })?;
    let mut stmt = conn
        .prepare(
            "SELECT part_number, etag, size FROM multipart_parts
             WHERE entry_id = ?1 ORDER BY part_number",
        )
        .map_err(StagingError::from)?;
    let completed_parts = stmt
        .query_map(params![entry_id], |part| {
            Ok(CompletedPart {
                part_number: part.get(0)?,
                etag: part.get(1)?,
                size: part.get(2)?,
            })
        })
        .map_err(StagingError::from)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StagingError::from)?;
    Ok(MultipartClaim {
        lease: MultipartLease {
            entry_id: entry_id.to_owned(),
            owner_token: owner_token.to_owned(),
        },
        upload_id: row.0,
        payload_hash: row.1,
        expected_hash,
        size: from_sql_u64(row.3, "multipart size")?,
        part_size: from_sql_usize(row.4, "multipart part size")?,
        completed_parts,
    })
}

fn lease_deadline(now: i64, duration: Duration) -> i64 {
    now.saturating_add(duration_seconds(duration).max(1))
}

fn duration_seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(i64::MAX)
}

fn to_sql_u64(value: u64, name: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StagingError::Internal(format!("{name} exceeds SQLite integer range")))
}

fn to_sql_usize(value: usize, name: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StagingError::Internal(format!("{name} exceeds SQLite integer range")))
}

fn from_sql_u64(value: i64, name: &str) -> Result<u64> {
    u64::try_from(value)
        .map_err(|_| StagingError::Internal(format!("{name} is negative in multipart journal")))
}

fn from_sql_usize(value: i64, name: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| StagingError::Internal(format!("{name} is negative in multipart journal")))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    const LEASE: Duration = Duration::from_secs(60);

    fn target() -> MultipartTarget {
        MultipartTarget {
            provider: "s3".into(),
            host: "endpoint".into(),
            container: "bucket".into(),
            key: "repo/xorb".into(),
        }
    }

    fn open_in_memory() -> MultipartRegistry {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let mut registry = MultipartRegistry { conn };
        registry.run_migrations().unwrap();
        registry
    }

    fn acquire(registry: &mut MultipartRegistry, owner: &str, now: i64) -> MultipartClaim {
        match registry
            .claim(&target(), b"payload", &[7; 32], 10, 4, owner, now, LEASE)
            .unwrap()
        {
            ClaimOutcome::Acquired(claim) => claim,
            ClaimOutcome::Busy => panic!("claim unexpectedly busy"),
        }
    }

    #[test]
    fn active_lease_excludes_other_owner() {
        let mut registry = open_in_memory();
        acquire(&mut registry, "owner-1", 100);
        let outcome = registry
            .claim(
                &target(),
                b"payload",
                &[7; 32],
                10,
                4,
                "owner-2",
                101,
                LEASE,
            )
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::Busy);
    }

    #[test]
    fn expired_lease_is_taken_over_with_parts_preserved() {
        let mut registry = open_in_memory();
        let first = acquire(&mut registry, "owner-1", 100);
        assert!(
            registry
                .bind_upload(&first.lease, "upload", 100, LEASE)
                .unwrap()
        );
        assert!(
            registry
                .record_part(
                    &first.lease,
                    &CompletedPart {
                        part_number: 0,
                        etag: "etag".into(),
                        size: 4,
                    },
                    100,
                    LEASE,
                )
                .unwrap()
        );
        let second = acquire(&mut registry, "owner-2", 161);
        assert_eq!(second.upload_id.as_deref(), Some("upload"));
        assert_eq!(second.completed_parts.len(), 1);
        assert_eq!(second.lease.owner_token, "owner-2");
    }

    #[test]
    fn stale_owner_cannot_record_complete_or_delete() {
        let mut registry = open_in_memory();
        let first = acquire(&mut registry, "owner-1", 100);
        let _second = acquire(&mut registry, "owner-2", 161);
        assert!(
            !registry
                .bind_upload(&first.lease, "stale", 161, LEASE)
                .unwrap()
        );
        assert!(
            !registry
                .record_part(
                    &first.lease,
                    &CompletedPart {
                        part_number: 0,
                        etag: "stale".into(),
                        size: 4,
                    },
                    161,
                    LEASE,
                )
                .unwrap()
        );
        assert!(!registry.complete_owned(&first.lease, 161).unwrap());
    }

    #[test]
    fn expired_owner_cannot_complete_without_reacquiring_lease() {
        let mut registry = open_in_memory();
        let claim = acquire(&mut registry, "owner", 100);

        assert!(!registry.complete_owned(&claim.lease, 161).unwrap());
        let resumed = acquire(&mut registry, "owner", 161);
        assert!(registry.complete_owned(&resumed.lease, 161).unwrap());
    }

    #[test]
    fn heartbeat_keeps_old_upload_out_of_fsck() {
        let mut registry = open_in_memory();
        let claim = acquire(&mut registry, "owner", 100);
        assert!(registry.renew(&claim.lease, 150, LEASE).unwrap());
        let abandoned = registry
            .find_abandoned(
                UNIX_EPOCH + Duration::from_secs(190),
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(abandoned.is_empty());
    }

    #[test]
    fn fsck_claim_loses_race_to_resumed_owner() {
        let mut registry = open_in_memory();
        let first = acquire(&mut registry, "owner-1", 100);
        assert!(registry.release_owned(&first.lease, 100).unwrap());
        let observed = registry
            .find_abandoned(UNIX_EPOCH + Duration::from_secs(101), Duration::ZERO)
            .unwrap()
            .pop()
            .expect("released upload is visible to fsck");
        let _resumed = acquire(&mut registry, "owner-2", 101);
        let fsck = registry
            .claim_abandoned("owner-1", observed.revision, "fsck", 101, LEASE)
            .unwrap();
        assert!(fsck.is_none());
    }

    #[test]
    fn fsck_claim_refuses_replacement_after_resume_race() {
        let mut registry = open_in_memory();
        let first = acquire(&mut registry, "owner-1", 100);
        assert!(
            registry
                .bind_upload(&first.lease, "old-upload", 100, LEASE)
                .unwrap()
        );
        assert!(registry.release_owned(&first.lease, 100).unwrap());
        let observed = registry
            .find_abandoned(UNIX_EPOCH + Duration::from_secs(101), Duration::ZERO)
            .unwrap()
            .pop()
            .expect("released upload is visible to fsck");

        let resumed = acquire(&mut registry, "owner-2", 101);
        assert!(
            registry
                .reset_owned(&resumed.lease, b"payload", &[7; 32], 10, 4, 101, LEASE,)
                .unwrap()
        );
        assert!(
            registry
                .bind_upload(&resumed.lease, "new-upload", 101, LEASE)
                .unwrap()
        );
        assert!(registry.release_owned(&resumed.lease, 101).unwrap());

        let claim = registry
            .claim_abandoned("owner-1", observed.revision, "fsck", 102, LEASE)
            .unwrap();

        assert!(claim.is_none());
    }

    #[test]
    fn exact_destination_distinguishes_provider_and_host() {
        let mut registry = open_in_memory();
        acquire(&mut registry, "owner-1", 100);
        let mut other = target();
        other.host = "other-endpoint".into();
        let outcome = registry
            .claim(&other, b"payload", &[7; 32], 10, 4, "owner-2", 100, LEASE)
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Acquired(_)));
    }

    #[test]
    fn prototype_schema_is_replaced_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE multipart_uploads (
                upload_id TEXT PRIMARY KEY,
                xorb_hash BLOB NOT NULL,
                bucket TEXT NOT NULL,
                key TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                completed INTEGER NOT NULL
             );",
        )
        .unwrap();
        drop(conn);
        let mut registry = MultipartRegistry::open(&path).unwrap();
        acquire(&mut registry, "owner", 100);
        drop(registry);
        MultipartRegistry::open(&path).unwrap();
    }

    #[test]
    fn concurrent_resumers_on_independent_connections_have_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.db");
        let mut registry = MultipartRegistry::open(&path).unwrap();
        let original = acquire(&mut registry, "original", 100);
        assert!(
            registry
                .bind_upload(&original.lease, "existing-upload", 100, LEASE)
                .unwrap()
        );
        let part = CompletedPart {
            part_number: 0,
            etag: "existing-part".into(),
            size: 4,
        };
        assert!(
            registry
                .record_part(&original.lease, &part, 100, LEASE)
                .unwrap()
        );
        drop(registry);

        // Separate SQLite connections share only the on-disk database, not a
        // process-local mutex. Both contenders resume the same expired session.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let contenders = ["resumer-a", "resumer-b"].map(|owner| {
            let path = path.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut registry = MultipartRegistry::open(&path).unwrap();
                barrier.wait();
                registry
                    .claim(&target(), b"payload", &[7; 32], 10, 4, owner, 161, LEASE)
                    .unwrap()
            })
        });
        let outcomes = contenders.map(|thread| thread.join().unwrap());
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, ClaimOutcome::Busy))
                .count(),
            1
        );
        let winner = outcomes
            .into_iter()
            .find_map(|outcome| match outcome {
                ClaimOutcome::Acquired(claim) => Some(claim),
                ClaimOutcome::Busy => None,
            })
            .unwrap();
        assert_eq!(winner.upload_id.as_deref(), Some("existing-upload"));
        assert_eq!(winner.completed_parts, vec![part]);
        let registry = MultipartRegistry::open(&path).unwrap();
        assert!(!registry.complete_owned(&original.lease, 162).unwrap());
        assert!(registry.complete_owned(&winner.lease, 162).unwrap());
    }

    #[test]
    fn concurrent_initializer_rechecks_schema_after_acquiring_write_lock() {
        use std::sync::atomic::{AtomicBool, Ordering};

        static WAITING_FOR_MIGRATION: AtomicBool = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multipart.db");
        let mut registry = MultipartRegistry::open(&path).unwrap();
        let claim = acquire(&mut registry, "owner", 100);
        registry
            .conn
            .pragma_update(None, "user_version", 0)
            .unwrap();
        let tx = registry
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .unwrap();

        // The second initializer sees the old committed version until this
        // transaction commits, exactly as during prototype retirement.
        let other = std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.busy_handler(Some(|_| {
                WAITING_FOR_MIGRATION.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1));
                true
            }))
            .unwrap();
            let mut other = MultipartRegistry { conn };
            other.run_migrations().unwrap();
            other
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !WAITING_FOR_MIGRATION.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "initializer did not reach the write lock"
            );
            std::thread::yield_now();
        }
        tx.commit().unwrap();
        let other = other.join().unwrap();

        assert!(other.renew(&claim.lease, 101, LEASE).unwrap());
    }
}
