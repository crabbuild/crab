use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use crab_git::{delta, pack::VerifiedPackIdentity};
use crab_metadata::git_object_locator::{
    GitObjectLocation, GitObjectLocator, GitObjectLocatorSession, GitObjectLookup,
    GitPackInventoryEntry,
};
use crab_storage::{Store, repo_pack_index_path, repo_pack_path, repo_pack_reverse_index_path};
use crab_xet::hash::MerkleHash;
use futures_util::stream::{self, StreamExt};
use gix_pack::data::entry::Header;
use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;

use crate::budget::OperationBudget;
use crate::pack::PackStreamVerifier;
use crate::{BudgetDimension, RemoteGitRuntime, RepositoryIdentity};
use crate::{CorruptionStage, Error, InflatedEntryError, RepositoryStateError, Result};

/// Resource limits applied to one remote Git object read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReaderLimits {
    /// Maximum compressed bytes fetched for one pack entry.
    pub max_packed_entry_bytes: u64,
    /// Maximum inflated bytes for an object or delta program.
    pub max_inflated_entry_bytes: u64,
    /// Maximum reconstructed Git object size.
    pub max_object_bytes: u64,
    /// Maximum pack-index size retained in memory.
    pub max_pack_index_bytes: u64,
    /// Maximum number of deltas resolved for one object.
    pub max_delta_depth: usize,
}

impl Default for ReaderLimits {
    fn default() -> Self {
        Self {
            max_packed_entry_bytes: 64 * 1024 * 1024,
            max_inflated_entry_bytes: 64 * 1024 * 1024,
            max_object_bytes: 64 * 1024 * 1024,
            max_pack_index_bytes: 128 * 1024 * 1024,
            max_delta_depth: 128,
        }
    }
}

impl ReaderLimits {
    pub(crate) fn from_options(options: crate::RepositoryOptions) -> Self {
        let object = options.object_limits();
        Self {
            max_packed_entry_bytes: object.max_packed_entry_bytes,
            max_inflated_entry_bytes: object.max_inflated_entry_bytes,
            max_object_bytes: object.max_object_bytes,
            max_pack_index_bytes: object.max_pack_index_bytes,
            max_delta_depth: object.max_delta_depth,
        }
    }

    fn validate(self) -> Result<Self> {
        for (name, value) in [
            ("max_packed_entry_bytes", self.max_packed_entry_bytes),
            ("max_inflated_entry_bytes", self.max_inflated_entry_bytes),
            ("max_object_bytes", self.max_object_bytes),
            ("max_pack_index_bytes", self.max_pack_index_bytes),
            ("max_delta_depth", self.max_delta_depth as u64),
        ] {
            if value == 0 {
                return Err(Error::InvalidLimit { name });
            }
        }
        Ok(self)
    }
}

/// A verified, fully reconstructed Git object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteGitObject {
    /// Object ID verified against `kind` and `data`.
    pub oid: gix_hash::ObjectId,
    /// Git object kind inherited from the full base of any delta chain.
    pub kind: gix_object::Kind,
    /// Canonical uncompressed object payload, without the Git hash header.
    pub data: Bytes,
}

pub(crate) type GitObject = RemoteGitObject;

const MAX_COALESCED_RANGE_BYTES: u64 = 8 * 1024 * 1024;
// Git pack entries are often separated by small unrelated entries. A wider
// gap removes thousands of object-store round trips while the range-size
// bound keeps transient response memory bounded.
const MAX_COALESCED_GAP_BYTES: u64 = 32 * 1024;
const DELTA_PREFETCH_BATCH_SIZE: usize = 50_000;
const MATERIALIZE_CHUNK_SIZE: usize = 256;
// Large object batches are cheaper to resolve from the immutable pack indexes
// than from one metadata point read per OID. Small reads retain the catalog
// path because loading an index would cost more than the lookup it replaces.
const PACK_INDEX_LOOKUP_MIN_OBJECTS: usize = 256;
const PACK_INDEX_LOAD_CONCURRENCY: usize = 4;

