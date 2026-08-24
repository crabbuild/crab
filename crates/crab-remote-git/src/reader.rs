use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use crab_metadata::git_object_locator::{
    GitObjectLocator, GitObjectLocatorSession, GitObjectLookup, GitPackInventoryEntry,
};
use crab_storage::{Store, repo_pack_index_path, repo_pack_path};
use crab_xet::hash::MerkleHash;
use futures_util::stream::{self, StreamExt};
use gix_pack::data::entry::Header;
use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;

use crate::budget::OperationBudget;
use crate::{BudgetDimension, RemoteGitRuntime, RepositoryIdentity};
use crate::{CorruptionStage, Error, InflatedEntryError, RepositoryStateError, Result, delta};

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
const MAX_COALESCED_GAP_BYTES: u64 = 4 * 1024;

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
        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        tracing::debug!(
            storage_request = "locator_lookup",
            storage_bytes = 0u64,
            "remote Git object-store request"
        );
        let lookups = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            lookups = session.lookup_batch(&oid_bytes, &self.inventory) => lookups?,
        };
        if lookups.len() != missing.len() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            });
        }
        let mut ready = Vec::new();
        // The caller charges the complete logical object set before this
        // locator queue is allocated, so its count is operation bounded.
        ready
            .try_reserve_exact(missing.len())
            .map_err(|source| Error::Allocation {
                requested: missing
                    .len()
                    .saturating_mul(std::mem::size_of::<(gix_hash::ObjectId, GitObjectLocator)>()),
                source,
            })?;
        for (oid, lookup) in missing.iter().copied().zip(lookups) {
            let locator = match lookup {
                GitObjectLookup::Hit(locator) => locator,
                GitObjectLookup::Corrupt => {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::Locator,
                    });
                }
                GitObjectLookup::Miss => return Err(Error::ObjectNotFound { oid }),
            };
            check_limit(
                "packed entry bytes",
                locator.location.entry_len,
                self.limits.max_packed_entry_bytes,
            )?;
            ready.push((oid, locator));
        }
        let ranges = coalesce_ranges(ready)?;
        tracing::debug!(
            storage_request = "range_get_coalesced",
            range_count = ranges.len(),
            object_count = missing.len(),
            "coalesced remote Git object ranges"
        );
        let caller_cancellation = cancellation.clone();
        // `stream::iter` is lazy: at most this byte- and object-derived number
        // of futures owns fetched or inflated bytes at once.
        let results = stream::iter(ranges.into_iter().map(|range| {
            let reader = Arc::clone(self);
            let caller_cancellation = caller_cancellation.clone();
            async move {
                let bytes = reader
                    .read_coalesced_range(&range, budget, &caller_cancellation)
                    .await?;
                let mut group_results = Vec::with_capacity(range.entries.len());
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
                        group_results.push((oid, Err(Error::PackedEntryCrcMismatch { oid })));
                        continue;
                    }
                    let result = reader
                        .read_from_prefetched_locator(
                            session,
                            oid,
                            locator,
                            Bytes::copy_from_slice(entry_bytes),
                            reader.limits.max_object_bytes,
                            reader.limits.max_delta_depth,
                            budget,
                            &caller_cancellation,
                        )
                        .await;
                    if let Ok(object) = &result {
                        let key = crate::runtime::ObjectCacheKey::new(
                            &reader.identity,
                            reader.generation,
                            oid,
                        );
                        reader
                            .runtime
                            .insert_object(key, Arc::new(object.clone()))
                            .await;
                    }
                    group_results.push((oid, result));
                }
                Ok::<_, Error>(group_results)
            }
        }))
        .buffer_unordered(concurrency)
        // Collect every started future before returning any error. Shared
        // single-flight work is separately owned by the runtime task tracker.
        .collect::<Vec<_>>()
        .await;
        for group_result in results {
            for (oid, result) in group_result? {
                completed.insert(oid, result?);
            }
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
        let oid_bytes = requested
            .iter()
            .map(|oid| {
                oid.as_bytes()
                    .try_into()
                    .map_err(|_| Error::UnsupportedObjectFormat)
            })
            .collect::<Result<Vec<[u8; 20]>>>()?;
        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        let lookups = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            lookups = session.lookup_batch(&oid_bytes, &self.inventory) => lookups?,
        };
        if lookups.len() != requested.len() {
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
        for (oid, lookup) in requested.iter().copied().zip(lookups) {
            let locator = match lookup {
                GitObjectLookup::Hit(locator) => locator,
                GitObjectLookup::Corrupt => {
                    return Err(Error::Corrupt {
                        stage: CorruptionStage::Locator,
                    });
                }
                GitObjectLookup::Miss => return Err(Error::ObjectNotFound { oid }),
            };
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
        reason = "prefetched reads carry the same explicit verification and budget inputs as locator reads"
    )]
    async fn read_from_prefetched_locator(
        self: &Arc<Self>,
        session: &GitObjectLocatorSession,
        requested: gix_hash::ObjectId,
        locator: GitObjectLocator,
        bytes: Bytes,
        max_object_bytes: u64,
        remaining_delta_depth: usize,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<GitObject> {
        let packed = self
            .decode_prefetched_entry(
                requested,
                locator.location.pack_offset,
                bytes,
                max_object_bytes,
                budget,
                cancellation,
            )
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
                delta::apply(&base_data, parsed, &token)
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

    async fn decode_prefetched_entry(
        &self,
        oid: gix_hash::ObjectId,
        pack_offset: u64,
        bytes: Bytes,
        max_object_bytes: u64,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<PackedEntry> {
        let inflated_bytes = packed_entry_allocation_bytes(
            oid,
            pack_offset,
            &bytes,
            self.limits.max_inflated_entry_bytes,
            max_object_bytes,
        )?;
        budget
            .charge(BudgetDimension::InflatedBytes, inflated_bytes)
            .await?;
        let decode_permit = self.runtime.decode_permit(cancellation).await?;
        let decode_cancellation = cancellation.clone();
        let runtime = Arc::clone(&self.runtime);
        let max_inflated = self.limits.max_inflated_entry_bytes;
        let packed = runtime
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
        Ok(PackedEntry {
            header: packed.header,
            inflated: packed.inflated,
            charged_budget: Some(budget.id()),
        })
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
        &self,
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
            .by_offset
            .get(&pack_offset)
            .copied()
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
        let origin_permit = self.runtime.origin_permit(cancellation).await?;
        budget.charge(BudgetDimension::StorageRequests, 1).await?;
        let metadata = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            metadata = self.store.head(&path) => metadata?,
        };
        drop(origin_permit);
        check_limit(
            "pack index bytes",
            metadata.size,
            self.limits.max_pack_index_bytes,
        )?;
        charge_origin_range(budget, metadata.size).await?;
        let store = self.store.clone();
        let size = metadata.size;
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
    ) -> Result<()> {
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
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(destination)
            .await
            .map_err(|source| {
                Error::Metadata(crab_metadata::error::MetadataError::Io { source })
            })?;
        let mut written = 0u64;
        while let Some(chunk) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            chunk = stream.next() => chunk,
        } {
            let chunk = chunk?;
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
        Ok(())
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
    pub(crate) by_offset: HashMap<u64, gix_hash::ObjectId>,
    pub(crate) object_ids: Vec<gix_hash::ObjectId>,
    pub(crate) pack_checksum: [u8; 20],
    pub(crate) source_bytes: u64,
}

impl PackIndex {
    pub(crate) fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.by_offset.capacity().saturating_mul(
                    std::mem::size_of::<u64>()
                        .saturating_add(std::mem::size_of::<gix_hash::ObjectId>())
                        .saturating_add(1),
                ),
            )
            .saturating_add(
                self.object_ids
                    .capacity()
                    .saturating_mul(std::mem::size_of::<gix_hash::ObjectId>()),
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
    delta::validate(&delta, cancellation).map_err(|error| map_delta_error(error, oid))?;
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
    let mut by_offset = HashMap::new();
    by_offset
        .try_reserve(capacity)
        .map_err(|source| Error::Allocation {
            requested: capacity.saturating_mul(std::mem::size_of::<(u64, gix_hash::ObjectId)>()),
            source,
        })?;
    let mut object_ids = Vec::new();
    object_ids
        .try_reserve_exact(capacity)
        .map_err(|source| Error::Allocation {
            requested: capacity.saturating_mul(std::mem::size_of::<gix_hash::ObjectId>()),
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
        if by_offset.insert(entry.pack_offset, entry.oid).is_some() {
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
    }
    Ok(PackIndex {
        by_offset,
        object_ids,
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
        };
        let ranges = coalesce_ranges(vec![
            (oid, locator(first_pack, 100, 20)),
            (oid, locator(first_pack, 130, 20)),
            (oid, locator(first_pack, 5_000, 20)),
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
}
