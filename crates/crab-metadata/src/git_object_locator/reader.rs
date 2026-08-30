use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::stream::{self, StreamExt, TryStreamExt};
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use slatedb::DbReaderMode;
use slatedb::config::{DbReaderOptions, ScanOptions};
use slatedb::db_cache::foyer::{FoyerCache, FoyerCacheOptions};

use super::format::{
    METADATA_KEY, OBJECT_FAMILY, ORDINAL_FAMILY, ORDINAL_METADATA_FAMILY, PACK_BINDINGS_KEY,
    PACK_FAMILY, decode_metadata, decode_object_key, decode_object_location,
    decode_object_metadata, decode_ordinal_key, decode_ordinal_metadata_key, decode_pack_bindings,
    decode_pack_key, decode_pack_record, object_key, ordinal_key, ordinal_metadata_key,
    validate_location_for_pack,
};
use super::{
    GitLocatorCoverage, GitObjectCatalogIdentity, GitObjectLocation, GitObjectLocator,
    GitObjectOrdinal, GitPackInventoryEntry, GitPackLocatorBinding, GitPackLocatorRecord,
    git_object_locator_path,
};
use crate::error::{MetadataError, Result};

const DB_LABEL: &str = "git_object_catalog_db";
const LOOKUP_CONCURRENCY: usize = 256;
// A scan must replace one full point-read wave and may inspect at most two
// rows per requested object before the exact-key path becomes cheaper.
const MIN_SCAN_LOOKUP_OBJECTS: usize = LOOKUP_CONCURRENCY;
const MAX_SCAN_AMPLIFICATION: usize = 2;
// Large shallow closures are cheaper to resolve with one bounded sequential
// scan than with tens of thousands of independent OID point reads.
const FULL_SCAN_MIN_LOOKUP_OBJECTS: usize = MIN_SCAN_LOOKUP_OBJECTS * 16;
const FULL_SCAN_REQUEST_RATIO: u64 = 64;
// Small catalogs are common for newly created repositories. Once their
// compacted SST fan-out makes a point-read wave more expensive than one
// bounded pass, scan the catalog even when the requested SHA range is broad.
const SMALL_CATALOG_MAX_OBJECTS: u64 = 4_096;
const SMALL_CATALOG_REQUEST_RATIO: u64 = 4;
const MIN_AMPLIFIED_SCAN_SSTS: u64 = 4;
const MIN_LAYERED_SCAN_SSTS: u64 = 3;
const AMPLIFIED_SCAN_COST_RATIO: u64 = 2;
// Git SHA-1 object IDs are uniformly distributed in their key space. A
// min/max range spanning most of that space turns a scan into a catalog-wide
// read, even when the request contains only a small shallow closure.
const MAX_SCAN_KEYSPAN_DIVISOR: u64 = 16;
// A request this close to the complete catalog is cheaper to serve with one
// sequential pass regardless of the requested IDs' key span.
const FULL_SCAN_MIN_COVERAGE_DIVISOR: u64 = 8;
const SCAN_READ_AHEAD_BYTES: usize = 2 * 1024 * 1024;
const SCAN_FETCH_TASKS: usize = 4;
// Catalog reads materialize one immutable dense dictionary. A larger
// read-ahead window and more fetch tasks keep that sequential transfer from
// degenerating into one object-store round trip per SlateDB block.
const CATALOG_SCAN_READ_AHEAD_BYTES: usize = 16 * 1024 * 1024;
const CATALOG_SCAN_FETCH_TASKS: usize = 8;
const ORDINAL_SCAN_READ_AHEAD_BYTES: usize = 16 * 1024 * 1024;
const ORDINAL_SCAN_FETCH_TASKS: usize = 8;
// One cache is private to one short-lived reader process. This keeps 32
// concurrent fetchers at a 512 MiB aggregate ceiling instead of SlateDB's
// 20 GiB default while still coalescing repeated SST metadata/block reads.
const SESSION_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const SESSION_CACHE_SHARDS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupStrategy {
    Exact,
    Scan { row_limit: usize },
    FullScan { row_limit: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OidKeySpan {
    Narrow,
    Broad,
}

/// Result of validating one compact row against a pinned pack inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitObjectLookup {
    /// No usable current row exists for this snapshot.
    Miss,
    /// The row joins to an immutable pack in the pinned inventory.
    Hit(GitObjectLocator),
    /// The compact row or its referenced pack record is malformed.
    Corrupt,
}

/// Read-only session for exact compact Git locator queries.
pub struct GitObjectLocatorSession {
    reader: Option<Arc<slatedb::DbReader>>,
    identity: Option<GitObjectCatalogIdentity>,
    bindings: HashMap<u64, GitPackLocatorRecord>,
    active_ssts: u64,
}

impl GitObjectLocatorSession {
    /// Open the compact locator, treating an absent database as an empty index.
    ///
    /// A published locator checkpoint is opened explicitly so a read-only
    /// session does not create or refresh durable reader state. A legacy
    /// database with no checkpoint uses SlateDB's compatibility path.
    pub async fn open(store: Arc<dyn ObjectStore>, repo_prefix: &str) -> Result<Self> {
        Self::open_with_published_checkpoint(store, repo_prefix, locator_reader_options()).await
    }

    /// Open a locator whose SlateDB checkpoint cannot refresh before `minimum`.
    ///
    /// The caller must close the session before `minimum` elapses. This keeps
    /// coverage, pack bindings, and object rows on one immutable manifest. A
    /// published locator checkpoint is opened explicitly, so the operation
    /// does not create or refresh durable reader state. A legacy database with
    /// no checkpoint uses SlateDB's one-time compatibility checkpoint path.
    pub async fn open_for_operation(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        minimum: Duration,
    ) -> Result<Self> {
        let manifest_poll_interval = minimum
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| MetadataError::Internal("locator operation duration overflow".into()))?;
        let checkpoint_lifetime = manifest_poll_interval.checked_mul(2).ok_or_else(|| {
            MetadataError::Internal("locator checkpoint duration overflow".into())
        })?;
        let options = DbReaderOptions {
            manifest_poll_interval,
            checkpoint_lifetime,
            ..locator_reader_options()
        };
        Self::open_with_published_checkpoint(store, repo_prefix, options).await
    }

    async fn open_with_published_checkpoint(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        options: DbReaderOptions,
    ) -> Result<Self> {
        let path = git_object_locator_path(repo_prefix);
        let checkpoint = reader_checkpoint_id(Arc::clone(&store), &path, None).await?;
        Self::open_with_checkpoint(store, repo_prefix, options, checkpoint).await
    }

    /// Open the immutable catalog checkpoint named by an exact catalog identity.
    pub async fn open_for_catalog(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        identity: GitObjectCatalogIdentity,
        minimum: Duration,
    ) -> Result<Self> {
        let manifest_poll_interval = minimum
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| MetadataError::Internal("catalog operation duration overflow".into()))?;
        let options = DbReaderOptions {
            manifest_poll_interval,
            checkpoint_lifetime: manifest_poll_interval.checked_mul(2).ok_or_else(|| {
                MetadataError::Internal("catalog checkpoint duration overflow".into())
            })?,
            ..locator_reader_options()
        };
        let path = git_object_locator_path(repo_prefix);
        let checkpoint =
            reader_checkpoint_id(Arc::clone(&store), &path, Some(identity.catalog_digest))
                .await?
                .ok_or_else(|| {
                    corrupt("checkpoint", "published Git catalog checkpoint is missing")
                })?;
        let session =
            Self::open_with_checkpoint(store, repo_prefix, options, Some(checkpoint)).await?;
        if session.identity != Some(identity) {
            return close_after_error(
                session.reader.ok_or_else(|| {
                    MetadataError::Internal("catalog checkpoint reader is absent".to_owned())
                })?,
                corrupt(
                    "metadata",
                    "catalog checkpoint identity does not match its name",
                ),
            )
            .await;
        }
        Ok(session)
    }

    #[cfg(test)]
    async fn open_with_options(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        options: DbReaderOptions,
    ) -> Result<Self> {
        Self::open_with_checkpoint(store, repo_prefix, options, None).await
    }

    async fn open_with_checkpoint(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        options: DbReaderOptions,
        checkpoint: Option<slatedb::Checkpoint>,
    ) -> Result<Self> {
        let path = git_object_locator_path(repo_prefix);
        let mut builder = slatedb::DbReader::builder(ObjectPath::from(path.as_str()), store)
            .with_options(options);
        if let Some(checkpoint) = checkpoint {
            builder = builder.with_reader_mode(DbReaderMode::Checkpoint(checkpoint.id));
        }
        let reader = match builder
            .with_db_cache(Arc::new(FoyerCache::new_with_opts(FoyerCacheOptions {
                max_capacity: SESSION_CACHE_BYTES,
                shards: SESSION_CACHE_SHARDS,
            })))
            .build()
            .await
        {
            Ok(reader) => Arc::new(reader),
            Err(error) if is_manifest_missing(&error) => {
                return Ok(Self {
                    reader: None,
                    identity: None,
                    bindings: HashMap::new(),
                    active_ssts: 0,
                });
            }
            Err(source) => {
                return Err(MetadataError::SlateDbOpen {
                    db: DB_LABEL.to_owned(),
                    path,
                    source,
                });
            }
        };

        let active_ssts = active_sst_count(&reader);
        match load_state(&reader).await {
            Ok((identity, bindings)) => Ok(Self {
                reader: Some(reader),
                identity,
                bindings,
                active_ssts,
            }),
            Err(operation) => close_after_error(reader, operation).await,
        }
    }

    /// Whether the compact locator database exists and passed format validation.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.reader.is_some()
    }

    /// Return the latest fully published manifest inventory, if any.
    #[must_use]
    pub fn coverage(&self) -> Option<GitLocatorCoverage> {
        self.identity.map(|identity| GitLocatorCoverage {
            generation: identity.generation,
            pack_index_hash: identity.pack_index_hash,
        })
    }

    /// Return the exact generation and checkpoint identity pinned by this session.
    #[must_use]
    pub fn catalog_identity(&self) -> Option<GitObjectCatalogIdentity> {
        self.identity
    }

    /// Resolve OID keys and validate every hit against pinned inventory.
    pub async fn lookup_batch(
        &self,
        object_ids: &[[u8; 20]],
        inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<GitObjectLookup>> {
        let Some(reader) = &self.reader else {
            return Ok(vec![GitObjectLookup::Miss; object_ids.len()]);
        };
        let inventory_objects = inventory
            .values()
            .fold(0_u64, |total, pack| total.saturating_add(pack.object_count));
        let mut unique_objects = object_ids.len();
        let key_span = oid_key_span(object_ids);
        let mut strategy = lookup_strategy(
            unique_objects,
            inventory_objects,
            self.active_ssts,
            key_span,
        );
        if !matches!(strategy, LookupStrategy::Exact) {
            unique_objects = object_ids.iter().collect::<HashSet<_>>().len();
            strategy = lookup_strategy(
                unique_objects,
                inventory_objects,
                self.active_ssts,
                key_span,
            );
        }
        let scan = match strategy {
            LookupStrategy::Exact => None,
            LookupStrategy::Scan { row_limit } => {
                Some((row_limit, SCAN_READ_AHEAD_BYTES, SCAN_FETCH_TASKS, "scan"))
            }
            LookupStrategy::FullScan { row_limit } => Some((
                row_limit,
                CATALOG_SCAN_READ_AHEAD_BYTES,
                CATALOG_SCAN_FETCH_TASKS,
                "full_scan",
            )),
        };
        if let Some((row_limit, read_ahead_bytes, fetch_tasks, mode)) = scan {
            tracing::debug!(
                locator_lookup_mode = mode,
                requested_objects = object_ids.len(),
                unique_objects,
                inventory_objects,
                active_ssts = self.active_ssts,
                row_limit,
                "compact Git locator lookup selected"
            );
            if let Some(lookups) = self
                .lookup_batch_by_scan(
                    reader,
                    object_ids,
                    inventory,
                    row_limit,
                    read_ahead_bytes,
                    fetch_tasks,
                    mode,
                )
                .await?
            {
                return Ok(lookups);
            }
            tracing::debug!(
                locator_lookup_mode = "exact_fallback",
                requested_objects = object_ids.len(),
                unique_objects,
                inventory_objects,
                active_ssts = self.active_ssts,
                row_limit,
                "compact Git locator scan exceeded its amplification bound"
            );
        }

        if object_ids.len() >= FULL_SCAN_MIN_LOOKUP_OBJECTS {
            tracing::debug!(
                locator_lookup_mode = "exact",
                requested_objects = object_ids.len(),
                unique_objects,
                inventory_objects,
                active_ssts = self.active_ssts,
                oid_key_span = ?key_span,
                "compact Git locator lookup selected"
            );
        }
        self.lookup_batch_exact(reader, object_ids, inventory).await
    }

    async fn lookup_batch_exact(
        &self,
        reader: &Arc<slatedb::DbReader>,
        object_ids: &[[u8; 20]],
        inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<GitObjectLookup>> {
        let bindings = &self.bindings;
        let unique_object_ids = object_ids.iter().copied().collect::<HashSet<_>>();
        let fetched: Vec<([u8; 20], GitObjectLookup)> =
            stream::iter(unique_object_ids.into_iter().map(|oid| {
                let reader = Arc::clone(reader);
                async move {
                    let value = reader.get(object_key(&oid)).await.map_err(read_error)?;
                    let lookup = value.map_or(GitObjectLookup::Miss, |value| {
                        classify_location(&value, bindings, inventory)
                    });
                    Ok::<_, MetadataError>((oid, lookup))
                }
            }))
            .buffer_unordered(LOOKUP_CONCURRENCY.min(object_ids.len()).max(1))
            .try_collect()
            .await?;

        let fetched = fetched.into_iter().collect::<HashMap<_, _>>();
        Ok(object_ids
            .iter()
            .map(|oid| fetched.get(oid).copied().unwrap_or(GitObjectLookup::Miss))
            .collect())
    }

    async fn lookup_batch_by_scan(
        &self,
        reader: &slatedb::DbReader,
        object_ids: &[[u8; 20]],
        inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
        row_limit: usize,
        read_ahead_bytes: usize,
        fetch_tasks: usize,
        mode: &'static str,
    ) -> Result<Option<Vec<GitObjectLookup>>> {
        let mut requested = object_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect::<Vec<_>>();
        requested.sort_unstable_by_key(|(oid, _)| *oid);
        let Some((first_oid, _)) = requested.first() else {
            return Ok(Some(Vec::new()));
        };
        let last_oid = requested
            .last()
            .map(|(oid, _)| oid)
            .ok_or_else(|| MetadataError::Internal("locator scan lost its request".to_owned()))?;
        let options = ScanOptions::default()
            .with_read_ahead_bytes(read_ahead_bytes)
            .with_max_fetch_tasks(fetch_tasks);
        let mut rows = reader
            .scan_prefix_with_options(
                [OBJECT_FAMILY],
                first_oid.as_slice()..=last_oid.as_slice(),
                &options,
            )
            .await
            .map_err(read_error)?;
        let mut lookups = vec![GitObjectLookup::Miss; object_ids.len()];
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
                lookups[output_index] = classify_location(&row.value, &self.bindings, inventory);
                request_index += 1;
            }
            if request_index == requested.len() {
                break;
            }
        }
        tracing::debug!(
            locator_lookup_mode = mode,
            requested_objects = object_ids.len(),
            rows_scanned,
            "compact Git locator lookup completed"
        );
        Ok(Some(lookups))
    }

    /// Return every validated slot binding in numeric slot order.
    pub async fn pack_bindings(&self) -> Result<Vec<GitPackLocatorBinding>> {
        let mut bindings: Vec<_> = self
            .bindings
            .iter()
            .map(|(pack_slot, record)| GitPackLocatorBinding {
                pack_slot: *pack_slot,
                record: *record,
            })
            .collect();
        bindings.sort_unstable_by_key(|binding| binding.pack_slot);
        Ok(bindings)
    }

    /// Resolve dense catalog ordinals to canonical binary object IDs.
    pub async fn object_ids_by_ordinal(
        &self,
        ordinals: &[GitObjectOrdinal],
    ) -> Result<Vec<Option<[u8; 20]>>> {
        let Some(reader) = &self.reader else {
            return Ok(vec![None; ordinals.len()]);
        };
        if ordinals.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(objects) = self.object_ids_by_ordinal_scan(reader, ordinals).await? {
            return Ok(objects);
        }
        let fetched = stream::iter(
            ordinals
                .iter()
                .copied()
                .enumerate()
                .map(|(index, ordinal)| {
                    let reader = Arc::clone(reader);
                    async move {
                        let value = reader.get(ordinal_key(ordinal)).await.map_err(read_error)?;
                        let oid = value
                            .map(|value| {
                                value.as_ref().try_into().map_err(|_| {
                                    corrupt("ordinal", "invalid Git catalog ordinal object ID")
                                })
                            })
                            .transpose()?;
                        Ok::<_, MetadataError>((index, oid))
                    }
                }),
        )
        .buffer_unordered(LOOKUP_CONCURRENCY.min(ordinals.len()).max(1))
        .try_collect::<Vec<_>>()
        .await?;
        let mut objects = vec![None; ordinals.len()];
        for (index, oid) in fetched {
            objects[index] = oid;
        }
        Ok(objects)
    }

    /// Return complete per-object metadata for the requested dense ordinals.
    ///
    /// `None` means this catalog predates the ordinal metadata sidecar or has
    /// an incomplete sidecar. Callers must use their bounded canonical
    /// traversal path in that case; a present sidecar row with unknown fields
    /// is represented by the corresponding default metadata value.
    pub async fn metadata_by_ordinal(
        &self,
        ordinals: &[GitObjectOrdinal],
    ) -> Result<Option<Vec<super::GitObjectMetadata>>> {
        let Some(reader) = &self.reader else {
            return Ok(None);
        };
        if ordinals.is_empty() {
            return Ok(Some(Vec::new()));
        }
        if let Some(metadata) = self.metadata_by_ordinal_scan(reader, ordinals).await? {
            return Ok(metadata.into_iter().collect());
        }

        let fetched = stream::iter(
            ordinals
                .iter()
                .copied()
                .enumerate()
                .map(|(index, ordinal)| {
                    let reader = Arc::clone(reader);
                    async move {
                        let value = reader
                            .get(ordinal_metadata_key(ordinal))
                            .await
                            .map_err(read_error)?;
                        let metadata = value
                            .map(|value| {
                                decode_object_metadata(&value).ok_or_else(|| {
                                    corrupt(
                                        "ordinal_metadata",
                                        "invalid Git catalog ordinal metadata",
                                    )
                                })
                            })
                            .transpose()?;
                        Ok::<_, MetadataError>((index, metadata))
                    }
                }),
        )
        .buffer_unordered(LOOKUP_CONCURRENCY.min(ordinals.len()).max(1))
        .try_collect::<Vec<_>>()
        .await?;
        let mut metadata = vec![None; ordinals.len()];
        for (index, value) in fetched {
            metadata[index] = value;
        }
        Ok(metadata.into_iter().collect())
    }

    async fn metadata_by_ordinal_scan(
        &self,
        reader: &slatedb::DbReader,
        ordinals: &[GitObjectOrdinal],
    ) -> Result<Option<Vec<Option<super::GitObjectMetadata>>>> {
        let requested = u64::try_from(ordinals.len()).unwrap_or(u64::MAX);
        if ordinals.len() < MIN_SCAN_LOOKUP_OBJECTS {
            return Ok(None);
        }
        let first = *ordinals
            .iter()
            .min()
            .ok_or_else(|| corrupt("ordinal_metadata", "metadata scan lost its request"))?;
        let last = *ordinals
            .iter()
            .max()
            .ok_or_else(|| corrupt("ordinal_metadata", "metadata scan lost its request"))?;
        let span = u64::from(last)
            .saturating_sub(u64::from(first))
            .saturating_add(1);
        if span > requested.saturating_mul(MAX_SCAN_AMPLIFICATION as u64) {
            return Ok(None);
        }
        tracing::debug!(
            locator_lookup_mode = "ordinal_metadata_scan",
            requested_objects = ordinals.len(),
            ordinal_span = span,
            "compact Git ordinal metadata lookup selected"
        );
        let options = ScanOptions::default()
            .with_read_ahead_bytes(ORDINAL_SCAN_READ_AHEAD_BYTES)
            .with_max_fetch_tasks(ORDINAL_SCAN_FETCH_TASKS);
        let mut rows = reader
            .scan_prefix_with_options(
                [ORDINAL_METADATA_FAMILY],
                first.to_be_bytes().as_slice()..=last.to_be_bytes().as_slice(),
                &options,
            )
            .await
            .map_err(read_error)?;
        let mut requested = ordinals.iter().copied().enumerate().collect::<Vec<_>>();
        requested.sort_unstable_by_key(|(_, ordinal)| *ordinal);
        let mut request_index = 0usize;
        let mut rows_scanned = 0u64;
        let mut metadata = vec![None; ordinals.len()];
        while let Some(row) = rows.next().await.map_err(read_error)? {
            rows_scanned = rows_scanned.saturating_add(1);
            if rows_scanned > span {
                return Err(corrupt(
                    "ordinal_metadata",
                    "Git catalog ordinal metadata scan returned too many rows",
                ));
            }
            let ordinal = decode_ordinal_metadata_key(&row.key).ok_or_else(|| {
                corrupt(
                    "ordinal_metadata",
                    "invalid Git catalog ordinal metadata key",
                )
            })?;
            while requested
                .get(request_index)
                .is_some_and(|(_, requested_ordinal)| *requested_ordinal < ordinal)
            {
                request_index += 1;
            }
            while requested
                .get(request_index)
                .is_some_and(|(_, requested_ordinal)| *requested_ordinal == ordinal)
            {
                let (output_index, _) = requested[request_index];
                metadata[output_index] =
                    Some(decode_object_metadata(&row.value).ok_or_else(|| {
                        corrupt("ordinal_metadata", "invalid Git catalog ordinal metadata")
                    })?);
                request_index += 1;
            }
        }
        tracing::debug!(
            locator_lookup_mode = "ordinal_metadata_scan",
            requested_objects = ordinals.len(),
            rows_scanned,
            "compact Git ordinal metadata lookup completed"
        );
        Ok(Some(metadata))
    }

    async fn object_ids_by_ordinal_scan(
        &self,
        reader: &slatedb::DbReader,
        ordinals: &[GitObjectOrdinal],
    ) -> Result<Option<Vec<Option<[u8; 20]>>>> {
        let expected = self.identity.map_or(0, |identity| identity.object_count);
        let requested = u64::try_from(ordinals.len()).unwrap_or(u64::MAX);
        if ordinals.len() < MIN_SCAN_LOOKUP_OBJECTS || expected == 0 {
            return Ok(None);
        }
        let first = *ordinals
            .iter()
            .min()
            .ok_or_else(|| corrupt("ordinal", "ordinal scan lost its request"))?;
        let last = *ordinals
            .iter()
            .max()
            .ok_or_else(|| corrupt("ordinal", "ordinal scan lost its request"))?;
        let span = u64::from(last)
            .saturating_sub(u64::from(first))
            .saturating_add(1);
        if span > requested.saturating_mul(MAX_SCAN_AMPLIFICATION as u64) {
            return Ok(None);
        }
        tracing::debug!(
            locator_lookup_mode = "ordinal_scan",
            requested_objects = ordinals.len(),
            ordinal_span = span,
            "compact Git ordinal lookup selected"
        );
        let options = ScanOptions::default()
            .with_read_ahead_bytes(ORDINAL_SCAN_READ_AHEAD_BYTES)
            .with_max_fetch_tasks(ORDINAL_SCAN_FETCH_TASKS);
        let mut rows = reader
            .scan_prefix_with_options(
                [ORDINAL_FAMILY],
                first.to_be_bytes().as_slice()..=last.to_be_bytes().as_slice(),
                &options,
            )
            .await
            .map_err(read_error)?;
        let mut requested = ordinals.iter().copied().enumerate().collect::<Vec<_>>();
        requested.sort_unstable_by_key(|(_, ordinal)| *ordinal);
        let mut request_index = 0usize;
        let mut rows_scanned = 0u64;
        let mut objects = vec![None; ordinals.len()];
        while let Some(row) = rows.next().await.map_err(read_error)? {
            rows_scanned = rows_scanned.saturating_add(1);
            if rows_scanned > span {
                return Err(corrupt(
                    "ordinal",
                    "Git catalog ordinal scan returned too many rows",
                ));
            }
            let ordinal = decode_ordinal_key(&row.key)
                .ok_or_else(|| corrupt("ordinal", "invalid Git catalog ordinal key"))?;
            while requested
                .get(request_index)
                .is_some_and(|(_, requested_ordinal)| *requested_ordinal < ordinal)
            {
                request_index += 1;
            }
            while requested
                .get(request_index)
                .is_some_and(|(_, requested_ordinal)| *requested_ordinal == ordinal)
            {
                let (output_index, _) = requested[request_index];
                objects[output_index] =
                    Some(row.value.as_ref().try_into().map_err(|_| {
                        corrupt("ordinal", "invalid Git catalog ordinal object ID")
                    })?);
                request_index += 1;
            }
        }
        tracing::debug!(
            locator_lookup_mode = "ordinal_scan",
            requested_objects = ordinals.len(),
            rows_scanned,
            "compact Git ordinal lookup completed"
        );
        Ok(Some(objects))
    }

    /// Read the complete dense OID order from this immutable catalog checkpoint.
    pub async fn all_object_ids(&self) -> Result<Vec<[u8; 20]>> {
        let Some(reader) = &self.reader else {
            return Ok(Vec::new());
        };
        let started = Instant::now();
        let expected = self.identity.map_or(0, |identity| identity.object_count);
        let capacity = usize::try_from(expected)
            .map_err(|_| corrupt("metadata", "catalog object count cannot be represented"))?;
        let mut objects = Vec::with_capacity(capacity);
        let options = ScanOptions::default()
            .with_read_ahead_bytes(CATALOG_SCAN_READ_AHEAD_BYTES)
            .with_max_fetch_tasks(CATALOG_SCAN_FETCH_TASKS);
        let mut rows = reader
            .scan_prefix_with_options([ORDINAL_FAMILY], .., &options)
            .await
            .map_err(read_error)?;
        while let Some(row) = rows.next().await.map_err(read_error)? {
            let ordinal = decode_ordinal_key(&row.key)
                .ok_or_else(|| corrupt("ordinal", "invalid Git catalog ordinal key"))?;
            if usize::try_from(ordinal).ok() != Some(objects.len()) {
                return Err(corrupt(
                    "ordinal",
                    "Git catalog ordinals are not dense and ordered",
                ));
            }
            objects.push(
                row.value
                    .as_ref()
                    .try_into()
                    .map_err(|_| corrupt("ordinal", "invalid Git catalog ordinal object ID"))?,
            );
        }
        if objects.len() != capacity {
            return Err(corrupt(
                "ordinal",
                "Git catalog ordinal count does not match metadata",
            ));
        }
        tracing::info!(
            telemetry_event = "catalog_materialization",
            catalog_objects = objects.len(),
            catalog_materialization_ms = started.elapsed().as_millis() as u64,
            "materialized Git catalog OID dictionary"
        );
        Ok(objects)
    }

    /// Read the complete catalog order and close the pinned SlateDB reader.
    pub async fn all_object_ids_and_close(self) -> Result<Vec<[u8; 20]>> {
        match self.all_object_ids().await {
            Ok(objects) => {
                self.close().await?;
                Ok(objects)
            }
            Err(operation) => {
                let Some(reader) = self.reader else {
                    return Err(operation);
                };
                close_after_error(reader, operation).await
            }
        }
    }

    /// Close the underlying SlateDB reader.
    pub async fn close(self) -> Result<()> {
        let Some(reader) = self.reader else {
            return Ok(());
        };
        reader
            .close()
            .await
            .map_err(|source| MetadataError::SlateDbClose {
                db: DB_LABEL.to_owned(),
                source,
            })
    }
}

