//! Thin wrapper over [`slatedb::Db`] / [`slatedb::DbReader`] exposing a
//! crab-native KV surface.
//!
//! One seam between the metadb module and the `slatedb` crate. Every
//! caller inside `metadata::metadb::*` talks to this type; the raw
//! SlateDB handles are never re-exported. The wrapper holds the handle
//! plus a static label ("file_index_db" or "chunk_index_db") so error
//! payloads and tracing spans can identify which logical database
//! failed without the caller needing to thread the name through.
//!
//! The surface is deliberately narrow: `open` / `open_readonly`, point
//! `get`, adaptive `get_batch`, a single `write(WriteBatch)`,
//! plus `flush` / `close` and a diagnostic `scan` for integrity
//! checks. Large same-keyspace batches use one ordered scan/merge because
//! SlateDB has no multi-get API; small or mixed-keyspace batches retain
//! bounded point reads.
//!
//! # Read-only vs read-write
//!
//! Opening a SlateDB in read-write mode fences any other writer
//! holding the same manifest. Hydrate, fsck, diff, and the `metadb
//! diagnose` / `doctor` surfaces only issue point reads, so they open
//! through [`Db::open_readonly`] which materialises a
//! [`slatedb::DbReader`] — no WAL writes, no fencing, so a concurrent
//! `crab push` keeps running uninterrupted. Write paths (push, gc,
//! metadb rebuild) keep using [`Db::open`] which hands back a full
//! writer handle.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use slatedb::config::ScanOptions;
use slatedb::db_cache::DbCache;
use slatedb::db_cache::foyer::{FoyerCache, FoyerCacheOptions};
use tracing::Instrument;

use crate::core::error::{CrabError, MetaDbError, Result};
use crate::core::metrics::Metrics;
use crate::metadata::metadb::MetaDbEngineConfig;

const GET_BATCH_CONCURRENCY: usize = 256;
const DENSE_BATCH_SCAN_THRESHOLD: usize = 16_384;
const DENSE_BATCH_MIN_HASH_PREFIX_SPAN: u32 = (u16::MAX as u32 + 1) / 4;
const DENSE_BATCH_SCAN_READ_AHEAD_BYTES: usize = 1024 * 1024;
pub(super) const DB_CACHE_CAPACITY_BYTES: u64 = 256 * 1024 * 1024;

pub(super) fn new_db_cache() -> Arc<dyn DbCache> {
    Arc::new(FoyerCache::new_with_opts(FoyerCacheOptions {
        max_capacity: DB_CACHE_CAPACITY_BYTES,
        ..FoyerCacheOptions::default()
    }))
}

pub(crate) fn compactor_options(
    config: &MetaDbEngineConfig,
) -> Result<slatedb::config::CompactorOptions> {
    validate_engine_config(config)?;
    let threshold =
        usize::try_from(config.compaction_threshold).map_err(|_| CrabError::Configuration {
            key: "metadb compaction_threshold".to_owned(),
            origin: "value cannot be represented by SlateDB".to_owned(),
        })?;
    let defaults = slatedb::config::SizeTieredCompactionSchedulerOptions::default();
    let scheduler = slatedb::config::SizeTieredCompactionSchedulerOptions {
        min_compaction_sources: threshold,
        max_compaction_sources: defaults.max_compaction_sources.max(threshold),
        ..defaults
    };
    Ok(slatedb::config::CompactorOptions {
        scheduler_options: scheduler.into(),
        ..slatedb::config::CompactorOptions::default()
    })
}

pub(crate) fn filter_policies(
    config: &MetaDbEngineConfig,
) -> Result<Vec<Arc<dyn slatedb::FilterPolicy>>> {
    validate_engine_config(config)?;
    Ok(vec![Arc::new(slatedb::BloomFilterPolicy::new(
        config.bloom_bits_per_key,
    ))])
}

fn validate_engine_config(config: &MetaDbEngineConfig) -> Result<()> {
    for (key, value) in [
        (
            "compaction_threshold",
            u64::from(config.compaction_threshold),
        ),
        ("wal_flush_size", config.wal_flush_size),
        ("bloom_bits_per_key", u64::from(config.bloom_bits_per_key)),
    ] {
        if value == 0 {
            return Err(CrabError::Configuration {
                key: format!("metadb {key}"),
                origin: "value must be greater than zero".to_owned(),
            });
        }
    }
    usize::try_from(config.wal_flush_size).map_err(|_| CrabError::Configuration {
        key: "metadb wal_flush_size".to_owned(),
        origin: "value cannot be represented by SlateDB".to_owned(),
    })?;
    Ok(())
}

fn slatedb_settings(config: &MetaDbEngineConfig) -> Result<slatedb::config::Settings> {
    Ok(slatedb::config::Settings {
        // Crab owns explicit durability boundaries. Disabling SlateDB's
        // 100 ms timer prevents idle and post-commit batch work from
        // generating one WAL object per timer tick.
        flush_interval: None,
        l0_sst_size_bytes: usize::try_from(config.wal_flush_size).map_err(|_| {
            CrabError::Configuration {
                key: "metadb wal_flush_size".to_owned(),
                origin: "value cannot be represented by SlateDB".to_owned(),
            }
        })?,
        compactor_options: Some(compactor_options(config)?),
        ..slatedb::config::Settings::default()
    })
}

fn dense_batch_scan_options() -> ScanOptions {
    ScanOptions::default()
        .with_read_ahead_bytes(DENSE_BATCH_SCAN_READ_AHEAD_BYTES)
        .with_max_fetch_tasks(1)
        .with_cache_blocks(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchLookupMode {
    Points,
    Scan,
}

impl BatchLookupMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Scan => "scan",
        }
    }
}

