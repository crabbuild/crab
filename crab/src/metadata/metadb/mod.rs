//! SlateDB-backed metadata session facade.
//!
//! Holds lazy-open slots for the two SlateDB instances that carry
//! crab metadata — the per-repo `file_index_db` and the globally
//! shared `chunk_index_db` — plus the two local cache tiers. A session
//! opens at most one of each remote database; the only writer is the
//! session itself, which keeps the close-on-exit invariant simple.
//!
//! Writes cross both databases through a single [`Transaction`]; the
//! session splits the transaction per database at commit time and runs
//! the two `slatedb::WriteBatch` writes in parallel.
//!
//! Local cache warming (install_shard on the in-memory `ChunkIndex`
//! and on the on-disk `PersistentChunkIndex`) is NOT done inside
//! `commit` — the push pipeline drives those calls alongside `commit`
//! so the local tiers stay shard-atomic. Keeping `commit` narrow means
//! failures on the local cache never bleed into the remote write path.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, MetaDbError, Result};
use crate::core::metrics::Metrics;
use crab_metadata::chunk_index::ChunkIndex;
use crab_metadata::persistent_chunk_index::PersistentChunkIndex;
use crab_metadata::{key_codec, value_codec};

pub mod db;
pub mod guard;
pub mod once;
pub mod stores;
pub mod transaction;

pub use db::Db;
pub use guard::MetaDbGuard;
pub use once::OnceAsync;
pub use stores::{ChunkIndexStore, FileIndexStore, XorbRef};
pub use transaction::{DbTarget, PushWriteReceipt, Transaction};

/// Default in-memory ceiling for the chunk-index local cache. Matches
/// the existing `ChunkIndex::CEILING` so the cache behaves identically
/// when reached through `MetaDb` vs. direct construction.
const DEFAULT_IN_MEMORY_CEILING_BYTES: u64 = 1024 * 1024 * 1024;

/// Default grace window (in GC generations) before drift between
/// local cache and remote forces a cache wipe.
const DEFAULT_CACHE_GC_GRACE: u64 = 3;

/// Default SlateDB compaction threshold (number of SSTables at level 0
/// before a compaction is scheduled).
const DEFAULT_COMPACTION_THRESHOLD: u32 = 4;

/// Default WAL flush size per instance (4 MiB).
const DEFAULT_WAL_FLUSH_SIZE: u64 = 4 * 1024 * 1024;

/// Default bloom-filter density per key in bits. Ten bits per key gives
/// a ~1% false-positive rate, which SlateDB recommends for point
/// lookups.
const DEFAULT_BLOOM_BITS_PER_KEY: u32 = 10;

/// Aggregated snapshot of `sys:*` keys in one database.
///
/// Returned by [`MetaDb::file_index_system_keys`] and
/// [`MetaDb::chunk_index_system_keys`]. Every field is `Option`: a
/// missing key reads as `None`, which is the expected state for a
/// freshly opened SlateDB where only content keys have been written.
/// Wrong-length values surface as [`MetaDbError::CorruptValue`] from
/// the accessor, never as `None`.
///
/// `created_at_unix_ms` is the raw u64 milliseconds-since-epoch value
/// as it sits in the DB; callers render it however their UI needs.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SystemKeySnapshot {
    pub format_version: Option<u32>,
    pub epoch: Option<u64>,
    pub created_at_unix_ms: Option<u64>,
    pub gc_generation: Option<u64>,
}

/// Outcome of a [`MetaDb::check_cache_gc_drift`] invocation.
///
/// Reported back so callers (push start, clone after shard sync,
/// diagnostics) can emit structured logs and distinguish the no-op
/// case from an actual invalidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheDriftOutcome {
    /// Local cache is within the grace window of the remote
    /// `sys:gc_generation`. Cache left intact.
    NoDrift {
        /// Local `cache_gc_generation` from the `PersistentChunkIndex`.
        local_generation: u64,
        /// Remote `sys:gc_generation` read from chunk_index_db. Zero
        /// if GC has never run against the database.
        remote_generation: u64,
    },
    /// Remote generation exceeded `local + grace`. Both cache tiers
    /// have been wiped and the local cursor advanced.
    WipedCache {
        /// Local generation observed before the wipe.
        old_generation: u64,
        /// New local generation after the wipe.
        new_generation: u64,
    },
}

/// Tunables and paths for a [`MetaDb`] session.
///
/// Defaults are safe for production: paths are derived from the repo
/// prefix, tunables match SlateDB's recommended point-lookup profile,
/// and the local cache lands at a placeholder path intended to be
/// overridden by callers that know the bucket / repo hash.
#[derive(Debug, Clone)]
pub struct MetaDbConfig {
    /// Object-store subpath for this repo's `file_index_db`.
    /// Default: `{repo_prefix}/file_index_db/`.
    pub file_index_path: String,

    /// Object-store subpath for the globally shared `chunk_index_db`.
    /// Default: `.crab/chunk_index_db/`.
    pub chunk_index_path: String,

    /// Path to the local `PersistentChunkIndex` SQLite file (warm tier).
    pub local_chunk_index_path: PathBuf,

    /// In-memory ceiling for the chunk-index hot tier, in bytes.
    pub in_memory_ceiling_bytes: u64,

    /// Grace window (in GC generations) before the local cache is
    /// wiped to stay consistent with an advanced remote.
    pub cache_gc_grace: u64,

    /// SlateDB compaction threshold — SSTables at level 0 before a
    /// compaction runs.
    pub compaction_threshold: u32,

    /// WAL flush size per SlateDB instance, in bytes.
    pub wal_flush_size: u64,

    /// Bloom-filter bits per key per SSTable.
    pub bloom_bits_per_key: u32,

    /// Open the underlying SlateDB instances in read-only mode.
    ///
    /// Read-only sessions use [`slatedb::DbReader`] under the hood,
    /// which reads through a pinned manifest checkpoint and does not
    /// fence other writers. Hydrate, clone, diff, fsck, and the
    /// `metadb diagnose` / `doctor --metadb` surfaces set this to
    /// `true` so a concurrent `crab push` is not interrupted.
    ///
    /// Attempting [`MetaDb::commit`] or
    /// [`MetaDb::bump_gc_generation`] on a read-only session returns
    /// [`crate::core::error::MetaDbError::ReadOnly`] immediately
    /// rather than opening the writer side. Defaults to `false` so
    /// existing callers keep their read-write behavior.
    pub read_only: bool,
}

impl MetaDbConfig {
    /// Build a config with paths anchored at `repo_prefix`. Other
    /// tunables take their defaults.
    ///
    /// Callers that know the final local cache location (bucket
    /// name, repo hash) should override `local_chunk_index_path` after
    /// construction. The default placeholder is a relative path so a
    /// forgetful caller gets a visible, relocatable file rather than
    /// a surprise write into `$HOME`.
    pub fn for_repo(repo_prefix: &str) -> Self {
        Self {
            file_index_path: format!("{}/file_index_db/", repo_prefix.trim_end_matches('/')),
            ..Self::default()
        }
    }

    /// Builder-style setter for [`Self::read_only`]. Chainable with
    /// [`Self::for_repo`] at the call site:
    ///
    /// ```ignore
    /// let cfg = MetaDbConfig::for_repo("org/ml").with_read_only(true);
    /// ```
    #[must_use]
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
}

impl Default for MetaDbConfig {
    fn default() -> Self {
        Self {
            file_index_path: String::from("file_index_db/"),
            chunk_index_path: String::from(crab_metadata::CHUNK_INDEX_DB_PATH),
            local_chunk_index_path: PathBuf::from(".cache/crab/chunk-index.sqlite"),
            in_memory_ceiling_bytes: DEFAULT_IN_MEMORY_CEILING_BYTES,
            cache_gc_grace: DEFAULT_CACHE_GC_GRACE,
            compaction_threshold: DEFAULT_COMPACTION_THRESHOLD,
            wal_flush_size: DEFAULT_WAL_FLUSH_SIZE,
            bloom_bits_per_key: DEFAULT_BLOOM_BITS_PER_KEY,
            read_only: false,
        }
    }
}

