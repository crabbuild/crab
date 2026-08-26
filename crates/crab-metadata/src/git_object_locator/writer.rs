use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use crab_xet::hash::MerkleHash;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use slatedb::config::{
    CheckpointOptions, CheckpointScope, CompactorOptions, CompressionCodec,
    GarbageCollectorOptions, Settings, WriteOptions,
};
use tracing::{debug, warn};

use super::format::{
    LocatorMetadata, METADATA_KEY, OBJECT_FAMILY, ORDINAL_METADATA_FAMILY, PACK_FAMILY,
    StoredObjectLocation, coverage, decode_metadata, decode_object_key, decode_object_location,
    decode_pack_key, decode_pack_record, encode_metadata, encode_object_location,
    encode_object_metadata, encode_pack_record, object_key, ordinal_key, ordinal_metadata_key,
    pack_key, validate_location_for_pack,
};
use super::{
    GitLocatorCoverage, GitObjectCatalogIdentity, GitObjectCatalogStats, GitObjectLocatorEntry,
    GitObjectOrdinal, GitPackLocatorBinding, GitPackLocatorRecord, git_object_locator_path,
};
use crate::error::{MetadataError, Result};

const DB_LABEL: &str = "git_object_catalog_db";
const MAX_BATCH_ROWS: usize = 25_000;
const MAX_BATCH_LOGICAL_BYTES: usize = 2 * 1024 * 1024;
const LOCATOR_L0_SST_BYTES: usize = 64 * 1024 * 1024;
const LOCATOR_L0_MAX_SSTS: usize = 32;
const LOCATOR_COMPACTION_TRIGGER_SSTS: usize = LOCATOR_L0_MAX_SSTS / 2;
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
const RETIRED_CHECKPOINT_LIFETIME: std::time::Duration =
    std::time::Duration::from_secs(2 * 60 * 60);

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
    replacement_ordinals: Option<HashMap<[u8; 20], GitObjectOrdinal>>,
    existing_ordinals: Option<HashMap<[u8; 20], GitObjectOrdinal>>,
    ordinal_lookup_candidates: u64,
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
            if let Err(error) = write_batch(
                &db,
                metadata_batch(metadata),
                "initialize compact locator metadata",
            )
            .await
            {
                return Err((db, error));
            }
            metadata
        };

        let bindings = match load_bindings(&db).await {
            Ok(bindings) => bindings,
            Err(error) => return Err((db, error)),
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

        self.prepare_ordinal_lookup(entries.len()).await?;

        let mut batch = slatedb::WriteBatch::new();
        let mut batch_rows = 0_usize;
        let mut batch_bytes = 0_usize;
        let mut submitted = HashSet::with_capacity(entries.len());
        for entry in entries {
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
            let existing_ordinal = self
                .replacement_ordinals
                .as_ref()
                .and_then(|ordinals| ordinals.get(&entry.oid).copied());
            let existing_ordinal = existing_ordinal.or_else(|| {
                self.existing_ordinals
                    .as_ref()
                    .and_then(|ordinals| ordinals.get(&entry.oid).copied())
            });
            let existing = if existing_ordinal.is_some()
                || self.existing_ordinals.is_some()
                || self.empty_catalog_binding == Some(binding.pack_slot)
            {
                None
            } else {
                self.db.get(key).await.map_err(read_error)?
            };
            let ordinal = match (existing_ordinal, existing) {
                (Some(ordinal), _) => ordinal,
                (None, Some(value)) => {
                    decode_object_location(&value)
                        .ok_or_else(|| corrupt("object", "invalid Git catalog object location"))?
                        .ordinal
                }
                (None, None) => {
                    let ordinal = self.allocate_ordinal()?;
                    if let Some(ordinals) = &mut self.replacement_ordinals {
                        ordinals.insert(entry.oid, ordinal);
                    }
                    if let Some(ordinals) = &mut self.existing_ordinals {
                        ordinals.insert(entry.oid, ordinal);
                    }
                    ordinal
                }
            };
            let value = encode_object_location(StoredObjectLocation {
                ordinal,
                pack_slot: binding.pack_slot,
                pack_offset: entry.location.pack_offset,
                entry_len: entry.location.entry_len,
                crc32: entry.location.crc32,
                metadata: entry.metadata,
            });
            batch.put(key, value);
            batch.put(ordinal_key(ordinal), entry.oid);
            let metadata_key = ordinal_metadata_key(ordinal);
            let metadata_value = encode_object_metadata(entry.metadata);
            batch.put(metadata_key, metadata_value);
            batch_rows += 1;
            batch_bytes += key.len()
                + value.len()
                + ordinal_key(ordinal).len()
                + entry.oid.len()
                + metadata_key.len()
                + metadata_value.len();
            if batch_rows >= MAX_BATCH_ROWS || batch_bytes >= MAX_BATCH_LOGICAL_BYTES {
                batch.put(METADATA_KEY, encode_metadata(self.metadata));
                write_batch(&self.db, batch, "write compact locator objects").await?;
                self.record_object_batch(batch_rows, batch_bytes);
                batch = slatedb::WriteBatch::new();
                batch_rows = 0;
                batch_bytes = 0;
            }
        }
        if batch_rows != 0 {
            batch.put(METADATA_KEY, encode_metadata(self.metadata));
            write_batch(&self.db, batch, "write compact locator objects").await?;
            self.record_object_batch(batch_rows, batch_bytes);
        }
        if !entries.is_empty() {
            self.catalog_dirty = true;
            self.checkpoint_required = true;
        }
        Ok(())
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
            self.existing_ordinals = Some(HashMap::new());
        } else if self
            .ordinal_lookup_candidates
            .saturating_mul(BULK_ORDINAL_LOOKUP_FACTOR)
            >= current_objects
        {
            self.load_existing_ordinals().await?;
        }
        Ok(())
    }

    async fn load_existing_ordinals(&mut self) -> Result<()> {
        if self.existing_ordinals.is_some() {
            return Ok(());
        }
        let mut rows = self
            .db
            .scan_prefix([OBJECT_FAMILY], ..)
            .await
            .map_err(read_error)?;
        let mut ordinals = HashMap::new();
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
            ordinals.insert(oid, location.ordinal);
        }
        self.existing_ordinals = Some(ordinals);
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

    /// Replace the current object/ordinal universe while retaining pack slots.
    ///
    /// Historical checkpoints retain the prior universe. The caller must
    /// rewrite every current pack before advancing coverage.
    pub async fn replace_object_catalog(&mut self) -> Result<()> {
        for family in [
            OBJECT_FAMILY,
            super::format::ORDINAL_FAMILY,
            ORDINAL_METADATA_FAMILY,
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
        write_batch(
            &self.db,
            metadata_batch(self.metadata),
            "reset Git object catalog metadata",
        )
        .await?;
        self.writes_durable = false;
        self.flush_objects().await?;
        self.empty_catalog_binding = None;
        self.replacement_ordinals = Some(HashMap::new());
        self.existing_ordinals = None;
        self.ordinal_lookup_candidates = 0;
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
                if let Some(ordinals) = &mut self.existing_ordinals
                    && let Some(oid) = decode_object_key(&row.key)
                {
                    ordinals.remove(&oid);
                }
                deletes.delete(row.key);
                deletes.delete(ordinal_key(location.ordinal));
                deletes.delete(ordinal_metadata_key(location.ordinal));
                delete_count += 1;
                stats.object_rows_deleted = stats.object_rows_deleted.saturating_add(1);
                self.catalog_dirty = true;
                self.checkpoint_required = true;
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
                self.catalog_dirty = true;
                self.checkpoint_required = true;
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

    /// Mark one immutable manifest inventory fully covered with one durability flush.
    pub async fn set_coverage(&mut self, coverage: GitLocatorCoverage) -> Result<()> {
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
        write_batch(
            &self.db,
            metadata_batch(metadata),
            "write compact locator coverage",
        )
        .await?;
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
            self.store
                .put(&marker_path, Bytes::from(marker_body).into())
                .await
                .map_err(|source| MetadataError::ObjectStore { source })?;
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
    match store.head(&path).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(source) => Err(MetadataError::ObjectStore { source }),
    }
}

async fn retire_old_catalog_checkpoints(
    path: &str,
    store: Arc<dyn ObjectStore>,
    current: &slatedb::CheckpointCreateResult,
    current_name: &str,
) -> Result<()> {
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
            match store.delete(&marker_path).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(source) => return Err(MetadataError::ObjectStore { source }),
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
    use crate::git_object_locator::{GitObjectLocation, GitObjectLookup, GitPackInventoryEntry};
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