#[derive(Debug)]
struct UniqueBatchKey {
    key: Bytes,
    input_indices: Vec<usize>,
}

#[derive(Debug)]
struct BatchLookupPlan {
    unique_keys: Vec<UniqueBatchKey>,
    mode: BatchLookupMode,
}

impl BatchLookupPlan {
    fn new(keys: &[Bytes]) -> Self {
        let mut indexed = keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, key)| (key, index))
            .collect::<Vec<_>>();
        indexed.sort_unstable_by(|left, right| left.0.cmp(&right.0));

        let mut unique_keys: Vec<UniqueBatchKey> = Vec::with_capacity(indexed.len());
        for (key, input_index) in indexed {
            if let Some(last) = unique_keys.last_mut()
                && last.key == key
            {
                last.input_indices.push(input_index);
                continue;
            }
            unique_keys.push(UniqueBatchKey {
                key,
                input_indices: vec![input_index],
            });
        }

        let first = unique_keys.first().map(|entry| entry.key.as_ref());
        let fixed_record_family = first.is_some_and(|first| {
            first.len() >= 3
                && unique_keys.iter().all(|entry| {
                    entry.key.len() == first.len() && entry.key.first() == first.first()
                })
        });
        let hash_prefix_span = unique_keys
            .first()
            .zip(unique_keys.last())
            .filter(|_| fixed_record_family)
            .map(|(first, last)| {
                let first_prefix = u16::from_be_bytes([first.key[1], first.key[2]]) as u32;
                let last_prefix = u16::from_be_bytes([last.key[1], last.key[2]]) as u32;
                last_prefix.saturating_sub(first_prefix) + 1
            })
            .unwrap_or(0);
        let mode = if unique_keys.len() >= DENSE_BATCH_SCAN_THRESHOLD
            && hash_prefix_span >= DENSE_BATCH_MIN_HASH_PREFIX_SPAN
        {
            BatchLookupMode::Scan
        } else {
            BatchLookupMode::Points
        };

        Self { unique_keys, mode }
    }

    const fn mode(&self) -> BatchLookupMode {
        self.mode
    }
}

/// Which SlateDB interface a [`Db`] wraps.
///
/// Read-only handles use [`slatedb::DbReader`] which replays the
/// manifest through a checkpoint and therefore does not fence any
/// other writer. Attempting a `write` / `flush` on a read-only handle
/// fails fast with [`MetaDbError::ReadOnly`].
#[derive(Clone)]
enum Inner {
    /// Read-write handle. Fences other writers on the same path.
    Writer(Arc<slatedb::Db>),

    /// Read-only handle over a pinned checkpoint. Non-fencing.
    Reader(Arc<slatedb::DbReader>),
}

/// Heuristic: does this SlateDB error indicate that the database has
/// never been written (no manifest on object storage)?
///
/// SlateDB 0.12 surfaces this as `ErrorKind::Data` with a message
/// that mentions "latest transactional object (e.g. manifest)
/// version". Matching on the message is fragile but the public API
/// does not (yet) expose a dedicated kind for this case, and
/// swallowing the wrong `Data` error would mask real corruption, so
/// the match is intentionally tight.
fn is_manifest_missing(err: &slatedb::Error) -> bool {
    if !matches!(err.kind(), slatedb::ErrorKind::Data) {
        return false;
    }
    let msg = err.to_string();
    msg.contains("failed to find latest transactional object")
}

/// Crab-native wrapper over [`slatedb::Db`] or [`slatedb::DbReader`].
///
/// Cheap to clone via `Arc<Self>` — the inner handle is itself
/// reference-counted, so handing a store a fresh `Arc<Db>` costs one
/// atomic increment and shares the underlying WAL, block cache, and
/// background compactor (or, for readers, the checkpoint poller).
#[derive(Clone)]
pub struct Db {
    /// Writer or reader handle, selected at open time.
    inner: Inner,

    /// Session cache retained for identity checks and shared ownership.
    _cache: Arc<dyn DbCache>,

    /// Stable label used in error payloads and structured logs. One of
    /// `"file_index_db"` or `"chunk_index_db"` today.
    label: &'static str,

    /// Optional metrics sink. When populated, every `open`, `get`,
    /// `get_batch`, `write`, and `close` call bumps the corresponding
    /// `metadb_*` counter. Left `None` for unit tests that don't
    /// exercise the observability surface.
    metrics: Option<Arc<Metrics>>,
}

