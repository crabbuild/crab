use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use slatedb::config::{
    CheckpointOptions, CheckpointScope, CompactorOptions, CompressionCodec,
    GarbageCollectorOptions, ScanOptions, Settings, WriteOptions,
};
use tracing::{debug, warn};

use super::format::{
    LocatorMetadata, METADATA_KEY, OBJECT_FAMILY, ORDINAL_METADATA_FAMILY, PACK_BINDINGS_KEY,
    PACK_FAMILY, PACK_OBJECT_FAMILY, PACK_OBJECT_INDEX_MARKER_KEY, PACK_OBJECT_INDEX_MARKER_VALUE,
    PACK_OBJECT_INDEX_REBUILDING_VALUE, PACK_OBJECT_VALUE_LEN, StoredObjectLocation, coverage,
    decode_metadata, decode_object_key, decode_object_location, decode_pack_bindings,
    decode_pack_key, decode_pack_object_key, decode_pack_object_ordinal, decode_pack_record,
    encode_metadata, encode_object_location, encode_object_metadata, encode_pack_bindings,
    encode_pack_object_ordinal, encode_pack_record, object_key, ordinal_key, ordinal_metadata_key,
    pack_key, pack_object_key, pack_object_prefix, validate_location_for_pack,
};
use super::{
    GitLocatorCoverage, GitObjectCatalogIdentity, GitObjectCatalogStats, GitObjectLocatorEntry,
    GitObjectMetadata, GitObjectOrdinal, GitPackLocatorBinding, GitPackLocatorRecord,
    git_object_locator_path,
};
use crate::error::{MetadataError, Result};

const DB_LABEL: &str = "git_object_catalog_db";
const MAX_BATCH_ROWS: usize = 25_000;
const MAX_BATCH_LOGICAL_BYTES: usize = 2 * 1024 * 1024;
const LOCATOR_L0_SST_BYTES: usize = 64 * 1024 * 1024;
const LOCATOR_L0_MAX_SSTS: usize = 32;
const LOCATOR_COMPACTION_TRIGGER_SSTS: usize = LOCATOR_L0_MAX_SSTS / 2;
const LOCATOR_COMPACTION_MAX_CONCURRENT: usize = 1;
const LOCATOR_COMPACTION_MAX_SUBCOMPACTIONS: usize = 1;
const LOCATOR_COMPACTION_MAX_FETCH_TASKS: usize = 4;
const LOCATOR_SCAN_READ_AHEAD_BYTES: usize = 16 * 1024 * 1024;
const LOCATOR_SCAN_FETCH_TASKS: usize = 8;
const LOCATOR_EXISTING_LOOKUP_CONCURRENCY: usize = 256;
// Each object now contributes an OID row, reverse-ordinal row, and metadata
// sidecar row. B-tree nodes and SlateDB bookkeeping make the in-memory rows
// materially larger than their 147 encoded bytes. This bound only starts
// maintenance early; the hard L0 limit remains authoritative.
const ESTIMATED_OBJECT_ROW_BYTES: u128 = 192;
const FIXED_PUBLICATION_SSTS: usize = 4;
// Amortize one directory scan over a normal fan-out while bounding the number
// of superseded locator generations. This cadence is cost policy, not safety.
const GC_GENERATION_INTERVAL: u64 = 32;
// A catalog scan is cheaper than one remote point lookup per object once a
// batch covers a meaningful fraction of the current ordinal universe. Keep
// small incremental pushes on the point-read path.
const BULK_ORDINAL_LOOKUP_FACTOR: u64 = 64;
// Small catalogs have fewer rows than the fixed SST/read-ahead overhead of a
// full scan. Keep their incremental writes on bounded OID point lookups.
const BULK_ORDINAL_LOOKUP_MIN_CATALOG_OBJECTS: u64 = 4_096;
// A point lookup can touch one index/data block in every active SST. When a
// publication covers enough of the catalog, one ordered scan is cheaper and
// keeps incremental repair from multiplying object-store reads by SST fan-out.
const EXISTING_LOOKUP_SCAN_MIN_SSTS: u64 = 4;
const EXISTING_LOOKUP_SCAN_CATALOG_COST_DIVISOR: u64 = 2;
const EXISTING_LOOKUP_SCAN_SMALL_CATALOG_MAX_OBJECTS: u64 = 4_096;
const EXISTING_LOOKUP_SCAN_SMALL_CATALOG_RATIO: u64 = 4;
const RETIRED_CHECKPOINT_LIFETIME: std::time::Duration =
    std::time::Duration::from_secs(2 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExistingObject {
    ordinal: GitObjectOrdinal,
    pack_slot: u64,
    metadata: GitObjectMetadata,
}

// Existing rows are read-mostly during repack. Keep the repository-sized base
// compact and mutate it in place; overlays hold only new OIDs and deletions.
#[derive(Debug, Default)]
struct ExistingOrdinalIndex {
    base: Vec<([u8; 20], ExistingObject)>,
    updates: HashMap<[u8; 20], ExistingObject>,
    removed: HashSet<[u8; 20]>,
}

impl ExistingOrdinalIndex {
    fn from_sorted_entries(entries: Vec<([u8; 20], ExistingObject)>) -> Self {
        Self {
            base: entries,
            updates: HashMap::new(),
            removed: HashSet::new(),
        }
    }

    fn get(&self, oid: &[u8; 20]) -> Option<ExistingObject> {
        if let Some(object) = self.updates.get(oid) {
            return Some(*object);
        }
        if self.removed.contains(oid) {
            return None;
        }
        self.base
            .binary_search_by_key(oid, |(existing, _)| *existing)
            .ok()
            .map(|index| self.base[index].1)
    }

    fn insert(&mut self, oid: [u8; 20], object: ExistingObject) {
        self.removed.remove(&oid);
        if let Ok(index) = self
            .base
            .binary_search_by_key(&oid, |(existing, _)| *existing)
        {
            self.base[index].1 = object;
        } else {
            self.updates.insert(oid, object);
        }
    }

    fn remove(&mut self, oid: &[u8; 20]) {
        self.updates.remove(oid);
        if self
            .base
            .binary_search_by_key(oid, |(existing, _)| *existing)
            .is_ok()
        {
            self.removed.insert(*oid);
        }
    }

    fn len(&self) -> usize {
        self.base
            .len()
            .saturating_add(self.updates.len())
            .saturating_sub(self.removed.len())
    }
}

/// Counts produced by one stale-locator sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
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
    /// Explicit object/memtable flushes completed.
    pub flushes: u64,
    /// Whether this session durably advanced exact inventory coverage.
    pub coverage_updated: bool,
}

/// Exclusive writer for the compact Git object locator.
pub struct GitObjectLocatorWriter {
    db: slatedb::Db,
    path: String,
    repo_prefix: String,
    store: Arc<dyn ObjectStore>,
    initial_coverage: Option<GitLocatorCoverage>,
    metadata: LocatorMetadata,
    bindings: HashMap<u64, GitPackLocatorRecord>,
    empty_catalog_binding: Option<u64>,
    replacement_ordinals: Option<HashMap<[u8; 20], ExistingObject>>,
    existing_ordinals: Option<ExistingOrdinalIndex>,
    ordinal_lookup_candidates: u64,
    pack_membership_index_ready: bool,
    rebuild_remaining_rows: Option<HashMap<u64, u64>>,
    catalog_dirty: bool,
    checkpoint_required: bool,
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
    /// `planned_object_rows` must bound the object rows the caller may submit;
    /// large plans also select the in-memory existing-ordinal lookup before
    /// the first pack is written.
    pub async fn open_for_publication(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        planned_object_rows: u64,
    ) -> Result<Self> {
        let path = git_object_locator_path(repo_prefix);
        let compact =
            locator_compaction_required(&path, Arc::clone(&store), planned_object_rows).await?;
        let mut writer =
            Self::open_with_settings(store, repo_prefix, locator_settings(compact)).await?;
        // The owner already knows the total uncovered inventory it will write.
        // Seed the lookup policy before the first pack so a large rebind does
        // not spend its initial batches on one remote point read per object.
        writer.ordinal_lookup_candidates = planned_object_rows;
        Ok(writer)
    }

    /// Open a bounded publication writer without starting a compactor.
    ///
    /// Latency-sensitive readers use this for a small coverage repair. The
    /// repository owner remains responsible for geometric locator maintenance.
    pub async fn open_for_incremental_publication(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
    ) -> Result<Self> {
        Self::open_with_settings(store, repo_prefix, locator_settings(false)).await
    }

    /// Open a writer for a coverage-only update without starting a compactor.
    ///
    /// The caller must already have proved that the current pack inventory is
    /// unchanged. This keeps a generation-only owner pass from waiting for a
    /// repository-sized locator compaction when no object rows will be written.
    pub async fn open_for_coverage_update(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
    ) -> Result<Self> {
        Self::open_with_settings(store, repo_prefix, locator_settings(false)).await
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

        let open_result = Self::load_or_initialize(db, path, repo_prefix.to_owned(), store).await;
        match open_result {
            Ok(writer) => Ok(writer),
            Err((db, operation)) => close_after_error(db, operation).await,
        }
    }

    async fn load_or_initialize(
        db: slatedb::Db,
        path: String,
        repo_prefix: String,
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
            let mut batch = metadata_batch(metadata);
            batch.put(PACK_OBJECT_INDEX_MARKER_KEY, PACK_OBJECT_INDEX_MARKER_VALUE);
            if let Err(error) = write_batch(&db, batch, "initialize compact locator metadata").await
            {
                return Err((db, error));
            }
            metadata
        };