struct CoalescedRange {
    pack_id: MerkleHash,
    start: u64,
    end: u64,
    entries: Vec<(gix_hash::ObjectId, GitObjectLocator)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteGitObjectMetadata {
    /// Object ID whose packed entry was inspected.
    pub oid: gix_hash::ObjectId,
    /// Verified or pack-header-derived Git object kind.
    pub kind: gix_object::Kind,
    /// Reconstructed Git object size in bytes.
    pub size: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteGitPackedEntry {
    pub(crate) oid: gix_hash::ObjectId,
    pub(crate) pack_offset: u64,
    pub(crate) header: Header,
    pub(crate) decompressed_size: u64,
    pub(crate) header_size: usize,
    pub(crate) base_oid: Option<gix_hash::ObjectId>,
    pub(crate) bytes: Bytes,
}

/// Reads Git objects directly from immutable Crab packs in object storage.
pub(crate) struct RemoteGitReader {
    store: Store,
    repo_prefix: String,
    inventory: HashMap<MerkleHash, GitPackInventoryEntry>,
    limits: ReaderLimits,
    runtime: Arc<RemoteGitRuntime>,
    identity: RepositoryIdentity,
    generation: u64,
}

impl RemoteGitReader {
    pub(crate) fn from_pinned(
        store: Store,
        repo_prefix: impl Into<String>,
        inventory: impl IntoIterator<Item = GitPackInventoryEntry>,
        limits: ReaderLimits,
        runtime: Arc<RemoteGitRuntime>,
        identity: RepositoryIdentity,
        generation: u64,
    ) -> Result<Self> {
        let limits = limits.validate()?;
        let mut canonical = HashMap::new();
        for pack in inventory {
            if canonical.insert(pack.pack_id, pack).is_some() {
                return Err(Error::RepositoryState {
                    reason: RepositoryStateError::DuplicatePack,
                });
            }
        }
        Ok(Self {
            store,
            repo_prefix: repo_prefix.into(),
            inventory: canonical,
            limits,
            runtime,
            identity,
            generation,
        })
    }

    pub(crate) async fn read_with_session(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        let key = crate::runtime::ObjectCacheKey::new(&self.identity, self.generation, requested);
        if let Some(object) = self
            .verified_cached_object(&key, self.limits.max_object_bytes)
            .await?
        {
            return Ok(object.as_ref().clone());
        }
        if self.runtime.exact_miss_is_cached(&key).await {
            return Err(Error::ObjectNotFound { oid: requested });
        }
        let result = async {
            let locator = self
                .locate(session, requested, budget, cancellation)
                .await?;
            self.read_from_locator(
                session,
                requested,
                locator,
                self.limits.max_object_bytes,
                self.limits.max_delta_depth,
                budget,
                cancellation,
            )
            .await
        }
        .await;
        match result {
            Ok(object) => {
                verify_object(object.oid, object.kind, &object.data)?;
                self.runtime
                    .insert_object(key, Arc::new(object.clone()))
                    .await;
                Ok(object)
            }
            Err(error @ Error::ObjectNotFound { .. }) => {
                self.runtime.insert_exact_miss(key).await;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn read_metadata_with_session(
        &self,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<RemoteGitObjectMetadata> {
        let mut current = requested;
        let mut visited = HashSet::new();
        let mut deltas = Vec::new();

        let kind_and_size = loop {
            check_cancelled(cancellation)?;
            if !visited.insert(current) {
                return Err(Error::Corrupt {
                    stage: CorruptionStage::Delta,
                });
            }
            let locator = self.locate(session, current, budget, cancellation).await?;
            let descriptor = self
                .inspect_packed_entry(current, locator, budget, cancellation)
                .await?;
            match descriptor {
                EntryDescriptor::Base { kind, size } => break (kind, size),
                EntryDescriptor::RefDelta {
                    base,
                    base_size,
                    result_size,
                    ..
                } => {
                    push_metadata_delta(&mut deltas, base_size, result_size, self.limits)?;
                    current = base;
                }
                EntryDescriptor::OfsDelta {
                    pack_id,
                    pack_offset,
                    base_distance,
                    base_size,
                    result_size,
                    ..
                } => {
                    push_metadata_delta(&mut deltas, base_size, result_size, self.limits)?;
                    let base_offset = Header::verified_base_pack_offset(pack_offset, base_distance)
                        .ok_or(Error::Corrupt {
                            stage: CorruptionStage::Delta,
                        })?;
                    current = self
                        .oid_at_pack_offset(pack_id, base_offset, budget, cancellation)
                        .await?;
                }
            }
        };
        let (kind, mut size) = kind_and_size;
        for delta in deltas.into_iter().rev() {
            if delta.base_size != size {
                return Err(Error::InvalidDelta {
                    oid: requested,
                    reason: crate::DeltaCorruption::BaseSizeMismatch,
                });
            }
            size = delta.result_size;
        }
        Ok(RemoteGitObjectMetadata {
            oid: requested,
            kind,
            size,
        })
    }

    pub(crate) async fn read_small_metadata_object_with_session(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        let locator = self
            .locate(session, requested, budget, cancellation)
            .await?;
        self.read_from_locator(
            session,
            requested,
            locator,
            self.limits
                .max_object_bytes
                .max(crab_git::MAX_LFS_POINTER_SIZE as u64),
            self.limits.max_delta_depth,
            budget,
            cancellation,
        )
        .await
    }

    async fn lookup_batch_for_read(
        &self,
        session: &GitObjectLocatorSession,
        requested: &[[u8; 20]],
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObjectLookup>> {
        if requested.len() < PACK_INDEX_LOOKUP_MIN_OBJECTS || self.inventory.is_empty() {
            return self
                .lookup_batch_from_catalog(session, requested, budget, cancellation)
                .await;
        }

        if session.coverage().is_some() {
            // A generation-bound catalog already contains the OID-to-pack
            // join. The operation layer has checked that coverage against its
            // pinned manifest, so avoid reopening every immutable pack index
            // for current repositories; use indexes only for catalog misses
            // from a partially repaired publication.
            let mut lookups = self
                .lookup_batch_from_catalog(session, requested, budget, cancellation)
                .await?;
            let missing = lookups
                .iter()
                .enumerate()
                .filter_map(|(index, lookup)| {
                    matches!(lookup, GitObjectLookup::Miss).then_some((index, requested[index]))
                })
                .collect::<Vec<_>>();
            if missing.is_empty() {
                return Ok(lookups);
            }

            let missing_ids = missing.iter().map(|(_, oid)| *oid).collect::<Vec<_>>();
            let fallback = self
                .lookup_batch_from_pack_indexes(&missing_ids, budget, cancellation)
                .await?;
            for ((index, _), lookup) in missing.into_iter().zip(fallback) {
                lookups[index] = lookup;
            }
            return Ok(lookups);
        }

        let mut lookups = self
            .lookup_batch_from_pack_indexes(requested, budget, cancellation)
            .await?;
        let missing = lookups
            .iter()
            .enumerate()
            .filter_map(|(index, lookup)| {
                matches!(lookup, GitObjectLookup::Miss).then_some((index, requested[index]))
            })
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(lookups);
        }

        let missing_ids = missing.iter().map(|(_, oid)| *oid).collect::<Vec<_>>();
        let fallback = self
            .lookup_batch_from_catalog(session, &missing_ids, budget, cancellation)
            .await?;
        for ((index, _), lookup) in missing.into_iter().zip(fallback) {
            lookups[index] = lookup;
        }
        Ok(lookups)
    }

    pub(crate) async fn lookup_packed_locators_with_session(
        &self,
        session: &GitObjectLocatorSession,
        requested: &[gix_hash::ObjectId],
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObjectLocator>> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let mut oid_bytes = Vec::new();
        oid_bytes
            .try_reserve_exact(requested.len())
            .map_err(|source| Error::Allocation {
                requested: requested
                    .len()
                    .saturating_mul(std::mem::size_of::<[u8; 20]>()),
                source,
            })?;
        for oid in requested {
            oid_bytes.push(
                oid.as_bytes()
                    .try_into()
                    .map_err(|_| Error::UnsupportedObjectFormat)?,
            );
        }
        let lookups = self
            .lookup_batch_for_read(session, &oid_bytes, budget, cancellation)
            .await?;
        drop(oid_bytes);
        if lookups.len() != requested.len() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            });
        }
        let mut locators = Vec::new();
        locators
            .try_reserve_exact(requested.len())
            .map_err(|source| Error::Allocation {
                requested: requested
                    .len()
                    .saturating_mul(std::mem::size_of::<GitObjectLocator>()),
                source,
            })?;
        for (oid, lookup) in requested.iter().copied().zip(lookups) {
            match lookup {
                GitObjectLookup::Hit(locator) => locators.push(locator),
                GitObjectLookup::Corrupt => {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::Locator,
                    });
                }
                GitObjectLookup::Miss => return Err(Error::ObjectNotFound { oid }),
            }
        }
        Ok(locators)
    }

    async fn lookup_batch_from_catalog(
        &self,
        session: &GitObjectLocatorSession,
        requested: &[[u8; 20]],
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObjectLookup>> {
        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        tracing::debug!(
            storage_request = "locator_lookup",
            storage_bytes = 0u64,
            object_count = requested.len(),
            "remote Git object-store request"
        );
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(Error::Cancelled),
            lookups = session.lookup_batch(requested, &self.inventory) => Ok(lookups?),
        }
    }

    async fn lookup_batch_from_pack_indexes(
        &self,
        requested: &[[u8; 20]],
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObjectLookup>> {
        let mut pack_ids = self.inventory.keys().copied().collect::<Vec<_>>();
        pack_ids.sort_unstable();
        let pack_count = pack_ids.len();
        // Do not retain every index for a repository-wide batch: pack count is
        // unbounded, while the stream keeps only the configured in-flight set.
        let mut indexes = stream::iter(pack_ids.into_iter().map(|pack_id| async move {
            let index = self.load_pack_index(pack_id, budget, cancellation).await?;
            Ok::<_, Error>((pack_id, index))
        }))
        .buffer_unordered(PACK_INDEX_LOAD_CONCURRENCY.min(pack_count).max(1));

        let mut lookups = vec![GitObjectLookup::Miss; requested.len()];
        let mut remaining = requested.len();
        while let Some(result) = indexes.next().await {
            let (pack_id, index) = result?;
            for (position, oid) in requested.iter().enumerate() {
                if !matches!(lookups[position], GitObjectLookup::Miss) {
                    continue;
                }
                let object_id = gix_hash::ObjectId::from(*oid);
                if let Some(location) = index.location_for(&object_id)? {
                    lookups[position] = GitObjectLookup::Hit(GitObjectLocator {
                        // Pack indexes provide a complete immutable location but
                        // not the catalog's dense ordinal. Batch object reads use
                        // only the location; catalog APIs continue to use the
                        // authoritative locator session.
                        ordinal: 0,
                        pack_id,
                        location,
                        metadata: Default::default(),
                    });
                    remaining = remaining.saturating_sub(1);
                }
            }
            if remaining == 0 {
                break;
            }
        }
        tracing::debug!(
            locator_lookup_mode = "pack_index",
            requested_objects = requested.len(),
            pack_count,
            "immutable Git pack indexes resolved a large object batch"
        );
        Ok(lookups)
    }

    pub(crate) async fn read_many_with_session(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: &[gix_hash::ObjectId],
        concurrency: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObject>> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let mut unique = Vec::new();
        let mut positions = HashMap::new();
        for oid in requested {
            if let std::collections::hash_map::Entry::Vacant(entry) = positions.entry(*oid) {
                entry.insert(unique.len());
                unique.push(*oid);
            }
        }
        let mut completed = HashMap::new();
        let mut missing = Vec::new();
        for oid in unique.iter().copied() {
            let key = crate::runtime::ObjectCacheKey::new(&self.identity, self.generation, oid);
            if let Some(object) = self
                .verified_cached_object(&key, self.limits.max_object_bytes)
                .await?
            {
                completed.insert(oid, object.as_ref().clone());
            } else if self.runtime.exact_miss_is_cached(&key).await {
                return Err(Error::ObjectNotFound { oid });
            } else {
                missing.push(oid);
            }
        }
        if missing.is_empty() {
            return order_completed_objects(requested, &completed);
        }
        let oid_bytes = missing
            .iter()
            .map(|oid| {
                oid.as_bytes()
                    .try_into()
                    .map_err(|_| Error::UnsupportedObjectFormat)
            })
            .collect::<Result<Vec<[u8; 20]>>>()?;
        let lookups = self
            .lookup_batch_for_read(session, &oid_bytes, budget, cancellation)
            .await?;
        if lookups.len() != missing.len() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            });
        }
        let locators = missing
            .iter()
            .copied()
            .zip(lookups)
            .map(|(oid, lookup)| match lookup {
                GitObjectLookup::Hit(locator) => Ok(locator),
                GitObjectLookup::Corrupt => Err(Error::Corrupt {
                    stage: CorruptionStage::Locator,
                }),
                GitObjectLookup::Miss => Err(Error::ObjectNotFound { oid }),
            })
            .collect::<Result<Vec<_>>>()?;
        // Batch delta dependencies with the selected entries. Resolving them
        // one object at a time multiplies locator and range reads on deep history.
        let packed = self
            .read_packed_many_with_session_and_locators(
                &missing,
                &locators,
                concurrency,
                budget,
                cancellation,
            )
            .await?;
        let objects = self
            .materialize_packed_entries(
                session,
                packed,
                &missing,
                concurrency,
                budget,
                cancellation,
            )
            .await?;
        for object in objects {
            completed.insert(object.oid, object);
        }
        order_completed_objects(requested, &completed)
    }

    pub(crate) async fn read_packed_many_with_session(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: &[gix_hash::ObjectId],
        concurrency: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RemoteGitPackedEntry>> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let locators = self
            .lookup_packed_locators_with_session(session, requested, budget, cancellation)
            .await?;
        self.read_packed_many_with_session_and_locators(
            requested,
            &locators,
            concurrency,
            budget,
            cancellation,
        )
        .await
    }

    pub(crate) async fn read_packed_many_with_session_and_locators(
        self: &Arc<Self>,
        requested: &[gix_hash::ObjectId],
        locators: &[GitObjectLocator],
        concurrency: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RemoteGitPackedEntry>> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        if requested.len() != locators.len() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            });
        }
        let mut ready = Vec::new();
        ready
            .try_reserve_exact(requested.len())
            .map_err(|source| Error::Allocation {
                requested: requested
                    .len()
                    .saturating_mul(std::mem::size_of::<(gix_hash::ObjectId, GitObjectLocator)>()),
                source,
            })?;
        for (oid, locator) in requested.iter().copied().zip(locators.iter().copied()) {
            check_limit(
                "packed entry bytes",
                locator.location.entry_len,
                self.limits.max_packed_entry_bytes,
            )?;
            ready.push((oid, locator));
        }
        let ranges = coalesce_ranges(ready)?;
        let caller_cancellation = cancellation.clone();
        let results = stream::iter(ranges.into_iter().map(|range| {
            let reader = Arc::clone(self);
            let caller_cancellation = caller_cancellation.clone();
            async move {
                let bytes = reader
                    .read_coalesced_range(&range, budget, &caller_cancellation)
                    .await?;
                let mut entries = Vec::with_capacity(range.entries.len());
                for (oid, locator) in range.entries {
                    let relative_start = locator
                        .location
                        .pack_offset
                        .checked_sub(range.start)
                        .ok_or(Error::Corrupt {
                            stage: CorruptionStage::PackEntry,
                        })?;
                    let relative_end = relative_start
                        .checked_add(locator.location.entry_len)
                        .ok_or(Error::Corrupt {
                            stage: CorruptionStage::PackEntry,
                        })?;
                    let start = usize::try_from(relative_start).map_err(|_| Error::Corrupt {
                        stage: CorruptionStage::PackEntry,
                    })?;
                    let end = usize::try_from(relative_end).map_err(|_| Error::Corrupt {
                        stage: CorruptionStage::PackEntry,
                    })?;
                    let entry_bytes = bytes.get(start..end).ok_or(Error::Corrupt {
                        stage: CorruptionStage::PackEntry,
                    })?;
                    if gix_features::hash::crc32(entry_bytes) != locator.location.crc32 {
                        return Err(Error::PackedEntryCrcMismatch { oid });
                    }
                    let parsed = gix_pack::data::Entry::from_bytes(
                        entry_bytes,
                        locator.location.pack_offset,
                        20,
                    )
                    .map_err(|source| Error::PackEntry { oid, source })?;
                    let maximum = if parsed.header.as_kind().is_some() {
                        reader
                            .limits
                            .max_inflated_entry_bytes
                            .min(reader.limits.max_object_bytes)
                    } else {
                        reader.limits.max_inflated_entry_bytes
                    };
                    check_limit(
                        "inflated pack entry bytes",
                        parsed.decompressed_size,
                        maximum,
                    )?;
                    let header_size = parsed.header_size();
                    if header_size >= entry_bytes.len() {
                        return Err(Error::Corrupt {
                            stage: CorruptionStage::PackEntry,
                        });
                    }
                    let base_oid = match parsed.header {
                        Header::RefDelta { base_id } => Some(base_id),
                        Header::OfsDelta { base_distance } => {
                            let base_offset = Header::verified_base_pack_offset(
                                locator.location.pack_offset,
                                base_distance,
                            )
                            .ok_or(Error::Corrupt {
                                stage: CorruptionStage::Delta,
                            })?;
                            Some(
                                reader
                                    .oid_at_pack_offset(
                                        locator.pack_id,
                                        base_offset,
                                        budget,
                                        &caller_cancellation,
                                    )
                                    .await?,
                            )
                        }
                        Header::Commit | Header::Tree | Header::Blob | Header::Tag => None,
                    };
                    entries.push(RemoteGitPackedEntry {
                        oid,
                        pack_offset: locator.location.pack_offset,
                        header: parsed.header,
                        decompressed_size: parsed.decompressed_size,
                        header_size,
                        base_oid,
                        bytes: Bytes::copy_from_slice(entry_bytes),
                    });
                }
                Ok::<_, Error>(entries)
            }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
        let mut completed = HashMap::with_capacity(requested.len());
        for result in results {
            for entry in result? {
                completed.insert(entry.oid, entry);
            }
        }
        requested
            .iter()
            .map(|oid| {
                completed.remove(oid).ok_or(Error::InternalInvariant {
                    invariant: "batched packed entry read is missing an object",
                })
            })
            .collect()
    }

    pub(crate) async fn materialize_packed_entries(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        initial_entries: Vec<RemoteGitPackedEntry>,
        requested: &[gix_hash::ObjectId],
        concurrency: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Vec<GitObject>> {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        let mut entries = HashMap::with_capacity(initial_entries.len());
        for entry in initial_entries {
            if entries.insert(entry.oid, entry).is_some() {
                return Err(Error::InternalInvariant {
                    invariant: "packed entry materializer received duplicate objects",
                });
            }
        }
        for oid in requested {
            if !entries.contains_key(oid) {
                return Err(Error::InternalInvariant {
                    invariant: "packed entry materializer is missing a requested object",
                });
            }
        }

        loop {
            check_cancelled(cancellation)?;
            let mut missing = Vec::new();
            let mut visited = HashMap::new();
            for oid in requested {
                collect_missing_delta_bases(
                    *oid,
                    0,
                    &entries,
                    &mut visited,
                    &mut missing,
                    self.limits.max_delta_depth,
                )?;
            }
            missing.sort_unstable();
            missing.dedup();
            if missing.is_empty() {
                break;
            }
            for batch in missing.chunks(DELTA_PREFETCH_BATCH_SIZE) {
                let fetched = self
                    .read_packed_many_with_session(
                        session,
                        batch,
                        concurrency,
                        budget,
                        cancellation,
                    )
                    .await?;
                for entry in fetched {
                    if entries.insert(entry.oid, entry).is_some() {
                        return Err(Error::InternalInvariant {
                            invariant: "delta base prefetch returned a duplicate object",
                        });
                    }
                }
            }
        }

        let mut resolver = PackedEntryResolver::new(
            entries,
            self.limits.max_object_bytes,
            self.limits.max_inflated_entry_bytes,
            self.limits.max_delta_depth,
        );
        let mut objects = Vec::with_capacity(requested.len());
        for requested_chunk in requested.chunks(MATERIALIZE_CHUNK_SIZE) {
            check_cancelled(cancellation)?;
            let requested_chunk = requested_chunk.to_vec();
            let token = cancellation.clone();
            let max_inflated = budget.remaining(BudgetDimension::InflatedBytes).await;
            let decode_permit = self.runtime.decode_permit(cancellation).await?;
            let (next_resolver, result) = tokio::task::spawn_blocking(move || {
                let mut resolver = resolver;
                let result = resolver.resolve_many(&requested_chunk, max_inflated, &token);
                (resolver, result)
            })
            .await
            .map_err(|source| Error::DecodeTask { source })?;
            drop(decode_permit);
            resolver = next_resolver;
            let (mut chunk_objects, inflated_bytes) = result?;
            budget
                .charge(BudgetDimension::InflatedBytes, inflated_bytes)
                .await?;
            objects.append(&mut chunk_objects);
        }
        // Retain verified dependencies as well as requested objects. Later
        // batches often use an earlier delta base, just like the single reader.
        for object in resolver.objects.values() {
            let key =
                crate::runtime::ObjectCacheKey::new(&self.identity, self.generation, object.oid);
            self.runtime
                .insert_object(key, Arc::new(object.clone()))
                .await;
        }
        Ok(objects)
    }

    async fn read_from_locator(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        locator: GitObjectLocator,
        max_object_bytes: u64,
        remaining_delta_depth: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        let packed = self
            .read_packed_entry(requested, locator, max_object_bytes, budget, cancellation)
            .await?;
        self.resolve_packed_entry(
            session,
            requested,
            locator,
            packed,
            max_object_bytes,
            remaining_delta_depth,
            budget,
            cancellation,
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "delta resolution carries explicit session, verification, budget, and cancellation inputs"
    )]
    async fn resolve_packed_entry(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        locator: GitObjectLocator,
        packed: PackedEntry,
        max_object_bytes: u64,
        remaining_delta_depth: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        if let Some(kind) = packed.header.as_kind() {
            verify_object(requested, kind, &packed.inflated)?;
            return Ok(GitObject {
                oid: requested,
                kind,
                data: packed.inflated,
            });
        }
        if remaining_delta_depth == 0 {
            return Err(Error::LimitExceeded {
                limit: "delta depth",
                actual: 1,
                maximum: 0,
            });
        }
        let base_oid = match packed.header {
            Header::RefDelta { base_id } => base_id,
            Header::OfsDelta { base_distance } => {
                let base_offset =
                    Header::verified_base_pack_offset(locator.location.pack_offset, base_distance)
                        .ok_or(Error::Corrupt {
                            stage: CorruptionStage::Delta,
                        })?;
                self.oid_at_pack_offset(locator.pack_id, base_offset, budget, cancellation)
                    .await?
            }
            Header::Commit | Header::Tree | Header::Blob | Header::Tag => {
                return Err(Error::InternalInvariant {
                    invariant: "base pack entry reached delta resolution",
                });
            }
        };
        if base_oid == requested {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Delta,
            });
        }
        let base_key =
            crate::runtime::ObjectCacheKey::new(&self.identity, self.generation, base_oid);
        let base = if let Some(base) = self
            .verified_cached_object(&base_key, max_object_bytes)
            .await?
        {
            base
        } else {
            let base_locator = self.locate(session, base_oid, budget, cancellation).await?;
            let base = Box::pin(self.read_from_locator(
                session,
                base_oid,
                base_locator,
                max_object_bytes,
                remaining_delta_depth - 1,
                budget,
                cancellation,
            ))
            .await?;
            self.runtime
                .insert_object(base_key, Arc::new(base.clone()))
                .await;
            Arc::new(base)
        };
        let maximum = usize_limit(max_object_bytes, "decoded object bytes")?;
        let token = cancellation.clone();
        let instructions = packed.inflated;
        let base_data = base.data.clone();
        let result_size = delta::parse(&instructions, maximum)
            .map_err(|error| map_delta_error(error, requested))?
            .result_size;
        budget
            .charge(BudgetDimension::InflatedBytes, result_size as u64)
            .await?;
        let decode_permit = self.runtime.decode_permit(cancellation).await?;
        let data = self
            .runtime
            .spawn_blocking(move || {
                let parsed = delta::parse(&instructions, maximum)?;
                delta::apply(&base_data, parsed, || token.is_cancelled())
            })
            .await
            .map_err(|source| Error::DecodeTask { source })?
            .map_err(|error| map_delta_error(error, requested))?;
        drop(decode_permit);
        verify_object(requested, base.kind, &data)?;
        Ok(GitObject {
            oid: requested,
            kind: base.kind,
            data: Bytes::from(data),
        })
    }

    async fn read_coalesced_range(
        &self,
        range: &CoalescedRange,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Bytes> {
        let length = range.end.checked_sub(range.start).ok_or(Error::Corrupt {
            stage: CorruptionStage::PackEntry,
        })?;
        let path = repo_pack_path(&self.repo_prefix, &range.pack_id);
        charge_origin_range(budget, length).await?;
        let origin_permit = self.runtime.origin_permit(cancellation).await?;
        let bytes = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            bytes = self.store.range_get(&path, range.start..range.end) => bytes?,
        };
        drop(origin_permit);
        check_cancelled(cancellation)?;
        if bytes.len() as u64 != length {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            });
        }
        Ok(bytes)
    }

    async fn locate(
        &self,
        session: &GitObjectLocatorSession,
        oid: gix_hash::ObjectId,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObjectLocator> {
        let oid_bytes: [u8; 20] = oid
            .as_bytes()
            .try_into()
            .map_err(|_| Error::UnsupportedObjectFormat)?;
        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        tracing::debug!(
            storage_request = "locator_lookup",
            storage_bytes = 0u64,
            "remote Git object-store request"
        );
        let oid_batch = [oid_bytes];
        let lookup = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            lookup = session.lookup_batch(&oid_batch, &self.inventory) => lookup?,
        };
        match lookup.into_iter().next() {
            Some(GitObjectLookup::Hit(locator)) => Ok(locator),
            Some(GitObjectLookup::Corrupt) => Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            }),
            Some(GitObjectLookup::Miss) | None => Err(Error::ObjectNotFound { oid }),
        }
    }

    async fn verified_cached_object(
        &self,
        key: &crate::runtime::ObjectCacheKey,
        max_object_bytes: u64,
    ) -> Result<Option<Arc<GitObject>>> {
        let Some(object) = self.runtime.cached_object(key).await else {
            return Ok(None);
        };
        check_limit(
            "decoded object bytes",
            object.data.len() as u64,
            max_object_bytes,
        )?;
        if let Err(source) = gix_object::Data::new(&object.data, object.kind, gix_hash::Kind::Sha1)
            .verify_checksum(&object.oid)
        {
            self.runtime.remove_object(key).await;
            return Err(Error::CacheCorrupt {
                oid: object.oid,
                source,
            });
        }
        Ok(Some(object))
    }

    async fn read_packed_entry(
        self: &Arc<Self>,
        oid: gix_hash::ObjectId,
        locator: GitObjectLocator,
        max_object_bytes: u64,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<PackedEntry> {
        check_limit(
            "packed entry bytes",
            locator.location.entry_len,
            self.limits.max_packed_entry_bytes,
        )?;
        charge_origin_range(budget, locator.location.entry_len).await?;
        let end = locator
            .location
            .pack_offset
            .checked_add(locator.location.entry_len)
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            })?;
        let path = repo_pack_path(&self.repo_prefix, &locator.pack_id);
        let store = self.store.clone();
        let runtime = Arc::clone(&self.runtime);
        let work_runtime = Arc::clone(&self.runtime);
        let cache_key = crate::runtime::ObjectCacheKey::new(&self.identity, self.generation, oid);
        let work_cache_key = cache_key.clone();
        let reader = Arc::clone(self);
        let max_inflated = self.limits.max_inflated_entry_bytes;
        let pack_offset = locator.location.pack_offset;
        let entry_len = locator.location.entry_len;
        let crc32 = locator.location.crc32;
        let flight_budget = budget.clone();
        let packed = runtime
            .read_packed_singleflight(
                cache_key,
                max_inflated,
                max_object_bytes,
                cancellation,
                move |shared_cancellation| async move {
                    // An earlier flight can populate the cache while this caller
                    // waits for lookup/admission. Recheck before another origin read.
                    if let Some(object) = reader
                        .verified_cached_object(&work_cache_key, max_object_bytes)
                        .await?
                    {
                        let header = match object.kind {
                            gix_object::Kind::Commit => Header::Commit,
                            gix_object::Kind::Tree => Header::Tree,
                            gix_object::Kind::Blob => Header::Blob,
                            gix_object::Kind::Tag => Header::Tag,
                        };
                        return Ok(PackedEntry {
                            header,
                            inflated: object.data.clone(),
                            charged_budget: None,
                        });
                    }
                    let origin_permit = work_runtime.origin_permit(&shared_cancellation).await?;
                    let bytes = tokio::select! {
                        biased;
                        () = shared_cancellation.cancelled() => return Err(Error::Cancelled),
                        bytes = store.range_get(&path, pack_offset..end) => bytes?,
                    };
                    drop(origin_permit);
                    check_cancelled(&shared_cancellation)?;
                    if bytes.len() as u64 != entry_len {
                        return Err(Error::Corrupt {
                            stage: CorruptionStage::PackEntry,
                        });
                    }
                    if gix_features::hash::crc32(&bytes) != crc32 {
                        return Err(Error::PackedEntryCrcMismatch { oid });
                    }
                    let inflated_bytes = packed_entry_allocation_bytes(
                        oid,
                        pack_offset,
                        &bytes,
                        max_inflated,
                        max_object_bytes,
                    )?;
                    flight_budget
                        .charge(BudgetDimension::InflatedBytes, inflated_bytes)
                        .await?;
                    let decode_permit = work_runtime.decode_permit(&shared_cancellation).await?;
                    let decode_cancellation = shared_cancellation.clone();
                    let packed = work_runtime
                        .spawn_blocking(move || {
                            inflate_entry(
                                oid,
                                pack_offset,
                                bytes,
                                max_inflated,
                                max_object_bytes,
                                &decode_cancellation,
                            )
                        })
                        .await
                        .map_err(|source| Error::DecodeTask { source })??;
                    drop(decode_permit);
                    if let Some(kind) = packed.header.as_kind() {
                        verify_object(oid, kind, &packed.inflated)?;
                        work_runtime
                            .insert_object(
                                work_cache_key,
                                Arc::new(GitObject {
                                    oid,
                                    kind,
                                    data: packed.inflated.clone(),
                                }),
                            )
                            .await;
                    }
                    Ok(PackedEntry {
                        header: packed.header,
                        inflated: packed.inflated,
                        charged_budget: Some(flight_budget.id()),
                    })
                },
            )
            .await?;
        if packed.charged_budget != Some(budget.id()) {
            budget
                .charge(BudgetDimension::InflatedBytes, packed.inflated.len() as u64)
                .await?;
        }
        Ok(PackedEntry {
            header: packed.header,
            inflated: packed.inflated.clone(),
            charged_budget: packed.charged_budget,
        })
    }

    async fn inspect_packed_entry(
        &self,
        oid: gix_hash::ObjectId,
        locator: GitObjectLocator,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<EntryDescriptor> {
        let bytes = self
            .fetch_packed_entry(oid, locator, budget, cancellation)
            .await?;
        let max_inflated = self.limits.max_inflated_entry_bytes;
        let pack_offset = locator.location.pack_offset;
        let pack_id = locator.pack_id;
        let entry = gix_pack::data::Entry::from_bytes(&bytes, pack_offset, 20)
            .map_err(|source| Error::PackEntry { oid, source })?;
        if entry.header.as_kind().is_none() {
            check_limit(
                "inflated pack entry bytes",
                entry.decompressed_size,
                max_inflated,
            )?;
            budget
                .charge(BudgetDimension::InflatedBytes, entry.decompressed_size)
                .await?;
        }
        let decode_permit = self.runtime.decode_permit(cancellation).await?;
        let token = cancellation.clone();
        let descriptor = self
            .runtime
            .spawn_blocking(move || {
                inspect_entry(oid, pack_id, pack_offset, bytes, max_inflated, &token)
            })
            .await
            .map_err(|source| Error::DecodeTask { source })??;
        drop(decode_permit);
        Ok(descriptor)
    }

    async fn fetch_packed_entry(
        &self,
        oid: gix_hash::ObjectId,
        locator: GitObjectLocator,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Bytes> {
        check_limit(
            "packed entry bytes",
            locator.location.entry_len,
            self.limits.max_packed_entry_bytes,
        )?;
        charge_origin_range(budget, locator.location.entry_len).await?;
        let end = locator
            .location
            .pack_offset
            .checked_add(locator.location.entry_len)
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            })?;
        let path = repo_pack_path(&self.repo_prefix, &locator.pack_id);
        let origin_permit = self.runtime.origin_permit(cancellation).await?;
        let bytes = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            bytes = self.store.range_get(&path, locator.location.pack_offset..end) => bytes?,
        };
        drop(origin_permit);
        check_cancelled(cancellation)?;
        if bytes.len() as u64 != locator.location.entry_len {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            });
        }
        if gix_features::hash::crc32(&bytes) != locator.location.crc32 {
            return Err(Error::PackedEntryCrcMismatch { oid });
        }
        Ok(bytes)
    }

    async fn oid_at_pack_offset(
        &self,
        pack_id: MerkleHash,
        pack_offset: u64,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<gix_hash::ObjectId> {
        let index = self.load_pack_index(pack_id, budget, cancellation).await?;
        index
            .oid_at_offset(pack_offset)
            .ok_or(Error::DeltaBaseNotFound {
                pack_id,
                pack_offset,
            })
    }

    async fn load_pack_index(
        &self,
        pack_id: MerkleHash,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Arc<PackIndex>> {
        let cache_key = crate::runtime::PackIndexCacheKey::new(&self.identity, pack_id);
        let cached = self
            .runtime
            .cached_pack_index(&cache_key, self.limits.max_pack_index_bytes)
            .await;
        if let Some(index) = cached {
            return Ok(index);
        }
        let inventory = self
            .inventory
            .get(&pack_id)
            .copied()
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::Inventory,
            })?;
        let path = repo_pack_index_path(&self.repo_prefix, &pack_id);
        let source_size = if let Some(source_size) =
            self.runtime.cached_pack_index_source_size(&cache_key).await
        {
            source_size
        } else {
            // HEAD is immutable metadata, but it still consumes an origin
            // request. Coalesce concurrent misses before reading the index so
            // a fanout of delta-base lookups does not multiply HEAD traffic.
            budget.charge(BudgetDimension::StorageRequests, 1).await?;
            let store = self.store.clone();
            let path = path.clone();
            let work_runtime = Arc::clone(&self.runtime);
            self.runtime
                .load_pack_index_size_singleflight(
                    cache_key.clone(),
                    cancellation,
                    move |shared_cancellation| async move {
                        let origin_permit =
                            work_runtime.origin_permit(&shared_cancellation).await?;
                        let metadata = tokio::select! {
                            biased;
                            () = shared_cancellation.cancelled() => return Err(Error::Cancelled),
                            metadata = store.head(&path) => metadata?,
                        };
                        drop(origin_permit);
                        Ok(metadata.size)
                    },
                )
                .await?
        };
        check_limit(
            "pack index bytes",
            source_size,
            self.limits.max_pack_index_bytes,
        )?;
        // A concurrent producer may have populated the parsed cache while this
        // caller waited for the size flight. Avoid charging or loading it
        // again when the verified index is already available.
        if let Some(index) = self
            .runtime
            .cached_pack_index(&cache_key, self.limits.max_pack_index_bytes)
            .await
        {
            return Ok(index);
        }
        charge_origin_range(budget, source_size).await?;
        let store = self.store.clone();
        let size = source_size;
        let flight_runtime = Arc::clone(&self.runtime);
        let work_runtime = Arc::clone(&self.runtime);
        let index = flight_runtime
            .load_pack_index_singleflight(
                cache_key,
                self.limits.max_pack_index_bytes,
                cancellation,
                move |shared_cancellation| async move {
                    let origin_permit = work_runtime.origin_permit(&shared_cancellation).await?;
                    let bytes = tokio::select! {
                        biased;
                        () = shared_cancellation.cancelled() => return Err(Error::Cancelled),
                        bytes = store.range_get(&path, 0..size) => bytes?,
                    };
                    drop(origin_permit);
                    check_cancelled(&shared_cancellation)?;
                    if bytes.len() as u64 != size {
                        return Err(Error::Corrupt {
                            stage: CorruptionStage::PackIndex,
                        });
                    }
                    let decode_permit = work_runtime.decode_permit(&shared_cancellation).await?;
                    let token = shared_cancellation.clone();
                    let index = work_runtime
                        .spawn_blocking(move || parse_pack_index(pack_id, inventory, bytes, &token))
                        .await
                        .map_err(|source| Error::DecodeTask { source })??;
                    drop(decode_permit);
                    Ok(index)
                },
            )
            .await?;
        Ok(index)
    }

    pub(crate) async fn pack_checksum_for_exact_objects(
        &self,
        pack_id: MerkleHash,
        object_ids: &[gix_hash::ObjectId],
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Option<[u8; 20]>> {
        let index = self.load_pack_index(pack_id, budget, cancellation).await?;
        if index.object_ids.len() != object_ids.len() {
            return Ok(None);
        }
        let matches = object_ids
            .iter()
            .all(|oid| index.object_ids.binary_search(oid).is_ok());
        Ok(matches.then_some(index.pack_checksum))
    }

    pub(crate) async fn download_pack_to_path(
        &self,
        pack_id: MerkleHash,
        expected_size: u64,
        destination: &std::path::Path,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<VerifiedPackIdentity> {
        use tokio::io::AsyncWriteExt as _;

        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        budget
            .charge(BudgetDimension::FetchedBytes, expected_size)
            .await?;
        tracing::debug!(
            storage_request = "pack_stream",
            storage_bytes = expected_size,
            "remote Git object-store request"
        );
        let path = repo_pack_path(&self.repo_prefix, &pack_id);
        let origin_permit = self.runtime.origin_permit(cancellation).await?;
        let (metadata, range, mut stream) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            result = self.store.get_stream(&path, None) => result?,
        };
        if metadata.size != expected_size || range != (0..expected_size) {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Inventory,
            });
        }
        if expected_size < 32 {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            });
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(destination)
            .await
            .map_err(|source| {
                Error::Metadata(crab_metadata::error::MetadataError::Io { source })
            })?;
        let mut verifier = PackStreamVerifier::default();
        let mut written = 0u64;
        while let Some(chunk) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk?;
            verifier.update(&chunk);
            file.write_all(&chunk).await.map_err(|source| {
                Error::Metadata(crab_metadata::error::MetadataError::Io { source })
            })?;
            written = written
                .checked_add(chunk.len() as u64)
                .ok_or(Error::Corrupt {
                    stage: CorruptionStage::PackEntry,
                })?;
        }
        file.flush().await.map_err(|source| {
            Error::Metadata(crab_metadata::error::MetadataError::Io { source })
        })?;
        drop(origin_permit);
        if written != expected_size {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            });
        }
        let identity = verifier.finish()?;
        let actual_content_hash = blake3::Hash::from_bytes(identity.content_hash).to_hex();
        if actual_content_hash.as_str() != pack_id.to_string() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            });
        }
        Ok(identity)
    }

    pub(crate) async fn download_pack_index_to_path(
        &self,
        pack_id: MerkleHash,
        maximum_size: u64,
        destination: &std::path::Path,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.download_pack_artifact_to_path(
            repo_pack_index_path(&self.repo_prefix, &pack_id),
            maximum_size,
            destination,
            budget,
            cancellation,
            "pack_index_stream",
            "pack index bytes",
        )
        .await
    }

    pub(crate) async fn download_pack_reverse_index_to_path(
        &self,
        pack_id: MerkleHash,
        maximum_size: u64,
        destination: &std::path::Path,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.download_pack_artifact_to_path(
            repo_pack_reverse_index_path(&self.repo_prefix, &pack_id),
            maximum_size,
            destination,
            budget,
            cancellation,
            "pack_reverse_index_stream",
            "pack reverse-index bytes",
        )
        .await
    }

    async fn download_pack_artifact_to_path(
        &self,
        path: object_store::path::Path,
        maximum_size: u64,
        destination: &std::path::Path,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
        storage_request: &'static str,
        limit: &'static str,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt as _;

        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        let origin_permit = self.runtime.origin_permit(cancellation).await?;
        let result = async {
            let (metadata, range, mut stream) = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                result = self.store.get_stream(&path, None) => result?,
            };
            if range != (0..metadata.size) {
                return Err(Error::Corrupt {
                    stage: CorruptionStage::PackIndex,
                });
            }
            check_limit(limit, metadata.size, maximum_size)?;
            budget
                .charge(BudgetDimension::FetchedBytes, metadata.size)
                .await?;
            tracing::debug!(
                storage_request,
                storage_bytes = metadata.size,
                "remote Git sidecar object-store request"
            );
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(destination)
                .await
                .map_err(|source| {
                    Error::Metadata(crab_metadata::error::MetadataError::Io { source })
                })?;
            let mut written = 0_u64;
            while let Some(chunk) = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                chunk = stream.next() => chunk,
            } {
                let chunk = chunk?;
                let next = written
                    .checked_add(chunk.len() as u64)
                    .ok_or(Error::Corrupt {
                        stage: CorruptionStage::PackIndex,
                    })?;
                if next > metadata.size {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::PackIndex,
                    });
                }
                if next > maximum_size {
                    return Err(Error::LimitExceeded {
                        limit,
                        actual: next,
                        maximum: maximum_size,
                    });
                }
                file.write_all(&chunk).await.map_err(|source| {
                    Error::Metadata(crab_metadata::error::MetadataError::Io { source })
                })?;
                written = next;
            }
            file.flush().await.map_err(|source| {
                Error::Metadata(crab_metadata::error::MetadataError::Io { source })
            })?;
            if written != metadata.size {
                return Err(Error::Corrupt {
                    stage: CorruptionStage::PackIndex,
                });
            }
            Ok(())
        }
        .await;
        drop(origin_permit);
        if result.is_err() {
            let _ = tokio::fs::remove_file(destination).await;
        }
        result
    }
}