/// Session-level facade over both SlateDB metadata databases.
///
/// Constructing a `MetaDb` does NOT open any SlateDB instance — opens
/// are lazy and happen on the first accessor call. The invariant this
/// type upholds is that every slot that ever became occupied gets
/// closed on [`MetaDb::close_all`].
pub struct MetaDb {
    /// Object store used to open the two SlateDB instances.
    store: Arc<dyn ObjectStore>,

    /// One bounded SlateDB cache shared by both lazy database handles.
    db_cache: Arc<dyn slatedb::db_cache::DbCache>,

    /// Repo-scoped prefix this session reads and writes under.
    repo_prefix: String,

    /// Tunables + resolved paths.
    config: MetaDbConfig,

    /// Optional metrics sink. Threaded into every lazy-opened
    /// [`Db`] handle so the `metadb_*` counters reflect traffic
    /// through both SlateDB instances.
    metrics: Option<Arc<Metrics>>,

    /// Lazy slot for the per-repo file_index_db handle.
    file_index_db: OnceAsync<Db>,

    /// Lazy slot for the global chunk_index_db handle.
    chunk_index_db: OnceAsync<Db>,

    /// Lazy slot for the in-memory `ChunkIndex` hot tier.
    in_memory_chunk_index: OnceAsync<Mutex<ChunkIndex>>,

    /// Lazy slot for the on-disk `PersistentChunkIndex` warm tier.
    persistent_chunk_index: OnceAsync<PersistentChunkIndex>,
}

impl MetaDb {
    /// Construct a new session. Does NOT open any SlateDB instance.
    pub fn new(store: Arc<dyn ObjectStore>, repo_prefix: String, config: MetaDbConfig) -> Self {
        Self {
            store,
            db_cache: db::new_db_cache(),
            repo_prefix,
            config,
            metrics: None,
            file_index_db: OnceAsync::new(),
            chunk_index_db: OnceAsync::new(),
            in_memory_chunk_index: OnceAsync::new(),
            persistent_chunk_index: OnceAsync::new(),
        }
    }

    /// Construct a new session that reuses an already-opened
    /// [`PersistentChunkIndex`] handle instead of opening the SQLite
    /// cache itself.
    ///
    /// Callers like the push pipeline open the persistent chunk index
    /// early (step 3's shard sync) and need to share that exact handle
    /// with every later step that touches the warm tier (step 9b's
    /// `warm_local_shard`). Prefer this constructor over [`Self::new`]
    /// whenever the caller already has an `Arc<PersistentChunkIndex>`.
    pub fn new_with_persistent_tier(
        store: Arc<dyn ObjectStore>,
        repo_prefix: String,
        config: MetaDbConfig,
        persistent: Arc<PersistentChunkIndex>,
    ) -> Self {
        let session = Self::new(store, repo_prefix, config);
        session.persistent_chunk_index.set(persistent);
        session
    }

    /// Construct a new session with a metrics sink attached.
    ///
    /// Both lazy-opened [`Db`] handles will bump `metadb_*` counters
    /// on every get / get_batch / write / close.
    pub fn new_with_metrics(
        store: Arc<dyn ObjectStore>,
        repo_prefix: String,
        config: MetaDbConfig,
        metrics: Arc<Metrics>,
    ) -> Self {
        let mut session = Self::new(store, repo_prefix, config);
        session.metrics = Some(metrics);
        session
    }

    /// Construct a new session with a metrics sink AND a pre-opened
    /// [`PersistentChunkIndex`] handle.
    ///
    /// See [`Self::new_with_persistent_tier`] for why sharing the
    /// handle matters.
    pub fn new_with_metrics_and_persistent_tier(
        store: Arc<dyn ObjectStore>,
        repo_prefix: String,
        config: MetaDbConfig,
        metrics: Arc<Metrics>,
        persistent: Arc<PersistentChunkIndex>,
    ) -> Self {
        let session = Self::new_with_metrics(store, repo_prefix, config, metrics);
        session.persistent_chunk_index.set(persistent);
        session
    }

    /// Install an already-opened [`PersistentChunkIndex`] handle into
    /// this session's lazy slot, but only if the slot is still empty.
    ///
    /// Returns `true` if the handle was installed and `false` if the
    /// session had already lazy-opened its own handle (in which case
    /// the passed `persistent` is dropped). Used when a long-running
    /// operation opens the warm tier before the session's first
    /// `chunk_index()` call — most notably the push pipeline, whose
    /// step 3 shard-sync opens the cache for classification before
    /// step 9b needs it for warming.
    pub fn install_persistent_tier(&self, persistent: Arc<PersistentChunkIndex>) -> bool {
        self.persistent_chunk_index.set(persistent)
    }

    /// Borrow the attached metrics sink, if any.
    pub fn metrics(&self) -> Option<&Arc<Metrics>> {
        self.metrics.as_ref()
    }

    /// Borrow the session config.
    pub fn config(&self) -> &MetaDbConfig {
        &self.config
    }

    /// Is this session opened in read-only mode?
    ///
    /// Read-only sessions reject [`Self::commit`] and
    /// [`Self::bump_gc_generation`] without opening the SlateDB
    /// writer side. Inspect this from the caller when branching on
    /// whether a write path is safe to take.
    pub fn is_read_only(&self) -> bool {
        self.config.read_only
    }

    /// The repo prefix this session reads and writes under.
    pub fn repo_prefix(&self) -> &str {
        &self.repo_prefix
    }

    /// Return an owned `FileIndexStore` over the per-repo
    /// `file_index_db`, lazy-opening the SlateDB on first call.
    ///
    /// The returned store is cheap to clone and carries no lifetime,
    /// so callers can stash it in long-lived structs. A failed open
    /// leaves the slot empty so a transient S3 error can be retried
    /// on the next call rather than poisoning the whole session.
    pub async fn file_index(&self) -> Result<FileIndexStore> {
        let db = self.open_file_index_db().await?;
        Ok(FileIndexStore::new(db))
    }

    /// Return an owned `ChunkIndexStore` over the global
    /// `chunk_index_db`, initialising every tier as needed.
    ///
    /// Both local cache tiers are initialised on the first call so
    /// the three-tier read path can run without extra branches. The
    /// remote SlateDB is lazy-opened on the first call as well — if
    /// a session only hits the local tiers, the remote open is still
    /// paid up front, matching the design expectation that mount and
    /// push callers almost always need the remote eventually.
    pub async fn chunk_index(&self) -> Result<ChunkIndexStore> {
        let memory = self.open_memory_tier().await?;
        let persistent = match self.open_persistent_tier().await {
            Ok(persistent) => Some(persistent),
            Err(e) => {
                warn!(
                    error = %e,
                    path = %self.config.local_chunk_index_path.display(),
                    "MetaDb chunk index: persistent tier unavailable; continuing with memory and remote tiers"
                );
                None
            }
        };
        let db = self.open_chunk_index_db().await?;
        let store = ChunkIndexStore::new_with_optional_persistent(db, memory, persistent);
        Ok(match self.metrics.as_ref() {
            Some(m) => store.with_metrics(Arc::clone(m)),
            None => store,
        })
    }

    /// Build a fresh empty transaction.
    pub fn new_transaction(&self) -> Transaction {
        Transaction::new()
    }

