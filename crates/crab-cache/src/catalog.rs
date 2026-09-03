//! Transactional accounting and maintenance for disposable local cache state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt as _;
#[cfg(test)]
use rusqlite::{Connection, OpenFlags};
use rusqlite::{OptionalExtension as _, Transaction, params};

use crate::clean::{EntryKind, entry_kind};
#[cfg(test)]
use crate::private_fs::open_database;
use crate::private_fs::{Database, DatabaseLease, DatabaseMode, PinnedRoot};
use crate::{CacheError, Result};

const CATALOG_FILE: &str = ".catalog.sqlite";
const MAINTENANCE_LOCK: &str = ".maintenance.lock";
const LOW_WATERMARK_PERCENT: u64 = 90;
static OWNER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

enum Eviction {
    Retained,
    Missing,
    Removed(u64),
}

/// Shared cache catalog rooted at one effective product cache directory.
#[derive(Debug, Clone)]
pub struct CacheCatalog {
    root: PathBuf,
    max_bytes: u64,
}

/// Read-only catalog totals.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheCatalogStats {
    pub entries: u64,
    pub total_bytes: u64,
    pub temporary_bytes: u64,
    pub reservations_bytes: u64,
    pub last_maintenance_unix_ms: Option<u64>,
}

/// Result of one coalesced maintenance pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheMaintenanceStats {
    pub scanned_entries: u64,
    pub scanned_bytes: u64,
    pub evicted_entries: u64,
    pub evicted_bytes: u64,
    pub final_bytes: u64,
    pub coalesced: bool,
}

/// Active catalog lease preventing eviction of one cache file.
pub struct CacheLease {
    root: Arc<PinnedRoot>,
    generation: DatabaseLease,
    relative_path: String,
    owner: String,
}

/// Active byte reservation covering one in-progress cache write.
pub struct CacheReservation {
    root: Arc<PinnedRoot>,
    generation: DatabaseLease,
    relative_path: PathBuf,
    payload_lease: Option<std::fs::File>,
    id: String,
}

pub(crate) struct ReservedFile {
    pending: crate::private_fs::PendingFile,
    reservation: CacheReservation,
}

impl ReservedFile {
    pub(crate) fn file(&self) -> Result<tokio::fs::File> {
        self.pending.file()
    }

    pub(crate) async fn commit(self) -> Result<CacheReservation> {
        tokio::task::spawn_blocking(move || self.commit_sync())
            .await
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
    }

    fn commit_sync(self) -> Result<CacheReservation> {
        let Self {
            pending,
            reservation,
        } = self;
        // A fill can outlive every SQLite connection. Check the retained
        // generation in the publication worker, after all data writes finish.
        reservation
            .generation
            .validate(&reservation.root, Path::new(CATALOG_FILE))?;
        pending.commit_sync()?;
        Ok(reservation)
    }
}

impl CacheReservation {
    #[cfg(all(feature = "remote-client", feature = "local-cache"))]
    pub(crate) async fn anonymous_file(self) -> Result<(std::fs::File, Self)> {
        tokio::task::spawn_blocking(move || {
            self.generation
                .validate(&self.root, Path::new(CATALOG_FILE))?;
            let file = self
                .root
                .pending_file(&self.relative_path)?
                .into_unlinked_file()?;
            Ok((file, self))
        })
        .await
        .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
    }

    fn pending_file_sync(mut self) -> Result<ReservedFile> {
        self.generation
            .validate(&self.root, Path::new(CATALOG_FILE))?;
        let pending = self.root.pending_file(&self.relative_path)?;
        self.payload_lease = Some(pending.lease()?);
        Ok(ReservedFile {
            pending,
            reservation: self,
        })
    }

    pub(crate) async fn pending_file(self) -> Result<ReservedFile> {
        tokio::task::spawn_blocking(move || self.pending_file_sync())
            .await
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
    }