fn active_sst_count(reader: &slatedb::DbReader) -> u64 {
    let manifest = reader.manifest();
    let l0 = u64::try_from(manifest.l0().len()).unwrap_or(u64::MAX);
    manifest.compacted().iter().fold(l0, |total, run| {
        total.saturating_add(u64::try_from(run.sst_views.len()).unwrap_or(u64::MAX))
    })
}

fn lookup_strategy(
    requested_objects: usize,
    inventory_objects: u64,
    active_ssts: u64,
    key_span: OidKeySpan,
) -> LookupStrategy {
    let requested = u64::try_from(requested_objects).unwrap_or(u64::MAX);
    if requested == 0 || inventory_objects == 0 {
        return LookupStrategy::Exact;
    }
    let near_complete =
        requested.saturating_mul(FULL_SCAN_MIN_COVERAGE_DIVISOR) >= inventory_objects;
    let compaction_amplified = active_ssts >= MIN_AMPLIFIED_SCAN_SSTS
        && requested.saturating_mul(active_ssts)
            >= inventory_objects.saturating_mul(AMPLIFIED_SCAN_COST_RATIO);
    let small_catalog_compacted = inventory_objects <= SMALL_CATALOG_MAX_OBJECTS
        && active_ssts >= MIN_AMPLIFIED_SCAN_SSTS
        && requested.saturating_mul(SMALL_CATALOG_REQUEST_RATIO) >= inventory_objects;
    // Broad SHA ranges cannot use a bounded key-span scan. Once the catalog
    // spans several SSTs, each exact OID probes every layer, so scale the
    // existing 1/64 crossover by that layer fan-out.
    let broad_multi_sst_dense = key_span == OidKeySpan::Broad
        && active_ssts >= MIN_LAYERED_SCAN_SSTS
        && requested
            .saturating_mul(FULL_SCAN_REQUEST_RATIO)
            .saturating_mul(active_ssts)
            >= inventory_objects;
    if small_catalog_compacted
        || (compaction_amplified && (key_span == OidKeySpan::Narrow || near_complete))
    {
        return LookupStrategy::FullScan {
            row_limit: usize::try_from(inventory_objects).unwrap_or(usize::MAX),
        };
    }
    if requested_objects < MIN_SCAN_LOOKUP_OBJECTS {
        return LookupStrategy::Exact;
    }
    let ordinary_full_scan_dense = requested.saturating_mul(FULL_SCAN_REQUEST_RATIO)
        >= inventory_objects
        && (key_span == OidKeySpan::Narrow || near_complete);
    if requested_objects >= FULL_SCAN_MIN_LOOKUP_OBJECTS
        && (ordinary_full_scan_dense || broad_multi_sst_dense)
    {
        return LookupStrategy::FullScan {
            row_limit: usize::try_from(inventory_objects).unwrap_or(usize::MAX),
        };
    }
    if key_span == OidKeySpan::Broad
        || requested.saturating_mul(MAX_SCAN_AMPLIFICATION as u64) < inventory_objects
    {
        return LookupStrategy::Exact;
    }
    LookupStrategy::Scan {
        row_limit: requested_objects.saturating_mul(MAX_SCAN_AMPLIFICATION),
    }
}