impl Db {
    /// Open (or create) a SlateDB instance at `path` on `store` in
    /// read-write mode.
    ///
    /// `label` is the logical name that will appear in every error
    /// variant produced by this instance — pick it carefully, callers
    /// grep logs by this string.
    ///
    /// Opening in read-write mode fences any other writer holding the
    /// same manifest. Use [`Self::open_readonly`] for surfaces that
    /// only issue point reads.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        path: ObjectPath,
        label: &'static str,
    ) -> Result<Self> {
        Self::open_with_cache(
            store,
            path,
            label,
            new_db_cache(),
            &MetaDbEngineConfig::default(),
        )
        .await
    }

    pub(super) async fn open_with_cache(
        store: Arc<dyn ObjectStore>,
        path: ObjectPath,
        label: &'static str,
        cache: Arc<dyn DbCache>,
        config: &MetaDbEngineConfig,
    ) -> Result<Self> {
        let span = tracing::debug_span!("metadb.open", db = label, path = %path, mode = "rw");
        let start = std::time::Instant::now();
        let settings = slatedb_settings(config)?;
        match slatedb::Db::builder(path.clone(), store)
            .with_settings(settings)
            .with_filter_policies(filter_policies(config)?)
            .with_db_cache(Arc::clone(&cache))
            .build()
            .instrument(span.clone())
            .await
        {
            Ok(db) => {
                let _enter = span.enter();
                tracing::debug!(
                    db = label,
                    path = %path,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "metadb: opened slatedb instance (read-write)"
                );
                Ok(Self {
                    inner: Inner::Writer(Arc::new(db)),
                    _cache: cache,
                    label,
                    metrics: None,
                })
            }
            Err(source) => Err(MetaDbError::Open {
                db: String::from(label),
                path: path.to_string(),
                source,
            }
            .into()),
        }
    }

    /// Open a read-only SlateDB view at `path` on `store`.
    ///
    /// Wraps [`slatedb::DbReader`], which reads through a pinned
    /// manifest checkpoint and never writes WAL records. Crucially,
    /// this does not fence other writers — a long-running `crab
    /// push` can keep writing while this handle serves hydrate,
    /// diff, fsck, or diagnose reads.
    ///
    /// The reader creates its own checkpoint on open and refreshes
    /// it periodically; [`Self::close`] cleans the checkpoint up.
    /// Writes through this handle fail fast with
    /// [`MetaDbError::ReadOnly`] — this is a programmer error and
    /// surfaces immediately rather than corrupting state.
    ///
    /// Returns [`MetaDbError::ReadOnlyUninitialized`] when the
    /// underlying database has never been written (no manifest on
    /// object storage). The read path uses this variant to treat a
    /// missing remote as "all lookups miss" rather than a hard
    /// failure — a freshly-cloned repo on a bucket whose
    /// `file_index_db` has never received a push is a legitimate
    /// state, not an error.
    pub async fn open_readonly(
        store: Arc<dyn ObjectStore>,
        path: ObjectPath,
        label: &'static str,
    ) -> Result<Self> {
        Self::open_readonly_with_cache(
            store,
            path,
            label,
            new_db_cache(),
            &MetaDbEngineConfig::default(),
        )
        .await
    }

    pub(super) async fn open_readonly_with_cache(
        store: Arc<dyn ObjectStore>,
        path: ObjectPath,
        label: &'static str,
        cache: Arc<dyn DbCache>,
        config: &MetaDbEngineConfig,
    ) -> Result<Self> {
        let span = tracing::debug_span!("metadb.open", db = label, path = %path, mode = "ro");
        let start = std::time::Instant::now();
        // `DbReader::builder` takes `P: Into<Path>`; our `ObjectPath`
        // value already satisfies that via the object_store re-export
        // slatedb uses. Clone the path so the error path below still
        // has the original to surface.
        match slatedb::DbReader::builder(path.clone(), store)
            .with_db_cache(Arc::clone(&cache))
            .with_filter_policies(filter_policies(config)?)
            .build()
            .instrument(span.clone())
            .await
        {
            Ok(reader) => {
                let _enter = span.enter();
                tracing::debug!(
                    db = label,
                    path = %path,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "metadb: opened slatedb reader (read-only)"
                );
                Ok(Self {
                    inner: Inner::Reader(Arc::new(reader)),
                    _cache: cache,
                    label,
                    metrics: None,
                })
            }
            Err(source) if is_manifest_missing(&source) => {
                // Fresh database: no manifest on object storage yet.
                // Surface a dedicated error so callers can map this
                // to "empty read" without having to string-match the
                // source chain.
                tracing::debug!(
                    db = label,
                    path = %path,
                    "metadb: read-only open against never-initialised database"
                );
                Err(MetaDbError::ReadOnlyUninitialized {
                    db: String::from(label),
                    path: path.to_string(),
                }
                .into())
            }
            Err(source) => Err(MetaDbError::Open {
                db: String::from(label),
                path: path.to_string(),
                source,
            }
            .into()),
        }
    }

    /// Attach a metrics sink to this handle (builder-style).
    ///
    /// All subsequent `get`, `get_batch`, `write`, and `close` calls
    /// bump the `metadb_*` counters. The open itself is already
    /// complete at this point, but the caller can bump the open
    /// counter explicitly via [`Metrics::inc_metadb_open_count`] once
    /// the handle is attached to a session.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Open a SlateDB instance with a metrics sink attached from the
    /// start. Bumps `metadb_open_count` on success so the counter
    /// reflects actual SlateDB opens, not just handle attachments.
    ///
    /// Equivalent to [`Self::open`] followed by
    /// [`Self::with_metrics`] but with the open counter bump folded
    /// in — use this when the caller already has an `Arc<Metrics>`
    /// available at open time.
    pub async fn open_with_metrics(
        store: Arc<dyn ObjectStore>,
        path: ObjectPath,
        label: &'static str,
        metrics: Arc<Metrics>,
    ) -> Result<Self> {
        let db = Self::open(store, path, label).await?;
        metrics.inc_metadb_open_count();
        Ok(db.with_metrics(metrics))
    }

    /// Open a read-only SlateDB instance with a metrics sink
    /// attached from the start. Sibling of [`Self::open_readonly`]
    /// that bumps `metadb_open_count` on success.
    pub async fn open_readonly_with_metrics(
        store: Arc<dyn ObjectStore>,
        path: ObjectPath,
        label: &'static str,
        metrics: Arc<Metrics>,
    ) -> Result<Self> {
        let db = Self::open_readonly(store, path, label).await?;
        metrics.inc_metadb_open_count();
        Ok(db.with_metrics(metrics))
    }

    /// Is this handle read-only?
    ///
    /// `true` for [`Self::open_readonly`] handles and their metrics
    /// sibling. Read-only handles reject `write` and `flush`.
    pub fn is_read_only(&self) -> bool {
        matches!(self.inner, Inner::Reader(_))
    }

    /// The logical name of this database.
    pub fn label(&self) -> &'static str {
        self.label
    }

    #[cfg(test)]
    pub(super) fn shares_cache_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self._cache, &other._cache)
    }

    /// Point lookup. Returns `Ok(None)` on a miss.
    ///
    /// Maps any SlateDB failure to [`MetaDbError::Read`] with this
    /// instance's label so the caller can tell which database produced
    /// the error without inspecting the source chain.
    pub async fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Some(m) = self.metrics.as_ref() {
            m.inc_metadb_get_count();
        }
        let span = tracing::trace_span!("metadb.get", db = self.label);
        let result = match &self.inner {
            Inner::Writer(db) => db.get(key).instrument(span).await,
            Inner::Reader(reader) => reader.get(key).instrument(span).await,
        }
        .map_err(|source| {
            CrabError::from(MetaDbError::Read {
                db: String::from(self.label),
                prefix: String::from("<content>"),
                source,
            })
        })?;
        if result.is_some()
            && let Some(m) = self.metrics.as_ref()
        {
            m.inc_metadb_get_hits();
        }
        Ok(result)
    }

    /// Adaptive stable-order batch lookup.
    ///
    /// Returns a `Vec` aligned with the input: `result[i]` corresponds
    /// to `keys[i]`. Duplicate keys are read once. Large batches in one
    /// fixed-width keyspace use an ordered range scan; other batches use
    /// bounded point reads. The first read error short-circuits.
    pub async fn get_batch(&self, keys: &[Bytes]) -> Result<Vec<Option<Bytes>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(m) = self.metrics.as_ref() {
            m.inc_metadb_batch_get_count();
        }

        let plan = BatchLookupPlan::new(keys);
        let span = tracing::trace_span!(
            "metadb.get_batch",
            db = self.label,
            count = keys.len(),
            unique = plan.unique_keys.len(),
            mode = plan.mode().as_str()
        );
        let fetched = match plan.mode() {
            BatchLookupMode::Points => self.get_unique_points(&plan).instrument(span).await?,
            BatchLookupMode::Scan => self.scan_unique_keys(&plan).instrument(span).await?,
        };

        let mut results: Vec<Option<Bytes>> = vec![None; keys.len()];
        for (entry, value) in plan.unique_keys.iter().zip(fetched) {
            for &input_index in &entry.input_indices {
                results[input_index] = value.clone();
            }
        }

        Ok(results)
    }

    async fn get_unique_points(&self, plan: &BatchLookupPlan) -> Result<Vec<Option<Bytes>>> {
        let inner = self.inner.clone();
        let label = self.label;
        let keys = plan
            .unique_keys
            .iter()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        let concurrency = GET_BATCH_CONCURRENCY.min(keys.len()).max(1);
        let fetched: Vec<(usize, Option<Bytes>)> =
            futures_util::stream::iter(keys.into_iter().enumerate().map(|(index, key)| {
                let inner = inner.clone();
                async move {
                    let value = match &inner {
                        Inner::Writer(db) => db.get(key.as_ref()).await,
                        Inner::Reader(reader) => reader.get(key.as_ref()).await,
                    }
                    .map_err(|source| MetaDbError::Read {
                        db: String::from(label),
                        prefix: String::from("<content>"),
                        source,
                    })?;
                    Ok::<(usize, Option<Bytes>), CrabError>((index, value))
                }
            }))
            .buffer_unordered(concurrency)
            .try_collect()
            .await?;
        let mut values = vec![None; plan.unique_keys.len()];
        for (index, value) in fetched {
            values[index] = value;
        }
        Ok(values)
    }

    async fn scan_unique_keys(&self, plan: &BatchLookupPlan) -> Result<Vec<Option<Bytes>>> {
        let Some(first) = plan.unique_keys.first() else {
            return Ok(Vec::new());
        };
        let Some(last) = plan.unique_keys.last() else {
            return Ok(Vec::new());
        };
        let range = first.key.clone()..=last.key.clone();
        let options = dense_batch_scan_options();
        let mut rows = match &self.inner {
            Inner::Writer(db) => db.scan_with_options(range, &options).await,
            Inner::Reader(reader) => reader.scan_with_options(range, &options).await,
        }
        .map_err(|source| {
            CrabError::from(MetaDbError::Read {
                db: String::from(self.label),
                prefix: String::from("<batch-scan>"),
                source,
            })
        })?;

        let mut values = vec![None; plan.unique_keys.len()];
        let mut requested = 0usize;
        while requested < plan.unique_keys.len() {
            let Some(row) = rows.next().await.map_err(|source| {
                CrabError::from(MetaDbError::Read {
                    db: String::from(self.label),
                    prefix: String::from("<batch-scan>"),
                    source,
                })
            })?
            else {
                break;
            };

            while requested < plan.unique_keys.len()
                && plan.unique_keys[requested].key.as_ref() < row.key.as_ref()
            {
                requested += 1;
            }
            if requested == plan.unique_keys.len() {
                break;
            }
            if plan.unique_keys[requested].key == row.key {
                values[requested] = Some(row.value);
                requested += 1;
            }
        }
        Ok(values)
    }

    /// Commit a pre-built [`slatedb::WriteBatch`] atomically.
    ///
    /// Any failure is mapped to [`MetaDbError::Write`]. SlateDB's
    /// `write` returns a `WriteHandle` on success which we discard —
    /// crab does not track per-write durability handles; WAL flush
    /// semantics are driven by the session-level close path.
    ///
    /// Fails fast with [`MetaDbError::ReadOnly`] when called on a
    /// handle opened via [`Self::open_readonly`].
    pub async fn write(&self, batch: slatedb::WriteBatch) -> Result<()> {
        self.write_inner(batch, true).await
    }

    /// Buffer one repairable acceleration batch until the caller's flush boundary.
    pub(crate) async fn write_buffered(&self, batch: slatedb::WriteBatch) -> Result<()> {
        self.write_inner(batch, false).await
    }

    async fn write_inner(&self, batch: slatedb::WriteBatch, durable: bool) -> Result<()> {
        let db = match &self.inner {
            Inner::Writer(db) => Arc::clone(db),
            Inner::Reader(_) => {
                return Err(MetaDbError::ReadOnly {
                    db: String::from(self.label),
                    op: "write",
                }
                .into());
            }
        };
        let span = tracing::debug_span!("metadb.write", db = self.label);
        let start = std::time::Instant::now();
        if let Some(m) = self.metrics.as_ref() {
            m.inc_metadb_batch_write_count();
            if !durable {
                m.inc_metadb_buffered_batch_write_count();
            }
        }
        let res = db
            .write_with_options(
                batch,
                &slatedb::config::WriteOptions {
                    await_durable: false,
                    ..slatedb::config::WriteOptions::default()
                },
            )
            .instrument(span.clone())
            .await
            .map_err(|source| {
                CrabError::from(MetaDbError::Write {
                    db: String::from(self.label),
                    source,
                })
            });
        let res = match (res, durable) {
            (Ok(_), true) => {
                let result = db.flush().await.map_err(|source| {
                    CrabError::from(MetaDbError::Write {
                        db: String::from(self.label),
                        source,
                    })
                });
                if result.is_ok()
                    && let Some(metrics) = self.metrics.as_ref()
                {
                    metrics.inc_metadb_wal_flush_count();
                }
                result
            }
            (Ok(_), false) => Ok(()),
            (Err(error), _) => Err(error),
        };
        if res.is_ok() {
            let _enter = span.enter();
            tracing::trace!(
                db = self.label,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "metadb: batch write committed"
            );
        }
        res
    }

    /// Flush any in-memory WAL segments to object storage.
    ///
    /// Read-only handles have no WAL to flush; `flush` on a read-only
    /// handle is a no-op that returns `Ok(())` rather than erroring.
    /// Treating it as a no-op keeps the session-level `flush_all`
    /// fan-out simple — callers don't have to branch on mode.
    pub async fn flush(&self) -> Result<()> {
        let db = match &self.inner {
            Inner::Writer(db) => Arc::clone(db),
            Inner::Reader(_) => return Ok(()),
        };
        db.flush().await.map_err(|source| {
            MetaDbError::Write {
                db: String::from(self.label),
                source,
            }
            .into()
        })
    }

    /// Flush the active memtable to object storage and release its entries.
    pub async fn flush_memtable(&self) -> Result<()> {
        let db = match &self.inner {
            Inner::Writer(db) => Arc::clone(db),
            Inner::Reader(_) => return Ok(()),
        };
        let result = db
            .flush_with_options(slatedb::config::FlushOptions {
                flush_type: slatedb::config::FlushType::MemTable,
            })
            .await
            .map_err(|source| {
                MetaDbError::Write {
                    db: String::from(self.label),
                    source,
                }
                .into()
            });
        if result.is_ok()
            && let Some(metrics) = self.metrics.as_ref()
        {
            metrics.inc_metadb_memtable_flush_count();
        }
        result
    }

    /// Full-range scan over all keys in the database.
    ///
    /// Returns a [`slatedb::DbIterator`] that yields `(key, value)`
    /// pairs in sorted order. Intended exclusively for diagnostic
    /// surfaces (`metadb diagnose --deep`, `fsck`) — production read
    /// paths use point lookups.
    ///
    /// The iterator holds references to in-memory tables and
    /// SSTables; callers should consume it promptly to avoid blocking
    /// compaction.
    pub async fn scan(&self) -> Result<slatedb::DbIterator> {
        let span = tracing::debug_span!("metadb.scan", db = self.label);
        let iter = match &self.inner {
            Inner::Writer(db) => db.scan(..).instrument(span).await,
            Inner::Reader(reader) => reader.scan(..).instrument(span).await,
        }
        .map_err(|source| {
            CrabError::from(MetaDbError::Read {
                db: String::from(self.label),
                prefix: String::from("<scan>"),
                source,
            })
        })?;
        Ok(iter)
    }

    /// Scan one ordered key prefix for a production generation lookup.
    ///
    /// Generation-pinned file/chunk records keep immutable history under a
    /// content-hash prefix. Point reads cannot select the newest record that
    /// is visible to an older manifest generation, so this is the one
    /// production exception to the point-only store surface.
    pub async fn scan_prefix(&self, prefix: &[u8]) -> Result<slatedb::DbIterator> {
        let span = tracing::trace_span!("metadb.scan_prefix", db = self.label);
        match &self.inner {
            Inner::Writer(db) => db.scan_prefix(prefix, ..).instrument(span).await,
            Inner::Reader(reader) => reader.scan_prefix(prefix, ..).instrument(span).await,
        }
        .map_err(|source| {
            CrabError::from(MetaDbError::Read {
                db: String::from(self.label),
                prefix: String::from("<committed-content>"),
                source,
            })
        })
    }

    /// Close the database cleanly.
    ///
    /// Idempotent at the crab layer — repeated calls against an
    /// already-closed SlateDB are mapped to [`MetaDbError::Close`]
    /// just like any other close failure; the session is expected to
    /// call this at most once via [`MetaDb::close_all`] anyway.
    ///
    /// For read-only handles this tears down the checkpoint poller
    /// and releases the reader's checkpoint on the manifest.
    pub async fn close(&self) -> Result<()> {
        let span = tracing::debug_span!("metadb.close", db = self.label);
        let start = std::time::Instant::now();
        let res: Result<()> = match &self.inner {
            Inner::Writer(db) => db.close().instrument(span.clone()).await.map_err(|source| {
                CrabError::from(MetaDbError::Close {
                    db: String::from(self.label),
                    source,
                })
            }),
            Inner::Reader(reader) => {
                reader
                    .close()
                    .instrument(span.clone())
                    .await
                    .map_err(|source| {
                        CrabError::from(MetaDbError::Close {
                            db: String::from(self.label),
                            source,
                        })
                    })
            }
        };
        if res.is_ok() {
            if let Some(m) = self.metrics.as_ref() {
                m.inc_metadb_close_count();
            }
            let _enter = span.enter();
            tracing::debug!(
                db = self.label,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "metadb: closed slatedb instance"
            );
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;

    use super::*;

    fn stub_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    #[test]
    fn engine_config_maps_to_slatedb_settings() {
        let config = MetaDbEngineConfig {
            compaction_threshold: 12,
            wal_flush_size: 8 * 1024 * 1024,
            bloom_bits_per_key: 14,
        };

        let settings = slatedb_settings(&config).expect("valid engine config");
        let compactor = settings.compactor_options.expect("compactor options");

        assert_eq!(settings.l0_sst_size_bytes, 8 * 1024 * 1024);
        assert_eq!(
            compactor.scheduler_options.get("min_compaction_sources"),
            Some(&String::from("12"))
        );
        assert_eq!(
            compactor.scheduler_options.get("max_compaction_sources"),
            Some(&String::from("12"))
        );
        assert_eq!(filter_policies(&config).expect("filter policy").len(), 1);
    }

    #[test]
    fn engine_config_rejects_zero_tunables() {
        for config in [
            MetaDbEngineConfig {
                compaction_threshold: 0,
                ..MetaDbEngineConfig::default()
            },
            MetaDbEngineConfig {
                wal_flush_size: 0,
                ..MetaDbEngineConfig::default()
            },
            MetaDbEngineConfig {
                bloom_bits_per_key: 0,
                ..MetaDbEngineConfig::default()
            },
        ] {
            assert!(slatedb_settings(&config).is_err());
        }
    }

    #[tokio::test]
    async fn db_get_miss_returns_none() {
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "file_index_db")
            .await
            .expect("open");
        let got = db.get(b"missing").await.expect("get on empty db");
        assert!(got.is_none());
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn db_put_get_round_trip() {
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "file_index_db")
            .await
            .expect("open");

        let mut batch = slatedb::WriteBatch::new();
        batch.put(b"k1".as_slice(), b"v1".as_slice());
        db.write(batch).await.expect("write");

        let got = db.get(b"k1").await.expect("get").expect("present");
        assert_eq!(got.as_ref(), b"v1");

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn db_write_commits_multiple_keys_atomically() {
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "chunk_index_db")
            .await
            .expect("open");

        let mut batch = slatedb::WriteBatch::new();
        batch.put(b"k1".as_slice(), b"v1".as_slice());
        batch.put(b"k2".as_slice(), b"v2".as_slice());
        batch.put(b"k3".as_slice(), b"v3".as_slice());
        db.write(batch).await.expect("write");

        for (k, v) in [("k1", "v1"), ("k2", "v2"), ("k3", "v3")] {
            let got = db.get(k.as_bytes()).await.expect("get").expect("present");
            assert_eq!(got.as_ref(), v.as_bytes());
        }

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn buffered_writes_do_not_trigger_timer_wal_flushes() {
        let store = stub_store();
        let path = ObjectPath::from("t/buffered");
        let wal_prefix = ObjectPath::from("t/buffered/wal");
        let metrics = Arc::new(crate::core::metrics::Metrics::new());
        let db = Db::open_with_metrics(
            Arc::clone(&store),
            path,
            "chunk_index_db",
            Arc::clone(&metrics),
        )
        .await
        .expect("open");
        let initial_wals = store.list(Some(&wal_prefix)).count().await;

        for index in 0_u8..8 {
            let mut batch = slatedb::WriteBatch::new();
            batch.put([index].as_slice(), [index].as_slice());
            db.write_buffered(batch).await.expect("buffered write");
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        assert_eq!(store.list(Some(&wal_prefix)).count().await, initial_wals);
        db.flush_memtable().await.expect("explicit batch flush");
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.metadb_buffered_batch_write_count, 8);
        assert_eq!(snapshot.metadb_wal_flush_count, 0);
        assert_eq!(snapshot.metadb_memtable_flush_count, 1);
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn db_get_batch_preserves_order() {
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "file_index_db")
            .await
            .expect("open");

        let mut batch = slatedb::WriteBatch::new();
        for i in 0u8..5 {
            batch.put([i].as_slice(), vec![i, i, i].as_slice());
        }
        db.write(batch).await.expect("seed");

        // Query in reverse order with a miss interleaved.
        let keys: Vec<Bytes> = vec![
            Bytes::from_static(&[4]),
            Bytes::from_static(&[99]), // miss
            Bytes::from_static(&[0]),
            Bytes::from_static(&[2]),
        ];
        let got = db.get_batch(&keys).await.expect("batch");

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].as_ref().map(|b| b.as_ref()), Some(&[4u8, 4, 4][..]));
        assert!(got[1].is_none());
        assert_eq!(got[2].as_ref().map(|b| b.as_ref()), Some(&[0u8, 0, 0][..]));
        assert_eq!(got[3].as_ref().map(|b| b.as_ref()), Some(&[2u8, 2, 2][..]));

        db.close().await.expect("close");
    }

    #[test]
    fn batch_lookup_plan_deduplicates_and_selects_one_dense_scan() {
        let mut keys = (0..DENSE_BATCH_SCAN_THRESHOLD as u32)
            .rev()
            .map(|index| {
                let hash_prefix = (index * 4) as u16;
                Bytes::from(vec![
                    0x03,
                    (hash_prefix >> 8) as u8,
                    hash_prefix as u8,
                    (index >> 8) as u8,
                    index as u8,
                ])
            })
            .collect::<Vec<_>>();
        let duplicate_a = keys[7].clone();
        let duplicate_b = keys[19].clone();
        keys.push(duplicate_a.clone());
        keys.push(duplicate_b.clone());

        let plan = BatchLookupPlan::new(&keys);

        assert_eq!(plan.mode(), BatchLookupMode::Scan);
        assert_eq!(plan.unique_keys.len(), DENSE_BATCH_SCAN_THRESHOLD);
        assert_eq!(
            plan.unique_keys
                .iter()
                .find(|entry| entry.key == duplicate_a)
                .expect("first duplicate key")
                .input_indices
                .len(),
            2
        );
        assert_eq!(
            plan.unique_keys
                .iter()
                .find(|entry| entry.key == duplicate_b)
                .expect("second duplicate key")
                .input_indices
                .len(),
            2
        );
    }

    #[test]
    fn batch_lookup_plan_keeps_clustered_large_batch_on_point_reads() {
        let keys = (0..DENSE_BATCH_SCAN_THRESHOLD as u32)
            .map(|index| {
                Bytes::from(vec![
                    0x03,
                    0,
                    (index % 256) as u8,
                    (index >> 8) as u8,
                    index as u8,
                ])
            })
            .collect::<Vec<_>>();

        let plan = BatchLookupPlan::new(&keys);

        assert_eq!(plan.mode(), BatchLookupMode::Points);
        assert_eq!(plan.unique_keys.len(), DENSE_BATCH_SCAN_THRESHOLD);
    }

    #[test]
    fn dense_batch_scan_reads_ahead_without_caching_payload_blocks() {
        let options = dense_batch_scan_options();

        assert_eq!(options.read_ahead_bytes, 1024 * 1024);
        assert_eq!(options.max_fetch_tasks, 1);
        assert!(!options.cache_blocks);
    }

    #[tokio::test]
    async fn db_get_batch_restores_unsorted_duplicate_keys() {
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "file_index_db")
            .await
            .expect("open");

        let mut batch = slatedb::WriteBatch::new();
        batch.put(b"alpha".as_slice(), b"one".as_slice());
        batch.put(b"beta".as_slice(), b"two".as_slice());
        db.write(batch).await.expect("seed");

        let keys = vec![
            Bytes::from_static(b"beta"),
            Bytes::from_static(b"missing"),
            Bytes::from_static(b"alpha"),
            Bytes::from_static(b"beta"),
            Bytes::from_static(b"alpha"),
        ];
        let got = db.get_batch(&keys).await.expect("batch");

        assert_eq!(
            got,
            vec![
                Some(Bytes::from_static(b"two")),
                None,
                Some(Bytes::from_static(b"one")),
                Some(Bytes::from_static(b"two")),
                Some(Bytes::from_static(b"one")),
            ]
        );

        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn db_get_batch_empty_input_is_empty_output() {
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "chunk_index_db")
            .await
            .expect("open");
        let got = db.get_batch(&[]).await.expect("empty batch");
        assert!(got.is_empty());
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn db_label_surfaces_in_open_error_for_a_closed_store() {
        // Ensure the label threaded into `Db::open` actually lands in
        // the error payload. We use a label distinct from the usual
        // two so a mismatch would jump out in the error message.
        let db = Db::open(stub_store(), ObjectPath::from("t/db"), "file_index_db")
            .await
            .expect("open");
        db.close().await.expect("close");
        assert_eq!(db.label(), "file_index_db");
    }

    #[tokio::test]
    async fn db_open_with_metrics_bumps_open_and_close_counters() {
        let metrics = Arc::new(crate::core::metrics::Metrics::new());

        let db = Db::open_with_metrics(
            stub_store(),
            ObjectPath::from("t/db"),
            "chunk_index_db",
            Arc::clone(&metrics),
        )
        .await
        .expect("open_with_metrics");

        assert_eq!(metrics.snapshot().metadb_open_count, 1);
        assert_eq!(metrics.snapshot().metadb_close_count, 0);

        // A plain get / write against this handle should bump the
        // corresponding counters because the metrics sink is attached.
        let mut batch = slatedb::WriteBatch::new();
        batch.put(b"k".as_slice(), b"v".as_slice());
        db.write(batch).await.expect("write");
        assert_eq!(metrics.snapshot().metadb_batch_write_count, 1);
        assert_eq!(metrics.snapshot().metadb_wal_flush_count, 1);

        let _ = db.get(b"k").await.expect("get");
        assert_eq!(metrics.snapshot().metadb_get_count, 1);
        assert_eq!(metrics.snapshot().metadb_get_hits, 1);

        let _ = db.get(b"missing").await.expect("get miss");
        assert_eq!(metrics.snapshot().metadb_get_count, 2);
        assert_eq!(metrics.snapshot().metadb_get_hits, 1);

        db.close().await.expect("close");
        assert_eq!(metrics.snapshot().metadb_close_count, 1);
    }

    #[tokio::test]
    async fn db_open_readonly_reads_values_a_writer_flushed() {
        // Open a writer, put one key, flush + close. Then open a
        // read-only reader against the same path and verify the key
        // round-trips through the reader's `get`.
        let store = stub_store();
        let path = ObjectPath::from("t/ro");

        {
            let writer = Db::open(Arc::clone(&store), path.clone(), "file_index_db")
                .await
                .expect("writer open");
            let mut batch = slatedb::WriteBatch::new();
            batch.put(b"persisted".as_slice(), b"value".as_slice());
            writer.write(batch).await.expect("writer write");
            writer.flush().await.expect("writer flush");
            writer.close().await.expect("writer close");
        }

        let reader = Db::open_readonly(Arc::clone(&store), path, "file_index_db")
            .await
            .expect("reader open");
        assert!(reader.is_read_only());
        let got = reader.get(b"persisted").await.expect("reader get");
        assert_eq!(got.as_deref(), Some(b"value".as_slice()));
        reader.close().await.expect("reader close");
    }

    #[tokio::test]
    async fn db_memtable_flush_keeps_writer_live_and_values_visible() {
        let db = Db::open(
            stub_store(),
            ObjectPath::from("t/memtable_flush"),
            "chunk_index_db",
        )
        .await
        .expect("writer open");
        for key in [b"first".as_slice(), b"second".as_slice()] {
            let mut batch = slatedb::WriteBatch::new();
            batch.put(key, b"value".as_slice());
            db.write(batch).await.expect("write");
            db.flush_memtable().await.expect("flush memtable");
            assert_eq!(
                db.get(key).await.expect("get").as_deref(),
                Some(b"value".as_slice())
            );
        }
        db.close().await.expect("close");
    }

    #[tokio::test]
    async fn db_open_readonly_rejects_write() {
        // A read-only handle must fail fast on `write` with the
        // `ReadOnly` error variant rather than forwarding to SlateDB.
        // Seed the DB via a prior writer so the reader's own open
        // succeeds — `DbReader::build` needs at least one durable
        // manifest entry.
        let store = stub_store();
        let path = ObjectPath::from("t/ro_reject");
        {
            let writer = Db::open(Arc::clone(&store), path.clone(), "chunk_index_db")
                .await
                .expect("seed writer");
            let mut batch = slatedb::WriteBatch::new();
            batch.put(b"seed".as_slice(), b"seed".as_slice());
            writer.write(batch).await.expect("seed write");
            writer.flush().await.expect("seed flush");
            writer.close().await.expect("seed close");
        }

        let reader = Db::open_readonly(Arc::clone(&store), path, "chunk_index_db")
            .await
            .expect("reader open");

        let mut batch = slatedb::WriteBatch::new();
        batch.put(b"k".as_slice(), b"v".as_slice());
        let err = reader.write(batch).await.expect_err("write must reject");

        match err {
            crate::core::error::CrabError::MetaDb(MetaDbError::ReadOnly { db, op }) => {
                assert_eq!(db, "chunk_index_db");
                assert_eq!(op, "write");
            }
            other => panic!("expected MetaDbError::ReadOnly, got {other:?}"),
        }

        // Flush is explicitly a no-op on a read-only handle —
        // callers don't have to branch on mode.
        reader.flush().await.expect("flush is no-op on reader");

        reader.close().await.expect("reader close");
    }

    #[tokio::test]
    async fn db_open_readonly_does_not_fence_existing_writer() {
        // Open a writer, then open a reader against the same path,
        // then issue another write through the original writer. In
        // the old implementation the reader's open would fence the
        // writer and this second write would surface as a `Fenced`
        // error. With `DbReader`, the writer keeps running.
        let store = stub_store();
        let path = ObjectPath::from("t/no_fence");

        let writer = Db::open(Arc::clone(&store), path.clone(), "file_index_db")
            .await
            .expect("writer open");

        // Put one key so the reader has something durable to
        // checkpoint against.
        let mut batch = slatedb::WriteBatch::new();
        batch.put(b"k1".as_slice(), b"v1".as_slice());
        writer.write(batch).await.expect("first write");
        writer.flush().await.expect("first flush");

        let reader = Db::open_readonly(Arc::clone(&store), path, "file_index_db")
            .await
            .expect("reader open");

        // The writer must still accept writes after the reader is up.
        let mut batch2 = slatedb::WriteBatch::new();
        batch2.put(b"k2".as_slice(), b"v2".as_slice());
        writer
            .write(batch2)
            .await
            .expect("writer keeps accepting writes after reader open");

        reader.close().await.expect("reader close");
        writer.close().await.expect("writer close");
    }
}
