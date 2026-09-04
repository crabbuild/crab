//! Persistent on-disk chunk index backed by SQLite.
//!
//! Maintains a write-through cache at
//! `~/.cache/crab/buckets/{bucket-hash}/chunk-index.sqlite` so dedup is
//! immediately effective across repositories and sessions in one bucket.
//!
//! Tables:
//! - `chunks_v1`: chunk hash (32 bytes) -> xorb hash (32 bytes), chunk index, uncompressed size
//! - `shards_v1`: shard hash (32 bytes) -> presence marker
//! - `meta_v1`: string key -> string value (schema version, GC generation, etc.)
//!
//! SQLite WAL mode gives concurrent readers plus a single serialized writer,
//! matching the cache's mix of many point reads and shard-atomic writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params, params_from_iter};
use tracing::info;

use crate::error::{MetadataError, Result};
use crab_xet::xorb::format::{MerkleHash, XorbRef};

const SCHEMA_VERSION_KEY: &str = "schema_version";
const SCHEMA_DESCRIPTOR_KEY: &str = "schema_descriptor";
pub const PERSISTENT_CHUNK_INDEX_SCHEMA_VERSION: &str = "1";
const SCHEMA_DESCRIPTOR: &str = "chunks_v1(chunk_hash,xorb_hash,chunk_index,uncompressed_size);shards_v1(shard_hash);meta_v1(key,value)";
const CACHE_GC_GENERATION_KEY: &str = "cache_gc_generation";

/// Persistent chunk index backed by a SQLite database file.
pub struct PersistentChunkIndex {
    conn: Mutex<Connection>,
    path: PathBuf,
}

/// Non-mutating summary of an existing persistent chunk index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistentChunkIndexStats {
    /// Number of cached chunk-to-xorb mappings.
    pub entry_count: u64,
    /// Number of shard installation receipts.
    pub installed_shard_count: u64,
    /// Last remote GC generation observed by this cache.
    pub cache_gc_generation: u64,
}

static SHARED_INDICES: OnceLock<Mutex<HashMap<PathBuf, Weak<PersistentChunkIndex>>>> =
    OnceLock::new();