fn coalesce_ranges(
    entries: Vec<(gix_hash::ObjectId, GitObjectLocator)>,
) -> Result<Vec<CoalescedRange>> {
    let mut by_pack: HashMap<MerkleHash, Vec<(gix_hash::ObjectId, GitObjectLocator)>> =
        HashMap::new();
    for (oid, locator) in entries {
        locator
            .location
            .pack_offset
            .checked_add(locator.location.entry_len)
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::PackEntry,
            })?;
        by_pack
            .entry(locator.pack_id)
            .or_default()
            .push((oid, locator));
    }

    let mut ranges = Vec::new();
    for mut entries in by_pack.into_values() {
        entries.sort_unstable_by_key(|(_, locator)| locator.location.pack_offset);
        let mut current: Option<CoalescedRange> = None;
        for (oid, locator) in entries {
            let start = locator.location.pack_offset;
            let end = start
                .checked_add(locator.location.entry_len)
                .ok_or(Error::Corrupt {
                    stage: CorruptionStage::PackEntry,
                })?;
            let can_extend = current.as_ref().is_some_and(|range| {
                range.pack_id == locator.pack_id
                    && start <= range.end.saturating_add(MAX_COALESCED_GAP_BYTES)
                    && end.saturating_sub(range.start) <= MAX_COALESCED_RANGE_BYTES
            });
            if can_extend {
                let range = current.as_mut().ok_or(Error::InternalInvariant {
                    invariant: "coalesced range disappeared while extending",
                })?;
                range.end = range.end.max(end);
                range.entries.push((oid, locator));
                continue;
            }
            if let Some(range) = current.take() {
                ranges.push(range);
            }
            current = Some(CoalescedRange {
                pack_id: locator.pack_id,
                start,
                end,
                entries: vec![(oid, locator)],
            });
        }
        if let Some(range) = current {
            ranges.push(range);
        }
    }
    Ok(ranges)
}

