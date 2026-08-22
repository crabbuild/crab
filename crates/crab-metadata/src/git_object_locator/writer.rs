use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use slatedb::config::{
    CheckpointOptions, CheckpointScope, CompactorOptions, CompressionCodec,
    GarbageCollectorOptions, Settings, WriteOptions,
};
use tracing::{debug, warn};

use super::format::{
    LocatorMetadata, METADATA_KEY, OBJECT_FAMILY, PACK_FAMILY, StoredObjectLocation,
    decode_metadata, decode_object_key, decode_object_location, decode_pack_key,
    decode_pack_record, encode_metadata, encode_object_location, encode_pack_record, object_key,
    pack_key, validate_location_for_pack,
};
use super::{
    GitLocatorCoverage, GitObjectLocatorEntry, GitPackLocatorBinding, GitPackLocatorRecord,
    git_object_locator_path,
};
use crate::error::{MetadataError, Result};

const DB_LABEL: &str = "git_locator_db";
const MAX_BATCH_ROWS: usize = 25_000;
const MAX_BATCH_LOGICAL_BYTES: usize = 2 * 1024 * 1024;
const LOCATOR_L0_SST_BYTES: usize = 64 * 1024 * 1024;
const LOCATOR_L0_MAX_SSTS: usize = 32;
const LOCATOR_COMPACTION_TRIGGER_SSTS: usize = LOCATOR_L0_MAX_SSTS / 2;
// B-tree nodes and SlateDB bookkeeping make the in-memory row materially
// larger than its 49 encoded bytes. This upper bound decides only whether to
// start maintenance early; the hard L0 limit remains authoritative.
const ESTIMATED_OBJECT_ROW_BYTES: u128 = 128;
const FIXED_PUBLICATION_SSTS: usize = 4;
// Amortize one directory scan over a normal fan-out while bounding the number
// of superseded locator generations. This cadence is cost policy, not safety.
const GC_GENERATION_INTERVAL: u64 = 32;

/// Counts produced by one stale-locator sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocatorSweepStats {
    /// Object rows examined.
    pub object_rows_scanned: u64,
    /// Object rows deleted because their pack slot is not retained.
    pub object_rows_deleted: u64,
    /// Pack rows examined.
    pub pack_rows_scanned: u64,
    /// Pack rows deleted because their slot is not retained.
    pub pack_rows_deleted: u64,
}

/// Counts produced by one locator writer session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocatorWriteStats {
    /// Object rows submitted to SlateDB.
    pub object_rows_written: u64,
    /// Approximate logical key and value bytes submitted.
    pub logical_bytes_written: u64,
    /// Explicit durability flushes completed.
    pub flushes: u64,
    /// Whether this session durably advanced exact inventory coverage.
    pub coverage_updated: bool,
}

/// Exclusive writer for the compact Git object locator.
pub struct GitObjectLocatorWriter {
    db: slatedb::Db,
    path: String,
    store: Arc<dyn ObjectStore>,
    initial_coverage: Option<GitLocatorCoverage>,
    metadata: LocatorMetadata,
    bindings: HashMap<u64, GitPackLocatorRecord>,
    stats: LocatorWriteStats,
    writes_durable: bool,
}

impl GitObjectLocatorWriter {
    /// Open the compact locator and require its exact format metadata.
    pub async fn open(store: Arc<dyn ObjectStore>, repo_prefix: &str) -> Result<Self> {
        Self::open_with_settings(store, repo_prefix, locator_settings(true)).await
    }

    /// Open a bounded publication writer, starting compaction only under L0 pressure.
    ///
    /// `planned_object_rows` must bound the object rows the caller may submit.
    pub async fn open_for_publication(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        planned_object_rows: u64,
    ) -> Result<Self> {
        let path = git_object_locator_path(repo_prefix);
        let compact =
            locator_compaction_required(&path, Arc::clone(&store), planned_object_rows).await?;
        Self::open_with_settings(store, repo_prefix, locator_settings(compact)).await
    }

    async fn open_with_settings(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        settings: Settings,
    ) -> Result<Self> {
        let path = git_object_locator_path(repo_prefix);
        let db = slatedb::Db::builder(ObjectPath::from(path.as_str()), Arc::clone(&store))
            .with_settings(settings)
            // Publication writes each row once and stale-row cleanup performs
            // one sequential scan. Caching that scan in SlateDB's default
            // 640 MiB block/metadata cache only inflates writer RSS.
            .with_db_cache_disabled()
            .build()
            .await
            .map_err(|source| MetadataError::SlateDbOpen {
                db: DB_LABEL.to_owned(),
                path: path.clone(),
                source,
            })?;

        let open_result = Self::load_or_initialize(db, path, store).await;
        match open_result {
            Ok(writer) => Ok(writer),
            Err((db, operation)) => close_after_error(db, operation).await,
        }
    }