impl PersistentChunkIndex {
    /// Inspect an existing index through a read-only SQLite connection.
    ///
    /// Returns `None` when the database file does not exist. Unlike
    /// [`Self::open_or_create`], this never creates, migrates, repairs, or
    /// replaces the cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the existing database cannot be read or does not
    /// use the current persistent chunk-index schema.
    pub fn inspect(path: &Path) -> Result<Option<PersistentChunkIndexStats>> {
        if !path.exists() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_sqlite_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(map_sqlite_err)?;
        validate_schema(&conn, path)?;
        let count = |table: &str| -> Result<u64> {
            let value: i64 = conn
                .query_row(&format!("SELECT COUNT(1) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .map_err(map_sqlite_err)?;
            u64::try_from(value).map_err(|error| {
                MetadataError::Internal(format!("negative row count for {table}: {error}"))
            })
        };
        let raw_generation: Option<String> = conn
            .query_row(
                "SELECT value FROM meta_v1 WHERE key = ?1",
                params![CACHE_GC_GENERATION_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;
        let cache_gc_generation = match raw_generation {
            Some(raw) => raw
                .parse::<u64>()
                .map_err(|error| MetadataError::CorruptObject {
                    path: path.display().to_string(),
                    reason: format!("cache_gc_generation not a u64: {raw:?} ({error})"),
                })?,
            None => 0,
        };
        Ok(Some(PersistentChunkIndexStats {
            entry_count: count("chunks_v1")?,
            installed_shard_count: count("shards_v1")?,
            cache_gc_generation,
        }))
    }

    /// Open or reuse the process-wide handle for an index path.
    ///
    /// SQLite permits multiple live connections, but sharing one handle per
    /// path keeps the in-process writer queue local and avoids redundant
    /// statement cache and WAL checkpoint churn in long-lived daemons.
    ///
    /// # Errors
    /// Returns `MetadataError::Io` on filesystem errors, or
    /// `MetadataError::Sqlite` on SQLite failures.
    pub fn open_shared(path: &Path) -> Result<Arc<Self>> {
        let key = normalize_index_path(path)?;
        let registry = SHARED_INDICES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut indices = registry.lock().map_err(|_| {
            MetadataError::Internal("persistent chunk index registry poisoned".into())
        })?;

        if let Some(index) = indices.get(&key).and_then(Weak::upgrade) {
            return Ok(index);
        }

        indices.retain(|_, index| index.strong_count() > 0);

        let index = Arc::new(Self::open_or_create(&key)?);
        indices.insert(key, Arc::downgrade(&index));
        Ok(index)
    }

    /// Open an existing index or create a new one at the given path.
    ///
    /// Existing non-v1 state is rejected without mutation. Delete that exact
    /// disposable cache file before retrying to create the canonical v1 shape.
    ///
    /// # Errors
    /// Returns `MetadataError::Io` on filesystem errors, or
    /// `MetadataError::Sqlite` on SQLite failures.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        let path = normalize_index_path(path)?;
        // SQLite's Unix VFS defaults new databases to 0644 and derives WAL
        // permissions from the main file. Establish private mode before any
        // connection writes bytes; never chmod or truncate an existing index.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let created = match options.open(&path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(error) => return Err(error.into()),
        };
        let existed = created.is_none();
        if existed {
            let readonly = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(map_sqlite_err)?;
            validate_schema(&readonly, &path)?;
        }

        let conn = Connection::open(&path).map_err(map_sqlite_err)?;
        configure_connection(&conn)?;
        if existed {
            validate_schema(&conn, &path)?;
        } else {
            initialize_schema(&conn)?;
        }

        let index = Self {
            conn: Mutex::new(conn),
            path,
        };
        Ok(index)
    }

    /// Look up a chunk hash in the persistent index.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite read failures.
    pub fn get(&self, chunk_hash: &MerkleHash) -> Result<Option<XorbRef>> {
        let conn = self.connection()?;
        let key = hash_bytes(chunk_hash);
        conn.query_row(
            "SELECT xorb_hash, chunk_index, uncompressed_size
             FROM chunks_v1
             WHERE chunk_hash = ?1",
            params![key.as_slice()],
            xorb_ref_from_row,
        )
        .optional()
        .map_err(map_sqlite_err)
    }

    /// Look up many chunk hashes while preserving input order.
    ///
    /// Duplicates in `chunk_hashes` are returned at every matching input
    /// position. Misses remain `None`.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite read failures.
    pub fn get_batch(&self, chunk_hashes: &[MerkleHash]) -> Result<Vec<Option<XorbRef>>> {
        if chunk_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.connection()?;
        let mut results = vec![None; chunk_hashes.len()];
        let mut indices_by_hash: HashMap<[u8; 32], Vec<usize>> =
            HashMap::with_capacity(chunk_hashes.len());
        for (idx, chunk_hash) in chunk_hashes.iter().enumerate() {
            indices_by_hash
                .entry(hash_bytes(chunk_hash))
                .or_default()
                .push(idx);
        }

        let unique_hashes: Vec<[u8; 32]> = indices_by_hash.keys().copied().collect();
        for batch in unique_hashes.chunks(900) {
            let placeholders = vec!["?"; batch.len()].join(",");
            let sql = format!(
                "SELECT chunk_hash, xorb_hash, chunk_index, uncompressed_size
                 FROM chunks_v1
                 WHERE chunk_hash IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let rows = stmt
                .query_map(
                    params_from_iter(batch.iter().map(|hash| hash.as_slice())),
                    |row| {
                        let chunk_hash = blob_to_hash(row.get::<_, Vec<u8>>(0)?)?;
                        let xorb_ref = xorb_ref_from_row_offset(row, 1)?;
                        Ok((hash_bytes(&chunk_hash), xorb_ref))
                    },
                )
                .map_err(map_sqlite_err)?;

            for row in rows {
                let (chunk_hash, xorb_ref) = row.map_err(map_sqlite_err)?;
                if let Some(indices) = indices_by_hash.get(&chunk_hash) {
                    for &idx in indices {
                        results[idx] = Some(xorb_ref);
                    }
                }
            }
        }

        Ok(results)
    }

    /// Install a shard's chunk entries atomically in a single transaction.
    ///
    /// Re-installing the same shard with the same entries is idempotent. The
    /// entries are still upserted before the shard marker so stale marker-only
    /// cache state from an older process can be repaired by the next shard sync.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite write failures.
    pub fn install_shard(
        &self,
        shard_hash: MerkleHash,
        entries: &[(MerkleHash, XorbRef)],
    ) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;

        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO chunks_v1
                     (chunk_hash, xorb_hash, chunk_index, uncompressed_size)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(map_sqlite_err)?;
            for (chunk_hash, xorb_ref) in entries {
                insert_chunk_with_statement(&mut stmt, chunk_hash, xorb_ref)?;
            }
        }

        let shard_key = hash_bytes(&shard_hash);
        tx.execute(
            "INSERT OR REPLACE INTO shards_v1 (shard_hash) VALUES (?1)",
            params![shard_key.as_slice()],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Insert a single chunk entry from a lazy-on-miss fill.
    ///
    /// Writes only to `chunks_v1`; does not mark any shard as installed.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite write failures.
    pub fn insert(&self, chunk_hash: &MerkleHash, xorb_ref: &XorbRef) -> Result<()> {
        self.insert_batch(&[(*chunk_hash, *xorb_ref)])
    }

    /// Insert chunk entries from a lazy batch fill in a single transaction.
    ///
    /// Writes only to `chunks_v1` and does not mark any shard as installed.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite write failures.
    pub fn insert_batch(&self, entries: &[(MerkleHash, XorbRef)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO chunks_v1
                     (chunk_hash, xorb_hash, chunk_index, uncompressed_size)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(map_sqlite_err)?;
            for (chunk_hash, xorb_ref) in entries {
                insert_chunk_with_statement(&mut stmt, chunk_hash, xorb_ref)?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Remove stale candidate mappings from the local acceleration tier.
    pub fn remove_batch(&self, chunk_hashes: &[MerkleHash]) -> Result<()> {
        if chunk_hashes.is_empty() {
            return Ok(());
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        for chunk_hash in chunk_hashes {
            let hash = hash_bytes(chunk_hash);
            tx.execute(
                "DELETE FROM chunks_v1 WHERE chunk_hash = ?1",
                params![hash.as_slice()],
            )
            .map_err(map_sqlite_err)?;
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Read the cached GC generation, returning 0 when absent.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite read failures, or
    /// `MetadataError::CorruptObject` if the stored value is not a valid u64.
    pub fn cache_gc_generation(&self) -> Result<u64> {
        let conn = self.connection()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM meta_v1 WHERE key = ?1",
                params![CACHE_GC_GENERATION_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        match raw {
            Some(raw) => raw
                .parse::<u64>()
                .map_err(|e| MetadataError::CorruptObject {
                    path: self.path.display().to_string(),
                    reason: format!("cache_gc_generation not a u64: {raw:?} ({e})"),
                }),
            None => Ok(0),
        }
    }

    /// Persist the cached GC generation as a decimal string.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite write failures.
    pub fn set_cache_gc_generation(&self, generation: u64) -> Result<()> {
        let encoded = generation.to_string();
        let conn = self.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO meta_v1 (key, value) VALUES (?1, ?2)",
            params![CACHE_GC_GENERATION_KEY, encoded],
        )
        .map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Wipe all chunk and shard entries while preserving schema metadata.
    ///
    /// Used when remote GC drift exceeds the grace window and the local
    /// cache must be invalidated without losing the generation cursor.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite write failures.
    pub fn clear_entries(&self) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM chunks_v1", [])
            .map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM shards_v1", [])
            .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Rebuild the entire index from a set of shards.
    ///
    /// Clears all existing chunk and shard entries and re-inserts
    /// everything in a single transaction.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite write failures.
    pub fn rebuild_from_cache(
        &self,
        shards: &[(MerkleHash, Vec<(MerkleHash, XorbRef)>)],
    ) -> Result<()> {
        info!(
            shard_count = shards.len(),
            "rebuilding persistent chunk index from shard cache"
        );

        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM chunks_v1", [])
            .map_err(map_sqlite_err)?;
        tx.execute("DELETE FROM shards_v1", [])
            .map_err(map_sqlite_err)?;

        {
            let mut chunk_stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO chunks_v1
                     (chunk_hash, xorb_hash, chunk_index, uncompressed_size)
                     VALUES (?1, ?2, ?3, ?4)",
                )
                .map_err(map_sqlite_err)?;
            let mut shard_stmt = tx
                .prepare("INSERT OR REPLACE INTO shards_v1 (shard_hash) VALUES (?1)")
                .map_err(map_sqlite_err)?;

            for (shard_hash, entries) in shards {
                for (chunk_hash, xorb_ref) in entries {
                    insert_chunk_with_statement(&mut chunk_stmt, chunk_hash, xorb_ref)?;
                }
                let shard_key = hash_bytes(shard_hash);
                shard_stmt
                    .execute(params![shard_key.as_slice()])
                    .map_err(map_sqlite_err)?;
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO meta_v1 (key, value) VALUES (?1, ?2)",
            params![SCHEMA_VERSION_KEY, PERSISTENT_CHUNK_INDEX_SCHEMA_VERSION],
        )
        .map_err(map_sqlite_err)?;
        tx.commit().map_err(map_sqlite_err)?;

        info!("persistent chunk index rebuild complete");
        Ok(())
    }

    /// Load all chunk entries from the persistent index into memory.
    ///
    /// Used at startup to populate the in-memory `ChunkIndex` mirror.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite read failures.
    pub fn load_all(&self) -> Result<Vec<(MerkleHash, XorbRef)>> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare(
                "SELECT chunk_hash, xorb_hash, chunk_index, uncompressed_size
                 FROM chunks_v1",
            )
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let chunk_hash = blob_to_hash(row.get::<_, Vec<u8>>(0)?)?;
                let xorb_ref = xorb_ref_from_row_offset(row, 1)?;
                Ok((chunk_hash, xorb_ref))
            })
            .map_err(map_sqlite_err)?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(map_sqlite_err)?);
        }
        Ok(result)
    }

    /// Check whether a specific shard has been installed.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite read failures.
    pub fn has_shard(&self, shard_hash: &MerkleHash) -> Result<bool> {
        let conn = self.connection()?;
        let shard_key = hash_bytes(shard_hash);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(1) FROM shards_v1 WHERE shard_hash = ?1",
                params![shard_key.as_slice()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_err)?;
        Ok(count > 0)
    }

    /// Return the list of shard hashes that have been installed.
    ///
    /// # Errors
    /// Returns `MetadataError::Internal` on SQLite read failures.
    pub fn installed_shards(&self) -> Result<Vec<MerkleHash>> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("SELECT shard_hash FROM shards_v1")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| blob_to_hash(row.get::<_, Vec<u8>>(0)?))
            .map_err(map_sqlite_err)?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(map_sqlite_err)?);
        }
        Ok(result)
    }

    #[cfg(test)]
    fn verify_schema(&self) -> Result<()> {
        let conn = self.connection()?;
        validate_schema(&conn, &self.path)
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| MetadataError::Internal("persistent chunk index mutex poisoned".into()))
    }
}

fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(map_sqlite_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(map_sqlite_err)?;
    conn.pragma_update(None, "busy_timeout", "5000")
        .map_err(map_sqlite_err)?;
    Ok(())
}

fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chunks_v1 (
            chunk_hash BLOB PRIMARY KEY NOT NULL CHECK(length(chunk_hash) = 32),
            xorb_hash BLOB NOT NULL CHECK(length(xorb_hash) = 32),
            chunk_index INTEGER NOT NULL CHECK(chunk_index >= 0),
            uncompressed_size INTEGER NOT NULL CHECK(uncompressed_size >= 0)
        );
        CREATE TABLE IF NOT EXISTS shards_v1 (
            shard_hash BLOB PRIMARY KEY NOT NULL CHECK(length(shard_hash) = 32)
        );
        CREATE TABLE IF NOT EXISTS meta_v1 (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );",
    )
    .map_err(map_sqlite_err)?;
    conn.execute(
        "INSERT INTO meta_v1 (key, value) VALUES (?1, ?2), (?3, ?4)",
        params![
            SCHEMA_VERSION_KEY,
            PERSISTENT_CHUNK_INDEX_SCHEMA_VERSION,
            SCHEMA_DESCRIPTOR_KEY,
            SCHEMA_DESCRIPTOR
        ],
    )
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn validate_schema(conn: &Connection, path: &Path) -> Result<()> {
    let metadata = |key: &str| -> Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM meta_v1 WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite_err)
    };
    let version = metadata(SCHEMA_VERSION_KEY)?;
    let descriptor = metadata(SCHEMA_DESCRIPTOR_KEY)?;
    let mut statement = conn
        .prepare(
            "SELECT type, name FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .map_err(map_sqlite_err)?;
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(map_sqlite_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_sqlite_err)?;
    let expected = vec![
        ("table".to_owned(), "chunks_v1".to_owned()),
        ("table".to_owned(), "meta_v1".to_owned()),
        ("table".to_owned(), "shards_v1".to_owned()),
    ];
    if version.as_deref() != Some(PERSISTENT_CHUNK_INDEX_SCHEMA_VERSION)
        || descriptor.as_deref() != Some(SCHEMA_DESCRIPTOR)
        || objects != expected
    {
        return Err(MetadataError::CorruptObject {
            path: path.display().to_string(),
            reason: format!(
                "persistent chunk index is not canonical v1; delete this cache file and retry (version={version:?}, descriptor={descriptor:?})"
            ),
        });
    }
    Ok(())
}