fn collect_missing_delta_bases(
    oid: gix_hash::ObjectId,
    depth: usize,
    entries: &HashMap<gix_hash::ObjectId, RemoteGitPackedEntry>,
    visited: &mut HashMap<gix_hash::ObjectId, usize>,
    missing: &mut Vec<gix_hash::ObjectId>,
    max_delta_depth: usize,
) -> Result<()> {
    if visited.get(&oid).is_some_and(|known| *known <= depth) {
        return Ok(());
    }
    visited.insert(oid, depth);
    let Some(entry) = entries.get(&oid) else {
        missing.push(oid);
        return Ok(());
    };
    let Some(base_oid) = entry.base_oid else {
        return Ok(());
    };
    if depth >= max_delta_depth {
        return Err(Error::LimitExceeded {
            limit: "delta depth",
            actual: depth.saturating_add(1) as u64,
            maximum: max_delta_depth as u64,
        });
    }
    collect_missing_delta_bases(
        base_oid,
        depth.saturating_add(1),
        entries,
        visited,
        missing,
        max_delta_depth,
    )
}

struct PackedEntryResolver {
    entries: HashMap<gix_hash::ObjectId, RemoteGitPackedEntry>,
    objects: HashMap<gix_hash::ObjectId, GitObject>,
    resolving: HashSet<gix_hash::ObjectId>,
    max_object_bytes: u64,
    max_inflated_entry_bytes: u64,
    max_delta_depth: usize,
}