        let bindings = match load_bindings(&db, metadata).await {
            Ok(bindings) => bindings,
            Err(error) => return Err((db, error)),
        };
        let pack_membership_index_ready = match db.get(PACK_OBJECT_INDEX_MARKER_KEY).await {
            Ok(Some(value)) if value.as_ref() == PACK_OBJECT_INDEX_MARKER_VALUE => true,
            Ok(Some(value)) if value.as_ref() == PACK_OBJECT_INDEX_REBUILDING_VALUE => false,
            Ok(Some(_)) => {
                return Err((
                    db,
                    corrupt(
                        "pack-object-index",
                        "invalid compact locator pack membership marker",
                    ),
                ));
            }
            // Marker-less empty databases predate the derived index and are
            // safe because they have no bindings or canonical object rows.
            // A populated marker-less catalog must rebuild before a sweep.
            Ok(None) => metadata.next_object_ordinal == 0 && bindings.is_empty(),
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
        let catalog_dirty = metadata.identity.map_or_else(
            || metadata.next_object_ordinal != 0 || !bindings.is_empty(),
            |identity| {
                identity.object_count != metadata.next_object_ordinal
                    || bindings
                        .values()
                        .any(|record| record.committed_generation > identity.generation)
            },
        );
        let checkpoint_required = if catalog_dirty {
            false
        } else if let Some(identity) = metadata.identity {
            match catalog_checkpoint_marker_exists(&store, &repo_prefix, identity).await {
                Ok(present) => !present,
                Err(error) => return Err((db, error)),
            }
        } else {
            false
        };
        Ok(Self {
            db,
            path,
            repo_prefix,
            store,
            initial_coverage: coverage(metadata),
            metadata,
            bindings,
            empty_catalog_binding: None,
            replacement_ordinals: None,
            existing_ordinals: None,
            ordinal_lookup_candidates: 0,
            pack_membership_index_ready,
            rebuild_remaining_rows: None,
            catalog_dirty,
            checkpoint_required,
            stats: LocatorWriteStats::default(),
            // The first metadata batch can share the first binding or
            // coverage flush; no reader checkpoint exists before that point.
            writes_durable: false,
        })
    }

    /// Return the last fully published manifest inventory, if any.
    #[must_use]
    pub fn coverage(&self) -> Option<GitLocatorCoverage> {
        coverage(self.metadata)
    }

    /// Return the exact immutable identity of the latest published catalog.
    #[must_use]
    pub fn catalog_identity(&self) -> Option<GitObjectCatalogIdentity> {
        self.metadata.identity
    }

    /// Return active layer and checkpoint facts without listing object-store keys.
    pub async fn catalog_stats(&self) -> Result<GitObjectCatalogStats> {
        let admin = slatedb::admin::AdminBuilder::new(
            ObjectPath::from(self.path.as_str()),
            Arc::clone(&self.store),
        )
        .build();
        let manifest = admin
            .read_manifest(None)
            .await
            .map_err(read_error)?
            .ok_or_else(|| corrupt("manifest", "Git object catalog manifest is missing"))?;
        let l0 = manifest.l0();
        let compacted = manifest.compacted();
        Ok(GitObjectCatalogStats {
            object_count: self
                .metadata
                .identity
                .map_or(0, |identity| identity.object_count),
            active_layers: u64::try_from(l0.len().saturating_add(compacted.len()))
                .unwrap_or(u64::MAX),
            active_ssts: u64::try_from(
                l0.len().saturating_add(
                    compacted
                        .iter()
                        .map(|run| run.sst_views.len())
                        .sum::<usize>(),
                ),
            )
            .unwrap_or(u64::MAX),
            active_bytes: l0
                .iter()
                .map(|sst| sst.estimate_size())
                .chain(compacted.iter().map(|run| run.estimate_size()))
                .fold(0_u64, u64::saturating_add),
            checkpoints: u64::try_from(manifest.checkpoints().len()).unwrap_or(u64::MAX),
        })
    }

    /// Return whether this session crossed the bounded garbage-collection cadence.
    #[must_use]
    pub fn maintenance_due(&self) -> bool {
        locator_gc_due(self.initial_coverage, coverage(self.metadata))
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
                .identity
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
            next_object_ordinal: self.metadata.next_object_ordinal,
            identity: self.metadata.identity,
        };
        let mut batch = metadata_batch(new_metadata);
        batch.delete(PACK_BINDINGS_KEY);
        for binding in &additions {
            let key = pack_key(binding.pack_slot).ok_or_else(|| {
                MetadataError::Internal("Git locator allocated pack slot zero".to_owned())
            })?;
            batch.put(key, encode_pack_record(binding.record));
        }
        write_batch(&self.db, batch, "bind compact locator packs").await?;
        flush(&self.db).await?;

        self.metadata = new_metadata;
        self.catalog_dirty = true;
        self.checkpoint_required = true;
        self.stats.flushes = self.stats.flushes.saturating_add(1);
        self.writes_durable = true;
        for binding in additions {
            self.bindings.insert(binding.pack_slot, binding.record);
        }
        if self.metadata.next_object_ordinal == 0 && results.len() == 1 {
            self.empty_catalog_binding = results.first().map(|binding| binding.pack_slot);
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

        let entry_count = u64::try_from(entries.len()).map_err(|_| {
            MetadataError::Internal("Git locator object batch is too large".to_owned())
        })?;
        if let Some(remaining) = &self.rebuild_remaining_rows {
            let expected = remaining.get(&binding.pack_slot).ok_or_else(|| {
                MetadataError::Internal(
                    "Git locator rebuild received an object batch for an unplanned pack".to_owned(),
                )
            })?;
            if entry_count > *expected {
                return Err(MetadataError::Internal(
                    "Git locator rebuild received more objects than the pack index reports"
                        .to_owned(),
                ));
            }
        }

        self.prepare_ordinal_lookup(entries.len()).await?;
        let skip_existing_lookup = self.existing_ordinals.is_some()
            || self.replacement_ordinals.is_some()
            || self.empty_catalog_binding == Some(binding.pack_slot);
        let existing_objects = if skip_existing_lookup {
            vec![None; entries.len()]
        } else {
            self.lookup_existing_objects(entries).await?
        };

        let mut batch = slatedb::WriteBatch::new();
        let mut batch_rows = 0_usize;
        let mut batch_bytes = 0_usize;
        let mut submitted = HashSet::with_capacity(entries.len());
        for (entry_index, entry) in entries.iter().enumerate() {
            if !submitted.insert(entry.oid) {
                return Err(MetadataError::Internal(
                    "Git locator object batch contains a duplicate OID".to_owned(),
                ));
            }
            if !validate_location_for_pack(entry.location, binding.record.pack_size) {
                return Err(MetadataError::Internal(
                    "Git locator object range falls outside its bound pack".to_owned(),
                ));
            }
            if entry.metadata.delta_base_oid == Some(entry.oid) {
                return Err(MetadataError::Internal(
                    "Git locator object cannot use itself as a delta base".to_owned(),
                ));
            }
            let key = object_key(&entry.oid);
            let existing_object = self
                .replacement_ordinals
                .as_ref()
                .and_then(|ordinals| ordinals.get(&entry.oid).copied());
            let existing_object = existing_object.or_else(|| {
                self.existing_ordinals
                    .as_ref()
                    .and_then(|ordinals| ordinals.get(&entry.oid))
            });
            let existing_object = existing_object.or(existing_objects[entry_index]);
            let (ordinal, previous_object) = match existing_object {
                Some(existing) => (existing.ordinal, Some(existing)),
                None => (self.allocate_ordinal()?, None),
            };
            // A repack changes the physical location but not the OID's
            // logical facts. Preserve facts already proven by the covered
            // catalog so owner maintenance need not download the whole new
            // pack merely to recover object kinds.
            let metadata = Self::merge_object_metadata(
                previous_object.map(|object| object.metadata),
                entry.metadata,
            );
            let object = ExistingObject {
                ordinal,
                pack_slot: binding.pack_slot,
                metadata,
            };
            if let Some(previous) = previous_object
                && previous.pack_slot != binding.pack_slot
            {
                batch.delete(
                    pack_object_key(previous.pack_slot, &entry.oid).ok_or_else(|| {
                        corrupt("pack-object-index", "object row has an invalid pack slot")
                    })?,
                );
            }
            if let Some(ordinals) = &mut self.replacement_ordinals {
                ordinals.insert(entry.oid, object);
            }
            if let Some(ordinals) = &mut self.existing_ordinals {
                ordinals.insert(entry.oid, object);
            }
            let value = encode_object_location(StoredObjectLocation {
                ordinal,
                pack_slot: binding.pack_slot,
                pack_offset: entry.location.pack_offset,
                entry_len: entry.location.entry_len,
                crc32: entry.location.crc32,
                metadata,
            });
            batch.put(key, value);
            batch.put(ordinal_key(ordinal), entry.oid);
            let pack_object_key =
                pack_object_key(binding.pack_slot, &entry.oid).ok_or_else(|| {
                    corrupt("pack-object-index", "object row has an invalid pack slot")
                })?;
            batch.put(pack_object_key, encode_pack_object_ordinal(ordinal));
            let metadata_key = ordinal_metadata_key(ordinal);
            let metadata_value = encode_object_metadata(metadata);
            batch.put(metadata_key, metadata_value);
            batch_rows += 1;
            batch_bytes += key.len()
                + value.len()
                + ordinal_key(ordinal).len()
                + entry.oid.len()
                + pack_object_key.len()
                + PACK_OBJECT_VALUE_LEN
                + metadata_key.len()
                + metadata_value.len();
            if batch_rows >= MAX_BATCH_ROWS || batch_bytes >= MAX_BATCH_LOGICAL_BYTES {
                if self.pack_membership_index_ready {
                    batch.put(PACK_OBJECT_INDEX_MARKER_KEY, PACK_OBJECT_INDEX_MARKER_VALUE);
                }
                batch.put(METADATA_KEY, encode_metadata(self.metadata));
                write_batch(&self.db, batch, "write compact locator objects").await?;
                self.record_object_batch(batch_rows, batch_bytes);
                batch = slatedb::WriteBatch::new();
                batch_rows = 0;
                batch_bytes = 0;
            }
        }
        if batch_rows != 0 {
            if self.pack_membership_index_ready {
                batch.put(PACK_OBJECT_INDEX_MARKER_KEY, PACK_OBJECT_INDEX_MARKER_VALUE);
            }
            batch.put(METADATA_KEY, encode_metadata(self.metadata));
            write_batch(&self.db, batch, "write compact locator objects").await?;
            self.record_object_batch(batch_rows, batch_bytes);
        }
        if !entries.is_empty() {
            self.catalog_dirty = true;
            self.checkpoint_required = true;
        }
        if let Some(remaining) = &mut self.rebuild_remaining_rows {
            let count = remaining.get_mut(&binding.pack_slot).ok_or_else(|| {
                MetadataError::Internal(
                    "Git locator rebuild received an object batch for an unplanned pack".to_owned(),
                )
            })?;
            *count = count.saturating_sub(entry_count);
        }
        Ok(())
    }