fn normalize_index_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    // A cold index open can create the shared cache root before payload caching.
    // Use private creation modes so cleanup does not reject our own root under
    // a permissive umask; never change permissions on existing directories.
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        directories.mode(0o700);
    }
    directories.create(parent)?;

    let parent = parent.canonicalize()?;
    let file_name = path.file_name().ok_or_else(|| {
        MetadataError::Internal(format!(
            "persistent chunk index path has no file name: {}",
            path.display()
        ))
    })?;

    Ok(parent.join(file_name))
}

fn hash_bytes(hash: &MerkleHash) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(hash.as_ref());
    bytes
}

fn blob_to_hash(bytes: Vec<u8>) -> rusqlite::Result<MerkleHash> {
    if bytes.len() != 32 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            bytes.len(),
            rusqlite::types::Type::Blob,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "expected 32-byte hash, got {} bytes",
                bytes.len()
            )),
        ));
    }

    let mut parts = [0u64; 4];
    for (i, part) in parts.iter_mut().enumerate() {
        let offset = i * 8;
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[offset..offset + 8]);
        *part = u64::from_le_bytes(word);
    }
    Ok(MerkleHash::from(parts))
}

fn xorb_ref_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<XorbRef> {
    xorb_ref_from_row_offset(row, 0)
}