impl PackedEntryResolver {
    fn new(
        entries: HashMap<gix_hash::ObjectId, RemoteGitPackedEntry>,
        max_object_bytes: u64,
        max_inflated_entry_bytes: u64,
        max_delta_depth: usize,
    ) -> Self {
        Self {
            entries,
            objects: HashMap::new(),
            resolving: HashSet::new(),
            max_object_bytes,
            max_inflated_entry_bytes,
            max_delta_depth,
        }
    }

    fn resolve_many(
        &mut self,
        requested: &[gix_hash::ObjectId],
        max_inflated_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<GitObject>, u64)> {
        let mut objects = Vec::with_capacity(requested.len());
        let mut inflated_bytes = 0u64;
        for oid in requested {
            let (object, added_bytes) =
                self.resolve(*oid, 0, inflated_bytes, max_inflated_bytes, cancellation)?;
            inflated_bytes =
                inflated_bytes
                    .checked_add(added_bytes)
                    .ok_or(Error::LimitExceeded {
                        limit: "inflated bytes",
                        actual: u64::MAX,
                        maximum: max_inflated_bytes,
                    })?;
            objects.push(object);
        }
        Ok((objects, inflated_bytes))
    }

    fn resolve(
        &mut self,
        oid: gix_hash::ObjectId,
        depth: usize,
        inflated_bytes: u64,
        max_inflated_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<(GitObject, u64)> {
        check_cancelled(cancellation)?;
        if let Some(object) = self.objects.get(&oid) {
            return Ok((object.clone(), 0));
        }
        if !self.resolving.insert(oid) {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Delta,
            });
        }
        let entry = self
            .entries
            .get(&oid)
            .cloned()
            .ok_or(Error::ObjectNotFound { oid })?;
        let entry_bytes = entry.decompressed_size;
        ensure_inflated_budget(inflated_bytes, entry_bytes, max_inflated_bytes)?;
        let packed = inflate_entry(
            oid,
            entry.pack_offset,
            entry.bytes,
            self.max_inflated_entry_bytes,
            self.max_object_bytes,
            cancellation,
        )?;
        let mut added_bytes = entry_bytes;
        let object = if let Some(kind) = packed.header.as_kind() {
            verify_object(oid, kind, &packed.inflated)?;
            GitObject {
                oid,
                kind,
                data: packed.inflated,
            }
        } else {
            if depth >= self.max_delta_depth {
                return Err(Error::LimitExceeded {
                    limit: "delta depth",
                    actual: depth.saturating_add(1) as u64,
                    maximum: self.max_delta_depth as u64,
                });
            }
            let base_oid = entry.base_oid.ok_or(Error::Corrupt {
                stage: CorruptionStage::Delta,
            })?;
            let (base, base_bytes) = self.resolve(
                base_oid,
                depth.saturating_add(1),
                inflated_bytes.saturating_add(entry_bytes),
                max_inflated_bytes,
                cancellation,
            )?;
            let maximum = usize_limit(self.max_object_bytes, "decoded object bytes")?;
            let instructions = packed.inflated;
            let parsed = delta::parse(&instructions, maximum)
                .map_err(|error| map_delta_error(error, oid))?;
            let result_size = parsed.result_size as u64;
            ensure_inflated_budget(
                inflated_bytes.saturating_add(entry_bytes),
                base_bytes,
                max_inflated_bytes,
            )?;
            ensure_inflated_budget(
                inflated_bytes
                    .saturating_add(entry_bytes)
                    .saturating_add(base_bytes),
                result_size,
                max_inflated_bytes,
            )?;
            let data = delta::apply(&base.data, parsed, || cancellation.is_cancelled())
                .map_err(|error| map_delta_error(error, oid))?;
            added_bytes = added_bytes
                .checked_add(base_bytes)
                .and_then(|value| value.checked_add(result_size))
                .ok_or(Error::LimitExceeded {
                    limit: "inflated bytes",
                    actual: u64::MAX,
                    maximum: max_inflated_bytes,
                })?;
            verify_object(oid, base.kind, &data)?;
            GitObject {
                oid,
                kind: base.kind,
                data: Bytes::from(data),
            }
        };
        ensure_inflated_budget(inflated_bytes, added_bytes, max_inflated_bytes)?;
        self.resolving.remove(&oid);
        self.objects.insert(oid, object.clone());
        Ok((object, added_bytes))
    }
}