    pub(crate) async fn write(self, data: &[u8]) -> Result<Self> {
        use tokio::io::AsyncWriteExt as _;
        let pending = self.pending_file().await?;
        let mut writer = pending.file()?;
        writer.write_all(data).await?;
        writer.sync_all().await?;
        drop(writer);
        pending.commit().await
    }
}

impl Drop for CacheLease {
    fn drop(&mut self) {
        remove_owner_row(
            &self.root,
            &self.generation,
            "DELETE FROM leases WHERE relative_path = ?1 AND owner = ?2",
            &self.relative_path,
            &self.owner,
        );
    }
}

impl Drop for CacheReservation {
    fn drop(&mut self) {
        // Fields drop after this body. Keep the payload flock through row
        // removal so explicit maintenance cannot race the registration handoff.
        remove_owner_row(
            &self.root,
            &self.generation,
            "DELETE FROM reservations WHERE id = ?1 AND id = ?2",
            &self.id,
            &self.id,
        );
    }
}

impl CacheCatalog {
    /// Describe a catalog without creating the cache root or database.
    #[must_use]
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self { root, max_bytes }
    }

    /// Effective cache root accounted by this catalog.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Product-wide byte budget.
    #[must_use]
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Register a completed cache file and opportunistically enforce the budget.
    pub(crate) async fn record_and_maintain(
        &self,
        family: &'static str,
        logical_key: String,
        size: u64,
        reservation: CacheReservation,
    ) -> Result<CacheMaintenanceStats> {
        let catalog = self.clone();
        tokio::task::spawn_blocking(move || {
            catalog.record_completed_sync(family, &logical_key, size, reservation)
        })
        .await
        .map_err(|error| CacheError::Internal(format!("cache maintenance task failed: {error}")))?
    }

    fn record_completed_sync(
        &self,
        family: &'static str,
        logical_key: &str,
        size: u64,
        reservation: CacheReservation,
    ) -> Result<CacheMaintenanceStats> {
        // Both sync hints and async payloads retain the fill and its file lease
        // until registration commits. Releasing first permits uncharged bytes.
        let root = Arc::clone(&reservation.root);
        let path = self.root.join(&reservation.relative_path);
        let catalog_path = self.root.join(CATALOG_FILE);
        let connection = reservation.generation.open(
            &root,
            Path::new(CATALOG_FILE),
            std::time::Duration::from_secs(2),
        )?;
        let mut connection = configure_catalog(connection, &catalog_path)?;
        self.record_sync(&connection, family, &path, logical_key, size)?;
        drop(reservation);
        // Keep this bound connection through accounting and eviction. Reopening
        // by name after owner release could maintain an unrelated generation.
        let total = self.total_registered_bytes_sync(&connection)?;
        if total > self.max_bytes {
            self.maintain_at(&root, 0, &mut connection)
        } else {
            Ok(CacheMaintenanceStats {
                final_bytes: total,
                ..CacheMaintenanceStats::default()
            })
        }
    }

    #[cfg(feature = "local-cache")]
    pub(crate) fn write_sync(&self, path: &Path, family: &'static str, data: &[u8]) -> Result<()> {
        let size = data.len() as u64;
        if size > self.max_bytes {
            return Ok(());
        }
        let Some(reservation) = self.reserve_sync(path, size)? else {
            return Ok(());
        };
        let pending = reservation.pending_file_sync()?;
        pending.pending.write_body_sync(data)?;
        let reservation = pending.commit_sync()?;
        self.record_completed_sync(family, family, size, reservation)?;
        Ok(())
    }

    /// Reconcile the catalog with disk and evict deterministic LRU entries.
    pub async fn maintain(&self) -> Result<CacheMaintenanceStats> {
        let catalog = self.clone();
        tokio::task::spawn_blocking(move || catalog.maintain_sync(0))
            .await
            .map_err(|error| {
                CacheError::Internal(format!("cache maintenance task failed: {error}"))
            })?
    }

    /// Acquire an eviction lease for an existing root-relative cache file.
    pub async fn lease(&self, path: &Path) -> Result<CacheLease> {
        let catalog = self.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || catalog.lease_sync(&path))
            .await
            .map_err(|error| CacheError::Internal(format!("cache lease task failed: {error}")))?
    }

    /// Reserve bytes for an in-progress write, or return `None` when it cannot fit.
    pub async fn reserve(&self, path: &Path, size: u64) -> Result<Option<CacheReservation>> {
        if size > self.max_bytes {
            return Ok(None);
        }
        let catalog = self.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || catalog.reserve_sync(&path, size))
            .await
            .map_err(|error| {
                CacheError::Internal(format!("cache reservation task failed: {error}"))
            })?
    }

    /// Read an existing catalog without initializing a missing root or database.
    ///
    /// Inspection never writes database or recovery files. A busy or unsafe
    /// catalog returns an error instead of repairing state or bypassing its WAL.
    pub fn read_only_stats(root: &Path) -> Result<CacheCatalogStats> {
        let pinned = match PinnedRoot::open(root) {
            Ok(pinned) => pinned,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CacheCatalogStats::default());
            }
            Err(error) => return Err(error),
        };
        Ok(Self::read_only_stats_at(&pinned, root)?.unwrap_or_default())
    }

    pub(crate) fn read_only_stats_at(
        root: &PinnedRoot,
        display_root: &Path,
    ) -> Result<Option<CacheCatalogStats>> {
        let path = display_root.join(CATALOG_FILE);
        let mut connection = match root.open_database(
            Path::new(CATALOG_FILE),
            DatabaseMode::ReadOnly,
            std::time::Duration::from_secs(5),
        ) {
            Ok(connection) => connection,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        // All totals describe one snapshot, including reservations and the
        // maintenance marker. Independent autocommit reads can mix generations.
        let connection = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .map_err(|source| index_error(&path, source))?;
        let (entries, total_bytes) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM cache_entries",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
            )
            .map_err(|source| index_error(&path, source))?;
        let temporary_bytes = connection
            .query_row(
                "SELECT COALESCE(SUM(size), 0) FROM cache_entries WHERE family = 'temporary'",
                [],
                |row| row.get(0),
            )
            .map_err(|source| index_error(&path, source))?;
        let reservations_bytes = connection
            .query_row(
                "SELECT COALESCE(SUM(size), 0) FROM reservations",
                [],
                |row| row.get(0),
            )
            .map_err(|source| index_error(&path, source))?;
        let last_maintenance_unix_ms = connection
            .query_row(
                "SELECT value FROM catalog_meta WHERE key = 'last_maintenance_unix_ms'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|source| index_error(&path, source))?
            .and_then(|value| value.parse().ok());
        Ok(Some(CacheCatalogStats {
            entries,
            total_bytes,
            temporary_bytes,
            reservations_bytes,
            last_maintenance_unix_ms,
        }))
    }

    fn record_sync(
        &self,
        connection: &Database,
        family: &str,
        path: &Path,
        logical_key: &str,
        size: u64,
    ) -> Result<()> {
        let relative = relative_path(&self.root, path)?;
        connection
            .execute(
                "INSERT INTO cache_entries(relative_path, family, logical_key, size, last_access_ns, scan_generation)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)
                 ON CONFLICT(relative_path) DO UPDATE SET
                   family = excluded.family,
                   logical_key = excluded.logical_key,
                   size = excluded.size,
                   last_access_ns = excluded.last_access_ns",
                params![relative, family, logical_key, size, now_unix_ns()],
            )
            .map_err(|source| index_error(&self.root.join(CATALOG_FILE), source))?;
        Ok(())
    }

    fn lease_sync(&self, path: &Path) -> Result<CacheLease> {
        let relative_path = relative_path(&self.root, path)?;
        let catalog_path = self.root.join(CATALOG_FILE);
        let root = Arc::new(PinnedRoot::create(&self.root)?);
        let connection = open_catalog(&root, &catalog_path)?;
        let owner = next_owner("lease");
        connection
            .execute(
                "INSERT INTO leases(relative_path, owner, pid, acquired_ns) VALUES (?1, ?2, ?3, ?4)",
                params![relative_path, owner, std::process::id(), now_unix_ns()],
            )
            .map_err(|source| index_error(&catalog_path, source))?;
        Ok(CacheLease {
            generation: DatabaseLease::capture(&connection),
            root,
            relative_path,
            owner,
        })
    }

    fn reserve_sync(&self, path: &Path, size: u64) -> Result<Option<CacheReservation>> {
        let relative = relative_path(&self.root, path)?;
        let catalog_path = self.root.join(CATALOG_FILE);
        let root = Arc::new(PinnedRoot::create(&self.root)?);
        let mut connection = open_catalog(&root, &catalog_path)?;
        // Retry once after making space, always checking under the same writer
        // transaction as insertion. A maintenance snapshot cannot authorize a
        // reservation after another process has consumed the released capacity.
        for attempt in 0..2 {
            let transaction = connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|source| index_error(&catalog_path, source))?;
            let total: u64 = transaction
                .query_row(
                    "SELECT
                       (SELECT COALESCE(SUM(size), 0) FROM cache_entries) +
                       (SELECT COALESCE(SUM(size), 0) FROM reservations)",
                    [],
                    |row| row.get(0),
                )
                .map_err(|source| index_error(&catalog_path, source))?;
            if size <= self.max_bytes && total <= self.max_bytes - size {
                let id = next_owner("reservation");
                transaction
                    .execute(
                        "INSERT INTO reservations(id, relative_path, size, pid, created_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![id, relative, size, std::process::id(), now_unix_ns()],
                    )
                    .map_err(|source| index_error(&catalog_path, source))?;
                transaction
                    .commit()
                    .map_err(|source| index_error(&catalog_path, source))?;
                return Ok(Some(CacheReservation {
                    generation: DatabaseLease::capture(&connection),
                    root,
                    relative_path: PathBuf::from(relative),
                    payload_lease: None,
                    id,
                }));
            }
            drop(transaction);
            if attempt == 0 && !self.maintain_at(&root, size, &mut connection)?.coalesced {
                continue;
            }
            break;
        }
        Ok(None)
    }

    fn maintain_sync(&self, incoming_bytes: u64) -> Result<CacheMaintenanceStats> {
        let root = PinnedRoot::create(&self.root)?;
        let mut connection = open_catalog(&root, &self.root.join(CATALOG_FILE))?;
        self.maintain_at(&root, incoming_bytes, &mut connection)
    }

    fn maintain_at(
        &self,
        root: &PinnedRoot,
        incoming_bytes: u64,
        connection: &mut Database,
    ) -> Result<CacheMaintenanceStats> {
        let lock = root.open_lock(Path::new(MAINTENANCE_LOCK))?;
        if !lock.try_lock_exclusive()? {
            return Ok(CacheMaintenanceStats {
                coalesced: true,
                ..CacheMaintenanceStats::default()
            });
        }

        let result = self.maintain_locked(root, incoming_bytes, connection);
        let _ = lock.unlock();
        result
    }

    fn total_registered_bytes_sync(&self, connection: &Database) -> Result<u64> {
        let path = self.root.join(CATALOG_FILE);
        connection
            .query_row(
                "SELECT
                   (SELECT COALESCE(SUM(size), 0) FROM cache_entries) +
                   (SELECT COALESCE(SUM(size), 0) FROM reservations)",
                [],
                |row| row.get(0),
            )
            .map_err(|source| index_error(&path, source))
    }

    fn maintain_locked(
        &self,
        root: &PinnedRoot,
        incoming_bytes: u64,
        connection: &mut Database,
    ) -> Result<CacheMaintenanceStats> {
        let catalog_path = self.root.join(CATALOG_FILE);
        let generation = now_unix_ns();
        // Acquire the writer before reading owners. SQLite cannot wait on a
        // read-to-write upgrade when a reservation writer changed the snapshot.
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|source| index_error(&catalog_path, source))?;
        remove_stale_owners(&transaction, &catalog_path)?;
        let mut result = CacheMaintenanceStats::default();
        scan_catalog(root, &self.root, generation, &transaction, &mut result)?;
        transaction
            .execute(
                "DELETE FROM cache_entries WHERE scan_generation != ?1",
                [generation],
            )
            .map_err(|source| index_error(&catalog_path, source))?;
        transaction
            .execute(
                "INSERT INTO catalog_meta(key, value) VALUES ('last_maintenance_unix_ms', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [now_unix_ms().to_string()],
            )
            .map_err(|source| index_error(&catalog_path, source))?;
        transaction
            .commit()
            .map_err(|source| index_error(&catalog_path, source))?;

        let (total_bytes, reserved_bytes): (u64, u64) = connection
            .query_row(
                "SELECT
                   (SELECT COALESCE(SUM(size), 0) FROM cache_entries),
                   (SELECT COALESCE(SUM(size), 0) FROM reservations)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|source| index_error(&catalog_path, source))?;
        result.scanned_bytes = total_bytes;
        // Space is needed for the incoming fill even when existing files alone
        // are below the high watermark. Reservations belong to other active
        // fills and cannot be offered to this writer a second time.
        let available = self
            .max_bytes
            .saturating_sub(reserved_bytes)
            .saturating_sub(incoming_bytes);
        if total_bytes <= available {
            result.final_bytes = total_bytes;
            return Ok(result);
        }
        let target = (self.max_bytes / 100 * LOW_WATERMARK_PERCENT
            + self.max_bytes % 100 * LOW_WATERMARK_PERCENT / 100)
            .min(available);

        let mut remaining = total_bytes;
        let mut cursor = (0u64, String::new());
        while remaining > target {
            let page = {
                // A cache-root path alone is not eviction authority: mirror
                // repositories and in-flight workspaces also live here today.
                // Only owned immutable payload families are currently eligible.
                let mut statement = connection
                    .prepare(
                        "SELECT relative_path, size, last_access_ns FROM cache_entries
                         WHERE family IN ('chunk', 'decoded-range', 'xorb', 'shard', 'manifest', 'stage')
                           AND NOT EXISTS (
                             SELECT 1 FROM leases
                             WHERE leases.relative_path = cache_entries.relative_path
                           )
                           AND NOT EXISTS (
                             SELECT 1 FROM reservations
                             WHERE reservations.relative_path = cache_entries.relative_path
                           )
                           AND (last_access_ns > ?1 OR (last_access_ns = ?1 AND relative_path > ?2))
                         ORDER BY last_access_ns ASC, relative_path ASC
                         LIMIT 512",
                    )
                    .map_err(|source| index_error(&catalog_path, source))?;
                let rows = statement
                    .query_map(params![cursor.0, cursor.1], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, u64>(2)?,
                        ))
                    })
                    .map_err(|source| index_error(&catalog_path, source))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(|source| index_error(&catalog_path, source))?
            };
            if page.is_empty() {
                break;
            }
            for (relative, size, last_access) in page {
                cursor = (last_access, relative.clone());
                match self.evict_candidate(root, connection, &relative)? {
                    Eviction::Retained => {}
                    Eviction::Missing => remaining = remaining.saturating_sub(size),
                    Eviction::Removed(bytes) => {
                        remaining = remaining.saturating_sub(size);
                        result.evicted_entries = result.evicted_entries.saturating_add(1);
                        result.evicted_bytes = result.evicted_bytes.saturating_add(bytes);
                    }
                }
                if remaining <= target {
                    break;
                }
            }
        }
        result.final_bytes = remaining;
        Ok(result)
    }

    fn evict_candidate(
        &self,
        root: &PinnedRoot,
        connection: &mut Database,
        relative: &str,
    ) -> Result<Eviction> {
        // A recorded family is an accounting hint, not deletion authority.
        // Use the same fixed payload layouts as explicit cleanup, including
        // when a stale/corrupt catalog names live or unknown state.
        if !matches!(entry_kind(Path::new(relative)), EntryKind::Payload) {
            return Ok(Eviction::Retained);
        }
        let path = self.root.join(relative);
        let catalog_path = self.root.join(CATALOG_FILE);
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|source| index_error(&catalog_path, source))?;
        // Hold the writer transaction through unlink and row removal so a
        // lease/reservation cannot be inserted after this final owner check.
        let protected = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM leases WHERE relative_path = ?1)
                 OR EXISTS(SELECT 1 FROM reservations WHERE relative_path = ?1)",
                [relative],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|source| index_error(&catalog_path, source))?;
        if protected {
            return Ok(Eviction::Retained);
        }
        let outcome = match root.remove_file(Path::new(relative)) {
            Ok(bytes) => Eviction::Removed(bytes),
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                Eviction::Missing
            }
            Err(error) => {
                tracing::warn!(
                    family = "catalog",
                    operation = "evict",
                    path = %path.display(),
                    recovery = "retain-and-continue",
                    %error,
                    "cache maintenance could not evict entry"
                );
                return Ok(Eviction::Retained);
            }
        };
        transaction
            .execute(
                "DELETE FROM cache_entries WHERE relative_path = ?1",
                [relative],
            )
            .map_err(|source| index_error(&catalog_path, source))?;
        transaction
            .commit()
            .map_err(|source| index_error(&catalog_path, source))?;
        Ok(outcome)
    }
}