    async fn load_or_initialize(
        db: slatedb::Db,
        path: String,
        store: Arc<dyn ObjectStore>,
    ) -> std::result::Result<Self, (slatedb::Db, MetadataError)> {
        let value = match db.get(METADATA_KEY).await {
            Ok(value) => value,
            Err(source) => {
                return Err((
                    db,
                    MetadataError::SlateDbRead {
                        db: DB_LABEL.to_owned(),
                        source,
                    },
                ));
            }
        };

        let metadata = if let Some(value) = value {
            let Some(metadata) = decode_metadata(&value) else {
                return Err((db, corrupt("metadata", "invalid compact locator metadata")));
            };
            metadata
        } else {
            match database_is_empty(&db).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err((
                        db,
                        corrupt("metadata", "locator rows exist without format metadata"),
                    ));
                }
                Err(error) => return Err((db, error)),
            }
            let metadata = LocatorMetadata::empty();
            if let Err(error) = write_batch(
                &db,
                metadata_batch(metadata),
                "initialize compact locator metadata",
            )
            .await
            {
                return Err((db, error));
            }
            if let Err(source) = db.flush().await {
                return Err((
                    db,
                    MetadataError::SlateDbWrite {
                        db: DB_LABEL.to_owned(),
                        source,
                    },
                ));
            }
            metadata
        };

        let bindings = match load_bindings(&db).await {
            Ok(bindings) => bindings,
            Err(error) => return Err((db, error)),
        };
        Ok(Self {
            db,
            path,
            store,
            initial_coverage: metadata.coverage,
            metadata,
            bindings,
            stats: LocatorWriteStats::default(),
            writes_durable: true,
        })
    }

    /// Return the last fully published manifest inventory, if any.
    #[must_use]
    pub fn coverage(&self) -> Option<GitLocatorCoverage> {
        self.metadata.coverage
    }

    /// Return whether this session crossed the bounded garbage-collection cadence.
    #[must_use]
    pub fn maintenance_due(&self) -> bool {
        locator_gc_due(self.initial_coverage, self.metadata.coverage)
    }

    /// Return whether this binding's object rows belong to exact published coverage.
    ///
    /// Bindings created by an interrupted newer publication remain durable, but
    /// their rows must be rebuilt until this writer advances coverage.
    #[must_use]
    pub fn binding_has_covered_objects(&self, binding: GitPackLocatorBinding) -> bool {
        self.bindings.get(&binding.pack_slot) == Some(&binding.record)
            && self
                .metadata
                .coverage
                .is_some_and(|coverage| binding.record.committed_generation <= coverage.generation)
    }

    /// Durably bind immutable packs to monotonic non-zero slots.
    pub async fn bind_packs(
        &mut self,
        packs: &[GitPackLocatorRecord],
    ) -> Result<Vec<GitPackLocatorBinding>> {
        let by_pack_id: HashMap<_, _> = self
            .bindings
            .iter()
            .map(|(slot, record)| (record.pack_id, (*slot, *record)))
            .collect();
        let mut seen = HashSet::with_capacity(packs.len());
        let mut results = Vec::with_capacity(packs.len());
        let mut additions = Vec::new();
        let mut next_pack_slot = self.metadata.next_pack_slot;

        for pack in packs {
            validate_pack_record(*pack)?;
            if !seen.insert(pack.pack_id) {
                return Err(MetadataError::Internal(
                    "Git locator pack binding request contains a duplicate pack".to_owned(),
                ));
            }
            if let Some((pack_slot, existing)) = by_pack_id.get(&pack.pack_id) {
                if existing.object_count != pack.object_count
                    || existing.pack_size != pack.pack_size
                {
                    return Err(MetadataError::Internal(
                        "Git locator pack identity is already bound to different physical facts"
                            .to_owned(),
                    ));
                }
                results.push(GitPackLocatorBinding {
                    pack_slot: *pack_slot,
                    record: *existing,
                });
                continue;
            }
            let pack_slot = next_pack_slot;
            next_pack_slot = next_pack_slot.checked_add(1).ok_or_else(|| {
                MetadataError::Internal("Git locator pack slots are exhausted".to_owned())
            })?;
            let binding = GitPackLocatorBinding {
                pack_slot,
                record: *pack,
            };
            additions.push(binding);
            results.push(binding);
        }

        if additions.is_empty() {
            return Ok(results);
        }

        let new_metadata = LocatorMetadata {
            next_pack_slot,
            coverage: self.metadata.coverage,
        };
        let mut batch = metadata_batch(new_metadata);
        for binding in &additions {
            let key = pack_key(binding.pack_slot).ok_or_else(|| {
                MetadataError::Internal("Git locator allocated pack slot zero".to_owned())
            })?;
            batch.put(key, encode_pack_record(binding.record));
        }
        write_batch(&self.db, batch, "bind compact locator packs").await?;
        flush(&self.db).await?;

        self.metadata = new_metadata;
        self.stats.flushes = self.stats.flushes.saturating_add(1);
        self.writes_durable = true;
        for binding in additions {
            self.bindings.insert(binding.pack_slot, binding.record);
        }
        Ok(results)
    }

    /// Write bounded current object rows for one previously bound pack.
    pub async fn write_locations(
        &mut self,
        binding: GitPackLocatorBinding,
        entries: &[GitObjectLocatorEntry],
    ) -> Result<()> {
        if self.bindings.get(&binding.pack_slot) != Some(&binding.record) {
            return Err(MetadataError::Internal(
                "Git locator object rows reference an unbound pack slot".to_owned(),
            ));
        }

        let mut batch = slatedb::WriteBatch::new();
        let mut batch_rows = 0_usize;
        let mut batch_bytes = 0_usize;
        for entry in entries {
            if !validate_location_for_pack(entry.location, binding.record.pack_size) {
                return Err(MetadataError::Internal(
                    "Git locator object range falls outside its bound pack".to_owned(),
                ));
            }
            let value = encode_object_location(StoredObjectLocation {
                pack_slot: binding.pack_slot,
                pack_offset: entry.location.pack_offset,
                entry_len: entry.location.entry_len,
                crc32: entry.location.crc32,
            });
            batch.put(object_key(&entry.oid), value);
            batch_rows += 1;
            batch_bytes += object_key(&entry.oid).len() + value.len();
            if batch_rows >= MAX_BATCH_ROWS || batch_bytes >= MAX_BATCH_LOGICAL_BYTES {
                write_batch(&self.db, batch, "write compact locator objects").await?;
                self.record_object_batch(batch_rows, batch_bytes);
                batch = slatedb::WriteBatch::new();
                batch_rows = 0;
                batch_bytes = 0;
            }
        }
        if batch_rows != 0 {
            write_batch(&self.db, batch, "write compact locator objects").await?;
            self.record_object_batch(batch_rows, batch_bytes);
        }
        Ok(())
    }

    /// Make all submitted object rows durable in object storage.
    pub async fn flush_objects(&mut self) -> Result<()> {
        if self.writes_durable {
            return Ok(());
        }
        flush(&self.db).await?;
        self.stats.flushes = self.stats.flushes.saturating_add(1);
        self.writes_durable = true;
        Ok(())
    }

    /// Delete object and pack rows whose slots are absent from canonical inventory.
    pub async fn sweep_unreferenced(
        &mut self,
        retained_slots: &HashSet<u64>,
    ) -> Result<LocatorSweepStats> {
        if retained_slots
            .iter()
            .any(|slot| *slot == 0 || !self.bindings.contains_key(slot))
        {
            return Err(MetadataError::Internal(
                "Git locator sweep retained an invalid pack slot".to_owned(),
            ));
        }
        if self
            .bindings
            .keys()
            .all(|slot| retained_slots.contains(slot))
        {
            return Ok(LocatorSweepStats::default());
        }

        let mut stats = LocatorSweepStats::default();
        let mut object_rows = self
            .db
            .scan_prefix([OBJECT_FAMILY], ..)
            .await
            .map_err(read_error)?;
        let mut deletes = slatedb::WriteBatch::new();
        let mut delete_count = 0_usize;
        while let Some(row) = object_rows.next().await.map_err(read_error)? {
            stats.object_rows_scanned = stats.object_rows_scanned.saturating_add(1);
            decode_object_key(&row.key)
                .ok_or_else(|| corrupt("object", "invalid compact locator object key"))?;
            let location = decode_object_location(&row.value)
                .ok_or_else(|| corrupt("object", "invalid compact locator object location"))?;
            if !retained_slots.contains(&location.pack_slot) {
                deletes.delete(row.key);
                delete_count += 1;
                stats.object_rows_deleted = stats.object_rows_deleted.saturating_add(1);
                if delete_count == MAX_BATCH_ROWS {
                    write_batch(&self.db, deletes, "sweep compact locator objects").await?;
                    self.writes_durable = false;
                    deletes = slatedb::WriteBatch::new();
                    delete_count = 0;
                }
            }
        }
        if delete_count != 0 {
            write_batch(&self.db, deletes, "sweep compact locator objects").await?;
            self.writes_durable = false;
        }

        let mut pack_rows = self
            .db
            .scan_prefix([PACK_FAMILY], ..)
            .await
            .map_err(read_error)?;
        let mut deletes = slatedb::WriteBatch::new();
        let mut delete_count = 0_usize;
        let mut removed_slots = Vec::new();
        while let Some(row) = pack_rows.next().await.map_err(read_error)? {
            stats.pack_rows_scanned = stats.pack_rows_scanned.saturating_add(1);
            let slot = decode_pack_key(&row.key)
                .ok_or_else(|| corrupt("pack", "invalid compact locator pack key"))?;
            if !retained_slots.contains(&slot) {
                deletes.delete(row.key);
                delete_count += 1;
                removed_slots.push(slot);
                stats.pack_rows_deleted = stats.pack_rows_deleted.saturating_add(1);
                if delete_count == MAX_BATCH_ROWS {
                    write_batch(&self.db, deletes, "sweep compact locator packs").await?;
                    self.writes_durable = false;
                    deletes = slatedb::WriteBatch::new();
                    delete_count = 0;
                }
            }
        }
        if delete_count != 0 {
            write_batch(&self.db, deletes, "sweep compact locator packs").await?;
            self.writes_durable = false;
        }
        self.flush_objects().await?;
        for slot in removed_slots {
            self.bindings.remove(&slot);
        }
        Ok(stats)
    }

    /// Mark one immutable manifest inventory fully covered after object flush.
    pub async fn set_coverage(&mut self, coverage: GitLocatorCoverage) -> Result<()> {
        if coverage.generation == 0 {
            return Err(MetadataError::Internal(
                "Git locator coverage generation must be non-zero".to_owned(),
            ));
        }
        self.flush_objects().await?;
        let metadata = LocatorMetadata {
            next_pack_slot: self.metadata.next_pack_slot,
            coverage: Some(coverage),
        };
        write_batch(
            &self.db,
            metadata_batch(metadata),
            "write compact locator coverage",
        )
        .await?;
        self.writes_durable = false;
        self.flush_objects().await?;
        self.metadata = metadata;
        self.stats.coverage_updated = true;
        Ok(())
    }

    /// Publish a read-only checkpoint without closing this writer.
    ///
    /// All submitted rows and coverage are flushed before the checkpoint. A
    /// long-lived exclusive owner can then serve multiple manifest generations
    /// from one SlateDB session while readers open only immutable checkpoints.
    pub async fn publish_checkpoint(&mut self) -> Result<()> {
        let checkpoint = self
            .db
            .create_checkpoint(
                CheckpointScope::All,
                &CheckpointOptions {
                    name: Some(super::READER_CHECKPOINT_NAME.to_owned()),
                    ..CheckpointOptions::default()
                },
            )
            .await
            .map_err(|source| MetadataError::SlateDbWrite {
                db: DB_LABEL.to_owned(),
                source,
            })?;
        self.stats.flushes = self.stats.flushes.saturating_add(1);
        remove_old_reader_checkpoints(&self.path, Arc::clone(&self.store), &checkpoint).await
    }

    /// Flush and close the SlateDB writer.
    pub async fn close(mut self) -> Result<LocatorWriteStats> {
        if let Err(operation) = self.publish_checkpoint().await {
            return close_after_error(self.db, operation).await;
        }
        let Self {
            db,
            path,
            store,
            initial_coverage,
            metadata,
            stats,
            ..
        } = self;
        let collect_garbage = locator_gc_due(initial_coverage, metadata.coverage);
        if let Err(source) = db.close().await {
            return Err(MetadataError::SlateDbClose {
                db: DB_LABEL.to_owned(),
                source,
            });
        }
        if collect_garbage {
            match run_locator_gc(&path, store).await {
                Ok(()) => debug!("Git locator garbage collection completed"),
                Err(error) => warn!(%error, "Git locator garbage collection requires retry"),
            }
        }
        Ok(stats)
    }

    fn record_object_batch(&mut self, rows: usize, bytes: usize) {
        self.writes_durable = false;
        self.stats.object_rows_written = self.stats.object_rows_written.saturating_add(rows as u64);
        self.stats.logical_bytes_written = self
            .stats
            .logical_bytes_written
            .saturating_add(bytes as u64);
    }
}