fn oid_key_span(object_ids: &[[u8; 20]]) -> OidKeySpan {
    let Some(first) = object_ids.first() else {
        return OidKeySpan::Narrow;
    };
    let mut minimum = u64::from_be_bytes(first[..8].try_into().unwrap_or([0; 8]));
    let mut maximum = minimum;
    for oid in &object_ids[1..] {
        let prefix = u64::from_be_bytes(oid[..8].try_into().unwrap_or([0; 8]));
        minimum = minimum.min(prefix);
        maximum = maximum.max(prefix);
    }
    let span = maximum.saturating_sub(minimum).saturating_add(1);
    if span <= u64::MAX / MAX_SCAN_KEYSPAN_DIVISOR {
        OidKeySpan::Narrow
    } else {
        OidKeySpan::Broad
    }
}

async fn reader_checkpoint_id(
    store: Arc<dyn ObjectStore>,
    path: &str,
    expected: Option<crab_xet::hash::MerkleHash>,
) -> Result<Option<slatedb::Checkpoint>> {
    let admin = slatedb::admin::AdminBuilder::new(ObjectPath::from(path), store).build();
    let checkpoints = match admin.list_checkpoints(None).await {
        Ok(checkpoints) => checkpoints,
        Err(error) if is_manifest_missing(&error) => return Ok(None),
        Err(source) => {
            return Err(MetadataError::SlateDbOpen {
                db: DB_LABEL.to_owned(),
                path: path.to_owned(),
                source,
            });
        }
    };
    if let Some(expected) = expected.map(super::catalog_checkpoint_name) {
        return Ok(checkpoints
            .into_iter()
            .filter(|checkpoint| checkpoint.name.as_deref() == Some(expected.as_str()))
            .max_by_key(|checkpoint| checkpoint.manifest_id));
    }
    let named = checkpoints
        .iter()
        .filter(|checkpoint| {
            checkpoint
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with(super::READER_CHECKPOINT_PREFIX))
        })
        .max_by_key(|checkpoint| checkpoint.manifest_id)
        .cloned();
    Ok(named.or_else(|| {
        checkpoints
            .into_iter()
            .max_by_key(|checkpoint| checkpoint.manifest_id)
    }))
}