fn open_catalog(root: &PinnedRoot, path: &Path) -> Result<Database> {
    let connection = root.open_database(
        Path::new(CATALOG_FILE),
        DatabaseMode::Create,
        std::time::Duration::from_secs(2),
    )?;
    configure_catalog(connection, path)
}

fn configure_catalog(connection: Database, path: &Path) -> Result<Database> {
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS cache_entries (
               relative_path TEXT PRIMARY KEY,
               family TEXT NOT NULL,
               logical_key TEXT NOT NULL,
               size INTEGER NOT NULL CHECK(size >= 0),
               last_access_ns INTEGER NOT NULL,
               scan_generation INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS cache_entries_lru
               ON cache_entries(last_access_ns, relative_path);
             CREATE TABLE IF NOT EXISTS reservations (
               id TEXT PRIMARY KEY,
               relative_path TEXT NOT NULL,
               size INTEGER NOT NULL CHECK(size >= 0),
               pid INTEGER NOT NULL,
               created_ns INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS leases (
               relative_path TEXT NOT NULL,
               owner TEXT NOT NULL,
               pid INTEGER NOT NULL,
               acquired_ns INTEGER NOT NULL,
               PRIMARY KEY(relative_path, owner)
             );
             CREATE TABLE IF NOT EXISTS catalog_meta (
               key TEXT PRIMARY KEY,
               value TEXT NOT NULL
             );",
        )
        .map_err(|source| index_error(path, source))?;
    Ok(connection)
}

fn scan_catalog(
    pinned: &PinnedRoot,
    root: &Path,
    generation: u64,
    transaction: &Transaction<'_>,
    result: &mut CacheMaintenanceStats,
) -> Result<()> {
    pinned.visit_files(&mut |relative, metadata| {
        // Lossy path conversion could merge distinct unknown entries into one
        // catalog key. Abort reconciliation rather than undercount such state.
        let relative = relative.to_str().ok_or_else(|| CacheError::UnsafeRoot {
            path: root.display().to_string(),
            reason: "cache inventory path is not valid UTF-8".into(),
        })?;
        let family = classify_family(relative);
        transaction
            .execute(
                "INSERT INTO cache_entries(relative_path, family, logical_key, size, last_access_ns, scan_generation)
                 VALUES (?1, ?2, ?1, ?3, ?4, ?5)
                 ON CONFLICT(relative_path) DO UPDATE SET
                   family = excluded.family,
                   size = excluded.size,
                   last_access_ns = MAX(cache_entries.last_access_ns, excluded.last_access_ns),
                   scan_generation = excluded.scan_generation",
                params![relative, family, metadata.size, metadata.modified_ns, generation],
            )
            .map_err(|source| index_error(&root.join(CATALOG_FILE), source))?;
        result.scanned_entries = result.scanned_entries.saturating_add(1);
        Ok(())
    })
}

pub(crate) fn classify_family(relative: &str) -> &'static str {
    if relative == CATALOG_FILE || relative.starts_with(&format!("{CATALOG_FILE}-")) {
        return "catalog";
    }
    if relative == MAINTENANCE_LOCK || relative.starts_with("locks/") {
        return "lock";
    }
    if relative.contains(".tmp.")
        || relative.contains("/.tmp-")
        || relative.starts_with(".tmp-")
        || relative.starts_with(".sqlite-temp-")
        || relative.contains("/.sqlite-temp-")
    {
        return "temporary";
    }
    match relative.split('/').next().unwrap_or("") {
        "chunks" if relative.split('/').count() == 3 => "chunk",
        "chunks" => "decoded-range",
        "xorbs" => "xorb",
        "shards" => "shard",
        "manifests" => "manifest",
        "stages" => "stage",
        "buckets" | "repos" => "chunk-index",
        "xorb-index" => "xorb-index",
        _ if relative.contains("bloom") => "bloom",
        _ if relative.contains("shard-hint") => "shard-hint",
        _ => "other",
    }
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| CacheError::UnsafeRoot {
            path: path.display().to_string(),
            reason: format!("path is outside cache root {}", root.display()),
        })?;
    let value = relative
        .to_str()
        .ok_or_else(|| CacheError::UnsafeRoot {
            path: root.display().to_string(),
            reason: "cache entry path is not valid UTF-8".into(),
        })?
        .replace(std::path::MAIN_SEPARATOR, "/");
    if value.is_empty() || value.split('/').any(|component| component == "..") {
        return Err(CacheError::UnsafeRoot {
            path: path.display().to_string(),
            reason: "cache entry path is not a safe root-relative path".into(),
        });
    }
    Ok(value)
}