async fn remove_old_reader_checkpoints(
    path: &str,
    store: Arc<dyn ObjectStore>,
    current: &slatedb::CheckpointCreateResult,
) -> Result<()> {
    let admin = slatedb::admin::AdminBuilder::new(ObjectPath::from(path), store).build();
    let checkpoints = admin
        .list_checkpoints(Some(super::READER_CHECKPOINT_NAME))
        .await
        .map_err(|source| MetadataError::SlateDbWrite {
            db: DB_LABEL.to_owned(),
            source,
        })?;
    for checkpoint in checkpoints {
        if checkpoint.id == current.id {
            continue;
        }
        admin
            .delete_checkpoint(checkpoint.id)
            .await
            .map_err(|source| MetadataError::SlateDbWrite {
                db: DB_LABEL.to_owned(),
                source,
            })?;
    }
    Ok(())
}

fn locator_gc_due(
    initial: Option<GitLocatorCoverage>,
    published: Option<GitLocatorCoverage>,
) -> bool {
    let initial_band = initial
        .map(|coverage| coverage.generation / GC_GENERATION_INTERVAL)
        .unwrap_or(0);
    published.is_some_and(|coverage| coverage.generation / GC_GENERATION_INTERVAL > initial_band)
}

async fn run_locator_gc(path: &str, store: Arc<dyn ObjectStore>) -> Result<()> {
    let admin = slatedb::admin::AdminBuilder::new(ObjectPath::from(path), store).build();
    admin
        .run_gc_once(locator_gc_options())
        .await
        .map_err(|source| MetadataError::SlateDbWrite {
            db: DB_LABEL.to_owned(),
            source,
        })
}