fn locator_reader_options() -> DbReaderOptions {
    DbReaderOptions {
        // The locator writer disables WAL and flushes every published batch.
        // Replaying WALs can only add open latency; no locator rows live there.
        skip_wal_replay: true,
        ..DbReaderOptions::default()
    }
}

fn classify_location(
    bytes: &[u8],
    bindings: &HashMap<u64, GitPackLocatorRecord>,
    inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
) -> GitObjectLookup {
    let Some(stored) = decode_object_location(bytes) else {
        return GitObjectLookup::Corrupt;
    };
    let Some(pack) = bindings.get(&stored.pack_slot) else {
        return GitObjectLookup::Corrupt;
    };
    let Some(canonical) = inventory.get(&pack.pack_id) else {
        return GitObjectLookup::Miss;
    };
    if canonical.pack_id != pack.pack_id
        || canonical.object_count != pack.object_count
        || canonical.pack_size != pack.pack_size
    {
        return GitObjectLookup::Miss;
    }
    let location = GitObjectLocation {
        pack_offset: stored.pack_offset,
        entry_len: stored.entry_len,
        crc32: stored.crc32,
    };
    if !validate_location_for_pack(location, canonical.pack_size) {
        return GitObjectLookup::Corrupt;
    }
    GitObjectLookup::Hit(GitObjectLocator {
        ordinal: stored.ordinal,
        pack_id: pack.pack_id,
        location,
        metadata: stored.metadata,
    })
}