    async fn lookup_existing_objects(
        &self,
        entries: &[GitObjectLocatorEntry],
    ) -> Result<Vec<Option<ExistingObject>>> {
        if self.should_scan_existing_objects(entries.len()) {
            if let Some(existing) = self.lookup_existing_objects_by_scan(entries).await? {
                return Ok(existing);
            }
            debug!(
                locator_lookup_mode = "exact_fallback",
                requested_objects = entries.len(),
                catalog_objects = self.metadata.next_object_ordinal,
                active_ssts = active_sst_count(&self.db),
                "compact Git locator scan exceeded its row bound"
            );
        }
        let fetched = stream::iter(entries.iter().enumerate().map(|(index, entry)| {
            let db = &self.db;
            async move {
                db.get(object_key(&entry.oid))
                    .await
                    .map(|value| (index, value))
                    .map_err(read_error)
            }
        }))
        .buffer_unordered(LOCATOR_EXISTING_LOOKUP_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        let mut existing = vec![None; entries.len()];
        for (index, value) in fetched {
            existing[index] = match value {
                None => None,
                Some(value) => Some(decode_existing_object(&value)?),
            };
        }
        Ok(existing)
    }

    fn should_scan_existing_objects(&self, requested_objects: usize) -> bool {
        should_scan_existing_objects(
            requested_objects,
            self.metadata.next_object_ordinal,
            active_sst_count(&self.db),
        )
    }

    async fn lookup_existing_objects_by_scan(
        &self,
        entries: &[GitObjectLocatorEntry],
    ) -> Result<Option<Vec<Option<ExistingObject>>>> {
        let mut requested = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.oid, index))
            .collect::<Vec<_>>();
        requested.sort_unstable_by_key(|(oid, _)| *oid);
        let Some((first_oid, _)) = requested.first() else {
            return Ok(Some(Vec::new()));
        };
        let last_oid = requested
            .last()
            .map(|(oid, _)| oid)
            .ok_or_else(|| MetadataError::Internal("locator scan lost its request".to_owned()))?;
        let row_limit = usize::try_from(self.metadata.next_object_ordinal).unwrap_or(usize::MAX);
        let options = slatedb::config::ScanOptions::default()
            .with_read_ahead_bytes(LOCATOR_SCAN_READ_AHEAD_BYTES)
            .with_max_fetch_tasks(LOCATOR_SCAN_FETCH_TASKS);
        let mut rows = self
            .db
            .scan_prefix_with_options(
                [OBJECT_FAMILY],
                first_oid.as_slice()..=last_oid.as_slice(),
                &options,
            )
            .await
            .map_err(read_error)?;
        let mut existing = vec![None; entries.len()];
        let mut request_index = 0_usize;
        let mut rows_scanned = 0_usize;
        while let Some(row) = rows.next().await.map_err(read_error)? {
            rows_scanned = rows_scanned.saturating_add(1);
            if rows_scanned > row_limit {
                return Ok(None);
            }
            let oid = decode_object_key(&row.key)
                .ok_or_else(|| corrupt("object", "invalid compact locator object key"))?;
            while requested
                .get(request_index)
                .is_some_and(|(requested_oid, _)| *requested_oid < oid)
            {
                request_index += 1;
            }
            while requested
                .get(request_index)
                .is_some_and(|(requested_oid, _)| *requested_oid == oid)
            {
                let (_, output_index) = requested[request_index];
                existing[output_index] = Some(decode_existing_object(&row.value)?);
                request_index += 1;
            }
            if request_index == requested.len() {
                break;
            }
        }
        debug!(
            locator_lookup_mode = "scan",
            requested_objects = entries.len(),
            rows_scanned,
            catalog_objects = self.metadata.next_object_ordinal,
            active_ssts = active_sst_count(&self.db),
            "compact Git locator existing-object scan completed"
        );
        Ok(Some(existing))
    }

    async fn prepare_ordinal_lookup(&mut self, entry_count: usize) -> Result<()> {
        if self.replacement_ordinals.is_some() || self.existing_ordinals.is_some() {
            return Ok(());
        }
        self.ordinal_lookup_candidates = self
            .ordinal_lookup_candidates
            .saturating_add(u64::try_from(entry_count).unwrap_or(u64::MAX));
        let current_objects = self.metadata.next_object_ordinal;
        if current_objects == 0 {
            self.existing_ordinals = Some(ExistingOrdinalIndex::default());
        } else if should_load_existing_ordinals(current_objects, self.ordinal_lookup_candidates) {
            self.load_existing_ordinals().await?;
        }
        Ok(())
    }

    async fn load_existing_ordinals(&mut self) -> Result<()> {
        if self.existing_ordinals.is_some() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let options = slatedb::config::ScanOptions::default()
            .with_read_ahead_bytes(LOCATOR_SCAN_READ_AHEAD_BYTES)
            .with_max_fetch_tasks(LOCATOR_SCAN_FETCH_TASKS);
        let mut rows = self
            .db
            .scan_prefix_with_options([OBJECT_FAMILY], .., &options)
            .await
            .map_err(read_error)?;
        let mut ordinals = Vec::new();
        while let Some(row) = rows.next().await.map_err(read_error)? {
            let oid = decode_object_key(&row.key)
                .ok_or_else(|| corrupt("object", "invalid compact locator object key"))?;
            let location = decode_object_location(&row.value)
                .ok_or_else(|| corrupt("object", "invalid compact locator object location"))?;
            if u64::from(location.ordinal) >= self.metadata.next_object_ordinal {
                return Err(corrupt(
                    "object",
                    "compact locator object ordinal exceeds catalog metadata",
                ));
            }
            ordinals.push((
                oid,
                ExistingObject {
                    ordinal: location.ordinal,
                    pack_slot: location.pack_slot,
                    metadata: location.metadata,
                },
            ));
        }
        ordinals.sort_unstable_by_key(|(oid, _)| *oid);
        if ordinals.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(corrupt(
                "object",
                "compact locator contains duplicate object rows",
            ));
        }
        self.existing_ordinals = Some(ExistingOrdinalIndex::from_sorted_entries(ordinals));
        tracing::debug!(
            locator_existing_objects = self
                .existing_ordinals
                .as_ref()
                .map_or(0, ExistingOrdinalIndex::len),
            locator_existing_objects_ms = started.elapsed().as_millis() as u64,
            "loaded existing Git locator ordinals"
        );
        Ok(())
    }

    fn allocate_ordinal(&mut self) -> Result<GitObjectOrdinal> {
        if self.metadata.next_object_ordinal >= u64::from(GitObjectOrdinal::MAX) {
            return Err(MetadataError::Internal(
                "Git object catalog ordinals are exhausted".to_owned(),
            ));
        }
        let ordinal =
            GitObjectOrdinal::try_from(self.metadata.next_object_ordinal).map_err(|_| {
                MetadataError::Internal("Git object catalog ordinals are exhausted".to_owned())
            })?;
        self.metadata.next_object_ordinal = self
            .metadata
            .next_object_ordinal
            .checked_add(1)
            .ok_or_else(|| {
                MetadataError::Internal("Git object catalog ordinals are exhausted".to_owned())
            })?;
        self.catalog_dirty = true;
        self.checkpoint_required = true;
        Ok(ordinal)
    }

    fn merge_object_metadata(
        existing: Option<GitObjectMetadata>,
        incoming: GitObjectMetadata,
    ) -> GitObjectMetadata {
        let existing = existing.unwrap_or_default();
        GitObjectMetadata {
            kind: incoming.kind.or(existing.kind),
            logical_size: incoming.logical_size.or(existing.logical_size),
            delta_base_oid: incoming.delta_base_oid.or(existing.delta_base_oid),
        }
    }

    /// Replace the current object/ordinal universe while retaining pack slots.
    ///
    /// Historical checkpoints retain the prior universe. The caller must
    /// rewrite every current pack before advancing coverage.
    pub async fn replace_object_catalog(&mut self, retained_slots: &HashSet<u64>) -> Result<()> {
        let mut rebuild_remaining_rows = HashMap::with_capacity(retained_slots.len());
        for slot in retained_slots {
            if *slot == 0 {
                return Err(MetadataError::Internal(
                    "Git locator rebuild retained an invalid pack slot".to_owned(),
                ));
            }
            let record = self.bindings.get(slot).ok_or_else(|| {
                MetadataError::Internal(
                    "Git locator rebuild retained an unbound pack slot".to_owned(),
                )
            })?;
            rebuild_remaining_rows.insert(*slot, record.object_count);
        }
        for family in [
            OBJECT_FAMILY,
            super::format::ORDINAL_FAMILY,
            ORDINAL_METADATA_FAMILY,
            PACK_OBJECT_FAMILY,
        ] {
            let mut rows = self
                .db
                .scan_prefix([family], ..)
                .await
                .map_err(read_error)?;
            let mut batch = slatedb::WriteBatch::new();
            let mut batch_rows = 0usize;
            while let Some(row) = rows.next().await.map_err(read_error)? {
                batch.delete(row.key);
                batch_rows += 1;
                if batch_rows == MAX_BATCH_ROWS {
                    write_batch(&self.db, batch, "replace Git object catalog").await?;
                    batch = slatedb::WriteBatch::new();
                    batch_rows = 0;
                    self.writes_durable = false;
                }
            }
            if batch_rows != 0 {
                write_batch(&self.db, batch, "replace Git object catalog").await?;
                self.writes_durable = false;
            }
        }
        self.metadata.next_object_ordinal = 0;
        self.metadata.identity = None;
        self.catalog_dirty = true;
        let mut reset = metadata_batch(self.metadata);
        reset.delete(PACK_BINDINGS_KEY);
        reset.put(
            PACK_OBJECT_INDEX_MARKER_KEY,
            PACK_OBJECT_INDEX_REBUILDING_VALUE,
        );
        write_batch(&self.db, reset, "reset Git object catalog metadata").await?;
        self.writes_durable = false;
        self.flush_objects().await?;
        self.empty_catalog_binding = None;
        self.replacement_ordinals = Some(HashMap::new());
        self.existing_ordinals = None;
        self.ordinal_lookup_candidates = 0;
        self.pack_membership_index_ready = false;
        self.rebuild_remaining_rows = Some(rebuild_remaining_rows);
        Ok(())
    }

    /// Mark a fully replayed object catalog's derived membership index ready.
    pub async fn complete_object_catalog_rebuild(&mut self) -> Result<()> {
        let Some(remaining) = &self.rebuild_remaining_rows else {
            if self.pack_membership_index_ready {
                return Ok(());
            }
            return Err(MetadataError::Internal(
                "Git locator rebuild completion was not planned".to_owned(),
            ));
        };
        if let Some((slot, count)) = remaining
            .iter()
            .find_map(|(slot, count)| (*count != 0).then_some((*slot, *count)))
        {
            return Err(MetadataError::Internal(format!(
                "Git locator rebuild is missing {count} objects for pack slot {slot}"
            )));
        }
        let mut marker = slatedb::WriteBatch::new();
        marker.put(PACK_OBJECT_INDEX_MARKER_KEY, PACK_OBJECT_INDEX_MARKER_VALUE);
        write_batch(
            &self.db,
            marker,
            "mark rebuilt compact locator pack membership index",
        )
        .await?;
        self.writes_durable = false;
        self.flush_objects().await?;
        self.pack_membership_index_ready = true;
        self.rebuild_remaining_rows = None;
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

    /// Delete rows whose pack slots are absent from canonical inventory.
    ///
    /// A complete membership index makes the object work proportional to stale
    /// rows; a marker-less historical catalog is rebuilt once before sweeping.
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
        let mut stale_slots = self
            .bindings
            .keys()
            .filter(|slot| !retained_slots.contains(slot))
            .copied()
            .collect::<Vec<_>>();
        stale_slots.sort_unstable();
        if stale_slots.is_empty() && self.pack_membership_index_ready {
            return Ok(LocatorSweepStats::default());
        }

        let mut stats = if self.pack_membership_index_ready {
            self.sweep_stale_pack_membership(&stale_slots).await?
        } else {
            self.rebuild_pack_membership_and_sweep(retained_slots)
                .await?
        };
        stats.pack_rows_scanned = u64::try_from(stale_slots.len()).unwrap_or(u64::MAX);
        stats.pack_rows_deleted = stats.pack_rows_scanned;

        let mut deletes = slatedb::WriteBatch::new();
        for slot in &stale_slots {
            deletes.delete(
                pack_key(*slot)
                    .ok_or_else(|| corrupt("pack", "stale locator pack slot is invalid"))?,
            );
        }
        if !stale_slots.is_empty() {
            deletes.delete(PACK_BINDINGS_KEY);
            write_batch(&self.db, deletes, "sweep compact locator packs").await?;
            self.writes_durable = false;
            self.catalog_dirty = true;
            self.checkpoint_required = true;
        }
        self.flush_objects().await?;
        for slot in stale_slots {
            self.bindings.remove(&slot);
        }
        Ok(stats)
    }

    async fn sweep_stale_pack_membership(
        &mut self,
        stale_slots: &[u64],
    ) -> Result<LocatorSweepStats> {
        let mut stats = LocatorSweepStats::default();
        let mut deletes = slatedb::WriteBatch::new();
        let mut delete_count = 0_usize;
        for slot in stale_slots {
            let prefix = pack_object_prefix(*slot).ok_or_else(|| {
                corrupt("pack-object-index", "stale locator pack slot is invalid")
            })?;
            let options = ScanOptions::default()
                .with_read_ahead_bytes(LOCATOR_SCAN_READ_AHEAD_BYTES)
                .with_max_fetch_tasks(LOCATOR_SCAN_FETCH_TASKS);
            let mut rows = self
                .db
                .scan_prefix_with_options(prefix, .., &options)
                .await
                .map_err(read_error)?;
            while let Some(row) = rows.next().await.map_err(read_error)? {
                stats.object_rows_scanned = stats.object_rows_scanned.saturating_add(1);
                let (row_slot, oid) = decode_pack_object_key(&row.key).ok_or_else(|| {
                    corrupt(
                        "pack-object-index",
                        "invalid compact locator pack membership key",
                    )
                })?;
                let ordinal = decode_pack_object_ordinal(&row.value).ok_or_else(|| {
                    corrupt(
                        "pack-object-index",
                        "invalid compact locator pack membership ordinal",
                    )
                })?;
                if row_slot != *slot {
                    return Err(corrupt(
                        "pack-object-index",
                        "pack membership row crossed its slot prefix",
                    ));
                }
                let delete_object = if let Some(ordinals) = &self.existing_ordinals {
                    let Some(current) = ordinals.get(&oid) else {
                        return Err(corrupt(
                            "pack-object-index",
                            "pack membership row has no canonical object row",
                        ));
                    };
                    if current.ordinal != ordinal {
                        return Err(corrupt(
                            "pack-object-index",
                            "pack membership row disagrees with its object ordinal",
                        ));
                    }
                    current.pack_slot == *slot
                } else {
                    let value = self
                        .db
                        .get(object_key(&oid))
                        .await
                        .map_err(read_error)?
                        .ok_or_else(|| {
                            corrupt(
                                "pack-object-index",
                                "pack membership row has no canonical object row",
                            )
                        })?;
                    let location = decode_object_location(&value).ok_or_else(|| {
                        corrupt("object", "invalid compact locator object location")
                    })?;
                    if location.pack_slot != *slot || location.ordinal != ordinal {
                        return Err(corrupt(
                            "pack-object-index",
                            "pack membership row disagrees with its object row",
                        ));
                    }
                    true
                };
                if let Some(ordinals) = &mut self.existing_ordinals
                    && delete_object
                {
                    ordinals.remove(&oid);
                }
                if delete_object {
                    deletes.delete(object_key(&oid));
                    deletes.delete(ordinal_key(ordinal));
                    deletes.delete(ordinal_metadata_key(ordinal));
                    stats.object_rows_deleted = stats.object_rows_deleted.saturating_add(1);
                    self.catalog_dirty = true;
                    self.checkpoint_required = true;
                }
                deletes.delete(row.key);
                delete_count += 1;
                if delete_count == MAX_BATCH_ROWS {
                    write_batch(&self.db, deletes, "sweep compact locator membership").await?;
                    self.writes_durable = false;
                    deletes = slatedb::WriteBatch::new();
                    delete_count = 0;
                }
            }
        }
        if delete_count != 0 {
            write_batch(&self.db, deletes, "sweep compact locator membership").await?;
            self.writes_durable = false;
        }
        Ok(stats)
    }

    async fn rebuild_pack_membership_and_sweep(
        &mut self,
        retained_slots: &HashSet<u64>,
    ) -> Result<LocatorSweepStats> {
        let mut stats = LocatorSweepStats::default();
        let mut rows = self
            .db
            .scan_prefix([OBJECT_FAMILY], ..)
            .await
            .map_err(read_error)?;
        let mut batch = slatedb::WriteBatch::new();
        let mut batch_rows = 0_usize;
        while let Some(row) = rows.next().await.map_err(read_error)? {
            stats.object_rows_scanned = stats.object_rows_scanned.saturating_add(1);
            let oid = decode_object_key(&row.key)
                .ok_or_else(|| corrupt("object", "invalid compact locator object key"))?;
            let location = decode_object_location(&row.value)
                .ok_or_else(|| corrupt("object", "invalid compact locator object location"))?;
            if retained_slots.contains(&location.pack_slot) {
                batch.put(
                    pack_object_key(location.pack_slot, &oid).ok_or_else(|| {
                        corrupt("pack-object-index", "object row has an invalid pack slot")
                    })?,
                    encode_pack_object_ordinal(location.ordinal),
                );
            } else {
                if let Some(ordinals) = &mut self.existing_ordinals {
                    ordinals.remove(&oid);
                }
                batch.delete(row.key);
                batch.delete(ordinal_key(location.ordinal));
                batch.delete(ordinal_metadata_key(location.ordinal));
                stats.object_rows_deleted = stats.object_rows_deleted.saturating_add(1);
                self.catalog_dirty = true;
                self.checkpoint_required = true;
            }
            batch_rows += 1;
            if batch_rows == MAX_BATCH_ROWS {
                write_batch(&self.db, batch, "rebuild compact locator membership").await?;
                self.writes_durable = false;
                batch = slatedb::WriteBatch::new();
                batch_rows = 0;
            }
        }
        if batch_rows != 0 {
            write_batch(&self.db, batch, "rebuild compact locator membership").await?;
            self.writes_durable = false;
        }
        let mut marker = slatedb::WriteBatch::new();
        marker.put(PACK_OBJECT_INDEX_MARKER_KEY, PACK_OBJECT_INDEX_MARKER_VALUE);
        write_batch(&self.db, marker, "mark rebuilt compact locator membership").await?;
        self.writes_durable = false;
        self.pack_membership_index_ready = true;
        Ok(stats)
    }

    /// Mark one immutable manifest inventory fully covered with one durability flush.
    pub async fn set_coverage(&mut self, coverage: GitLocatorCoverage) -> Result<()> {
        if !self.pack_membership_index_ready {
            return Err(MetadataError::Internal(
                "Git locator coverage cannot advance before catalog rebuild completion".to_owned(),
            ));
        }
        if coverage.generation == 0 {
            return Err(MetadataError::Internal(
                "Git locator coverage generation must be non-zero".to_owned(),
            ));
        }
        let identity = GitObjectCatalogIdentity {
            generation: coverage.generation,
            pack_index_hash: coverage.pack_index_hash,
            object_count: self.metadata.next_object_ordinal,
            catalog_digest: catalog_digest(
                coverage,
                self.metadata.next_pack_slot,
                self.metadata.next_object_ordinal,
                &self.bindings,
            ),
        };
        let metadata = LocatorMetadata {
            next_pack_slot: self.metadata.next_pack_slot,
            next_object_ordinal: self.metadata.next_object_ordinal,
            identity: Some(identity),
        };
        let mut publication = metadata_batch(metadata);
        let bindings = self.bindings.iter().map(|(slot, record)| (*slot, *record));
        let bindings = bindings.collect::<Vec<_>>();
        publication.put(
            PACK_BINDINGS_KEY,
            encode_pack_bindings(identity, metadata.next_pack_slot, &bindings),
        );
        write_batch(&self.db, publication, "write compact locator coverage").await?;
        self.writes_durable = false;
        // Object rows and their coverage marker share the same active
        // memtable, so one flush makes the marker impossible to observe
        // without the rows it covers.
        self.flush_objects().await?;
        self.metadata = metadata;
        self.replacement_ordinals = None;
        self.catalog_dirty = false;
        self.checkpoint_required = true;
        self.stats.coverage_updated = true;
        Ok(())
    }

    /// Publish a read-only checkpoint without closing this writer.
    ///
    /// All submitted rows and coverage are flushed before the checkpoint. A
    /// long-lived exclusive owner can then serve multiple manifest generations
    /// from one SlateDB session while readers open only immutable checkpoints.
    pub async fn publish_checkpoint(&mut self) -> Result<()> {
        if !self.checkpoint_required {
            return Ok(());
        }
        self.flush_objects().await?;
        let published_identity = match (self.metadata.identity, self.catalog_dirty) {
            (Some(identity), false) => Some(identity),
            _ => None,
        };
        let name = published_identity.map_or_else(
            || super::UNPUBLISHED_CHECKPOINT_NAME.to_owned(),
            |identity| super::catalog_checkpoint_name(identity.catalog_digest),
        );
        let checkpoint = self
            .db
            .create_checkpoint(
                // `set_coverage` flushes object rows and the identity before
                // publication, so another best-effort memtable flush is redundant.
                CheckpointScope::Durable,
                &CheckpointOptions {
                    name: Some(name.clone()),
                    ..CheckpointOptions::default()
                },
            )
            .await
            .map_err(|source| MetadataError::SlateDbWrite {
                db: DB_LABEL.to_owned(),
                source,
            })?;

        if let Some(identity) = published_identity {
            let marker_path = ObjectPath::from(super::catalog_checkpoint_marker_path(
                &self.repo_prefix,
                identity.catalog_digest,
            ));
            let marker_body =
                serde_json::to_vec(&super::CatalogCheckpointMarker::for_identity(identity))
                    .map_err(|error| {
                        MetadataError::Internal(format!(
                            "catalog checkpoint marker serialize: {error}"
                        ))
                    })?;
            crab_storage::Store::new(Arc::clone(&self.store))
                .put(&marker_path, Bytes::from(marker_body))
                .await
                .map_err(|source| MetadataError::Storage { source })?;
        }
        self.checkpoint_required = false;
        if let Err(error) =
            retire_old_catalog_checkpoints(&self.path, Arc::clone(&self.store), &checkpoint, &name)
                .await
        {
            warn!(%error, "old Git catalog checkpoints require retirement retry");
        }
        Ok(())
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
        let collect_garbage = locator_gc_due(initial_coverage, coverage(metadata));
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

async fn catalog_checkpoint_marker_exists(
    store: &Arc<dyn ObjectStore>,
    repo_prefix: &str,
    identity: GitObjectCatalogIdentity,
) -> Result<bool> {
    let path = ObjectPath::from(super::catalog_checkpoint_marker_path(
        repo_prefix,
        identity.catalog_digest,
    ));
    match crab_storage::Store::new(Arc::clone(store))
        .head(&path)
        .await
    {
        Ok(_) => Ok(true),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(false),
        Err(source) => Err(MetadataError::Storage { source }),
    }
}

async fn retire_old_catalog_checkpoints(
    path: &str,
    store: Arc<dyn ObjectStore>,
    current: &slatedb::CheckpointCreateResult,
    current_name: &str,
) -> Result<()> {
    let storage = crab_storage::Store::new(Arc::clone(&store));
    let admin =
        slatedb::admin::AdminBuilder::new(ObjectPath::from(path), Arc::clone(&store)).build();
    let checkpoints =
        admin
            .list_checkpoints(None)
            .await
            .map_err(|source| MetadataError::SlateDbWrite {
                db: DB_LABEL.to_owned(),
                source,
            })?;
    for checkpoint in checkpoints.into_iter().filter(|checkpoint| {
        let Some(name) = checkpoint.name.as_deref() else {
            return false;
        };
        let retired_publication = name == super::UNPUBLISHED_CHECKPOINT_NAME
            || (current_name != super::UNPUBLISHED_CHECKPOINT_NAME
                && name.starts_with(super::READER_CHECKPOINT_PREFIX));
        checkpoint.id != current.id && checkpoint.expire_time.is_none() && retired_publication
    }) {
        admin
            .refresh_checkpoint(checkpoint.id, Some(RETIRED_CHECKPOINT_LIFETIME))
            .await
            .map_err(|source| MetadataError::SlateDbWrite {
                db: DB_LABEL.to_owned(),
                source,
            })?;
        if let Some(digest) = checkpoint
            .name
            .as_deref()
            .and_then(catalog_digest_from_checkpoint_name)
        {
            let marker_path =
                ObjectPath::from(format!("{}checkpoints/{}.json", path, digest.hex()));
            match storage.delete(&marker_path).await {
                Ok(()) | Err(crab_storage::StorageError::NotFound { .. }) => {}
                Err(source) => return Err(MetadataError::Storage { source }),
            }
        }
    }
    Ok(())
}

fn catalog_digest_from_checkpoint_name(name: &str) -> Option<MerkleHash> {
    name.strip_prefix(super::READER_CHECKPOINT_PREFIX)
        .and_then(|digest| MerkleHash::from_hex(digest).ok())
}

fn catalog_digest(
    coverage: GitLocatorCoverage,
    next_pack_slot: u64,
    object_count: u64,
    bindings: &HashMap<u64, GitPackLocatorRecord>,
) -> MerkleHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.git-object-catalog.v1\0");
    hasher.update(&coverage.generation.to_be_bytes());
    hasher.update(&<[u8; 32]>::from(coverage.pack_index_hash));
    hasher.update(&next_pack_slot.to_be_bytes());
    hasher.update(&object_count.to_be_bytes());
    let mut bindings = bindings.iter().collect::<Vec<_>>();
    bindings.sort_unstable_by_key(|(slot, _)| **slot);
    for (slot, record) in bindings {
        hasher.update(&slot.to_be_bytes());
        hasher.update(&<[u8; 32]>::from(record.pack_id));
        hasher.update(&record.object_count.to_be_bytes());
        hasher.update(&record.pack_size.to_be_bytes());
    }
    MerkleHash::from(*hasher.finalize().as_bytes())
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
            max_concurrent_compactions: LOCATOR_COMPACTION_MAX_CONCURRENT,
            ..CompactorOptions::default()
        };
        // The locator is one unsharded keyspace. Let one compaction consume the
        // whole bounded L0 frontier so repeated short-lived writers do not
        // rewrite the same history through several eight-source jobs. Keep
        // read-ahead fetches bounded but concurrent enough to finish before
        // the repository maintenance lease needs another renewal.
        compactor.scheduler_options.insert(
            "max_compaction_sources".to_owned(),
            LOCATOR_L0_MAX_SSTS.to_string(),
        );
        if let Some(worker) = &mut compactor.worker {
            worker.max_concurrent_compactions = LOCATOR_COMPACTION_MAX_CONCURRENT;
            worker.compactions_poll_interval = std::time::Duration::from_millis(500);
            worker.max_subcompactions = LOCATOR_COMPACTION_MAX_SUBCOMPACTIONS;
            worker.max_fetch_tasks = LOCATOR_COMPACTION_MAX_FETCH_TASKS;
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

async fn load_bindings(
    db: &slatedb::Db,
    metadata: LocatorMetadata,
) -> Result<HashMap<u64, GitPackLocatorRecord>> {
    if let Some(value) = db.get(PACK_BINDINGS_KEY).await.map_err(read_error)? {
        let identity = metadata
            .identity
            .ok_or_else(|| corrupt("pack", "binding snapshot has no catalog identity"))?;
        return Ok(
            decode_pack_bindings(&value, identity, metadata.next_pack_slot)
                .ok_or_else(|| corrupt("pack", "invalid compact locator binding snapshot"))?
                .into_iter()
                .collect(),
        );
    }

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
        if slot >= metadata.next_pack_slot
            || bindings.insert(slot, record).is_some()
            || !pack_ids.insert(record.pack_id)
        {
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

fn active_sst_count(db: &slatedb::Db) -> u64 {
    let manifest = db.manifest();
    let l0 = u64::try_from(manifest.l0().len()).unwrap_or(u64::MAX);
    manifest.compacted().iter().fold(l0, |total, run| {
        total.saturating_add(u64::try_from(run.sst_views.len()).unwrap_or(u64::MAX))
    })
}

fn decode_existing_object(value: &[u8]) -> Result<ExistingObject> {
    let location = decode_object_location(value)
        .ok_or_else(|| corrupt("object", "invalid Git catalog object location"))?;
    Ok(ExistingObject {
        ordinal: location.ordinal,
        pack_slot: location.pack_slot,
        metadata: location.metadata,
    })
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

fn should_scan_existing_objects(
    requested_objects: usize,
    catalog_objects: u64,
    active_ssts: u64,
) -> bool {
    let requested = u64::try_from(requested_objects).unwrap_or(u64::MAX);
    if requested == 0 || catalog_objects == 0 || active_ssts < EXISTING_LOOKUP_SCAN_MIN_SSTS {
        return false;
    }
    let small_catalog = catalog_objects <= EXISTING_LOOKUP_SCAN_SMALL_CATALOG_MAX_OBJECTS
        && requested.saturating_mul(EXISTING_LOOKUP_SCAN_SMALL_CATALOG_RATIO) >= catalog_objects;
    small_catalog
        || requested.saturating_mul(active_ssts)
            >= catalog_objects.saturating_add(EXISTING_LOOKUP_SCAN_CATALOG_COST_DIVISOR - 1)
                / EXISTING_LOOKUP_SCAN_CATALOG_COST_DIVISOR
}

fn should_load_existing_ordinals(current_objects: u64, candidate_objects: u64) -> bool {
    current_objects >= BULK_ORDINAL_LOOKUP_MIN_CATALOG_OBJECTS
        && candidate_objects.saturating_mul(BULK_ORDINAL_LOOKUP_FACTOR) >= current_objects
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use futures_util::TryStreamExt;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };

    use super::*;
    use crate::git_object_locator::{
        GitObjectKind, GitObjectLocation, GitObjectLookup, GitObjectMetadata, GitPackInventoryEntry,
    };
    use crab_xet::hash::MerkleHash;

    #[derive(Debug)]
    struct FailFirstPutStore {
        inner: Arc<InMemory>,
        fail_path: Mutex<Option<String>>,
        fail_next_put: AtomicBool,
    }

    impl FailFirstPutStore {
        fn new() -> Self {
            Self {
                inner: Arc::new(InMemory::new()),
                fail_path: Mutex::new(None),
                fail_next_put: AtomicBool::new(false),
            }
        }

        fn fail_next_put_at(&self, path: &ObjectPath) {
            *self.fail_path.lock().expect("test lock") = Some(path.to_string());
            self.fail_next_put.store(true, Ordering::Release);
        }
    }

    impl std::fmt::Display for FailFirstPutStore {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("FailFirstPutStore")
        }
    }

    #[async_trait]
    impl ObjectStore for FailFirstPutStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            let should_fail = self.fail_path.lock().expect("test lock").as_deref()
                == Some(location.as_ref())
                && self.fail_next_put.swap(false, Ordering::AcqRel);
            if should_fail {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "service unavailable: slow down".into(),
                });
            }
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

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
            metadata: Default::default(),
        }
    }

    fn generation_entry(generation: u64) -> GitObjectLocatorEntry {
        let mut oid = [0_u8; 20];
        oid[..8].copy_from_slice(&generation.to_be_bytes());
        GitObjectLocatorEntry {
            oid,
            location: GitObjectLocation {
                pack_offset: 12,
                entry_len: 96,
                crc32: generation as u32,
            },
            metadata: Default::default(),
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
    fn locator_compaction_uses_one_full_frontier_worker() {
        let options = locator_settings(true)
            .compactor_options
            .expect("compaction settings");
        assert_eq!(
            options.max_concurrent_compactions,
            LOCATOR_COMPACTION_MAX_CONCURRENT
        );
        assert_eq!(
            options.scheduler_options.get("max_compaction_sources"),
            Some(&LOCATOR_L0_MAX_SSTS.to_string())
        );
        let worker = options.worker.expect("embedded locator compaction worker");
        assert_eq!(
            worker.max_concurrent_compactions,
            LOCATOR_COMPACTION_MAX_CONCURRENT
        );
        assert_eq!(
            worker.max_subcompactions,
            LOCATOR_COMPACTION_MAX_SUBCOMPACTIONS
        );
        assert_eq!(worker.max_fetch_tasks, LOCATOR_COMPACTION_MAX_FETCH_TASKS);
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

    #[test]
    fn ordinal_lookup_scans_only_large_dense_catalogs() {
        assert!(!should_load_existing_ordinals(204, 32));
        assert!(!should_load_existing_ordinals(
            BULK_ORDINAL_LOOKUP_MIN_CATALOG_OBJECTS,
            63,
        ));
        assert!(should_load_existing_ordinals(1_000_000, 20_000));
        assert!(!should_load_existing_ordinals(1_000_000, 10_000));
    }

    #[test]
    fn existing_ordinal_index_updates_sorted_rows_without_duplicate_storage() {
        let first = [1; 20];
        let second = [2; 20];
        let new = [3; 20];
        let mut index = ExistingOrdinalIndex::from_sorted_entries(vec![
            (
                first,
                ExistingObject {
                    ordinal: GitObjectOrdinal::try_from(1).expect("ordinal"),
                    pack_slot: 10,
                    metadata: GitObjectMetadata::default(),
                },
            ),
            (
                second,
                ExistingObject {
                    ordinal: GitObjectOrdinal::try_from(2).expect("ordinal"),
                    pack_slot: 10,
                    metadata: GitObjectMetadata::default(),
                },
            ),
        ]);

        index.insert(
            first,
            ExistingObject {
                ordinal: GitObjectOrdinal::try_from(1).expect("ordinal"),
                pack_slot: 20,
                metadata: GitObjectMetadata::default(),
            },
        );
        index.insert(
            new,
            ExistingObject {
                ordinal: GitObjectOrdinal::try_from(3).expect("ordinal"),
                pack_slot: 20,
                metadata: GitObjectMetadata::default(),
            },
        );

        assert_eq!(index.get(&first).map(|object| object.pack_slot), Some(20));
        assert_eq!(index.get(&second).map(|object| object.pack_slot), Some(10));
        assert_eq!(index.get(&new).map(|object| object.pack_slot), Some(20));
        assert_eq!(index.updates.len(), 1);
        assert_eq!(index.len(), 3);

        index.remove(&second);
        index.remove(&new);
        assert!(index.get(&second).is_none());
        assert!(index.get(&new).is_none());
        assert_eq!(index.len(), 1);
    }

    #[tokio::test]
    async fn publication_hint_primes_existing_ordinals_before_first_rebind() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut initial = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open initial writer");
        let original = GitPackLocatorRecord {
            pack_id: hash(1),
            committed_generation: 1,
            pack_index_hash: hash(11),
            object_count: 4_096,
            pack_size: 128,
        };
        let binding = initial
            .bind_packs(&[original])
            .await
            .expect("bind original")[0];
        let entries = (1..=4_096).map(generation_entry).collect::<Vec<_>>();
        initial
            .write_locations(binding, &entries)
            .await
            .expect("write original catalog");
        initial
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: original.pack_index_hash,
            })
            .await
            .expect("cover original catalog");
        initial.close().await.expect("close initial writer");

        let mut writer =
            GitObjectLocatorWriter::open_for_publication(Arc::clone(&store), "org/repo", 64)
                .await
                .expect("open planned publication writer");
        let replacement = GitPackLocatorRecord {
            pack_id: hash(2),
            committed_generation: 2,
            pack_index_hash: hash(12),
            object_count: 64,
            pack_size: 128,
        };
        let binding = writer
            .bind_packs(&[replacement])
            .await
            .expect("bind replacement")[0];
        writer
            .write_locations(binding, &[generation_entry(5_000)])
            .await
            .expect("write first replacement object");

        assert!(writer.existing_ordinals.is_some());
        writer.close().await.expect("close planned writer");
    }

    #[tokio::test]
    async fn checkpoint_marker_put_retries_throttling() {
        let store = Arc::new(FailFirstPutStore::new());
        let store_handle: Arc<dyn ObjectStore> = store.clone();
        let mut writer = GitObjectLocatorWriter::open(store_handle, "org/repo")
            .await
            .expect("open writer");
        let pack = pack(1);
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &[entry(1)])
            .await
            .expect("write object");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: pack.pack_index_hash,
            })
            .await
            .expect("set coverage");
        let identity = writer.catalog_identity().expect("catalog identity");
        let marker_path = ObjectPath::from(super::super::catalog_checkpoint_marker_path(
            "org/repo",
            identity.catalog_digest,
        ));
        store.fail_next_put_at(&marker_path);

        writer.close().await.expect("close writer after retry");
        assert!(!store.fail_next_put.load(Ordering::Acquire));
        assert!(store.inner.head(&marker_path).await.is_ok());
    }

    #[test]
    fn existing_object_lookup_scans_when_sst_amplification_dominates() {
        assert!(should_scan_existing_objects(12, 52, 14));
        assert!(should_scan_existing_objects(4, 52, 12));
        assert!(!should_scan_existing_objects(4, 52, 4));
        assert!(should_scan_existing_objects(
            1_024,
            EXISTING_LOOKUP_SCAN_SMALL_CATALOG_MAX_OBJECTS,
            EXISTING_LOOKUP_SCAN_MIN_SSTS,
        ));
        assert!(!should_scan_existing_objects(12, 52, 3));
        assert!(!should_scan_existing_objects(64, 100_000, 22));
    }

    #[tokio::test]
    async fn existing_object_scan_preserves_request_order_and_missing_rows() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open_for_incremental_publication(
            Arc::clone(&store),
            "org/repo",
        )
        .await
        .expect("open writer");

        for seed in 1..=4 {
            let binding = writer.bind_packs(&[pack(seed)]).await.expect("bind pack")[0];
            writer
                .write_locations(binding, &[entry(seed as u8)])
                .await
                .expect("write object");
        }
        // Flush the fourth object into the fourth immutable SST without
        // adding another catalog row. This forces the adaptive scan branch.
        writer
            .bind_packs(&[pack(5)])
            .await
            .expect("flush locator rows");

        let existing = writer
            .lookup_existing_objects(&[entry(4), entry(2), entry(5)])
            .await
            .expect("scan existing objects");
        assert_eq!(existing[0].map(|object| object.ordinal), Some(3));
        assert_eq!(existing[1].map(|object| object.ordinal), Some(1));
        assert!(existing[2].is_none());
        writer.close().await.expect("close writer");
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
    async fn duplicate_oid_in_one_pack_batch_is_rejected_before_publication() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack(1)]).await.expect("bind pack")[0];

        let error = writer
            .write_locations(binding, &[entry(1), entry(1)])
            .await
            .expect_err("reject duplicate OID");

        assert!(error.to_string().contains("duplicate OID"));
        writer.close().await.expect("close writer");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "large-repository qualification stress"]
    async fn thousand_incremental_publications_keep_layers_bounded_and_catalog_exact() {
        const PUBLICATIONS: u64 = 1_000;

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let mut first_identity = None;
        for generation in 1..=PUBLICATIONS {
            let record = GitPackLocatorRecord {
                pack_id: hash(generation),
                committed_generation: generation,
                pack_index_hash: hash(10_000 + generation),
                object_count: 1,
                pack_size: 128,
            };
            let binding = writer.bind_packs(&[record]).await.expect("bind pack")[0];
            let rows_before = writer.stats.object_rows_written;
            writer
                .write_locations(binding, &[generation_entry(generation)])
                .await
                .expect("append one catalog object");
            writer
                .set_coverage(GitLocatorCoverage {
                    generation,
                    pack_index_hash: record.pack_index_hash,
                })
                .await
                .expect("advance catalog coverage");
            writer
                .publish_checkpoint()
                .await
                .expect("publish catalog checkpoint");
            assert_eq!(writer.stats.object_rows_written - rows_before, 1);
            first_identity.get_or_insert_with(|| writer.catalog_identity().expect("identity"));
        }

        let latest_identity = writer.catalog_identity().expect("latest identity");
        let stats = writer.catalog_stats().await.expect("catalog stats");
        let logarithmic_layer_bound = 4 * u64::from(u64::BITS - PUBLICATIONS.leading_zeros());
        assert_eq!(stats.object_count, PUBLICATIONS);
        assert!(
            stats.active_layers <= logarithmic_layer_bound,
            "{} active layers exceed logarithmic bound {logarithmic_layer_bound}",
            stats.active_layers
        );
        writer.close().await.expect("close writer");

        let first = crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
            Arc::clone(&store),
            "org/repo",
            first_identity.expect("first identity"),
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("open first checkpoint");
        assert_eq!(
            first.all_object_ids_and_close().await.expect("first IDs"),
            vec![generation_entry(1).oid]
        );

        let latest = crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
            store,
            "org/repo",
            latest_identity,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("open latest checkpoint");
        let expected = (1..=PUBLICATIONS)
            .map(|generation| generation_entry(generation).oid)
            .collect::<Vec<_>>();
        assert_eq!(
            latest.all_object_ids_and_close().await.expect("latest IDs"),
            expected
        );
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
        let published_identity = initial.catalog_identity().expect("published identity");
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
        interrupted
            .write_locations(uncovered, &[entry(2)])
            .await
            .expect("write interrupted object");
        assert!(interrupted.binding_has_covered_objects(retained));
        assert!(!interrupted.binding_has_covered_objects(uncovered));
        interrupted.close().await.expect("close interrupted writer");

        let reopened = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("reopen writer");
        assert!(reopened.binding_has_covered_objects(retained));
        assert!(!reopened.binding_has_covered_objects(uncovered));
        reopened.close().await.expect("close reopened writer");

        let published = crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
            store,
            "org/repo",
            published_identity,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("open published checkpoint");
        assert_eq!(
            published
                .all_object_ids_and_close()
                .await
                .expect("published IDs"),
            vec![entry(1).oid]
        );
    }

    #[tokio::test]
    async fn sweep_preserves_locator_coverage_for_retained_pack() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let retained = writer.bind_packs(&[pack(1)]).await.expect("bind retained")[0];
        writer
            .write_locations(retained, &[entry(1)])
            .await
            .expect("write retained location");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: hash(100),
            })
            .await
            .expect("cover retained pack");

        let interrupted = writer
            .bind_packs(&[pack(2)])
            .await
            .expect("bind interrupted")[0];
        writer
            .write_locations(interrupted, &[entry(2)])
            .await
            .expect("write interrupted location");
        let sweep = writer
            .sweep_unreferenced(&HashSet::from([retained.pack_slot]))
            .await
            .expect("sweep interrupted pack");

        assert_eq!(sweep.pack_rows_deleted, 1);
        assert!(writer.binding_has_covered_objects(retained));
        assert!(!writer.binding_has_covered_objects(interrupted));
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn sweep_after_rewriting_all_objects_preserves_dense_catalog() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let original = writer.bind_packs(&[pack(1)]).await.expect("bind original")[0];
        writer
            .write_locations(original, &[entry(1)])
            .await
            .expect("write original location");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: hash(100),
            })
            .await
            .expect("cover original pack");
        writer
            .load_existing_ordinals()
            .await
            .expect("load existing ordinals before repack");

        let repacked = writer.bind_packs(&[pack(2)]).await.expect("bind repacked")[0];
        let mut moved = entry(1);
        moved.location.pack_offset = 24;
        moved.location.entry_len = 80;
        writer
            .write_locations(repacked, &[moved])
            .await
            .expect("write repacked location");
        let sweep = writer
            .sweep_unreferenced(&HashSet::from([repacked.pack_slot]))
            .await
            .expect("sweep original pack");

        assert_eq!(sweep.object_rows_deleted, 0);
        assert_eq!(sweep.object_rows_scanned, 0);
        assert_eq!(sweep.pack_rows_deleted, 1);
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 2,
                pack_index_hash: hash(101),
            })
            .await
            .expect("cover repacked pack");
        assert_eq!(
            writer
                .catalog_identity()
                .expect("catalog identity")
                .object_count,
            1
        );
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn interrupted_catalog_rebuild_replays_before_membership_ready() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut initial = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open initial writer");
        let original = initial.bind_packs(&[pack(1)]).await.expect("bind original")[0];
        initial
            .write_locations(original, &[entry(1)])
            .await
            .expect("write original location");
        initial
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: hash(100),
            })
            .await
            .expect("cover original pack");
        initial.close().await.expect("close initial writer");

        let mut interrupted = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open interrupted writer");
        let replacement = interrupted
            .bind_packs(&[pack(2)])
            .await
            .expect("bind replacement")[0];
        interrupted
            .write_locations(replacement, &[entry(2)])
            .await
            .expect("write replacement location");
        let sweep = interrupted
            .sweep_unreferenced(&HashSet::from([replacement.pack_slot]))
            .await
            .expect("sweep original pack");
        assert_eq!(sweep.object_rows_deleted, 1);
        interrupted
            .replace_object_catalog(&HashSet::from([replacement.pack_slot]))
            .await
            .expect("start catalog rebuild");
        assert!(!interrupted.pack_membership_index_ready);
        let error = interrupted
            .complete_object_catalog_rebuild()
            .await
            .expect_err("incomplete catalog rebuild must not become ready");
        assert!(error.to_string().contains("missing 1 objects"));
        interrupted
            .close()
            .await
            .expect("close interrupted rebuild");

        let mut resumed = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("reopen interrupted rebuild");
        assert!(!resumed.pack_membership_index_ready);
        resumed
            .write_locations(replacement, &[entry(2)])
            .await
            .expect("replay replacement location");
        let error = resumed
            .set_coverage(GitLocatorCoverage {
                generation: 2,
                pack_index_hash: hash(101),
            })
            .await
            .expect_err("coverage must wait for membership completion");
        assert!(error.to_string().contains("rebuild completion"));
        let recovery = resumed
            .sweep_unreferenced(&HashSet::from([replacement.pack_slot]))
            .await
            .expect("recover rebuilding membership index");
        assert_eq!(recovery.object_rows_scanned, 1);
        resumed
            .complete_object_catalog_rebuild()
            .await
            .expect("complete catalog rebuild");
        resumed
            .set_coverage(GitLocatorCoverage {
                generation: 2,
                pack_index_hash: hash(101),
            })
            .await
            .expect("cover replayed pack");
        let identity = resumed.catalog_identity().expect("replayed identity");
        resumed.close().await.expect("close resumed writer");

        let reader = super::super::GitObjectLocatorSession::open_for_catalog(
            store,
            "org/repo",
            identity,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("open replayed catalog");
        assert_eq!(
            reader.all_object_ids().await.expect("replayed object IDs"),
            vec![entry(2).oid]
        );
        reader.close().await.expect("close replayed reader");
    }

    #[tokio::test]
    async fn rebound_pack_preserves_proven_object_metadata() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let original = writer.bind_packs(&[pack(1)]).await.expect("bind original")[0];
        let mut known = entry(1);
        known.metadata = GitObjectMetadata {
            kind: Some(GitObjectKind::Commit),
            logical_size: Some(128),
            delta_base_oid: Some([2; 20]),
        };
        writer
            .write_locations(original, &[known])
            .await
            .expect("write original location");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 1,
                pack_index_hash: hash(100),
            })
            .await
            .expect("cover original pack");
        writer
            .publish_checkpoint()
            .await
            .expect("publish original checkpoint");

        let repacked = writer.bind_packs(&[pack(2)]).await.expect("bind repacked")[0];
        let mut moved = entry(1);
        moved.location.pack_offset = 24;
        moved.location.entry_len = 80;
        writer
            .write_locations(repacked, &[moved])
            .await
            .expect("write rebound location");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 2,
                pack_index_hash: hash(101),
            })
            .await
            .expect("cover rebound pack");
        writer
            .publish_checkpoint()
            .await
            .expect("publish rebound checkpoint");
        let identity = writer.catalog_identity().expect("rebound identity");
        writer.close().await.expect("close writer");

        let reader = super::super::GitObjectLocatorSession::open_for_catalog(
            store,
            "org/repo",
            identity,
            std::time::Duration::from_secs(60),
        )
        .await
        .expect("open rebound catalog");
        let inventory = HashMap::from([(
            repacked.record.pack_id,
            GitPackInventoryEntry {
                pack_id: repacked.record.pack_id,
                object_count: repacked.record.object_count,
                pack_size: repacked.record.pack_size,
            },
        )]);
        let lookups = reader
            .lookup_batch(&[known.oid], &inventory)
            .await
            .expect("lookup rebound object");
        match lookups.as_slice() {
            [GitObjectLookup::Hit(locator)] => assert_eq!(locator.metadata, known.metadata),
            other => panic!("expected one rebound locator hit, got {other:?}"),
        }
        reader.close().await.expect("close rebound reader");
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
        let first_identity = writer.catalog_identity().expect("first catalog identity");

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
        let second_identity = writer.catalog_identity().expect("second catalog identity");
        let stats = writer.catalog_stats().await.expect("catalog stats");
        assert_ne!(
            first_identity.catalog_digest,
            second_identity.catalog_digest
        );
        assert_eq!(stats.object_count, 2);
        assert_eq!(stats.checkpoints, 2);
        assert!(stats.active_layers > 0);
        assert!(stats.active_ssts >= stats.active_layers);
        assert!(stats.active_bytes > 0);
        assert!(writer.binding_has_covered_objects(first));
        assert!(writer.binding_has_covered_objects(second));
        let checkpoints = slatedb::admin::AdminBuilder::new(
            ObjectPath::from(git_object_locator_path("org/repo")),
            Arc::clone(&store),
        )
        .build()
        .list_checkpoints(None)
        .await
        .expect("list catalog checkpoints");
        let first_name = super::super::catalog_checkpoint_name(first_identity.catalog_digest);
        let second_name = super::super::catalog_checkpoint_name(second_identity.catalog_digest);
        assert!(checkpoints.iter().any(|checkpoint| {
            checkpoint.name.as_deref() == Some(first_name.as_str())
                && checkpoint.expire_time.is_some()
        }));
        assert!(checkpoints.iter().any(|checkpoint| {
            checkpoint.name.as_deref() == Some(second_name.as_str())
                && checkpoint.expire_time.is_none()
        }));
        let first_marker = ObjectPath::from(super::super::catalog_checkpoint_marker_path(
            "org/repo",
            first_identity.catalog_digest,
        ));
        let second_marker = ObjectPath::from(super::super::catalog_checkpoint_marker_path(
            "org/repo",
            second_identity.catalog_digest,
        ));
        assert!(matches!(
            store.head(&first_marker).await,
            Err(object_store::Error::NotFound { .. })
        ));
        assert!(store.head(&second_marker).await.is_ok());

        let second_reader =
            super::super::GitObjectLocatorSession::open(Arc::clone(&store), "org/repo")
                .await
                .expect("open second reader");
        assert_eq!(second_reader.coverage(), Some(second_coverage));
        assert_eq!(
            second_reader
                .all_object_ids()
                .await
                .expect("second objects"),
            vec![[1; 20], [2; 20]]
        );
        second_reader.close().await.expect("close second reader");

        let first_reader = super::super::GitObjectLocatorSession::open_for_catalog(
            Arc::clone(&store),
            "org/repo",
            first_identity,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("open exact first catalog");
        assert_eq!(
            first_reader.all_object_ids().await.expect("first objects"),
            vec![[1; 20]]
        );
        first_reader
            .close()
            .await
            .expect("close exact first reader");
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

        let prefix = ObjectPath::from("org/repo/git_object_catalog_db");
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
    async fn stale_pack_sweep_uses_pack_membership_rows() {
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
            .expect("write retained location");
        writer
            .write_locations(bindings[1], &[entry(2)])
            .await
            .expect("write stale location");
        let stats = writer
            .sweep_unreferenced(&HashSet::from([bindings[0].pack_slot]))
            .await
            .expect("sweep stale pack");
        assert_eq!(stats.object_rows_scanned, 1);
        assert_eq!(stats.object_rows_deleted, 1);

        let stats = writer
            .sweep_unreferenced(&HashSet::from([bindings[0].pack_slot]))
            .await
            .expect("repeat stale sweep");
        assert_eq!(stats, LocatorSweepStats::default());
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn missing_pack_membership_marker_rebuilds_once() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack(1)]).await.expect("bind pack")[0];
        writer.pack_membership_index_ready = false;
        writer
            .write_locations(binding, &[entry(1)])
            .await
            .expect("write location");
        let mut marker_delete = slatedb::WriteBatch::new();
        marker_delete.delete(PACK_OBJECT_INDEX_MARKER_KEY);
        write_batch(&writer.db, marker_delete, "remove legacy membership marker")
            .await
            .expect("remove legacy membership marker");
        writer.writes_durable = false;
        writer
            .flush_objects()
            .await
            .expect("flush legacy marker removal");
        writer.close().await.expect("close legacy writer");

        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("reopen legacy writer");
        assert!(!writer.pack_membership_index_ready);

        let stats = writer
            .sweep_unreferenced(&HashSet::from([binding.pack_slot]))
            .await
            .expect("rebuild pack membership index");
        assert_eq!(stats.object_rows_scanned, 1);
        assert_eq!(stats.object_rows_deleted, 0);
        assert!(writer.pack_membership_index_ready);
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
    async fn coverage_batches_pending_object_rows_into_one_durability_flush() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let coverage = GitLocatorCoverage {
            generation: 3,
            pack_index_hash: hash(9),
        };
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack(1)]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &[entry(1)])
            .await
            .expect("write location");
        let before_coverage = writer.stats.flushes;

        writer.set_coverage(coverage).await.expect("set coverage");

        assert_eq!(writer.stats.flushes, before_coverage + 1);
        writer.close().await.expect("close writer");
    }

    #[tokio::test]
    async fn checkpoint_publishes_dirty_object_rows() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let pack = pack(1);
        let object = entry(1);
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &[object])
            .await
            .expect("write location");
        writer
            .publish_checkpoint()
            .await
            .expect("publish checkpoint");

        let reader = super::super::GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let inventory = std::collections::HashMap::from([(
            pack.pack_id,
            GitPackInventoryEntry {
                pack_id: pack.pack_id,
                object_count: pack.object_count,
                pack_size: pack.pack_size,
            },
        )]);
        assert!(matches!(
            reader.lookup_batch(&[object.oid], &inventory).await,
            Ok(lookups) if matches!(lookups.as_slice(), [GitObjectLookup::Hit(locator)] if locator.location == object.location)
        ));
        reader.close().await.expect("close reader");
        writer.close().await.expect("close writer");
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