fn locator_gc_options() -> GarbageCollectorOptions {
    GarbageCollectorOptions {
        // Locator WALs contain only a permanent fencing object; there are no
        // clone parents. Collect only directories that publication supersedes.
        wal_options: None,
        wal_fence_options: None,
        detach_options: None,
        ..GarbageCollectorOptions::default()
    }
}

async fn locator_compaction_required(
    path: &str,
    store: Arc<dyn ObjectStore>,
    planned_object_rows: u64,
) -> Result<bool> {
    let admin = slatedb::admin::AdminBuilder::new(ObjectPath::from(path), store).build();
    let manifest =
        admin
            .read_manifest(None)
            .await
            .map_err(|source| MetadataError::SlateDbOpen {
                db: DB_LABEL.to_owned(),
                path: path.to_owned(),
                source,
            })?;
    let l0_ssts = manifest.map_or(0, |manifest| manifest.l0().len());
    Ok(should_start_compactor(l0_ssts, planned_object_rows))
}

fn should_start_compactor(l0_ssts: usize, planned_object_rows: u64) -> bool {
    let object_bytes = u128::from(planned_object_rows).saturating_mul(ESTIMATED_OBJECT_ROW_BYTES);
    let object_ssts = object_bytes.saturating_add(LOCATOR_L0_SST_BYTES as u128 - 1)
        / LOCATOR_L0_SST_BYTES as u128;
    let planned_ssts = object_ssts
        .saturating_add(FIXED_PUBLICATION_SSTS as u128)
        .min(usize::MAX as u128) as usize;
    l0_ssts.saturating_add(planned_ssts) >= LOCATOR_COMPACTION_TRIGGER_SSTS
}