async fn load_state(
    reader: &slatedb::DbReader,
) -> Result<(
    Option<GitObjectCatalogIdentity>,
    HashMap<u64, GitPackLocatorRecord>,
)> {
    let value = reader
        .get(METADATA_KEY)
        .await
        .map_err(read_error)?
        .ok_or_else(|| corrupt("metadata", "compact locator metadata is missing"))?;
    let metadata = decode_metadata(&value)
        .ok_or_else(|| corrupt("metadata", "invalid compact locator metadata"))?;
    let bindings = match reader.get(PACK_BINDINGS_KEY).await.map_err(read_error)? {
        Some(value) => {
            let identity = metadata
                .identity
                .ok_or_else(|| corrupt("pack", "binding snapshot has no catalog identity"))?;
            decode_pack_bindings(&value, identity, metadata.next_pack_slot)
                .ok_or_else(|| corrupt("pack", "invalid compact locator binding snapshot"))?
                .into_iter()
                .collect()
        }
        None => load_bindings(reader, metadata.next_pack_slot).await?,
    };
    Ok((metadata.identity, bindings))
}

async fn load_bindings(
    reader: &slatedb::DbReader,
    next_pack_slot: u64,
) -> Result<HashMap<u64, GitPackLocatorRecord>> {
    let mut rows = reader
        .scan_prefix([PACK_FAMILY], ..)
        .await
        .map_err(read_error)?;
    let mut bindings = HashMap::new();
    let mut pack_ids = std::collections::HashSet::new();
    while let Some(row) = rows.next().await.map_err(read_error)? {
        let slot = decode_pack_key(&row.key)
            .ok_or_else(|| corrupt("pack", "invalid compact locator pack key"))?;
        let record = decode_pack_record(&row.value)
            .ok_or_else(|| corrupt("pack", "invalid compact locator pack record"))?;
        if slot >= next_pack_slot
            || bindings.insert(slot, record).is_some()
            || !pack_ids.insert(record.pack_id)
        {
            return Err(corrupt(
                "pack",
                "pack slot is unallocated or duplicates an existing binding",
            ));
        }
    }
    Ok(bindings)
}