fn ensure_inflated_budget(current: u64, additional: u64, maximum: u64) -> Result<()> {
    let actual = current
        .checked_add(additional)
        .ok_or(Error::LimitExceeded {
            limit: "inflated bytes",
            actual: u64::MAX,
            maximum,
        })?;
    if actual > maximum {
        return Err(Error::LimitExceeded {
            limit: "inflated bytes",
            actual,
            maximum,
        });
    }
    Ok(())
}

fn order_completed_objects(
    requested: &[gix_hash::ObjectId],
    completed: &HashMap<gix_hash::ObjectId, GitObject>,
) -> Result<Vec<GitObject>> {
    let mut ordered = Vec::new();
    ordered
        .try_reserve_exact(requested.len())
        .map_err(|source| Error::Allocation {
            requested: requested
                .len()
                .saturating_mul(std::mem::size_of::<GitObject>()),
            source,
        })?;
    for oid in requested {
        ordered.push(
            completed
                .get(oid)
                .cloned()
                .ok_or(Error::InternalInvariant {
                    invariant: "completed object is absent from batch result",
                })?,
        );
    }
    Ok(ordered)
}

async fn charge_origin_range(budget: &OperationBudget, bytes: u64) -> Result<()> {
    budget.charge(BudgetDimension::StorageRequests, 1).await?;
    budget.charge(BudgetDimension::FetchedBytes, bytes).await?;
    tracing::debug!(
        storage_request = "range_get",
        storage_bytes = bytes,
        "remote Git object-store request"
    );
    Ok(())
}

pub(crate) struct PackedEntry {
    pub(crate) header: Header,
    pub(crate) inflated: Bytes,
    pub(crate) charged_budget: Option<u64>,
}

struct MetadataDelta {
    base_size: u64,
    result_size: u64,
}

#[derive(Debug, Clone, Copy)]
enum EntryDescriptor {
    Base {
        kind: gix_object::Kind,
        size: u64,
    },
    RefDelta {
        base: gix_hash::ObjectId,
        base_size: u64,
        result_size: u64,
    },
    OfsDelta {
        pack_id: MerkleHash,
        pack_offset: u64,
        base_distance: u64,
        base_size: u64,
        result_size: u64,
    },
}

pub(crate) struct PackIndex {
    pub(crate) object_ids: Vec<gix_hash::ObjectId>,
    pub(crate) pack_offsets: Vec<u64>,
    pub(crate) crc32: Vec<u32>,
    pub(crate) offset_order: Vec<u32>,
    pub(crate) pack_data_end: u64,
    pub(crate) pack_checksum: [u8; 20],
    pub(crate) source_bytes: u64,
}

impl PackIndex {
    fn location_for(&self, oid: &gix_hash::ObjectId) -> Result<Option<GitObjectLocation>> {
        let Some(position) = self.object_ids.binary_search(oid).ok() else {
            return Ok(None);
        };
        let pack_offset = *self.pack_offsets.get(position).ok_or(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        })?;
        let sorted_position = self
            .offset_order
            .binary_search_by_key(&pack_offset, |entry_position| {
                self.pack_offsets
                    .get(*entry_position as usize)
                    .copied()
                    .unwrap_or(u64::MAX)
            })
            .map_err(|_| Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            })?;
        let next_offset = self
            .offset_order
            .get(sorted_position.saturating_add(1))
            .and_then(|entry_position| self.pack_offsets.get(*entry_position as usize))
            .copied()
            .unwrap_or(self.pack_data_end);
        let entry_len = next_offset.checked_sub(pack_offset).ok_or(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        })?;
        let crc32 = *self.crc32.get(position).ok_or(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        })?;
        Ok(Some(GitObjectLocation {
            pack_offset,
            entry_len,
            crc32,
        }))
    }

    fn oid_at_offset(&self, pack_offset: u64) -> Option<gix_hash::ObjectId> {
        let sorted_position = self
            .offset_order
            .binary_search_by_key(&pack_offset, |entry_position| {
                self.pack_offsets
                    .get(*entry_position as usize)
                    .copied()
                    .unwrap_or(u64::MAX)
            })
            .ok()?;
        let position = *self.offset_order.get(sorted_position)? as usize;
        self.object_ids.get(position).copied()
    }

    pub(crate) fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.object_ids
                    .capacity()
                    .saturating_mul(std::mem::size_of::<gix_hash::ObjectId>()),
            )
            .saturating_add(
                self.pack_offsets
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u64>()),
            )
            .saturating_add(
                self.crc32
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u32>()),
            )
            .saturating_add(
                self.offset_order
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u32>()),
            )
    }
}

fn inflate_entry(
    oid: gix_hash::ObjectId,
    pack_offset: u64,
    bytes: Bytes,
    max_inflated: u64,
    max_object: u64,
    cancellation: &CancellationToken,
) -> Result<PackedEntry> {
    check_cancelled(cancellation)?;
    let entry = gix_pack::data::Entry::from_bytes(&bytes, pack_offset, 20)
        .map_err(|source| Error::PackEntry { oid, source })?;
    let (limit, maximum) = if entry.header.as_kind().is_some() {
        ("decoded object bytes", max_inflated.min(max_object))
    } else {
        ("inflated pack entry bytes", max_inflated)
    };
    check_limit(limit, entry.decompressed_size, maximum)?;
    let size = usize_limit(entry.decompressed_size, "inflated pack entry bytes")?;
    let header_size =
        usize::try_from(entry.data_offset.saturating_sub(pack_offset)).map_err(|_| {
            Error::InvalidInflatedEntry {
                oid,
                reason: InflatedEntryError::HeaderNotAddressable,
            }
        })?;
    let compressed = bytes
        .get(header_size..)
        .ok_or(Error::InvalidInflatedEntry {
            oid,
            reason: InflatedEntryError::HeaderExceedsRange,
        })?;
    let allocation = size.max(1);
    let mut inflated = Vec::new();
    inflated
        .try_reserve_exact(allocation)
        .map_err(|source| Error::Allocation {
            requested: allocation,
            source,
        })?;
    inflated.resize(allocation, 0);
    let mut inflater = gix_features::zlib::Inflate::default();
    let (status, consumed_in, consumed_out) = inflater
        .once(compressed, &mut inflated)
        .map_err(|source| Error::Inflate { oid, source })?;
    check_cancelled(cancellation)?;
    if status != gix_features::zlib::Status::StreamEnd {
        return Err(Error::InvalidInflatedEntry {
            oid,
            reason: InflatedEntryError::StreamDidNotTerminate,
        });
    }
    if consumed_in != compressed.len() {
        return Err(Error::InvalidInflatedEntry {
            oid,
            reason: InflatedEntryError::TrailingBytes,
        });
    }
    if consumed_out != size {
        return Err(Error::InvalidInflatedEntry {
            oid,
            reason: InflatedEntryError::SizeMismatch,
        });
    }
    inflated.truncate(size);
    Ok(PackedEntry {
        header: entry.header,
        inflated: Bytes::from(inflated),
        charged_budget: None,
    })
}

fn packed_entry_allocation_bytes(
    oid: gix_hash::ObjectId,
    pack_offset: u64,
    bytes: &[u8],
    max_inflated: u64,
    max_object: u64,
) -> Result<u64> {
    let entry = gix_pack::data::Entry::from_bytes(bytes, pack_offset, 20)
        .map_err(|source| Error::PackEntry { oid, source })?;
    let (limit, maximum) = if entry.header.as_kind().is_some() {
        ("decoded object bytes", max_inflated.min(max_object))
    } else {
        ("inflated pack entry bytes", max_inflated)
    };
    check_limit(limit, entry.decompressed_size, maximum)?;
    Ok(entry.decompressed_size)
}

fn inspect_entry(
    oid: gix_hash::ObjectId,
    pack_id: MerkleHash,
    pack_offset: u64,
    bytes: Bytes,
    max_inflated: u64,
    cancellation: &CancellationToken,
) -> Result<EntryDescriptor> {
    check_cancelled(cancellation)?;
    let entry = gix_pack::data::Entry::from_bytes(&bytes, pack_offset, 20)
        .map_err(|source| Error::PackEntry { oid, source })?;
    if let Some(kind) = entry.header.as_kind() {
        return Ok(EntryDescriptor::Base {
            kind,
            size: entry.decompressed_size,
        });
    }
    let packed = inflate_entry(
        oid,
        pack_offset,
        bytes,
        max_inflated,
        u64::MAX,
        cancellation,
    )?;
    check_cancelled(cancellation)?;
    let delta =
        delta::parse(&packed.inflated, usize::MAX).map_err(|error| map_delta_error(error, oid))?;
    delta::validate(&delta, || cancellation.is_cancelled())
        .map_err(|error| map_delta_error(error, oid))?;
    let base_size = delta.base_size as u64;
    let result_size = delta.result_size as u64;
    match packed.header {
        Header::RefDelta { base_id } => Ok(EntryDescriptor::RefDelta {
            base: base_id,
            base_size,
            result_size,
        }),
        Header::OfsDelta { base_distance } => Ok(EntryDescriptor::OfsDelta {
            pack_id,
            pack_offset,
            base_distance,
            base_size,
            result_size,
        }),
        Header::Commit | Header::Tree | Header::Blob | Header::Tag => {
            Err(Error::InternalInvariant {
                invariant: "base pack entry changed kind while inspecting metadata",
            })
        }
    }
}