fn locator_settings(compact: bool) -> Settings {
    let compactor_options = compact.then(|| {
        let mut compactor = CompactorOptions {
            commit_compacted_interval: std::time::Duration::from_millis(500),
            ..CompactorOptions::default()
        };
        if let Some(worker) = &mut compactor.worker {
            worker.compactions_poll_interval = std::time::Duration::from_millis(500);
        }
        compactor
    });
    Settings {
        flush_interval: None,
        wal_enabled: false,
        // SlateDB excludes the active memtable from max_unflushed_bytes.
        // Keep both limits small so B-tree and flush-encoding amplification
        // cannot turn a compact locator rebuild into multi-GiB process RSS.
        l0_sst_size_bytes: LOCATOR_L0_SST_BYTES,
        max_unflushed_bytes: 96 * 1024 * 1024,
        // Publication reads the persisted L0 count under the locator lock and
        // starts compaction before reaching half this ceiling. The remaining
        // headroom covers one conservatively estimated publication.
        l0_max_ssts: LOCATOR_L0_MAX_SSTS,
        l0_max_ssts_per_key: LOCATOR_L0_MAX_SSTS,
        l0_flush_parallelism: 1,
        // SlateDB tickers fire immediately. Starting these tasks only under
        // measured L0 pressure amortizes their fixed object-store request cost.
        compactor_options,
        // SlateDB tickers fire immediately, so default background GC runs its
        // full directory scan on every short-lived publication session.
        garbage_collector_options: None,
        compression_codec: Some(CompressionCodec::Zstd),
        ..Settings::default()
    }
}