    /// Commit a transaction: split per database, write both batches
    /// in parallel via [`tokio::try_join!`].
    ///
    /// Empty per-database slices are skipped — if the transaction has
    /// no file-index ops, `file_index_db` is not opened; same for
    /// chunk-index. This preserves the lazy-open invariant: a
    /// chunk-only push never materialises the per-repo file_index_db
    /// handle.
    ///
    /// Returns [`crate::core::error::MetaDbError::ReadOnly`] without
    /// opening either database if the session was configured with
    /// `read_only = true`.
    pub async fn commit(&self, txn: Transaction) -> Result<PushWriteReceipt> {
        if self.config.read_only {
            return Err(crate::core::error::MetaDbError::ReadOnly {
                db: String::from("metadb"),
                op: "commit",
            }
            .into());
        }
        let start = Instant::now();

        let (file_ops, chunk_ops) = txn.counts();
        let (file_bytes, chunk_bytes) = txn.byte_volume();

        if file_ops == 0 && chunk_ops == 0 {
            // Empty transaction: no opens, no writes. Return a
            // zero-receipt so the caller can still bump metrics
            // uniformly.
            return Ok(PushWriteReceipt {
                file_index_epoch: 0,
                chunk_index_epoch: 0,
                file_ops_written: 0,
                chunk_ops_written: 0,
                bytes_written: 0,
                elapsed: start.elapsed(),
            });
        }

        let (fi_batch, ci_batch) = transaction::into_per_db_batches(txn);

        // Two independent futures; only the ones that have work to
        // do actually open their database. Empty-batch branches are
        // cheap no-ops so `try_join!` can still bind both arms.
        let file_future = async {
            if file_ops == 0 {
                return Ok::<(), crate::core::error::CrabError>(());
            }
            let db = self.open_file_index_db().await?;
            db.write(fi_batch).await
        };
        let chunk_future = async {
            if chunk_ops == 0 {
                return Ok::<(), crate::core::error::CrabError>(());
            }
            let db = self.open_chunk_index_db().await?;
            db.write(ci_batch).await
        };

        tokio::try_join!(file_future, chunk_future)?;

        let receipt = PushWriteReceipt {
            file_index_epoch: 0,
            chunk_index_epoch: 0,
            file_ops_written: file_ops as u64,
            chunk_ops_written: chunk_ops as u64,
            bytes_written: file_bytes + chunk_bytes,
            elapsed: start.elapsed(),
        };

        if let Some(m) = self.metrics.as_ref() {
            m.add_metadb_write_bytes(receipt.bytes_written);
            m.set_metadb_last_push_batch_ms(receipt.elapsed.as_millis() as u64);
        }

        debug!(
            file_ops = receipt.file_ops_written,
            chunk_ops = receipt.chunk_ops_written,
            bytes_written = receipt.bytes_written,
            elapsed_ms = receipt.elapsed.as_millis() as u64,
            "metadb commit completed"
        );

        Ok(receipt)
    }

    /// Check whether the local chunk-index cache has drifted past the
    /// remote GC cursor and, if so, invalidate the local tiers.
    ///
    /// Reads a single `sys:gc_generation` entry from chunk_index_db
    /// (not 16 — there is one instance) and compares to the local
    /// cursor persisted in `PersistentChunkIndex`. When the remote
    /// exceeds `local + cache_gc_grace`, both cache tiers are wiped
    /// and the local cursor advances to the remote value.
    ///
    /// Missing keys are treated as generation `0` (fresh database).
    /// This is called explicitly by push start and clone/pull/fetch
    /// after shard sync; not from `chunk_index()` directly, which
    /// would force an eager remote open.
    pub async fn check_cache_gc_drift(&self) -> Result<CacheDriftOutcome> {
        let persistent = self.open_persistent_tier().await?;
        let memory = self.open_memory_tier().await?;
        let db = self.open_chunk_index_db().await?;

        let remote_key = key_codec::encode_system_key(key_codec::SYS_GC_GENERATION);
        let raw = db.get(&remote_key).await?;
        let remote_generation = decode_gc_generation_value(raw.as_deref())?;

        let local_generation = persistent.cache_gc_generation()?;
        let grace = self.config.cache_gc_grace;

        // Saturating add so a massive `grace` value (config typo)
        // can't overflow to zero and spuriously wipe the cache.
        if remote_generation > local_generation.saturating_add(grace) {
            info!(
                local_generation,
                remote_generation,
                grace,
                "chunk-index cache GC drift exceeds grace window; wiping local cache"
            );
            persistent.clear_entries()?;
            // Swap the in-memory tier in lockstep. Re-creating it is
            // simpler than clearing in place and keeps the
            // installed_shards marker consistent with the now-empty
            // persistent tier.
            {
                let mut guard = memory.lock().map_err(|_| {
                    crate::core::error::CrabError::Internal(String::from(
                        "in-memory chunk index mutex poisoned",
                    ))
                })?;
                *guard = ChunkIndex::new();
            }
            persistent.set_cache_gc_generation(remote_generation)?;
            return Ok(CacheDriftOutcome::WipedCache {
                old_generation: local_generation,
                new_generation: remote_generation,
            });
        }

        debug!(
            local_generation,
            remote_generation,
            grace,
            "chunk-index cache GC drift within grace window; cache preserved"
        );
        Ok(CacheDriftOutcome::NoDrift {
            local_generation,
            remote_generation,
        })
    }

    /// Read the four `sys:*` keys tracked in the per-repo
    /// `file_index_db` as a single snapshot.
    ///
    /// Opens `file_index_db` lazily; a never-written sys key reads
    /// as `None`. Every decode error surfaces as
    /// [`MetaDbError::CorruptValue`] with `db = "file_index_db"` and
    /// the offending key name so operators can locate the damage
    /// without inspecting the source chain.
    pub async fn file_index_system_keys(&self) -> Result<SystemKeySnapshot> {
        let db = self.open_file_index_db().await?;
        read_system_keys(&db, stores::file_index::DB_LABEL).await
    }

    /// Read the four `sys:*` keys tracked in the globally shared
    /// `chunk_index_db` as a single snapshot. `sys:gc_generation` is
    /// meaningful here (bumped by `crab gc`); the other three are
    /// written at initialisation and otherwise immutable.
    ///
    /// Opens `chunk_index_db` lazily; same error semantics as
    /// [`Self::file_index_system_keys`].
    pub async fn chunk_index_system_keys(&self) -> Result<SystemKeySnapshot> {
        let db = self.open_chunk_index_db().await?;
        read_system_keys(&db, stores::chunk_index::DB_LABEL).await
    }

    /// Expose the raw `file_index_db` handle for diagnostic scans.
    ///
    /// Intended exclusively for `metadb diagnose --deep` and `fsck`.
    /// Production code should use [`Self::file_index`] which returns
    /// the typed store.
    pub async fn file_index_db_handle(&self) -> Result<Arc<Db>> {
        self.open_file_index_db().await
    }

    /// Expose the raw `chunk_index_db` handle for diagnostic scans.
    ///
    /// Intended exclusively for `metadb diagnose --deep` and `fsck`.
    /// Production code should use [`Self::chunk_index`] which returns
    /// the typed store.
    pub async fn chunk_index_db_handle(&self) -> Result<Arc<Db>> {
        self.open_chunk_index_db().await
    }

    /// Atomically bump `sys:gc_generation` in the global
    /// `chunk_index_db`, returning the new value.
    ///
    /// Intended for the `crab gc` sweep: after dead chunks have been
    /// tombstoned, a bumped `sys:gc_generation` tells every client's
    /// next [`Self::check_cache_gc_drift`] call that a GC round
    /// completed and that its local cache should be compared against
    /// the new cursor.
    ///
    /// This is NOT atomic across read + write — two concurrent GC
    /// runs racing to bump the same counter may produce an
    /// out-of-order final value. In practice GC is operator-invoked
    /// and single-writer, so the race is theoretical; if it's ever
    /// hit, the higher of the two values wins and the cache-drift
    /// check still triggers correctly.
    pub async fn bump_gc_generation(&self) -> Result<u64> {
        if self.config.read_only {
            return Err(crate::core::error::MetaDbError::ReadOnly {
                db: String::from(stores::chunk_index::DB_LABEL),
                op: "bump_gc_generation",
            }
            .into());
        }
        let db = self.open_chunk_index_db().await?;

        let key = key_codec::encode_system_key(key_codec::SYS_GC_GENERATION);
        let current = decode_gc_generation_value(db.get(&key).await?.as_deref())?;
        let next = current.saturating_add(1);

        let mut batch = slatedb::WriteBatch::new();
        batch.put(
            key.as_slice(),
            value_codec::encode_gc_generation_value(next).as_slice(),
        );
        db.write(batch).await?;

        debug!(
            old_generation = current,
            new_generation = next,
            "bumped sys:gc_generation"
        );
        Ok(next)
    }