fn now_unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos().min(u128::from(u64::MAX)) as u64)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| {
            value.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

fn next_owner(prefix: &str) -> String {
    let sequence = OWNER_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{prefix}-{}-{sequence}", std::process::id())
}

fn remove_owner_row(
    root: &PinnedRoot,
    generation: &DatabaseLease,
    sql: &str,
    first: &str,
    second: &str,
) {
    // Release only through the captured root and main/owner generation.
    // Do not initialize a missing database during Drop; stale owners cannot
    // authorize schema creation or mutation in a newly selected cache root.
    let Ok(connection) = generation.open(
        root,
        Path::new(CATALOG_FILE),
        std::time::Duration::from_secs(5),
    ) else {
        return;
    };
    let _ = connection.execute(sql, params![first, second]);
}

fn remove_stale_owners(transaction: &Transaction<'_>, catalog_path: &Path) -> Result<()> {
    let mut pids = Vec::new();
    for table in ["leases", "reservations"] {
        let sql = format!("SELECT DISTINCT pid FROM {table}");
        let mut statement = transaction
            .prepare(&sql)
            .map_err(|source| index_error(catalog_path, source))?;
        let rows = statement
            .query_map([], |row| row.get::<_, u32>(0))
            .map_err(|source| index_error(catalog_path, source))?;
        for row in rows {
            pids.push(row.map_err(|source| index_error(catalog_path, source))?);
        }
    }
    pids.sort_unstable();
    pids.dedup();
    for pid in pids {
        if pid_is_alive(pid) {
            continue;
        }
        transaction
            .execute("DELETE FROM leases WHERE pid = ?1", [pid])
            .map_err(|source| index_error(catalog_path, source))?;
        transaction
            .execute("DELETE FROM reservations WHERE pid = ?1", [pid])
            .map_err(|source| index_error(catalog_path, source))?;
    }
    Ok(())
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: signal zero performs an existence/permission check only.
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

fn index_error(path: &Path, source: rusqlite::Error) -> CacheError {
    CacheError::Index {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests;