fn non_durable_write_options() -> WriteOptions {
    WriteOptions {
        await_durable: false,
        ..WriteOptions::default()
    }
}

fn metadata_batch(metadata: LocatorMetadata) -> slatedb::WriteBatch {
    let mut batch = slatedb::WriteBatch::new();
    batch.put(METADATA_KEY, encode_metadata(metadata));
    batch
}

async fn write_batch(
    db: &slatedb::Db,
    batch: slatedb::WriteBatch,
    _operation: &'static str,
) -> Result<()> {
    db.write_with_options(batch, &non_durable_write_options())
        .await
        .map(|_| ())
        .map_err(|source| MetadataError::SlateDbWrite {
            db: DB_LABEL.to_owned(),
            source,
        })
}

async fn flush(db: &slatedb::Db) -> Result<()> {
    db.flush()
        .await
        .map_err(|source| MetadataError::SlateDbWrite {
            db: DB_LABEL.to_owned(),
            source,
        })
}

async fn database_is_empty(db: &slatedb::Db) -> Result<bool> {
    let mut rows = db.scan(..).await.map_err(read_error)?;
    rows.next()
        .await
        .map(|row| row.is_none())
        .map_err(read_error)
}

async fn load_bindings(db: &slatedb::Db) -> Result<HashMap<u64, GitPackLocatorRecord>> {
    let mut rows = db
        .scan_prefix([PACK_FAMILY], ..)
        .await
        .map_err(read_error)?;
    let mut bindings = HashMap::new();
    let mut pack_ids = HashSet::new();
    while let Some(row) = rows.next().await.map_err(read_error)? {
        let slot = decode_pack_key(&row.key)
            .ok_or_else(|| corrupt("pack", "invalid compact locator pack key"))?;
        let record = decode_pack_record(&row.value)
            .ok_or_else(|| corrupt("pack", "invalid compact locator pack record"))?;
        if bindings.insert(slot, record).is_some() || !pack_ids.insert(record.pack_id) {
            return Err(corrupt(
                "pack",
                "duplicate compact locator pack slot or identity",
            ));
        }
    }
    Ok(bindings)
}

fn validate_pack_record(pack: GitPackLocatorRecord) -> Result<()> {
    if decode_pack_record(&encode_pack_record(pack)) != Some(pack) {
        return Err(MetadataError::Internal(
            "Git locator pack record is incomplete".to_owned(),
        ));
    }
    Ok(())
}

fn read_error(source: slatedb::Error) -> MetadataError {
    MetadataError::SlateDbRead {
        db: DB_LABEL.to_owned(),
        source,
    }
}

fn corrupt(path: &str, reason: &str) -> MetadataError {
    MetadataError::CorruptObject {
        path: format!("{DB_LABEL}:{path}"),
        reason: reason.to_owned(),
    }
}