fn parse_pack_index(
    pack_id: MerkleHash,
    inventory: GitPackInventoryEntry,
    bytes: Bytes,
    cancellation: &CancellationToken,
) -> Result<PackIndex> {
    check_cancelled(cancellation)?;
    let source_bytes = bytes.len() as u64;
    if bytes.len() < 20 {
        return Err(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        });
    }
    let checksum_start = bytes.len() - 20;
    let mut hasher = Sha1::new();
    for chunk in bytes[..checksum_start].chunks(1024 * 1024) {
        check_cancelled(cancellation)?;
        hasher.update(chunk);
    }
    let actual = hasher.finalize();
    if actual.as_slice() != &bytes[checksum_start..] {
        return Err(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        });
    }
    let index = gix_pack::index::File::from_data(
        bytes,
        PathBuf::from(format!("pack-{pack_id}.idx")),
        gix_hash::Kind::Sha1,
    )
    .map_err(|source| Error::PackIndex { pack_id, source })?;
    check_cancelled(cancellation)?;
    if index.version() != gix_pack::index::Version::V2 {
        return Err(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        });
    }
    if u64::from(index.num_objects()) != inventory.object_count {
        return Err(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        });
    }
    let pack_checksum =
        index
            .pack_checksum()
            .as_bytes()
            .try_into()
            .map_err(|_| Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            })?;
    let pack_data_end = inventory.pack_size.checked_sub(20).ok_or(Error::Corrupt {
        stage: CorruptionStage::Inventory,
    })?;
    let capacity = index.num_objects() as usize;
    let mut object_ids = Vec::new();
    object_ids
        .try_reserve_exact(capacity)
        .map_err(|source| Error::Allocation {
            requested: capacity.saturating_mul(std::mem::size_of::<gix_hash::ObjectId>()),
            source,
        })?;
    let mut pack_offsets = Vec::new();
    pack_offsets
        .try_reserve_exact(capacity)
        .map_err(|source| Error::Allocation {
            requested: capacity.saturating_mul(std::mem::size_of::<u64>()),
            source,
        })?;
    let mut crc32 = Vec::new();
    crc32
        .try_reserve_exact(capacity)
        .map_err(|source| Error::Allocation {
            requested: capacity.saturating_mul(std::mem::size_of::<u32>()),
            source,
        })?;
    let mut previous = None;
    for (position, entry) in index.iter().enumerate() {
        if position % 4_096 == 0 {
            check_cancelled(cancellation)?;
        }
        if entry.pack_offset < 12 || entry.pack_offset >= pack_data_end {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            });
        }
        if previous.is_some_and(|previous| previous >= entry.oid) {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            });
        }
        previous = Some(entry.oid);
        object_ids.push(entry.oid);
        pack_offsets.push(entry.pack_offset);
        crc32.push(entry.crc32.ok_or(Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        })?);
    }
    let mut offset_order = Vec::new();
    offset_order
        .try_reserve_exact(capacity)
        .map_err(|source| Error::Allocation {
            requested: capacity.saturating_mul(std::mem::size_of::<u32>()),
            source,
        })?;
    for position in 0..capacity {
        offset_order.push(u32::try_from(position).map_err(|_| Error::Corrupt {
            stage: CorruptionStage::PackIndex,
        })?);
    }
    offset_order.sort_unstable_by_key(|position| {
        pack_offsets
            .get(*position as usize)
            .copied()
            .unwrap_or(u64::MAX)
    });
    for window in offset_order.windows(2) {
        let current = pack_offsets
            .get(window[0] as usize)
            .copied()
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            })?;
        let next = pack_offsets
            .get(window[1] as usize)
            .copied()
            .ok_or(Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            })?;
        if next <= current {
            return Err(Error::Corrupt {
                stage: CorruptionStage::PackIndex,
            });
        }
    }
    Ok(PackIndex {
        object_ids,
        pack_offsets,
        crc32,
        offset_order,
        pack_data_end,
        pack_checksum,
        source_bytes,
    })
}

fn push_metadata_delta(
    deltas: &mut Vec<MetadataDelta>,
    base_size: u64,
    result_size: u64,
    limits: ReaderLimits,
) -> Result<()> {
    let depth = deltas.len().saturating_add(1);
    if depth > limits.max_delta_depth {
        return Err(Error::LimitExceeded {
            limit: "delta depth",
            actual: depth as u64,
            maximum: limits.max_delta_depth as u64,
        });
    }
    deltas.push(MetadataDelta {
        base_size,
        result_size,
    });
    Ok(())
}

fn verify_object(oid: gix_hash::ObjectId, kind: gix_object::Kind, data: &[u8]) -> Result<()> {
    gix_object::Data::new(data, kind, gix_hash::Kind::Sha1)
        .verify_checksum(&oid)
        .map_err(|source| Error::ObjectIdMismatch { oid, source })?;
    Ok(())
}