    /// Flush both opened SlateDB instances.
    ///
    /// Slots that were never populated are skipped — flushing an
    /// uninitialised `OnceAsync` is free. Runs the two flushes in
    /// parallel via [`tokio::try_join!`].
    pub async fn flush_all(&self) -> Result<()> {
        let fi = self.file_index_db.get().cloned();
        let ci = self.chunk_index_db.get().cloned();

        let fi_future = async {
            if let Some(db) = fi.as_ref() {
                db.flush().await
            } else {
                Ok(())
            }
        };
        let ci_future = async {
            if let Some(db) = ci.as_ref() {
                db.flush().await
            } else {
                Ok(())
            }
        };

        tokio::try_join!(fi_future, ci_future)?;
        Ok(())
    }

    /// Flush both opened writer memtables to bound long-running batch memory.
    pub async fn flush_memtables(&self) -> Result<()> {
        let fi = self.file_index_db.get().cloned();
        let ci = self.chunk_index_db.get().cloned();

        let fi_future = async {
            if let Some(db) = fi.as_ref() {
                db.flush_memtable().await
            } else {
                Ok(())
            }
        };
        let ci_future = async {
            if let Some(db) = ci.as_ref() {
                db.flush_memtable().await
            } else {
                Ok(())
            }
        };

        tokio::try_join!(fi_future, ci_future)?;
        Ok(())
    }

    /// Close every SlateDB instance this session opened.
    ///
    /// Runs both closes in parallel via [`tokio::try_join!`]. Slots
    /// that were never populated are skipped.
    pub async fn close_all(self) -> Result<()> {
        let Self {
            file_index_db,
            chunk_index_db,
            db_cache,
            ..
        } = self;

        let fi = file_index_db.get().cloned();
        let ci = chunk_index_db.get().cloned();

        let close_result = match (fi, ci) {
            (Some(fi), Some(ci)) => tokio::try_join!(fi.close(), ci.close()).map(|_| ()),
            (Some(fi), None) => fi.close().await,
            (None, Some(ci)) => ci.close().await,
            (None, None) => Ok(()),
        };
        let cache_result = db_cache.close().await.map_err(|source| {
            CrabError::from(MetaDbError::Close {
                db: String::from("shared_db_cache"),
                source,
            })
        });
        close_result?;
        cache_result
    }

    #[cfg(test)]
    const fn db_cache_capacity_bytes(&self) -> u64 {
        db::DB_CACHE_CAPACITY_BYTES
    }

    // --- internal lazy-open helpers ---

    /// Lazy-open the per-repo `file_index_db`.
    ///
    /// Opens in read-only mode when [`MetaDbConfig::read_only`] is set,
    /// handing back a non-fencing [`slatedb::DbReader`]-backed handle.
    /// The read-only path is otherwise indistinguishable to callers
    /// that only issue `get` / `get_batch` against the returned
    /// [`Db`].
    async fn open_file_index_db(&self) -> Result<Arc<Db>> {
        let handle = self
            .file_index_db
            .get_or_init(|| async {
                let path = ObjectPath::from(self.config.file_index_path.as_str());
                let db = match (self.config.read_only, self.metrics.as_ref()) {
                    (false, Some(m)) => {
                        let db = Db::open_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::file_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?;
                        m.inc_metadb_open_count();
                        db.with_metrics(Arc::clone(m))
                    }
                    (false, None) => {
                        Db::open_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::file_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?
                    }
                    (true, Some(m)) => {
                        let db = Db::open_readonly_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::file_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?;
                        m.inc_metadb_open_count();
                        db.with_metrics(Arc::clone(m))
                    }
                    (true, None) => {
                        Db::open_readonly_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::file_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?
                    }
                };
                Ok(Arc::new(db))
            })
            .await?;
        Ok(Arc::clone(handle))
    }

    /// Lazy-open the global `chunk_index_db`.
    ///
    /// Same read-only semantics as [`Self::open_file_index_db`]: a
    /// [`MetaDbConfig::read_only`] session hands back a non-fencing
    /// [`slatedb::DbReader`]-backed handle.
    async fn open_chunk_index_db(&self) -> Result<Arc<Db>> {
        let handle = self
            .chunk_index_db
            .get_or_init(|| async {
                let path = ObjectPath::from(self.config.chunk_index_path.as_str());
                let db = match (self.config.read_only, self.metrics.as_ref()) {
                    (false, Some(m)) => {
                        let db = Db::open_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::chunk_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?;
                        m.inc_metadb_open_count();
                        db.with_metrics(Arc::clone(m))
                    }
                    (false, None) => {
                        Db::open_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::chunk_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?
                    }
                    (true, Some(m)) => {
                        let db = Db::open_readonly_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::chunk_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?;
                        m.inc_metadb_open_count();
                        db.with_metrics(Arc::clone(m))
                    }
                    (true, None) => {
                        Db::open_readonly_with_cache(
                            Arc::clone(&self.store),
                            path,
                            stores::chunk_index::DB_LABEL,
                            Arc::clone(&self.db_cache),
                        )
                        .await?
                    }
                };
                Ok(Arc::new(db))
            })
            .await?;
        Ok(Arc::clone(handle))
    }

    /// Lazy-initialise the in-memory `ChunkIndex` tier.
    async fn open_memory_tier(&self) -> Result<Arc<Mutex<ChunkIndex>>> {
        let handle = self
            .in_memory_chunk_index
            .get_or_init(|| async {
                Ok(Arc::new(Mutex::new(ChunkIndex::with_ceiling(
                    self.config.in_memory_ceiling_bytes,
                ))))
            })
            .await?;
        Ok(Arc::clone(handle))
    }

    /// Lazy-initialise the on-disk `PersistentChunkIndex` tier.
    async fn open_persistent_tier(&self) -> Result<Arc<PersistentChunkIndex>> {
        let handle = self
            .persistent_chunk_index
            .get_or_init(|| async {
                let path = self.config.local_chunk_index_path.clone();
                Ok(PersistentChunkIndex::open_shared(&path)?)
            })
            .await?;
        Ok(Arc::clone(handle))
    }
}

/// Issue the four `sys:*` reads against `db` in parallel and stitch
/// the results into a [`SystemKeySnapshot`].
///
/// `db_label` is threaded through so decode errors point at the
/// logical database ("file_index_db" / "chunk_index_db") rather than
/// the generic `Db` wrapper. All four reads are independent, so
/// [`tokio::try_join!`] is the right fit — the first failure
/// short-circuits the others.
async fn read_system_keys(db: &Db, db_label: &'static str) -> Result<SystemKeySnapshot> {
    let fv_key = key_codec::encode_system_key(key_codec::SYS_FORMAT_VERSION);
    let ep_key = key_codec::encode_system_key(key_codec::SYS_EPOCH);
    let ca_key = key_codec::encode_system_key(key_codec::SYS_CREATED_AT);
    let gc_key = key_codec::encode_system_key(key_codec::SYS_GC_GENERATION);

    let (fv_raw, ep_raw, ca_raw, gc_raw) = tokio::try_join!(
        db.get(&fv_key),
        db.get(&ep_key),
        db.get(&ca_key),
        db.get(&gc_key),
    )?;

    debug!(
        db = db_label,
        format_version_present = fv_raw.is_some(),
        epoch_present = ep_raw.is_some(),
        created_at_present = ca_raw.is_some(),
        gc_generation_present = gc_raw.is_some(),
        "metadb: read sys:* snapshot"
    );

    Ok(SystemKeySnapshot {
        format_version: decode_u32_le_value(
            fv_raw.as_deref(),
            db_label,
            key_codec::SYS_FORMAT_VERSION,
        )?,
        epoch: decode_u64_le_value(ep_raw.as_deref(), db_label, key_codec::SYS_EPOCH)?,
        created_at_unix_ms: decode_u64_le_value(
            ca_raw.as_deref(),
            db_label,
            key_codec::SYS_CREATED_AT,
        )?,
        gc_generation: decode_u64_le_value(
            gc_raw.as_deref(),
            db_label,
            key_codec::SYS_GC_GENERATION,
        )?,
    })
}