async fn close_after_error<T>(db: slatedb::Db, operation: MetadataError) -> Result<T> {
    match db.close().await {
        Ok(()) => Err(operation),
        Err(close) => Err(MetadataError::SlateDbOperationAndClose {
            db: DB_LABEL.to_owned(),
            operation: Box::new(operation),
            close,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_util::TryStreamExt;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;

    use super::*;
    use crate::git_object_locator::GitObjectLocation;
    use crab_xet::hash::MerkleHash;

    fn hash(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed + 1, seed + 2, seed + 3])
    }

    fn pack(seed: u64) -> GitPackLocatorRecord {
        GitPackLocatorRecord {
            pack_id: hash(seed),
            committed_generation: seed,
            pack_index_hash: hash(seed + 10),
            object_count: 1,
            pack_size: 128,
        }
    }

    fn entry(seed: u8) -> GitObjectLocatorEntry {
        GitObjectLocatorEntry {
            oid: [seed; 20],
            location: GitObjectLocation {
                pack_offset: 12,
                entry_len: 96,
                crc32: u32::from(seed),
            },
        }
    }

    #[test]
    fn locator_settings_bound_writer_memory() {
        let settings = locator_settings(true);
        assert_eq!(settings.l0_sst_size_bytes, 64 * 1024 * 1024);
        assert_eq!(settings.max_unflushed_bytes, 96 * 1024 * 1024);
        assert_eq!(settings.l0_max_ssts, LOCATOR_L0_MAX_SSTS);
        assert_eq!(settings.l0_max_ssts_per_key, LOCATOR_L0_MAX_SSTS);
        assert_eq!(settings.l0_flush_parallelism, 1);
        assert!(settings.garbage_collector_options.is_none());
        let compactor = settings
            .compactor_options
            .expect("locator compactor enabled");
        assert_eq!(
            compactor.poll_interval,
            CompactorOptions::default().poll_interval
        );
        assert_eq!(
            compactor.commit_compacted_interval,
            std::time::Duration::from_millis(500)
        );
        assert_eq!(
            compactor
                .worker
                .expect("locator compaction worker")
                .compactions_poll_interval,
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn locator_publication_starts_compaction_before_bounded_l0_headroom_is_consumed() {
        assert!(!should_start_compactor(0, 0));
        assert!(!should_start_compactor(
            LOCATOR_COMPACTION_TRIGGER_SSTS - FIXED_PUBLICATION_SSTS - 1,
            0,
        ));
        assert!(should_start_compactor(
            LOCATOR_COMPACTION_TRIGGER_SSTS - FIXED_PUBLICATION_SSTS,
            0,
        ));
        assert!(should_start_compactor(0, u64::MAX));
        assert!(locator_settings(false).compactor_options.is_none());
    }

    #[test]
    fn locator_gc_is_due_only_when_exact_coverage_crosses_a_generation_band() {
        let coverage = |generation| {
            Some(GitLocatorCoverage {
                generation,
                pack_index_hash: hash(generation),
            })
        };

        assert!(!locator_gc_due(None, None));
        assert!(!locator_gc_due(None, coverage(1)));
        assert!(!locator_gc_due(coverage(1), coverage(31)));
        assert!(locator_gc_due(coverage(31), coverage(32)));
        assert!(!locator_gc_due(coverage(32), coverage(63)));
        assert!(locator_gc_due(coverage(32), coverage(64)));
        assert!(!locator_gc_due(coverage(64), coverage(64)));
    }

    #[test]
    fn locator_gc_collects_only_superseded_publication_state() {
        let options = locator_gc_options();

        assert!(options.manifest_options.is_some());
        assert!(options.compacted_options.is_some());
        assert!(options.compactions_options.is_some());
        assert!(options.wal_options.is_none());
        assert!(options.wal_fence_options.is_none());
        assert!(options.detach_options.is_none());
    }

    #[tokio::test]
    async fn pack_slot_is_durable_before_any_object_row_can_reference_it() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let bindings = writer.bind_packs(&[pack(1)]).await.expect("bind pack");
        assert_eq!(bindings[0].pack_slot, 1);
        writer.close().await.expect("close writer");

        let mut reopened = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("reopen writer");
        let bindings = reopened.bind_packs(&[pack(2)]).await.expect("bind second");
        assert_eq!(bindings[0].pack_slot, 2);
        reopened.close().await.expect("close reopened writer");
    }

    #[tokio::test]
    async fn retained_pack_reuses_slot_without_rebinding_commit_evidence() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let original = pack(1);
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let original_binding = writer.bind_packs(&[original]).await.expect("bind pack")[0];
        writer.close().await.expect("close writer");

        let retained = GitPackLocatorRecord {
            committed_generation: 9,
            pack_index_hash: hash(99),
            ..original
        };
        let mut reopened = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("reopen writer");
        let retained_binding = reopened
            .bind_packs(&[retained])
            .await
            .expect("reuse retained pack")[0];

        assert_eq!(retained_binding, original_binding);
        reopened.close().await.expect("close reopened writer");
    }

    #[tokio::test]
    async fn published_coverage_distinguishes_retained_rows_from_interrupted_bindings() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut initial = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open initial writer");
        let covered = initial.bind_packs(&[pack(1)]).await.expect("bind covered")[0];
        initial
            .write_locations(covered, &[entry(1)])
            .await
            .expect("write covered location");
        initial
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: hash(100),
            })
            .await
            .expect("set initial coverage");
        assert!(initial.binding_has_covered_objects(covered));
        initial.close().await.expect("close initial writer");

        let mut interrupted = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open interrupted writer");
        let retained = interrupted
            .bind_packs(&[pack(1)])
            .await
            .expect("bind retained")[0];
        let uncovered = interrupted
            .bind_packs(&[pack(2)])
            .await
            .expect("bind uncovered")[0];
        assert!(interrupted.binding_has_covered_objects(retained));
        assert!(!interrupted.binding_has_covered_objects(uncovered));
        interrupted.close().await.expect("close interrupted writer");

        let reopened = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("reopen writer");
        assert!(reopened.binding_has_covered_objects(retained));
        assert!(!reopened.binding_has_covered_objects(uncovered));
        reopened.close().await.expect("close reopened writer");
    }

    #[tokio::test]
    async fn checkpoints_publish_multiple_generations_without_closing_writer() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let first = writer.bind_packs(&[pack(1)]).await.expect("bind first")[0];
        writer
            .write_locations(first, &[entry(1)])
            .await
            .expect("write first");
        let first_coverage = GitLocatorCoverage {
            generation: 1,
            pack_index_hash: hash(100),
        };
        writer
            .set_coverage(first_coverage)
            .await
            .expect("cover first generation");
        writer
            .publish_checkpoint()
            .await
            .expect("publish first checkpoint");

        let first_reader =
            super::super::GitObjectLocatorSession::open(Arc::clone(&store), "org/repo")
                .await
                .expect("open first reader");
        assert_eq!(first_reader.coverage(), Some(first_coverage));
        first_reader.close().await.expect("close first reader");

        let second = writer.bind_packs(&[pack(2)]).await.expect("bind second")[0];
        writer
            .write_locations(second, &[entry(2)])
            .await
            .expect("write second");
        let second_coverage = GitLocatorCoverage {
            generation: 2,
            pack_index_hash: hash(200),
        };
        writer
            .set_coverage(second_coverage)
            .await
            .expect("cover second generation");
        writer
            .publish_checkpoint()
            .await
            .expect("publish second checkpoint");
        assert!(writer.binding_has_covered_objects(first));
        assert!(writer.binding_has_covered_objects(second));

        let second_reader =
            super::super::GitObjectLocatorSession::open(Arc::clone(&store), "org/repo")
                .await
                .expect("open second reader");
        assert_eq!(second_reader.coverage(), Some(second_coverage));
        second_reader.close().await.expect("close second reader");
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn locator_store_writes_only_zero_byte_fence_wal() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack(1)]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &[entry(1)])
            .await
            .expect("write location");
        writer.flush_objects().await.expect("flush objects");
        writer.close().await.expect("close writer");

        let prefix = ObjectPath::from("org/repo/git_locator_db");
        let objects = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list locator objects");
        let wal_objects: Vec<_> = objects
            .iter()
            .filter(|meta| meta.location.as_ref().contains("/wal/"))
            .collect();
        assert_eq!(wal_objects.len(), 1, "locator objects: {objects:#?}");
        assert_eq!(wal_objects[0].size, 0);
    }

    #[tokio::test]
    async fn interrupted_rows_do_not_advance_coverage_and_sweep_removes_stale_slot() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let bindings = writer
            .bind_packs(&[pack(1), pack(2)])
            .await
            .expect("bind packs");
        writer
            .write_locations(bindings[0], &[entry(1)])
            .await
            .expect("write first");
        writer
            .write_locations(bindings[1], &[entry(2)])
            .await
            .expect("write second");
        assert_eq!(writer.coverage(), None);

        let stats = writer
            .sweep_unreferenced(&HashSet::from([bindings[1].pack_slot]))
            .await
            .expect("sweep stale slot");
        assert_eq!(stats.object_rows_deleted, 1);
        assert_eq!(stats.pack_rows_deleted, 1);
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn sweep_skips_object_scan_when_every_binding_is_retained() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let bindings = writer
            .bind_packs(&[pack(1), pack(2)])
            .await
            .expect("bind packs");
        writer
            .write_locations(bindings[0], &[entry(1)])
            .await
            .expect("write first");
        writer
            .write_locations(bindings[1], &[entry(2)])
            .await
            .expect("write second");

        let stats = writer
            .sweep_unreferenced(&HashSet::from([
                bindings[0].pack_slot,
                bindings[1].pack_slot,
            ]))
            .await
            .expect("skip retained sweep");

        assert_eq!(stats, LocatorSweepStats::default());
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn coverage_is_flushed_after_objects_and_survives_reopen() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let coverage = GitLocatorCoverage {
            generation: 3,
            pack_index_hash: hash(9),
        };
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack(1)]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &[entry(1)])
            .await
            .expect("write location");
        writer.set_coverage(coverage).await.expect("set coverage");
        writer.close().await.expect("close writer");

        let reopened = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("reopen writer");
        assert_eq!(reopened.coverage(), Some(coverage));
        reopened.close().await.expect("close reopened writer");
    }

    #[tokio::test]
    async fn clean_object_flush_is_not_repeated_before_coverage() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack(1)]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &[entry(1)])
            .await
            .expect("write location");
        writer.flush_objects().await.expect("flush objects");
        let durable_flushes = writer.stats.flushes;

        writer.flush_objects().await.expect("repeat clean flush");
        assert_eq!(writer.stats.flushes, durable_flushes);
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 3,
                pack_index_hash: hash(9),
            })
            .await
            .expect("publish coverage");
        assert_eq!(writer.stats.flushes, durable_flushes + 1);
        writer.close().await.expect("close writer");
    }
}