fn is_manifest_missing(error: &slatedb::Error) -> bool {
    matches!(error.kind(), slatedb::ErrorKind::Data)
        && error
            .to_string()
            .contains("failed to find latest transactional object")
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

async fn close_after_error<T>(
    reader: Arc<slatedb::DbReader>,
    operation: MetadataError,
) -> Result<T> {
    match reader.close().await {
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
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };

    use super::*;
    use crate::git_object_locator::{
        GitObjectKind, GitObjectLocatorEntry, GitObjectLocatorWriter, GitObjectMetadata,
        GitPackLocatorRecord,
    };
    use crab_xet::hash::MerkleHash;

    struct Fixture {
        oid: [u8; 20],
        pack: GitPackLocatorRecord,
        inventory: GitPackInventoryEntry,
    }

    #[derive(Debug)]
    struct ReadCountingStore {
        inner: Arc<InMemory>,
        reads: AtomicUsize,
    }

    impl std::fmt::Display for ReadCountingStore {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("read-counting-store")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for ReadCountingStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
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
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
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

    fn oid(seed: u32) -> [u8; 20] {
        let mut oid = [0_u8; 20];
        oid[16..].copy_from_slice(&seed.to_be_bytes());
        oid
    }

    #[test]
    fn locator_reader_skips_wal_replay() {
        assert!(locator_reader_options().skip_wal_replay);
    }

    #[test]
    fn dense_lookup_requires_a_full_exact_wave_and_bounded_scan_amplification() {
        assert_eq!(
            lookup_strategy(
                MIN_SCAN_LOOKUP_OBJECTS,
                MIN_SCAN_LOOKUP_OBJECTS as u64,
                1,
                OidKeySpan::Narrow,
            ),
            LookupStrategy::Scan {
                row_limit: MIN_SCAN_LOOKUP_OBJECTS * 2,
            }
        );
        assert_eq!(
            lookup_strategy(
                MIN_SCAN_LOOKUP_OBJECTS - 1,
                (MIN_SCAN_LOOKUP_OBJECTS - 1) as u64,
                1,
                OidKeySpan::Narrow,
            ),
            LookupStrategy::Exact
        );
        assert_eq!(
            lookup_strategy(
                MIN_SCAN_LOOKUP_OBJECTS,
                (MIN_SCAN_LOOKUP_OBJECTS * 2 + 1) as u64,
                1,
                OidKeySpan::Narrow,
            ),
            LookupStrategy::Exact
        );
        assert_eq!(
            lookup_strategy(
                FULL_SCAN_MIN_LOOKUP_OBJECTS,
                (FULL_SCAN_MIN_LOOKUP_OBJECTS as u64) * FULL_SCAN_REQUEST_RATIO,
                1,
                OidKeySpan::Narrow,
            ),
            LookupStrategy::FullScan {
                row_limit: FULL_SCAN_MIN_LOOKUP_OBJECTS * FULL_SCAN_REQUEST_RATIO as usize,
            }
        );
        assert_eq!(
            lookup_strategy(
                FULL_SCAN_MIN_LOOKUP_OBJECTS,
                (FULL_SCAN_MIN_LOOKUP_OBJECTS as u64) * FULL_SCAN_REQUEST_RATIO,
                1,
                OidKeySpan::Broad,
            ),
            LookupStrategy::Exact
        );
        assert_eq!(
            lookup_strategy(
                FULL_SCAN_MIN_LOOKUP_OBJECTS,
                (FULL_SCAN_MIN_LOOKUP_OBJECTS as u64) * 4,
                1,
                OidKeySpan::Broad,
            ),
            LookupStrategy::FullScan {
                row_limit: FULL_SCAN_MIN_LOOKUP_OBJECTS * 4,
            }
        );
    }

    #[test]
    fn compacted_small_catalogs_use_a_bounded_full_scan_for_broad_requests() {
        assert_eq!(
            lookup_strategy(256, 1_024, MIN_AMPLIFIED_SCAN_SSTS, OidKeySpan::Broad),
            LookupStrategy::FullScan { row_limit: 1_024 }
        );
        assert_eq!(
            lookup_strategy(12, 52, MIN_AMPLIFIED_SCAN_SSTS, OidKeySpan::Broad),
            LookupStrategy::Exact
        );
        assert_eq!(
            lookup_strategy(12, 52, 14, OidKeySpan::Broad),
            LookupStrategy::FullScan { row_limit: 52 }
        );
    }

    #[test]
    fn compacted_large_catalogs_scan_broad_multiwave_requests() {
        assert_eq!(
            lookup_strategy(30_031, 1_643_226, 6, OidKeySpan::Broad),
            LookupStrategy::FullScan {
                row_limit: 1_643_226,
            }
        );
        assert_eq!(
            lookup_strategy(6_900, 1_643_226, 6, OidKeySpan::Broad),
            LookupStrategy::FullScan {
                row_limit: 1_643_226,
            }
        );
        assert_eq!(
            lookup_strategy(4_279, 1_643_226, 6, OidKeySpan::Broad),
            LookupStrategy::Exact
        );
        assert_eq!(
            lookup_strategy(28_929, 1_611_847, 3, OidKeySpan::Broad),
            LookupStrategy::FullScan {
                row_limit: 1_611_847,
            }
        );
        assert_eq!(
            lookup_strategy(28_929, 1_611_847, 2, OidKeySpan::Broad),
            LookupStrategy::Exact
        );
    }

    #[test]
    fn broad_sha1_ranges_use_exact_lookup_for_sparse_batches() {
        assert_eq!(oid_key_span(&[[0; 20], [0xff; 20]]), OidKeySpan::Broad);
        assert_eq!(oid_key_span(&[oid(1), oid(2)]), OidKeySpan::Narrow);
    }

    async fn publish(
        store: Arc<dyn ObjectStore>,
        pack: GitPackLocatorRecord,
        oid: [u8; 20],
        coverage: Option<GitLocatorCoverage>,
    ) -> Fixture {
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(
                binding,
                &[GitObjectLocatorEntry {
                    oid,
                    location: GitObjectLocation {
                        pack_offset: 12,
                        entry_len: 96,
                        crc32: 7,
                    },
                    metadata: Default::default(),
                }],
            )
            .await
            .expect("write object");
        writer.flush_objects().await.expect("flush object");
        if let Some(coverage) = coverage {
            writer
                .set_coverage(coverage)
                .await
                .expect("publish coverage");
        }
        writer.close().await.expect("close writer");
        Fixture {
            oid,
            pack,
            inventory: GitPackInventoryEntry {
                pack_id: pack.pack_id,
                object_count: pack.object_count,
                pack_size: pack.pack_size,
            },
        }
    }

    async fn publish_many(
        store: Arc<dyn ObjectStore>,
        object_count: usize,
    ) -> (Vec<[u8; 20]>, HashMap<MerkleHash, GitPackInventoryEntry>) {
        let mut pack = pack(1);
        pack.object_count = object_count as u64;
        let object_ids = (0..object_count)
            .map(|seed| oid(seed as u32))
            .collect::<Vec<_>>();
        let entries = object_ids
            .iter()
            .copied()
            .map(|oid| GitObjectLocatorEntry {
                oid,
                location: GitObjectLocation {
                    pack_offset: 12,
                    entry_len: 96,
                    crc32: 7,
                },
                metadata: Default::default(),
            })
            .collect::<Vec<_>>();
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &entries)
            .await
            .expect("write objects");
        writer.flush_objects().await.expect("flush objects");
        writer.close().await.expect("close writer");
        let inventory = GitPackInventoryEntry {
            pack_id: pack.pack_id,
            object_count: pack.object_count,
            pack_size: pack.pack_size,
        };
        (object_ids, HashMap::from([(pack.pack_id, inventory)]))
    }

    #[tokio::test]
    async fn exact_get_joins_pack_slot_and_requires_pinned_inventory() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fixture = publish(Arc::clone(&store), pack(1), [3; 20], None).await;
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let hit_inventory = HashMap::from([(fixture.pack.pack_id, fixture.inventory)]);
        assert!(matches!(
            session
                .lookup_batch(&[fixture.oid], &hit_inventory)
                .await
                .expect("hit")
                .as_slice(),
            [GitObjectLookup::Hit(_)]
        ));
        assert_eq!(
            session
                .lookup_batch(&[fixture.oid], &HashMap::new())
                .await
                .expect("miss"),
            vec![GitObjectLookup::Miss]
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn object_kind_metadata_round_trips_through_the_catalog() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fixture = publish(Arc::clone(&store), pack(1), [33; 20], None).await;
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[fixture.pack]).await.expect("bind pack")[0];
        writer
            .write_locations(
                binding,
                &[GitObjectLocatorEntry {
                    oid: fixture.oid,
                    location: GitObjectLocation {
                        pack_offset: 12,
                        entry_len: 96,
                        crc32: 7,
                    },
                    metadata: GitObjectMetadata {
                        kind: Some(GitObjectKind::Blob),
                        ..Default::default()
                    },
                }],
            )
            .await
            .expect("write kind metadata");
        writer.flush_objects().await.expect("flush kind metadata");
        writer.close().await.expect("close writer");

        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let lookup = session
            .lookup_batch(
                &[fixture.oid],
                &HashMap::from([(fixture.pack.pack_id, fixture.inventory)]),
            )
            .await
            .expect("lookup object kind");
        assert!(matches!(
            lookup.as_slice(),
            [GitObjectLookup::Hit(locator)]
                if locator.metadata.kind == Some(GitObjectKind::Blob)
        ));
        assert_eq!(
            session
                .metadata_by_ordinal(&[0])
                .await
                .expect("lookup ordinal metadata")
                .expect("metadata sidecar"),
            vec![GitObjectMetadata {
                kind: Some(GitObjectKind::Blob),
                ..Default::default()
            }]
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn operation_reader_does_not_write_a_checkpoint() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        publish(Arc::clone(&store), pack(1), [31; 20], None).await;
        let prefix = ObjectPath::from("org/repo/git_object_catalog_db");
        let before = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list before reader");

        let session = GitObjectLocatorSession::open_for_operation(
            Arc::clone(&store),
            "org/repo",
            Duration::from_secs(1),
        )
        .await
        .expect("open operation reader");
        session.close().await.expect("close operation reader");

        let after = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list after reader");
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn published_locator_open_is_storage_read_only() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        publish(Arc::clone(&store), pack(1), [32; 20], None).await;
        let prefix = ObjectPath::from("org/repo/git_object_catalog_db");
        let before = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list before reader");

        let session = GitObjectLocatorSession::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open reader");
        session.close().await.expect("close reader");

        let after = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list after reader");
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn newer_current_row_is_a_miss_for_an_old_snapshot() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old = publish(Arc::clone(&store), pack(1), [4; 20], None).await;
        publish(Arc::clone(&store), pack(2), old.oid, None).await;

        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let old_inventory = HashMap::from([(old.pack.pack_id, old.inventory)]);
        assert_eq!(
            session
                .lookup_batch(&[old.oid], &old_inventory)
                .await
                .expect("old snapshot lookup"),
            vec![GitObjectLookup::Miss]
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn batch_lookup_preserves_request_order_and_reports_missing_ids() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fixture = publish(Arc::clone(&store), pack(1), [5; 20], None).await;
        let inventory = HashMap::from([(fixture.pack.pack_id, fixture.inventory)]);
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let lookups = session
            .lookup_batch(&[[9; 20], fixture.oid, [8; 20]], &inventory)
            .await
            .expect("batch lookup");
        assert!(matches!(
            lookups.as_slice(),
            [
                GitObjectLookup::Miss,
                GitObjectLookup::Hit(_),
                GitObjectLookup::Miss
            ]
        ));
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn exact_batch_reuses_one_lookup_for_repeated_oids() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fixture = publish(Arc::clone(&store), pack(1), [6; 20], None).await;
        let inventory = HashMap::from([(fixture.pack.pack_id, fixture.inventory)]);
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let lookups = session
            .lookup_batch(
                &[fixture.oid, fixture.oid, [7; 20], fixture.oid],
                &inventory,
            )
            .await
            .expect("duplicate batch lookup");
        assert_eq!(lookups.len(), 4);
        assert!(matches!(lookups[0], GitObjectLookup::Hit(_)));
        assert_eq!(lookups[0], lookups[1]);
        assert_eq!(lookups[0], lookups[3]);
        assert_eq!(lookups[2], GitObjectLookup::Miss);
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn exact_batch_coalesces_shared_sst_reads() {
        let inner = Arc::new(InMemory::new());
        let writer_store: Arc<dyn ObjectStore> = inner.clone();
        let (object_ids, inventory) = publish_many(writer_store, 64).await;
        let store = Arc::new(ReadCountingStore {
            inner,
            reads: AtomicUsize::new(0),
        });
        let reader_store: Arc<dyn ObjectStore> = store.clone();
        let session = GitObjectLocatorSession::open(reader_store, "org/repo")
            .await
            .expect("open reader");
        store.reads.store(0, Ordering::Relaxed);

        let lookups = session
            .lookup_batch(&object_ids, &inventory)
            .await
            .expect("exact batch");
        let first_batch_reads = store.reads.load(Ordering::Relaxed);
        assert!(
            first_batch_reads < object_ids.len(),
            "shared SST reads were not coalesced: {first_batch_reads} reads"
        );
        assert!(
            lookups
                .iter()
                .all(|lookup| matches!(lookup, GitObjectLookup::Hit(_)))
        );

        session
            .lookup_batch(&object_ids, &inventory)
            .await
            .expect("cached exact batch");
        assert_eq!(store.reads.load(Ordering::Relaxed), first_batch_reads);
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn dense_scan_preserves_request_order_and_reports_missing_ids() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (mut object_ids, inventory) =
            publish_many(Arc::clone(&store), FULL_SCAN_MIN_LOOKUP_OBJECTS).await;
        object_ids.reverse();
        let missing_index = MIN_SCAN_LOOKUP_OBJECTS / 2;
        object_ids[missing_index] = [0xff; 20];
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let lookups = session
            .lookup_batch(&object_ids, &inventory)
            .await
            .expect("dense lookup");
        assert_eq!(lookups.len(), object_ids.len());
        assert_eq!(lookups[missing_index], GitObjectLookup::Miss);
        assert!(lookups.iter().enumerate().all(|(index, lookup)| {
            index == missing_index || matches!(lookup, GitObjectLookup::Hit(_))
        }));
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn dense_ordinal_scan_preserves_request_order() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (object_ids, _inventory) =
            publish_many(Arc::clone(&store), MIN_SCAN_LOOKUP_OBJECTS).await;
        let ordinals = (0..object_ids.len() as u32)
            .rev()
            .collect::<Vec<GitObjectOrdinal>>();
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let resolved = session
            .object_ids_by_ordinal(&ordinals)
            .await
            .expect("dense ordinal lookup");
        let expected = ordinals
            .iter()
            .map(|ordinal| Some(object_ids[*ordinal as usize]))
            .collect::<Vec<_>>();
        assert_eq!(resolved, expected);
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn dense_ordinal_metadata_scan_preserves_request_order() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (object_ids, _inventory) =
            publish_many(Arc::clone(&store), MIN_SCAN_LOOKUP_OBJECTS).await;
        let ordinals = (0..object_ids.len() as u32)
            .rev()
            .collect::<Vec<GitObjectOrdinal>>();
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let metadata = session
            .metadata_by_ordinal(&ordinals)
            .await
            .expect("dense ordinal metadata lookup")
            .expect("metadata sidecar");
        assert_eq!(metadata, vec![GitObjectMetadata::default(); ordinals.len()]);
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn dense_scan_abandons_work_beyond_its_row_limit() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (object_ids, inventory) = publish_many(Arc::clone(&store), 3).await;
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let reader = session.reader.as_ref().expect("reader exists");

        assert_eq!(
            session
                .lookup_batch_by_scan(
                    reader,
                    &[object_ids[0], object_ids[2]],
                    &inventory,
                    1,
                    SCAN_READ_AHEAD_BYTES,
                    SCAN_FETCH_TASKS,
                    "scan",
                )
                .await
                .expect("bounded scan"),
            None
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn missing_new_database_ignores_any_old_prefix_and_closes() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from("org/repo/git_locator_db/legacy"),
                bytes::Bytes::from_static(b"legacy").into(),
            )
            .await
            .expect("write old prefix");

        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open missing compact reader");
        assert!(!session.is_available());
        assert_eq!(
            session
                .lookup_batch(&[[1; 20]], &HashMap::new())
                .await
                .expect("lookup unavailable"),
            vec![GitObjectLookup::Miss]
        );
        session.close().await.expect("close unavailable reader");
    }

    #[tokio::test]
    async fn refreshing_reader_can_mix_open_state_with_concurrent_publication() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old_pack = pack(1);
        let old = publish(
            Arc::clone(&store),
            old_pack,
            [6; 20],
            Some(GitLocatorCoverage {
                generation: old_pack.committed_generation,
                pack_index_hash: old_pack.pack_index_hash,
            }),
        )
        .await;
        let options = DbReaderOptions {
            manifest_poll_interval: Duration::from_millis(10),
            checkpoint_lifetime: Duration::from_secs(1),
            ..locator_reader_options()
        };
        let session =
            GitObjectLocatorSession::open_with_options(Arc::clone(&store), "org/repo", options)
                .await
                .expect("open old reader");
        let reader = Arc::clone(session.reader.as_ref().expect("reader exists"));
        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: old.pack.committed_generation,
                pack_index_hash: old.pack.pack_index_hash,
            })
        );

        let new_pack = pack(2);
        let new = publish(
            Arc::clone(&store),
            new_pack,
            [7; 20],
            Some(GitLocatorCoverage {
                generation: new_pack.committed_generation,
                pack_index_hash: new_pack.pack_index_hash,
            }),
        )
        .await;
        let inventory = HashMap::from([
            (old.pack.pack_id, old.inventory),
            (new.pack.pack_id, new.inventory),
        ]);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let lookup = session
                    .lookup_batch(&[new.oid], &inventory)
                    .await
                    .expect("poll refreshed reader");
                if lookup == [GitObjectLookup::Corrupt] {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reader refreshes");

        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: old.pack.committed_generation,
                pack_index_hash: old.pack.pack_index_hash,
            })
        );
        session.close().await.expect("close old reader");
        let error = reader
            .get(object_key(&old.oid))
            .await
            .expect_err("closed reader rejects reads");
        assert_eq!(
            error.kind(),
            slatedb::ErrorKind::Closed(slatedb::CloseReason::Clean)
        );

        let refreshed = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open refreshed reader");
        assert_eq!(
            refreshed.coverage(),
            Some(GitLocatorCoverage {
                generation: new.pack.committed_generation,
                pack_index_hash: new.pack.pack_index_hash,
            })
        );
        assert!(matches!(
            refreshed
                .lookup_batch(&[new.oid], &inventory)
                .await
                .expect("lookup new object")
                .as_slice(),
            [GitObjectLookup::Hit(_)]
        ));
        refreshed.close().await.expect("close refreshed reader");
    }

    #[tokio::test]
    async fn operation_reader_does_not_refresh_before_its_bound() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old_pack = pack(1);
        let old = publish(
            Arc::clone(&store),
            old_pack,
            [8; 20],
            Some(GitLocatorCoverage {
                generation: old_pack.committed_generation,
                pack_index_hash: old_pack.pack_index_hash,
            }),
        )
        .await;
        let session = GitObjectLocatorSession::open_for_operation(
            Arc::clone(&store),
            "org/repo",
            Duration::from_secs(1),
        )
        .await
        .expect("open operation reader");

        let new_pack = pack(2);
        let new = publish(
            Arc::clone(&store),
            new_pack,
            [9; 20],
            Some(GitLocatorCoverage {
                generation: new_pack.committed_generation,
                pack_index_hash: new_pack.pack_index_hash,
            }),
        )
        .await;
        let inventory = HashMap::from([
            (old.pack.pack_id, old.inventory),
            (new.pack.pack_id, new.inventory),
        ]);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(matches!(
            session
                .lookup_batch(&[old.oid], &inventory)
                .await
                .expect("lookup pinned object")
                .as_slice(),
            [GitObjectLookup::Hit(_)]
        ));
        assert_eq!(
            session
                .lookup_batch(&[new.oid], &inventory)
                .await
                .expect("lookup post-open object"),
            [GitObjectLookup::Miss]
        );
        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: old.pack.committed_generation,
                pack_index_hash: old.pack.pack_index_hash,
            })
        );
        session.close().await.expect("close operation reader");
    }
}