/// Decode a raw `sys:*` value as a little-endian u32.
///
/// Returns `Ok(None)` on absence, `Ok(Some(n))` on a 4-byte value,
/// and [`MetaDbError::CorruptValue`] on any other length. `db` and
/// `key_name` are echoed back into the error payload so operators
/// can grep logs by database and key.
pub(crate) fn decode_u32_le_value(
    raw: Option<&[u8]>,
    db: &'static str,
    key_name: &str,
) -> Result<Option<u32>> {
    raw.map(|bytes| {
        value_codec::decode_u32_system_value(bytes)
            .map_err(|error| map_system_value_codec_error(error, db, key_name))
    })
    .transpose()
}

/// Decode a raw `sys:*` value as a little-endian u64. Mirrors
/// [`decode_u32_le_value`] for the eight-byte system keys
/// (`epoch`, `created_at`, `gc_generation`).
pub(crate) fn decode_u64_le_value(
    raw: Option<&[u8]>,
    db: &'static str,
    key_name: &str,
) -> Result<Option<u64>> {
    raw.map(|bytes| {
        value_codec::decode_u64_system_value(bytes)
            .map_err(|error| map_system_value_codec_error(error, db, key_name))
    })
    .transpose()
}

/// Decode a raw `sys:gc_generation` value as a u64 LE.
///
/// Returns `Ok(0)` when the key is absent (fresh database, GC never
/// ran). Returns [`MetaDbError::CorruptValue`] when the stored value
/// is not exactly 8 bytes.
fn decode_gc_generation_value(raw: Option<&[u8]>) -> Result<u64> {
    raw.map_or(Ok(0), |bytes| {
        value_codec::decode_gc_generation_value(bytes).map_err(|error| {
            map_system_value_codec_error(
                error,
                stores::chunk_index::DB_LABEL,
                key_codec::SYS_GC_GENERATION,
            )
        })
    })
}