fn xorb_ref_from_row_offset(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<XorbRef> {
    let xorb_hash = blob_to_hash(row.get::<_, Vec<u8>>(start)?)?;
    let chunk_index = integer_to_u32(row.get::<_, i64>(start + 1)?, "chunk_index")?;
    let uncompressed_size = integer_to_u32(row.get::<_, i64>(start + 2)?, "uncompressed_size")?;
    Ok(XorbRef {
        xorb_hash,
        chunk_index,
        uncompressed_size,
    })
}

fn integer_to_u32(value: i64, field: &'static str) -> rusqlite::Result<u32> {
    u32::try_from(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::<dyn std::error::Error + Send + Sync>::from(format!(
                "{field} out of range: {value} ({e})"
            )),
        )
    })
}

fn insert_chunk_with_statement(
    stmt: &mut rusqlite::Statement<'_>,
    chunk_hash: &MerkleHash,
    xorb_ref: &XorbRef,
) -> Result<()> {
    let chunk_key = hash_bytes(chunk_hash);
    let xorb_key = hash_bytes(&xorb_ref.xorb_hash);
    stmt.execute(params![
        chunk_key.as_slice(),
        xorb_key.as_slice(),
        i64::from(xorb_ref.chunk_index),
        i64::from(xorb_ref.uncompressed_size)
    ])
    .map_err(map_sqlite_err)?;
    Ok(())
}