fn map_delta_error(error: delta::DeltaError, oid: gix_hash::ObjectId) -> Error {
    match error {
        delta::DeltaError::Invalid(reason) => Error::InvalidDelta { oid, reason },
        delta::DeltaError::ResultTooLarge { actual, maximum } => Error::LimitExceeded {
            limit: "decoded object bytes",
            actual: actual as u64,
            maximum: maximum as u64,
        },
        delta::DeltaError::Allocation { requested, source } => {
            Error::Allocation { requested, source }
        }
        delta::DeltaError::Cancelled => Error::Cancelled,
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn check_limit(limit: &'static str, actual: u64, maximum: u64) -> Result<()> {
    if actual > maximum {
        Err(Error::LimitExceeded {
            limit,
            actual,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn usize_limit(value: u64, limit: &'static str) -> Result<usize> {
    usize::try_from(value).map_err(|_| Error::LimitExceeded {
        limit,
        actual: value,
        maximum: usize::MAX as u64,
    })
}

#[cfg(test)]
mod tests {
    use crab_metadata::git_object_locator::{
        GitLocatorCoverage, GitObjectLocatorEntry, GitObjectLocatorWriter, GitPackLocatorRecord,
    };
    use object_store::memory::InMemory;

    use super::*;

    #[test]
    fn rejects_reconstructed_bytes_with_wrong_object_id() {
        let error = verify_object(
            gix_hash::ObjectId::empty_blob(gix_hash::Kind::Sha1),
            gix_object::Kind::Blob,
            b"not empty",
        )
        .expect_err("object ID mismatch must fail");
        assert!(matches!(error, Error::ObjectIdMismatch { .. }));
    }

    #[test]
    fn streamed_pack_identity_matches_git_and_storage_hashes_across_chunks() {
        let mut content = b"PACK".to_vec();
        content.extend_from_slice(&2_u32.to_be_bytes());
        content.extend_from_slice(&3_u32.to_be_bytes());
        content.extend((0_u8..37).map(|value| value.wrapping_mul(7)));
        let trailer: [u8; 20] = Sha1::digest(&content).into();
        let mut pack = content;
        pack.extend_from_slice(&trailer);

        let mut verifier = PackStreamVerifier::default();
        for chunk in pack.chunks(7) {
            verifier.update(chunk);
        }

        assert_eq!(
            verifier.finish().expect("valid streamed pack"),
            VerifiedPackIdentity {
                git_sha1: trailer,
                content_hash: *blake3::hash(&pack).as_bytes(),
            }
        );
    }

    #[test]
    fn streamed_pack_identity_rejects_a_corrupt_trailer() {
        let mut pack = b"PACK\0\0\0\x02\0\0\0\0payload".to_vec();
        pack.extend_from_slice(&[0_u8; 20]);
        let mut verifier = PackStreamVerifier::default();
        verifier.update(&pack);

        assert!(matches!(
            verifier.finish(),
            Err(Error::Corrupt {
                stage: CorruptionStage::PackEntry
            })
        ));
    }

    #[tokio::test]
    async fn corrupt_cached_object_is_discarded_and_reported() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let oid = gix_hash::ObjectId::empty_blob(gix_hash::Kind::Sha1);
        let key = crate::runtime::ObjectCacheKey::new(&identity, 1, oid);
        runtime
            .insert_object(
                key.clone(),
                Arc::new(GitObject {
                    oid,
                    kind: gix_object::Kind::Blob,
                    data: Bytes::from_static(b"not empty"),
                }),
            )
            .await;
        let object_store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let reader = RemoteGitReader::from_pinned(
            Store::new(object_store),
            "repository",
            [],
            ReaderLimits::default(),
            Arc::clone(&runtime),
            identity,
            1,
        )
        .expect("reader");

        let error = reader
            .verified_cached_object(&key, u64::MAX)
            .await
            .expect_err("corrupt cache entry must fail");
        assert!(matches!(error, Error::CacheCorrupt { .. }));
        assert!(runtime.cached_object(&key).await.is_none());
    }

    #[tokio::test]
    async fn admitted_packed_read_rechecks_verified_cache_and_caller_limits() {
        for scenario in ["valid", "corrupt", "object_limit", "budget_limit"] {
            let runtime = Arc::new(
                RemoteGitRuntime::new(
                    crate::RuntimeOptions {
                        max_object_flights: 1,
                        ..Default::default()
                    },
                    Arc::new(crate::NoopMetrics),
                )
                .unwrap(),
            );
            let identity = RepositoryIdentity::new("provider", "repository", 1).unwrap();
            let started = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let blocker_runtime = Arc::clone(&runtime);
            let blocker_started = Arc::clone(&started);
            let blocker_release = Arc::clone(&release);
            let blocker_key = crate::runtime::ObjectCacheKey::new(
                &identity,
                1,
                gix_hash::ObjectId::empty_tree(gix_hash::Kind::Sha1),
            );
            let blocker = tokio::spawn(async move {
                blocker_runtime
                    .read_packed_singleflight(
                        blocker_key,
                        1024,
                        1024,
                        &CancellationToken::new(),
                        move |_| async move {
                            blocker_started.notify_one();
                            blocker_release.notified().await;
                            Ok(PackedEntry {
                                header: Header::Tree,
                                inflated: Bytes::new(),
                                charged_budget: None,
                            })
                        },
                    )
                    .await
            });
            started.notified().await;
            let reader = Arc::new(
                RemoteGitReader::from_pinned(
                    Store::new(Arc::new(InMemory::new())),
                    "repository",
                    [],
                    ReaderLimits::default(),
                    Arc::clone(&runtime),
                    identity.clone(),
                    1,
                )
                .unwrap(),
            );
            let data = Bytes::from_static(b"cached base");
            let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, &data)
                .unwrap();
            let key = crate::runtime::ObjectCacheKey::new(&identity, 1, oid);
            let budget = OperationBudget::new(
                crate::OperationLimits {
                    max_inflated_bytes: if scenario == "budget_limit" { 1 } else { 1024 },
                    ..Default::default()
                },
                Arc::clone(&runtime),
                1,
            );
            let cancel = CancellationToken::new();
            let read = reader.read_packed_entry(
                oid,
                GitObjectLocator {
                    ordinal: 0,
                    pack_id: MerkleHash::from_hex(&"11".repeat(32)).unwrap(),
                    location: GitObjectLocation {
                        pack_offset: 12,
                        entry_len: 20,
                        crc32: 0,
                    },
                    metadata: Default::default(),
                },
                if scenario == "object_limit" { 1 } else { 1024 },
                &budget,
                &cancel,
            );
            tokio::pin!(read);
            assert!(futures_util::poll!(read.as_mut()).is_pending());
            // The object arrives after the caller missed it and queued for admission.
            runtime
                .insert_object(
                    key.clone(),
                    Arc::new(GitObject {
                        oid,
                        kind: gix_object::Kind::Blob,
                        data: if scenario == "corrupt" {
                            Bytes::from_static(b"corrupt")
                        } else {
                            data.clone()
                        },
                    }),
                )
                .await;
            release.notify_one();
            blocker.await.unwrap().unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), read)
                .await
                .unwrap();
            if scenario == "valid" {
                let packed = result.unwrap();
                assert_eq!((packed.header, packed.inflated), (Header::Blob, data));
            } else {
                let error = result.err().unwrap();
                let mut error = &error;
                while let Error::SharedRead { source } = error {
                    error = source;
                }
                match scenario {
                    "corrupt" => {
                        assert!(matches!(error, Error::CacheCorrupt { .. }));
                        assert!(runtime.cached_object(&key).await.is_none());
                    }
                    "object_limit" => assert!(matches!(
                        error,
                        Error::LimitExceeded {
                            limit: "decoded object bytes",
                            ..
                        }
                    )),
                    "budget_limit" => assert!(matches!(error, Error::LimitExceeded { .. })),
                    _ => unreachable!(),
                }
            }
            runtime.shutdown().await;
        }
    }

    #[test]
    fn coalesces_nearby_entries_without_crossing_bounds() {
        let first_pack = MerkleHash::from_hex(&"11".repeat(32)).expect("first pack hash");
        let second_pack = MerkleHash::from_hex(&"22".repeat(32)).expect("second pack hash");
        let oid = gix_hash::ObjectId::empty_blob(gix_hash::Kind::Sha1);
        let locator = |pack_id, pack_offset, entry_len| GitObjectLocator {
            ordinal: 0,
            pack_id,
            location: crab_metadata::git_object_locator::GitObjectLocation {
                pack_offset,
                entry_len,
                crc32: 0,
            },
            metadata: crab_metadata::git_object_locator::GitObjectMetadata::default(),
        };
        let ranges = coalesce_ranges(vec![
            (oid, locator(first_pack, 100, 20)),
            (oid, locator(first_pack, 130, 20)),
            (oid, locator(first_pack, 100_000, 20)),
            (oid, locator(second_pack, 100, 20)),
        ])
        .expect("valid ranges");

        assert_eq!(ranges.len(), 3);
        let merged = ranges
            .iter()
            .find(|range| range.entries.len() == 2)
            .expect("nearby entries must share one range");
        assert_eq!(merged.start, 100);
        assert_eq!(merged.end, 150);
        assert_eq!(
            ranges
                .iter()
                .filter(|range| range.entries.len() == 1)
                .count(),
            2
        );
    }

    #[test]
    fn pack_index_resolves_oid_locations_and_ofs_bases_without_a_map() {
        let oid = |value: u8| gix_hash::ObjectId::from([value; 20]);
        let index = PackIndex {
            object_ids: vec![oid(1), oid(2), oid(3)],
            pack_offsets: vec![100, 300, 200],
            crc32: vec![11, 33, 22],
            offset_order: vec![0, 2, 1],
            pack_data_end: 400,
            pack_checksum: [0; 20],
            source_bytes: 1,
        };

        assert_eq!(
            index.location_for(&oid(1)).expect("location"),
            Some(GitObjectLocation {
                pack_offset: 100,
                entry_len: 100,
                crc32: 11,
            })
        );
        assert_eq!(index.oid_at_offset(200), Some(oid(3)));
        assert_eq!(index.oid_at_offset(250), None);
        assert_eq!(index.location_for(&oid(9)).expect("missing lookup"), None);
    }

    #[tokio::test]
    async fn large_batch_lookup_uses_cached_pack_index_locations() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let pack_id = MerkleHash::from_hex(&"11".repeat(32)).expect("pack hash");
        let oid = |value: u8| gix_hash::ObjectId::from([value; 20]);
        runtime
            .insert_pack_index(
                crate::runtime::PackIndexCacheKey::new(&identity, pack_id),
                Arc::new(PackIndex {
                    object_ids: vec![oid(1)],
                    pack_offsets: vec![100],
                    crc32: vec![11],
                    offset_order: vec![0],
                    pack_data_end: 200,
                    pack_checksum: [0; 20],
                    source_bytes: 1,
                }),
            )
            .await;
        let store = Store::new(Arc::new(InMemory::new()));
        let reader = RemoteGitReader::from_pinned(
            store,
            "repository",
            [GitPackInventoryEntry {
                pack_id,
                object_count: 1,
                pack_size: 220,
            }],
            ReaderLimits::default(),
            Arc::clone(&runtime),
            identity.clone(),
            1,
        )
        .expect("reader");
        let budget = OperationBudget::new(crate::OperationLimits::default(), runtime, 1);
        let lookups = reader
            .lookup_batch_from_pack_indexes(&[[1; 20]], &budget, &CancellationToken::new())
            .await
            .expect("pack-index lookup");

        assert!(matches!(
            lookups.as_slice(),
            [GitObjectLookup::Hit(GitObjectLocator {
                pack_id: actual_pack,
                location: GitObjectLocation {
                    pack_offset: 100,
                    entry_len: 100,
                    crc32: 11,
                },
                ..
            })] if *actual_pack == pack_id
        ));
    }

    #[tokio::test]
    async fn large_batch_prefers_catalog_and_falls_back_for_catalog_misses() {
        let store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let pack_id = MerkleHash::from_hex(&"11".repeat(32)).expect("pack hash");
        let pack_index_hash = MerkleHash::from_hex(&"22".repeat(32)).expect("index hash");
        let pack = GitPackLocatorRecord {
            pack_id,
            committed_generation: 1,
            pack_index_hash,
            object_count: 2,
            pack_size: 220,
        };
        let catalog_oid = [1; 20];
        let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open catalog writer");
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(
                binding,
                &[GitObjectLocatorEntry {
                    oid: catalog_oid,
                    location: GitObjectLocation {
                        pack_offset: 12,
                        entry_len: 96,
                        crc32: 7,
                    },
                    metadata: Default::default(),
                }],
            )
            .await
            .expect("write catalog row");
        writer.flush_objects().await.expect("flush catalog row");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: pack.committed_generation,
                pack_index_hash,
            })
            .await
            .expect("publish catalog coverage");
        writer.close().await.expect("close catalog writer");

        let session = GitObjectLocatorSession::open(Arc::clone(&store), "org/repo")
            .await
            .expect("open catalog reader");
        assert!(session.is_available());
        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: pack.committed_generation,
                pack_index_hash,
            })
        );

        let runtime = Arc::new(RemoteGitRuntime::default());
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        runtime
            .insert_pack_index(
                crate::runtime::PackIndexCacheKey::new(&identity, pack_id),
                Arc::new(PackIndex {
                    object_ids: vec![gix_hash::ObjectId::from([2; 20])],
                    pack_offsets: vec![100],
                    crc32: vec![11],
                    offset_order: vec![0],
                    pack_data_end: 200,
                    pack_checksum: [0; 20],
                    source_bytes: 1,
                }),
            )
            .await;
        let reader = RemoteGitReader::from_pinned(
            Store::new(Arc::clone(&store)),
            "org/repo",
            [GitPackInventoryEntry {
                pack_id,
                object_count: pack.object_count,
                pack_size: pack.pack_size,
            }],
            ReaderLimits::default(),
            Arc::clone(&runtime),
            identity,
            pack.committed_generation,
        )
        .expect("reader");
        let budget = OperationBudget::new(crate::OperationLimits::default(), runtime, 1);
        let catalog_request = vec![catalog_oid; PACK_INDEX_LOOKUP_MIN_OBJECTS];
        let catalog_lookups = reader
            .lookup_batch_for_read(
                &session,
                &catalog_request,
                &budget,
                &CancellationToken::new(),
            )
            .await
            .expect("catalog lookup");
        assert!(
            catalog_lookups
                .iter()
                .all(|lookup| matches!(lookup, GitObjectLookup::Hit(_)))
        );
        let catalog_oids =
            vec![gix_hash::ObjectId::from(catalog_oid); PACK_INDEX_LOOKUP_MIN_OBJECTS];
        let catalog_locators = reader
            .lookup_packed_locators_with_session(
                &session,
                &catalog_oids,
                &budget,
                &CancellationToken::new(),
            )
            .await
            .expect("packed locator lookup");
        assert_eq!(catalog_locators.len(), catalog_oids.len());
        assert!(catalog_locators.iter().all(|locator| {
            locator.pack_id == pack_id
                && locator.location.pack_offset == 12
                && locator.location.entry_len == 96
        }));

        let fallback_request = vec![[2; 20]; PACK_INDEX_LOOKUP_MIN_OBJECTS];
        let fallback_lookups = reader
            .lookup_batch_for_read(
                &session,
                &fallback_request,
                &budget,
                &CancellationToken::new(),
            )
            .await
            .expect("pack-index fallback lookup");
        assert!(
            fallback_lookups
                .iter()
                .all(|lookup| matches!(lookup, GitObjectLookup::Hit(_)))
        );
        session.close().await.expect("close catalog reader");
    }

    #[test]
    fn collects_each_missing_delta_base_once_across_requested_objects() {
        let oid = |value: u8| gix_hash::ObjectId::from([value; 20]);
        let entry = |value: u8, base: Option<u8>| {
            let base_oid = base.map(oid);
            RemoteGitPackedEntry {
                oid: oid(value),
                pack_offset: 0,
                header: base_oid.map_or(Header::Blob, |base_id| Header::RefDelta { base_id }),
                decompressed_size: 1,
                header_size: 1,
                base_oid,
                bytes: Bytes::new(),
            }
        };
        let entries = HashMap::from([
            (oid(1), entry(1, Some(2))),
            (oid(2), entry(2, Some(3))),
            (oid(4), entry(4, Some(3))),
        ]);
        let mut visited = HashMap::new();
        let mut missing = Vec::new();
        for requested in [oid(1), oid(4)] {
            collect_missing_delta_bases(requested, 0, &entries, &mut visited, &mut missing, 128)
                .expect("delta dependency walk");
        }

        missing.sort_unstable();
        missing.dedup();
        assert_eq!(missing, [oid(3)]);
    }
}