fn map_system_value_codec_error(
    error: crab_metadata::error::MetadataError,
    db: &'static str,
    key_name: &str,
) -> crate::core::CrabError {
    match error {
        crab_metadata::error::MetadataError::CorruptObject { reason, .. } => {
            crate::core::error::MetaDbError::CorruptValue {
                db: String::from(db),
                key: format!("sys:{key_name}"),
                reason,
            }
            .into()
        }
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use tempfile::TempDir;

    use super::*;
    use crab_metadata::key_codec;
    use crab_xet::hash::MerkleHash;
    use crab_xet::xorb::format::XorbRef;

    fn stub_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    fn test_metadb(store: Arc<dyn ObjectStore>) -> (MetaDb, TempDir) {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_path,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new(store, String::from("org/test-repo"), cfg);
        (metadb, cache_dir)
    }

    fn hash_from_seed(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed.wrapping_mul(31), seed.wrapping_mul(97), seed])
    }

    fn xorb_ref_for(xorb_seed: u64, chunk_index: u32, size: u32) -> XorbRef {
        XorbRef {
            xorb_hash: hash_from_seed(xorb_seed),
            chunk_index,
            uncompressed_size: size,
        }
    }

    fn committed_receipt(
        chunk_hash: MerkleHash,
        xorb_ref: XorbRef,
    ) -> crab_metadata::receipts::CommittedChunkReceipt {
        crab_metadata::receipts::CommittedChunkReceipt {
            schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
            chunk_hash: chunk_hash.into(),
            xorb_hash: xorb_ref.xorb_hash.into(),
            chunk_index: xorb_ref.chunk_index,
            uncompressed_size: xorb_ref.uncompressed_size,
            origin: crab_metadata::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                crab_storage::canonical_global_content_path("xorbs", &xorb_ref.xorb_hash.hex())
                    .to_string(),
                xorb_ref.xorb_hash.into(),
                [9; 32],
                1,
                Some("test-etag".to_owned()),
                None,
            ),
            source_repo_prefix: "org/source-repo".to_owned(),
            source_shard_hash: hash_from_seed(80_001).into(),
            committed_generation: 1,
            shard_index_hash: hash_from_seed(80_002).into(),
            gc_registry_generation: 1,
        }
    }

    fn committed_transaction_cost(
        entries: &[(MerkleHash, crab_metadata::receipts::CommittedChunkReceipt)],
    ) -> (u64, u64) {
        let mut operations = 0u64;
        let mut bytes = 0usize;
        let mut proofs = std::collections::HashSet::new();
        let mut anchors = std::collections::HashSet::new();
        for (chunk_hash, receipt) in entries {
            let source = receipt.source_anchor();
            let placement = receipt.compact_placement();
            let placement_value = placement.encode().expect("encode placement");
            operations += 2;
            bytes += key_codec::encode_committed_chunk_key(chunk_hash, &placement.placement_id())
                .len()
                + placement_value.len()
                + key_codec::encode_committed_chunk_head_key(chunk_hash).len()
                + placement_value.len();
            if proofs.insert(placement.origin_proof_id) {
                operations += 1;
                bytes += key_codec::encode_origin_proof_key(&placement.origin_proof_id).len()
                    + serde_json::to_vec(&receipt.origin)
                        .expect("serialize origin proof")
                        .len();
            }
            if anchors.insert(placement.source_anchor_id) {
                operations += 1;
                bytes += key_codec::encode_source_anchor_key(&placement.source_anchor_id).len()
                    + serde_json::to_vec(&source)
                        .expect("serialize source anchor")
                        .len();
            }
        }
        (operations, bytes as u64)
    }

    // --- config ---

    #[test]
    fn config_default_fills_in_sane_tunables() {
        let cfg = MetaDbConfig::default();
        assert_eq!(cfg.in_memory_ceiling_bytes, 1024 * 1024 * 1024);
        assert_eq!(cfg.cache_gc_grace, 3);
        assert_eq!(cfg.compaction_threshold, 4);
        assert_eq!(cfg.wal_flush_size, 4 * 1024 * 1024);
        assert_eq!(cfg.bloom_bits_per_key, 10);
        assert_eq!(cfg.chunk_index_path, ".crab/chunk_index_db/");
    }

    #[test]
    fn config_for_repo_anchors_file_index_path() {
        let cfg = MetaDbConfig::for_repo("org/my-repo");
        assert_eq!(cfg.file_index_path, "org/my-repo/file_index_db/");
        assert_eq!(cfg.chunk_index_path, ".crab/chunk_index_db/");
    }

    #[test]
    fn config_for_repo_trims_trailing_slash() {
        let cfg = MetaDbConfig::for_repo("org/my-repo/");
        assert_eq!(cfg.file_index_path, "org/my-repo/file_index_db/");
    }

    // --- construct / close ---

    #[tokio::test]
    async fn metadb_construct_does_not_open_anything() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        assert!(metadb.file_index_db.get().is_none());
        assert!(metadb.chunk_index_db.get().is_none());
        assert!(metadb.in_memory_chunk_index.get().is_none());
        assert!(metadb.persistent_chunk_index.get().is_none());
    }

    #[tokio::test]
    async fn metadb_close_all_is_noop_when_nothing_opened() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        metadb
            .close_all()
            .await
            .expect("close_all on fresh session");
    }

    #[tokio::test]
    async fn metadb_file_index_lazy_opens_and_closes_cleanly() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        {
            let store = metadb.file_index().await.expect("file_index accessor");
            let missing = hash_from_seed(1);
            assert!(store.get_legacy(&missing).await.expect("get").is_none());
        }
        assert!(metadb.file_index_db.get().is_some());
        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn metadb_file_and_chunk_databases_share_one_bounded_cache() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        metadb.file_index().await.expect("file index");
        metadb.chunk_index().await.expect("chunk index");

        let file_db = metadb.file_index_db.get().expect("file db opened");
        let chunk_db = metadb.chunk_index_db.get().expect("chunk db opened");
        assert!(file_db.shares_cache_with(chunk_db));
        assert_eq!(metadb.db_cache_capacity_bytes(), 256 * 1024 * 1024);

        metadb.close_all().await.expect("close_all");
    }

    // --- commit ---

    #[tokio::test]
    async fn commit_empty_transaction_opens_nothing_and_returns_zero() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        let txn = metadb.new_transaction();
        let receipt = metadb.commit(txn).await.expect("empty commit");
        assert_eq!(receipt.file_ops_written, 0);
        assert_eq!(receipt.chunk_ops_written, 0);
        assert_eq!(receipt.bytes_written, 0);
        assert!(metadb.file_index_db.get().is_none());
        assert!(metadb.chunk_index_db.get().is_none());
        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn commit_file_only_opens_only_file_index_db() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        let file_store = metadb.file_index().await.expect("file_index");

        let mut txn = metadb.new_transaction();
        file_store.save_legacy(&mut txn, &hash_from_seed(1), &hash_from_seed(2));
        file_store.save_legacy(&mut txn, &hash_from_seed(3), &hash_from_seed(4));

        let receipt = metadb.commit(txn).await.expect("commit");
        assert_eq!(receipt.file_ops_written, 2);
        assert_eq!(receipt.chunk_ops_written, 0);
        // 2 entries × (33 + 32) bytes = 130 bytes.
        assert_eq!(receipt.bytes_written, 2 * (33 + 32));

        assert!(metadb.file_index_db.get().is_some());
        assert!(
            metadb.chunk_index_db.get().is_none(),
            "file-only commit must not open chunk_index_db"
        );

        // Round-trip.
        assert_eq!(
            file_store
                .get_legacy(&hash_from_seed(1))
                .await
                .expect("get")
                .expect("present"),
            hash_from_seed(2)
        );

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn commit_chunk_only_opens_only_chunk_index_db() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        let chunk_store = metadb.chunk_index().await.expect("chunk_index");

        let entries = vec![
            (hash_from_seed(100), xorb_ref_for(1, 0, 10)),
            (hash_from_seed(101), xorb_ref_for(1, 1, 11)),
        ];
        let committed = entries
            .iter()
            .map(|(chunk_hash, xorb_ref)| (*chunk_hash, committed_receipt(*chunk_hash, *xorb_ref)))
            .collect::<Vec<_>>();
        let mut txn = metadb.new_transaction();
        chunk_store
            .save_committed_receipts(&mut txn, &committed)
            .expect("save committed receipts");

        let receipt = metadb.commit(txn).await.expect("commit");
        let (expected_ops, expected_bytes) = committed_transaction_cost(&committed);
        assert_eq!(receipt.chunk_ops_written, expected_ops);
        assert_eq!(receipt.file_ops_written, 0);
        assert_eq!(receipt.bytes_written, expected_bytes);

        assert!(metadb.chunk_index_db.get().is_some());
        assert!(
            metadb.file_index_db.get().is_none(),
            "chunk-only commit must not open file_index_db"
        );

        // Round-trip via the store's own three-tier path.
        for (hash, expected) in &entries {
            assert_eq!(chunk_store.get(hash).await.expect("get"), Some(*expected));
        }

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn commit_mixed_opens_both_dbs_and_sums_bytes() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        let file_store = metadb.file_index().await.expect("file_index");
        let chunk_store = metadb.chunk_index().await.expect("chunk_index");

        let chunk_hash = hash_from_seed(10);
        let committed = committed_receipt(chunk_hash, xorb_ref_for(1, 0, 100));
        let mut txn = metadb.new_transaction();
        file_store.save_legacy(&mut txn, &hash_from_seed(1), &hash_from_seed(2));
        chunk_store
            .save_committed_receipts(&mut txn, &[(chunk_hash, committed.clone())])
            .expect("save committed chunk receipt");

        let receipt = metadb.commit(txn).await.expect("mixed commit");
        assert_eq!(receipt.file_ops_written, 1);
        let (committed_ops, committed_bytes) =
            committed_transaction_cost(&[(chunk_hash, committed)]);
        assert_eq!(receipt.chunk_ops_written, committed_ops);
        assert_eq!(receipt.bytes_written, (33 + 32) + committed_bytes);

        assert!(metadb.file_index_db.get().is_some());
        assert!(metadb.chunk_index_db.get().is_some());

        metadb.close_all().await.expect("close_all");
    }

    // --- check_cache_gc_drift ---

    /// Write a raw `sys:gc_generation` value into chunk_index_db
    /// bypassing the MetaDb's owned handle so the tier-drift test
    /// simulates a "GC ran elsewhere" scenario cleanly.
    async fn seed_remote_gc_generation(
        store: Arc<dyn ObjectStore>,
        chunk_index_path: &str,
        generation: u64,
    ) {
        let db = Db::open(
            store,
            ObjectPath::from(chunk_index_path),
            stores::chunk_index::DB_LABEL,
        )
        .await
        .expect("open seed");
        let mut batch = slatedb::WriteBatch::new();
        batch.put(
            key_codec::encode_system_key(key_codec::SYS_GC_GENERATION).as_slice(),
            value_codec::encode_gc_generation_value(generation).as_slice(),
        );
        db.write(batch).await.expect("seed sys:gc_generation");
        db.close().await.expect("close seed");
    }

    #[tokio::test]
    async fn cache_gc_drift_no_drift_keeps_cache() {
        let store = stub_store();
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        // Seed the remote BEFORE MetaDb opens chunk_index_db.
        // SlateDB fences the older handle on a second open at the
        // same path, so establishing the remote state up front keeps
        // the MetaDb-owned handle authoritative for the drift check
        // itself. Production callers see the same ordering: rebuild
        // or an external GC writes generations before the next
        // session opens.
        seed_remote_gc_generation(Arc::clone(&store), &metadb.config.chunk_index_path, 5).await;

        // Initialise local tiers by routing through the chunk_index
        // accessor. This also opens the remote chunk_index_db; the
        // drift check then reuses both handles via `OnceAsync`.
        let _chunk_store = metadb.chunk_index().await.expect("chunk_index");

        // Seed a canary into both local tiers so we can assert it
        // survives the drift check when no drift is observed.
        let canary = hash_from_seed(42);
        let canary_value = xorb_ref_for(100, 0, 512);
        let persistent = metadb
            .persistent_chunk_index
            .get()
            .expect("persistent slot");
        persistent
            .insert(&canary, &canary_value)
            .expect("persistent insert");
        persistent.set_cache_gc_generation(5).expect("set local");

        let outcome = metadb
            .check_cache_gc_drift()
            .await
            .expect("drift check succeeds");
        assert_eq!(
            outcome,
            CacheDriftOutcome::NoDrift {
                local_generation: 5,
                remote_generation: 5,
            }
        );

        // Cache intact.
        assert_eq!(
            persistent.get(&canary).expect("persistent get"),
            Some(canary_value)
        );
        assert_eq!(
            persistent.cache_gc_generation().expect("local generation"),
            5
        );

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn cache_gc_drift_within_grace_keeps_cache() {
        // local=5, remote=7, grace=3 → 7 > 5+3 is false, no wipe.
        let store = stub_store();
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        seed_remote_gc_generation(Arc::clone(&store), &metadb.config.chunk_index_path, 7).await;

        let _ = metadb.chunk_index().await.expect("chunk_index");

        let canary = hash_from_seed(7);
        let canary_value = xorb_ref_for(200, 1, 256);
        let persistent = metadb
            .persistent_chunk_index
            .get()
            .expect("persistent slot");
        persistent
            .insert(&canary, &canary_value)
            .expect("persistent insert");
        persistent.set_cache_gc_generation(5).expect("set local");

        let outcome = metadb.check_cache_gc_drift().await.expect("drift check");
        assert_eq!(
            outcome,
            CacheDriftOutcome::NoDrift {
                local_generation: 5,
                remote_generation: 7,
            }
        );
        assert_eq!(
            persistent.get(&canary).expect("persistent get"),
            Some(canary_value)
        );
        assert_eq!(
            persistent.cache_gc_generation().expect("local"),
            5,
            "local generation must not advance inside grace"
        );

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn cache_gc_drift_beyond_grace_wipes_cache() {
        // local=5, remote=20, grace=3 → wipe.
        let store = stub_store();
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        seed_remote_gc_generation(Arc::clone(&store), &metadb.config.chunk_index_path, 20).await;

        let _ = metadb.chunk_index().await.expect("chunk_index");

        let canary = hash_from_seed(13);
        let canary_value = xorb_ref_for(300, 0, 1024);
        let persistent = metadb
            .persistent_chunk_index
            .get()
            .expect("persistent slot");
        persistent
            .insert(&canary, &canary_value)
            .expect("persistent insert");
        persistent.set_cache_gc_generation(5).expect("set local");

        // Also seed the in-memory tier so we can assert it gets wiped too.
        {
            let memory = metadb
                .in_memory_chunk_index
                .get()
                .expect("memory slot populated");
            let mut guard = memory.lock().expect("lock");
            guard.insert(canary, canary_value);
        }

        let outcome = metadb.check_cache_gc_drift().await.expect("drift check");
        assert_eq!(
            outcome,
            CacheDriftOutcome::WipedCache {
                old_generation: 5,
                new_generation: 20,
            }
        );

        assert!(
            persistent.get(&canary).expect("persistent get").is_none(),
            "persistent tier must be wiped beyond grace"
        );
        assert_eq!(
            persistent.cache_gc_generation().expect("local generation"),
            20,
            "local generation must advance to remote"
        );
        let memory = metadb.in_memory_chunk_index.get().expect("memory slot");
        {
            let guard = memory.lock().expect("lock");
            assert!(
                guard.get(&canary).is_none(),
                "in-memory tier must be wiped beyond grace"
            );
            assert!(guard.is_empty(), "in-memory tier must be fully empty");
        }

        metadb.close_all().await.expect("close_all");
    }

    // --- bump_gc_generation ---

    #[tokio::test]
    async fn bump_gc_generation_starts_at_one_from_fresh_db() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        let first = metadb
            .bump_gc_generation()
            .await
            .expect("bump_gc_generation");
        assert_eq!(first, 1, "fresh database → first bump yields generation 1");
        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn bump_gc_generation_monotonic_across_calls() {
        let (metadb, _cache_dir) = test_metadb(stub_store());
        let a = metadb.bump_gc_generation().await.expect("bump 1");
        let b = metadb.bump_gc_generation().await.expect("bump 2");
        let c = metadb.bump_gc_generation().await.expect("bump 3");
        assert_eq!((a, b, c), (1, 2, 3));
        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn bump_gc_generation_persists_across_handle_reopen() {
        // Bumping through one MetaDb session, then observing the
        // bumped value via `check_cache_gc_drift` on a second session
        // proves the write landed durably in chunk_index_db.
        let store = stub_store();
        {
            let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
            for _ in 0..5 {
                metadb.bump_gc_generation().await.expect("bump");
            }
            metadb.close_all().await.expect("close_all");
        }

        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));
        // Seed the persistent tier with a low cursor so the drift
        // check observes the remote's higher value.
        let persistent = metadb
            .open_persistent_tier()
            .await
            .expect("persistent tier");
        persistent
            .set_cache_gc_generation(0)
            .expect("set local generation");

        let outcome = metadb.check_cache_gc_drift().await.expect("drift check");
        match outcome {
            CacheDriftOutcome::WipedCache { new_generation, .. } => {
                assert_eq!(new_generation, 5, "expected 5 persisted bumps");
            }
            CacheDriftOutcome::NoDrift {
                remote_generation, ..
            } => {
                panic!(
                    "expected wipe after 5 bumps beyond grace, observed NoDrift with remote={remote_generation}"
                );
            }
        }

        metadb.close_all().await.expect("close_all");
    }

    // --- system_keys accessors ---

    /// Write raw `sys:*` values into one of the two databases by
    /// bypassing `MetaDb` entirely. Mirrors
    /// [`seed_remote_gc_generation`]: callers establish the remote
    /// state BEFORE the session under test lazy-opens the same path,
    /// so the `MetaDb` handle stays authoritative for the reads.
    async fn seed_raw_sys_values(
        store: Arc<dyn ObjectStore>,
        db_path: &str,
        db_label: &'static str,
        entries: &[(&str, Vec<u8>)],
    ) {
        let db = Db::open(store, ObjectPath::from(db_path), db_label)
            .await
            .expect("open seed");
        let mut batch = slatedb::WriteBatch::new();
        for (name, value) in entries {
            batch.put(
                key_codec::encode_system_key(name).as_slice(),
                value.as_slice(),
            );
        }
        db.write(batch).await.expect("seed sys:* batch");
        db.close().await.expect("close seed");
    }

    #[tokio::test]
    async fn system_keys_missing_returns_none_fields() {
        // A freshly-opened chunk_index_db has no sys:* keys written.
        // Every field should come back `None`; no corrupt-value
        // errors, no spurious zero defaults.
        let (metadb, _cache_dir) = test_metadb(stub_store());

        let snapshot = metadb
            .chunk_index_system_keys()
            .await
            .expect("snapshot on fresh db");

        assert_eq!(snapshot.format_version, None);
        assert_eq!(snapshot.epoch, None);
        assert_eq!(snapshot.created_at_unix_ms, None);
        assert_eq!(snapshot.gc_generation, None);

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn system_keys_roundtrip() {
        // Seed all four sys:* values, then verify the accessor
        // decodes them correctly. Run against chunk_index_db so the
        // gc_generation field is also exercised.
        let store = stub_store();
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        seed_raw_sys_values(
            Arc::clone(&store),
            &metadb.config.chunk_index_path,
            stores::chunk_index::DB_LABEL,
            &[
                (key_codec::SYS_FORMAT_VERSION, 1u32.to_le_bytes().to_vec()),
                (key_codec::SYS_EPOCH, 42u64.to_le_bytes().to_vec()),
                (
                    key_codec::SYS_CREATED_AT,
                    123_456_789u64.to_le_bytes().to_vec(),
                ),
                (key_codec::SYS_GC_GENERATION, 7u64.to_le_bytes().to_vec()),
            ],
        )
        .await;

        let snapshot = metadb.chunk_index_system_keys().await.expect("snapshot");

        assert_eq!(snapshot.format_version, Some(1));
        assert_eq!(snapshot.epoch, Some(42));
        assert_eq!(snapshot.created_at_unix_ms, Some(123_456_789));
        assert_eq!(snapshot.gc_generation, Some(7));

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn system_keys_corrupt_value_reports_corrupt_value_error() {
        // A 3-byte sys:epoch is unambiguously wrong. The accessor
        // must refuse it with MetaDbError::CorruptValue carrying the
        // database label and key name so operators can locate the
        // damage without parsing the source chain.
        let store = stub_store();
        let (metadb, _cache_dir) = test_metadb(Arc::clone(&store));

        seed_raw_sys_values(
            Arc::clone(&store),
            &metadb.config.chunk_index_path,
            stores::chunk_index::DB_LABEL,
            &[(key_codec::SYS_EPOCH, vec![0xAAu8, 0xBB, 0xCC])],
        )
        .await;

        let err = metadb
            .chunk_index_system_keys()
            .await
            .expect_err("3-byte epoch must fail");

        match err {
            crate::core::error::CrabError::MetaDb(
                crate::core::error::MetaDbError::CorruptValue { db, key, reason },
            ) => {
                assert_eq!(db, "chunk_index_db");
                assert!(key.contains("sys:epoch"), "key: {key}");
                assert!(
                    reason.contains("got 3"),
                    "reason should name the bad length: {reason}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }

        metadb.close_all().await.expect("close_all");
    }

    // --- read-only mode ---

    #[tokio::test]
    async fn read_only_session_rejects_commit_without_opening_databases() {
        // Seed the store via a writer so the read-only reader can
        // open. Then verify that committing a populated transaction
        // against a read-only session fails fast with
        // `MetaDbError::ReadOnly` — the reader itself would not
        // accept a write either, but the MetaDb layer must surface
        // the condition with its own error rather than letting it
        // leak through from SlateDB.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache_dir = TempDir::new().expect("tempdir");

        // Seed: one tiny write through a fresh writer session.
        {
            let cfg = MetaDbConfig {
                local_chunk_index_path: cache_dir.path().join("chunk-index.sqlite"),
                ..MetaDbConfig::for_repo("org/test-repo")
            };
            let seed = MetaDb::new(Arc::clone(&store), String::from("org/test-repo"), cfg);
            let file_store = seed.file_index().await.expect("seed file_index");
            let mut txn = seed.new_transaction();
            file_store.save_legacy(&mut txn, &hash_from_seed(1), &hash_from_seed(2));
            seed.commit(txn).await.expect("seed commit");
            seed.close_all().await.expect("seed close");
        }

        // Now open read-only and attempt a commit.
        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_dir.path().join("chunk-index.sqlite"),
            read_only: true,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new(Arc::clone(&store), String::from("org/test-repo"), cfg);
        let file_store = metadb.file_index().await.expect("ro file_index");
        let mut txn = metadb.new_transaction();
        file_store.save_legacy(&mut txn, &hash_from_seed(7), &hash_from_seed(8));
        assert!(!txn.is_empty());

        let err = metadb.commit(txn).await.expect_err("commit must reject");
        match err {
            crate::core::error::CrabError::MetaDb(crate::core::error::MetaDbError::ReadOnly {
                op,
                ..
            }) => {
                assert_eq!(op, "commit");
            }
            other => panic!("expected MetaDbError::ReadOnly, got {other:?}"),
        }

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn chunk_index_store_works_with_live_local_sqlite_handle() {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let _held = crab_metadata::persistent_chunk_index::PersistentChunkIndex::open_or_create(
            &cache_path,
        )
        .expect("open sqlite");

        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_path,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new(stub_store(), String::from("org/test-repo"), cfg);
        let chunk_store = metadb.chunk_index().await.expect("chunk_index");
        let chunk = hash_from_seed(21);
        let xorb_ref = xorb_ref_for(22, 3, 4096);
        let committed = committed_receipt(chunk, xorb_ref);

        let mut txn = metadb.new_transaction();
        chunk_store
            .save_committed_receipts(&mut txn, &[(chunk, committed)])
            .expect("save committed receipt");
        metadb.commit(txn).await.expect("commit chunk index");

        let got = chunk_store.get(&chunk).await.expect("get");
        assert_eq!(got, Some(xorb_ref));

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn read_only_session_rejects_bump_gc_generation() {
        let store = stub_store();
        let cache_dir = TempDir::new().expect("tempdir");
        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_dir.path().join("chunk-index.sqlite"),
            read_only: true,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new(store, String::from("org/test-repo"), cfg);

        let err = metadb
            .bump_gc_generation()
            .await
            .expect_err("bump_gc_generation must reject");
        match err {
            crate::core::error::CrabError::MetaDb(crate::core::error::MetaDbError::ReadOnly {
                op,
                ..
            }) => {
                assert_eq!(op, "bump_gc_generation");
            }
            other => panic!("expected MetaDbError::ReadOnly, got {other:?}"),
        }

        metadb.close_all().await.expect("close_all");
    }

    #[tokio::test]
    async fn read_only_session_reads_entries_seeded_by_writer() {
        // Seed a file_index entry via a writer session, then open a
        // read-only session against the same store and verify the
        // entry round-trips through `FileIndexStore::get`.
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache_dir = TempDir::new().expect("tempdir");
        let file_hash = hash_from_seed(101);
        let shard_hash = hash_from_seed(202);

        {
            let cfg = MetaDbConfig {
                local_chunk_index_path: cache_dir.path().join("chunk-index.sqlite"),
                ..MetaDbConfig::for_repo("org/test-repo")
            };
            let metadb = MetaDb::new(Arc::clone(&store), String::from("org/test-repo"), cfg);
            let file_store = metadb.file_index().await.expect("file_index");
            let mut txn = metadb.new_transaction();
            file_store.save_legacy(&mut txn, &file_hash, &shard_hash);
            metadb.commit(txn).await.expect("commit");
            metadb.close_all().await.expect("close_all");
        }

        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_dir.path().join("chunk-index.sqlite"),
            read_only: true,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new(Arc::clone(&store), String::from("org/test-repo"), cfg);
        assert!(metadb.is_read_only());
        let file_store = metadb.file_index().await.expect("file_index");
        let got = file_store.get_legacy(&file_hash).await.expect("get");
        assert_eq!(got, Some(shard_hash));

        metadb.close_all().await.expect("close_all");
    }

    /// Regression: the push pipeline must be able to share its
    /// `PersistentChunkIndex` handle with the MetaDb session so
    /// step 9b's `warm_local_shard` reuses the handle opened by
    /// step 3's shard-sync instead of calling `open_or_create` a
    /// second time on the same warm-tier path.
    #[tokio::test]
    async fn install_persistent_tier_shares_single_handle() {
        use crab_metadata::persistent_chunk_index::PersistentChunkIndex;

        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");

        // Caller opens the handle once (mirroring push step 3).
        let shared =
            Arc::new(PersistentChunkIndex::open_or_create(&cache_path).expect("open first"));

        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_path.clone(),
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new_with_persistent_tier(
            stub_store(),
            String::from("org/test-repo"),
            cfg,
            Arc::clone(&shared),
        );

        // Asking the session for the persistent tier returns the
        // exact same Arc — no new `open_or_create` call fired.
        let got = metadb
            .open_persistent_tier()
            .await
            .expect("persistent tier");
        assert!(
            Arc::ptr_eq(&got, &shared),
            "persistent tier slot must reuse the pre-installed handle"
        );
    }

    /// `install_persistent_tier` on a guard that has not yet lazily
    /// opened its own handle accepts the value and returns `true`.
    /// A second install call returns `false` because the slot is
    /// already populated (and the passed `Arc` is dropped).
    #[tokio::test]
    async fn install_persistent_tier_is_idempotent() {
        use crab_metadata::persistent_chunk_index::PersistentChunkIndex;

        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let shared = Arc::new(PersistentChunkIndex::open_or_create(&cache_path).expect("open"));

        let cfg = MetaDbConfig {
            local_chunk_index_path: cache_path,
            ..MetaDbConfig::for_repo("org/test-repo")
        };
        let metadb = MetaDb::new(stub_store(), String::from("org/test-repo"), cfg);

        assert!(
            metadb.install_persistent_tier(Arc::clone(&shared)),
            "first install must succeed"
        );
        assert!(
            !metadb.install_persistent_tier(Arc::clone(&shared)),
            "second install must be a no-op"
        );
    }
}