fn map_sqlite_err(e: rusqlite::Error) -> MetadataError {
    let context = match e.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            "persistent chunk index SQLite lock"
        }
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
            "persistent chunk index SQLite corruption"
        }
        _ => "persistent chunk index SQLite",
    };
    MetadataError::Sqlite { context, source: e }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn hash(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed, seed, seed])
    }

    fn xorb_ref(xorb_seed: u64, idx: u32) -> XorbRef {
        XorbRef {
            xorb_hash: hash(xorb_seed),
            chunk_index: idx,
            uncompressed_size: 0,
        }
    }

    fn temp_index() -> (TempDir, PersistentChunkIndex) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");
        let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
        (dir, idx)
    }

    #[test]
    fn open_creates_new_db() {
        let (_dir, idx) = temp_index();
        idx.verify_schema().unwrap();
        assert!(idx.installed_shards().unwrap().is_empty());
        assert!(idx.load_all().unwrap().is_empty());
    }

    #[test]
    fn get_missing_returns_none() {
        let (_dir, idx) = temp_index();
        assert!(idx.get(&hash(42)).unwrap().is_none());
    }

    #[test]
    fn get_batch_preserves_order_duplicates_and_misses() {
        let (_dir, idx) = temp_index();
        let entries = vec![
            (hash(1), xorb_ref(100, 0)),
            (hash(2), xorb_ref(100, 1)),
            (hash(3), xorb_ref(101, 0)),
        ];
        idx.install_shard(hash(999), &entries).unwrap();

        let got = idx
            .get_batch(&[hash(2), hash(404), hash(1), hash(2), hash(3)])
            .unwrap();

        assert_eq!(
            got,
            vec![
                Some(xorb_ref(100, 1)),
                None,
                Some(xorb_ref(100, 0)),
                Some(xorb_ref(100, 1)),
                Some(xorb_ref(101, 0)),
            ]
        );
    }

    #[test]
    fn install_shard_and_get() {
        let (_dir, idx) = temp_index();
        let shard = hash(999);
        let entries = vec![
            (hash(1), xorb_ref(100, 0)),
            (hash(2), xorb_ref(100, 1)),
            (hash(3), xorb_ref(101, 0)),
        ];

        idx.install_shard(shard, &entries).unwrap();

        assert_eq!(idx.get(&hash(1)).unwrap(), Some(xorb_ref(100, 0)));
        assert_eq!(idx.get(&hash(2)).unwrap(), Some(xorb_ref(100, 1)));
        assert_eq!(idx.get(&hash(3)).unwrap(), Some(xorb_ref(101, 0)));
        assert!(idx.get(&hash(4)).unwrap().is_none());

        let shards = idx.installed_shards().unwrap();
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0], shard);
    }

    #[test]
    fn install_shard_is_idempotent_for_same_entries() {
        let (_dir, idx) = temp_index();
        let shard = hash(999);
        let entries = vec![(hash(1), xorb_ref(100, 0))];

        idx.install_shard(shard, &entries).unwrap();
        idx.install_shard(shard, &entries).unwrap();

        assert!(idx.get(&hash(1)).unwrap().is_some());
        assert_eq!(idx.load_all().unwrap().len(), 1);
        assert_eq!(idx.installed_shards().unwrap().len(), 1);
    }

    #[test]
    fn install_shard_repairs_marker_without_entries() {
        let (_dir, idx) = temp_index();
        let shard = hash(999);
        let entries = vec![(hash(1), xorb_ref(100, 0)), (hash(2), xorb_ref(100, 1))];

        idx.install_shard(shard, &[]).unwrap();
        assert!(idx.has_shard(&shard).unwrap());
        assert!(idx.load_all().unwrap().is_empty());

        idx.install_shard(shard, &entries).unwrap();

        assert_eq!(idx.get(&hash(1)).unwrap(), Some(xorb_ref(100, 0)));
        assert_eq!(idx.get(&hash(2)).unwrap(), Some(xorb_ref(100, 1)));
        assert_eq!(idx.installed_shards().unwrap(), vec![shard]);
    }

    #[test]
    fn insert_batch_adds_entries_without_marking_shards_installed() {
        let (_dir, idx) = temp_index();
        let entries = vec![
            (hash(1), xorb_ref(100, 0)),
            (hash(2), xorb_ref(100, 1)),
            (hash(3), xorb_ref(101, 0)),
        ];

        idx.insert_batch(&entries).unwrap();

        assert_eq!(idx.get(&hash(1)).unwrap(), Some(xorb_ref(100, 0)));
        assert_eq!(idx.get(&hash(2)).unwrap(), Some(xorb_ref(100, 1)));
        assert_eq!(idx.get(&hash(3)).unwrap(), Some(xorb_ref(101, 0)));
        assert!(
            idx.installed_shards().unwrap().is_empty(),
            "lazy chunk fills must not claim a full shard is installed"
        );
    }

    #[test]
    fn load_all_returns_all_entries() {
        let (_dir, idx) = temp_index();
        let entries = vec![(hash(1), xorb_ref(100, 0)), (hash(2), xorb_ref(100, 1))];
        idx.install_shard(hash(999), &entries).unwrap();

        let all = idx.load_all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .any(|(h, r)| *h == hash(1) && *r == xorb_ref(100, 0))
        );
        assert!(
            all.iter()
                .any(|(h, r)| *h == hash(2) && *r == xorb_ref(100, 1))
        );
    }

    #[test]
    fn rebuild_from_cache_replaces_all_data() {
        let (_dir, idx) = temp_index();

        idx.install_shard(hash(1), &[(hash(10), xorb_ref(100, 0))])
            .unwrap();

        let new_shards = vec![
            (
                hash(2),
                vec![(hash(20), xorb_ref(200, 0)), (hash(21), xorb_ref(200, 1))],
            ),
            (hash(3), vec![(hash(30), xorb_ref(300, 0))]),
        ];
        idx.rebuild_from_cache(&new_shards).unwrap();

        assert!(idx.get(&hash(10)).unwrap().is_none());
        assert!(idx.get(&hash(20)).unwrap().is_some());
        assert!(idx.get(&hash(21)).unwrap().is_some());
        assert!(idx.get(&hash(30)).unwrap().is_some());

        let shards = idx.installed_shards().unwrap();
        assert_eq!(shards.len(), 2);
        assert!(shards.contains(&hash(2)));
        assert!(shards.contains(&hash(3)));
        assert!(!shards.contains(&hash(1)));
    }

    #[test]
    fn open_rejects_schema_mismatch_without_mutation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");

        {
            let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
            idx.insert(&hash(1), &xorb_ref(2, 0)).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE meta_v1 SET value = ?1 WHERE key = ?2",
                params!["0", SCHEMA_VERSION_KEY],
            )
            .unwrap();
        }

        assert!(PersistentChunkIndex::open_or_create(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        let version: String = conn
            .query_row(
                "SELECT value FROM meta_v1 WHERE key = ?1",
                params![SCHEMA_VERSION_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "0");
        let entries: i64 = conn
            .query_row("SELECT COUNT(1) FROM chunks_v1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(entries, 1);
    }

    #[test]
    fn open_rejects_non_sqlite_cache_file_without_mutation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");
        let retired = b"this used to be a different cache engine";
        std::fs::write(&path, retired).unwrap();

        assert!(PersistentChunkIndex::open_or_create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), retired);
    }

    #[test]
    fn reopen_preserves_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");

        {
            let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
            idx.install_shard(hash(1), &[(hash(10), xorb_ref(100, 0))])
                .unwrap();
        }

        let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
        assert_eq!(idx.get(&hash(10)).unwrap(), Some(xorb_ref(100, 0)));
        assert_eq!(idx.installed_shards().unwrap().len(), 1);
    }

    #[test]
    fn inspect_is_read_only_and_counts_without_loading_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");
        let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
        idx.install_shard(
            hash(1),
            &[(hash(10), xorb_ref(100, 0)), (hash(11), xorb_ref(100, 1))],
        )
        .unwrap();
        idx.set_cache_gc_generation(7).unwrap();
        drop(idx);

        let stats = PersistentChunkIndex::inspect(&path).unwrap().unwrap();

        assert_eq!(stats.entry_count, 2);
        assert_eq!(stats.installed_shard_count, 1);
        assert_eq!(stats.cache_gc_generation, 7);
    }

    #[test]
    fn inspect_missing_index_does_not_create_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.sqlite");

        assert_eq!(PersistentChunkIndex::inspect(&path).unwrap(), None);
        assert!(!path.exists());
    }

    #[test]
    fn hash_serialization_round_trip() {
        let original = hash(98765);
        let bytes = hash_bytes(&original);
        let deserialized = blob_to_hash(bytes.to_vec()).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn has_shard_returns_true_for_installed() {
        let (_dir, idx) = temp_index();
        let shard = hash(999);
        idx.install_shard(shard, &[(hash(1), xorb_ref(100, 0))])
            .unwrap();

        assert!(idx.has_shard(&shard).unwrap());
        assert!(!idx.has_shard(&hash(888)).unwrap());
    }

    #[test]
    fn insert_and_get_round_trip() {
        let (_dir, idx) = temp_index();
        let chunk = hash(7);
        let target = xorb_ref(42, 3);

        idx.insert(&chunk, &target).unwrap();

        assert_eq!(idx.get(&chunk).unwrap(), Some(target));
        assert!(!idx.has_shard(&chunk).unwrap());
        assert!(idx.installed_shards().unwrap().is_empty());
    }

    #[test]
    fn cache_gc_generation_defaults_to_zero() {
        let (_dir, idx) = temp_index();
        assert_eq!(idx.cache_gc_generation().unwrap(), 0);
    }

    #[test]
    fn cache_gc_generation_round_trips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");

        {
            let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
            idx.set_cache_gc_generation(42).unwrap();
            assert_eq!(idx.cache_gc_generation().unwrap(), 42);
        }

        let idx = PersistentChunkIndex::open_or_create(&path).unwrap();
        assert_eq!(idx.cache_gc_generation().unwrap(), 42);
    }

    #[test]
    fn clear_entries_wipes_chunks_and_shards_but_keeps_generation() {
        let (_dir, idx) = temp_index();

        let shard = hash(900);
        let entries = vec![(hash(1), xorb_ref(100, 0)), (hash(2), xorb_ref(100, 1))];
        idx.install_shard(shard, &entries).unwrap();
        idx.set_cache_gc_generation(7).unwrap();

        idx.clear_entries().unwrap();

        assert!(idx.get(&hash(1)).unwrap().is_none());
        assert!(idx.get(&hash(2)).unwrap().is_none());
        assert!(idx.load_all().unwrap().is_empty());
        assert!(idx.installed_shards().unwrap().is_empty());
        assert!(!idx.has_shard(&shard).unwrap());
        idx.verify_schema().unwrap();
        assert_eq!(idx.cache_gc_generation().unwrap(), 7);
    }

    #[test]
    fn open_or_create_allows_multiple_live_handles() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");

        let first = PersistentChunkIndex::open_or_create(&path).unwrap();
        let shard = hash(999);
        let entries = vec![(hash(1), xorb_ref(100, 0)), (hash(2), xorb_ref(100, 1))];
        first.install_shard(shard, &entries).unwrap();

        let second = PersistentChunkIndex::open_or_create(&path).unwrap();
        assert_eq!(second.load_all().unwrap().len(), 2);
        assert!(second.has_shard(&shard).unwrap());

        second.insert(&hash(3), &xorb_ref(101, 0)).unwrap();
        assert_eq!(first.get(&hash(3)).unwrap(), Some(xorb_ref(101, 0)));
    }

    #[test]
    fn open_shared_reuses_live_handle_for_same_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");

        let first = PersistentChunkIndex::open_shared(&path).unwrap();
        first
            .install_shard(hash(999), &[(hash(1), xorb_ref(100, 0))])
            .unwrap();

        let second = PersistentChunkIndex::open_shared(&path).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.get(&hash(1)).unwrap(), Some(xorb_ref(100, 0)));

        drop(first);
        drop(second);

        let reopened = PersistentChunkIndex::open_or_create(&path).unwrap();
        assert_eq!(reopened.get(&hash(1)).unwrap(), Some(xorb_ref(100, 0)));
    }

    #[cfg(unix)]
    #[test]
    fn cold_index_creates_private_database_and_sqlite_side_files() {
        use std::os::unix::fs::PermissionsExt as _;

        for shared in [false, true] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("chunk-index.sqlite");
            let index = if shared {
                PersistentChunkIndex::open_shared(&path).unwrap()
            } else {
                Arc::new(PersistentChunkIndex::open_or_create(&path).unwrap())
            };
            index
                .install_shard(hash(1), &[(hash(2), xorb_ref(3, 0))])
                .unwrap();
            let modes: Vec<_> = [
                "chunk-index.sqlite",
                "chunk-index.sqlite-wal",
                "chunk-index.sqlite-shm",
            ]
            .into_iter()
            .map(|name| {
                std::fs::metadata(dir.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            })
            .collect();
            assert_eq!(modes, [0o600; 3], "shared={shared}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn cold_index_open_creates_private_ancestors_without_changing_existing_parent() {
        use std::os::unix::fs::PermissionsExt as _;

        for shared in [false, true] {
            let dir = TempDir::new().unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            let root = dir.path().join("cache");
            let parent = root.join("buckets").join("bucket");
            let path = parent.join("chunk-index.sqlite");
            if shared {
                drop(PersistentChunkIndex::open_shared(&path).unwrap());
            } else {
                drop(PersistentChunkIndex::open_or_create(&path).unwrap());
            }
            let modes: Vec<_> = [dir.path(), &root, &root.join("buckets"), &parent]
                .into_iter()
                .map(|path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777)
                .collect();
            assert_eq!(modes, [0o755, 0o700, 0o700, 0o700], "shared={shared}");
        }
    }

    #[test]
    fn concurrent_install_shard_stress() {
        use std::sync::Arc;
        use std::thread;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("chunk-index.sqlite");
        let idx = Arc::new(PersistentChunkIndex::open_or_create(&path).unwrap());

        const NUM_SHARDS: u64 = 50;
        const ENTRIES_PER_SHARD: u64 = 20;

        let mut handles = Vec::new();
        for i in 0..NUM_SHARDS {
            let idx = Arc::clone(&idx);
            handles.push(thread::spawn(move || {
                let shard = hash(1000 + i);
                let entries: Vec<_> = (0..ENTRIES_PER_SHARD)
                    .map(|j| {
                        let chunk = hash(i * 10_000 + j);
                        let xr = xorb_ref(2000 + i, j as u32);
                        (chunk, xr)
                    })
                    .collect();
                idx.install_shard(shard, &entries).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let installed = idx.installed_shards().unwrap();
        assert_eq!(installed.len(), NUM_SHARDS as usize);
        let all_entries = idx.load_all().unwrap();
        assert_eq!(all_entries.len(), (NUM_SHARDS * ENTRIES_PER_SHARD) as usize);

        drop(idx);
        let reopened = PersistentChunkIndex::open_or_create(&path).unwrap();
        assert_eq!(
            reopened.installed_shards().unwrap().len(),
            NUM_SHARDS as usize
        );
        assert_eq!(
            reopened.load_all().unwrap().len(),
            (NUM_SHARDS * ENTRIES_PER_SHARD) as usize
        );
    }

    #[test]
    fn install_shard_idempotency_under_repeat() {
        let (_dir, idx) = temp_index();
        let shard = hash(42);
        let entries = vec![(hash(1), xorb_ref(100, 0)), (hash(2), xorb_ref(100, 1))];

        for _ in 0..10 {
            idx.install_shard(shard, &entries).unwrap();
        }

        assert_eq!(idx.load_all().unwrap().len(), 2);
        assert_eq!(idx.installed_shards().unwrap().len(), 1);
    }
}
