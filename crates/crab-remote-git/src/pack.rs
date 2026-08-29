//! Git pack production from verified remote entries and negotiated thin bases.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crab_metadata::git_object_locator::GitPackInventoryEntry;
use flate2::{Compression, write::ZlibEncoder};
use futures_util::stream::{self, StreamExt as _, TryStreamExt as _};
use gix_hash::ObjectId;
use gix_pack::data::entry::Header;
use sha1::{Digest, Sha1};
use tempfile::NamedTempFile;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::{BudgetDimension, Error, OperationKind, RemoteGitObject, RemoteGitRepository, Result};

// Keep enough packed entries together for delta dependencies to cross the
// default read batch boundary. Operation object and byte budgets still bound
// the total selection and each coalesced range bounds transient range memory.
const OBJECT_BATCH_SIZE: usize = 50_000;
const SIDEBAND_PAYLOAD: usize = 65_515;
const GENERATED_PACK_CACHE_VERSION: u32 = 2;
const GENERATED_PACK_DESCRIPTOR_MAX_BYTES: u64 = 4 * 1024;
const GENERATED_PACK_UPLOAD_PART_BYTES: usize = 8 * 1024 * 1024;
// Generated response packs can require a large catalog lookup plus pack
// production. Match the repository lease safety window and renew well before
// expiry so a short object-store stall cannot create duplicate producers.
const GENERATED_PACK_LEASE_TTL: Duration = Duration::from_secs(5 * 60);
const GENERATED_PACK_LEASE_RENEWAL: Duration = Duration::from_secs(60);
const GENERATED_PACK_LEASE_POLL_INITIAL: Duration = Duration::from_millis(250);
const GENERATED_PACK_LEASE_POLL_MAX: Duration = Duration::from_secs(5);
const COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS: usize = 100_000;
const SELECTED_PACK_REPACK_MIN_OBJECTS: usize = 100_000;
const SOURCE_PACK_DOWNLOAD_CONCURRENCY: usize = 4;

/// Immutable key for one authorization- and generation-bound response pack.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GeneratedPackCacheKey {
    digest: [u8; 32],
    selection_digest: [u8; 32],
}

/// Immutable key for a response pack coordinated before object planning.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GeneratedPackRequestCacheKey {
    digest: [u8; 32],
}

/// Failure from request-bound generated-pack caching.
#[derive(Debug)]
pub enum GeneratedPackRequestCacheError<E> {
    /// Planning or pack production failed.
    Producer(E),
    /// Cache storage or cross-process coordination failed.
    Cache(Error),
}

impl<E: std::fmt::Display> std::fmt::Display for GeneratedPackRequestCacheError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Producer(source) => source.fmt(formatter),
            Self::Cache(source) => source.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for GeneratedPackRequestCacheError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Producer(source) => Some(source),
            Self::Cache(source) => Some(source),
        }
    }
}

/// Source-preserving failure returned by a product-owned lease integration.
#[derive(Debug)]
pub struct GeneratedPackLeaseError {
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl GeneratedPackLeaseError {
    /// Wrap a concrete coordination error without discarding its source.
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl std::fmt::Display for GeneratedPackLeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for GeneratedPackLeaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// One acquired cross-process lease for generated response-pack production.
pub trait GeneratedPackLease: Send {
    /// Renew the lease before its configured lifetime expires.
    fn renew(
        &mut self,
    ) -> futures_util::future::BoxFuture<'_, std::result::Result<(), GeneratedPackLeaseError>>;

    /// Release the lease with holder-checked semantics.
    fn release(
        self: Box<Self>,
    ) -> futures_util::future::BoxFuture<'static, std::result::Result<(), GeneratedPackLeaseError>>;
}

/// Result of one non-blocking generated-pack lease acquisition attempt.
pub enum GeneratedPackLeaseAttempt {
    Acquired(Box<dyn GeneratedPackLease>),
    Held,
}

/// Product-owned cross-process coordination for generated pack production.
pub trait GeneratedPackLeaseProvider: Send + Sync {
    /// Try to acquire one opaque repository-scoped resource.
    fn try_acquire<'a>(
        &'a self,
        resource: &'a str,
        ttl: Duration,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<GeneratedPackLeaseAttempt, GeneratedPackLeaseError>,
    >;
}

impl GeneratedPackCacheKey {
    fn hex(self) -> String {
        self.digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn matches_selection(self, object_ids: &[ObjectId]) -> bool {
        self.selection_digest == selected_object_digest(object_ids)
    }
}

impl std::fmt::Debug for GeneratedPackCacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GeneratedPackCacheKey(<redacted>)")
    }
}

impl GeneratedPackRequestCacheKey {
    fn hex(self) -> String {
        self.digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

impl std::fmt::Debug for GeneratedPackRequestCacheKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GeneratedPackRequestCacheKey(<redacted>)")
    }
}

#[derive(Debug)]
struct GeneratedPackDescriptor {
    version: u32,
    request_hash: String,
    content_hash: String,
    checksum: String,
    size: u64,
    object_count: u32,
    selection_object_count: u64,
}

/// A temporary, checksummed Git pack generated from one pinned snapshot.
#[derive(Clone)]
pub struct GeneratedPack {
    file: Arc<NamedTempFile>,
    size: u64,
    checksum: [u8; 20],
    content_hash: [u8; 32],
    object_count: u32,
}

impl std::fmt::Debug for GeneratedPack {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GeneratedPack")
            .field("size", &self.size)
            .field("object_count", &self.object_count)
            .finish()
    }
}

impl GeneratedPack {
    /// Return the temporary pack path while this generated pack is alive.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.file.path()
    }

    /// Return the complete pack size, including its trailing checksum.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Return the number of objects in the pack.
    #[must_use]
    pub const fn object_count(&self) -> u32 {
        self.object_count
    }

    /// Return the pack checksum.
    #[must_use]
    pub const fn checksum(&self) -> [u8; 20] {
        self.checksum
    }

    /// Return the pack checksum as lowercase hexadecimal text.
    #[must_use]
    pub fn checksum_hex(&self) -> String {
        self.checksum
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn content_hash_hex(&self) -> String {
        self.content_hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn verify_checksum(&self) -> Result<()> {
        let mut file = std::fs::File::open(self.file.path()).map_err(io_error)?;
        if self.size < 20 {
            return Err(Error::Metadata(
                crab_metadata::error::MetadataError::CorruptObject {
                    path: self.file.path().display().to_string(),
                    reason: "generated pack is shorter than its checksum".to_owned(),
                },
            ));
        }
        let mut body = Read::by_ref(&mut file).take(self.size - 20);
        let mut hash = Sha1::new();
        let mut content_hash = blake3::Hasher::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let read = body.read(&mut chunk).map_err(io_error)?;
            if read == 0 {
                break;
            }
            hash.update(&chunk[..read]);
            content_hash.update(&chunk[..read]);
        }
        let mut trailer = [0u8; 20];
        file.read_exact(&mut trailer).map_err(io_error)?;
        content_hash.update(&trailer);
        let actual: [u8; 20] = hash.finalize().into();
        if actual != trailer
            || actual != self.checksum
            || content_hash.finalize().as_bytes() != &self.content_hash
        {
            return Err(Error::Metadata(
                crab_metadata::error::MetadataError::CorruptObject {
                    path: self.file.path().display().to_string(),
                    reason: "generated pack checksum verification failed".to_owned(),
                },
            ));
        }
        Ok(())
    }

    /// Stream the pack through protocol-v2 sideband channel 1.
    pub async fn write_sideband<W: AsyncWrite + Unpin>(
        &self,
        writer: &mut W,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut file = tokio::fs::File::open(self.file.path())
            .await
            .map_err(io_error)?;
        let mut chunk = vec![0u8; SIDEBAND_PAYLOAD];
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let read = tokio::io::AsyncReadExt::read(&mut file, &mut chunk)
                .await
                .map_err(io_error)?;
            if read == 0 {
                break;
            }
            write_packet(writer, &chunk[..read], Some(1), cancellation).await?;
        }
        Ok(())
    }
}

impl RemoteGitRepository {
    /// Bind canonical request semantics and object selection to this pinned
    /// repository and authorization state.
    #[must_use]
    pub fn generated_pack_cache_key(
        &self,
        authorization_digest: [u8; 32],
        request_digest: [u8; 32],
        object_ids: &[ObjectId],
        thin_pack: bool,
    ) -> GeneratedPackCacheKey {
        generated_pack_cache_key(
            &self.state.identity,
            &self.state.git_validation_digest,
            authorization_digest,
            request_digest,
            object_ids,
            thin_pack,
        )
    }

    /// Bind an exact fetch request to this pinned repository and authorization state.
    ///
    /// This key permits identical callers to coordinate before an expensive object plan is
    /// materialized. The caller-provided digest must cover every request semantic that can
    /// affect the response pack.
    #[must_use]
    pub fn generated_pack_request_cache_key(
        &self,
        authorization_digest: [u8; 32],
        request_digest: [u8; 32],
    ) -> GeneratedPackRequestCacheKey {
        generated_pack_request_cache_key(
            &self.state.identity,
            &self.state.git_validation_digest,
            authorization_digest,
            request_digest,
        )
    }

    /// Generate a self-contained pack from verified object IDs in this pinned
    /// repository generation. An exact single-pack closure reuses its verified
    /// immutable pack; other selections are read and written in bounded batches.
    pub async fn generate_pack(
        &self,
        object_ids: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        self.generate_pack_with_bases_mode(object_ids, &[], false, cancellation)
            .await
    }

    /// Generate a pack that may retain deltas against client-proven base
    /// objects. Every base not in `object_ids` must already be held by the
    /// receiving Git client under the pinned negotiation result.
    pub async fn generate_pack_with_bases(
        &self,
        object_ids: &[ObjectId],
        thin_bases: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        self.generate_pack_with_bases_mode(object_ids, thin_bases, false, cancellation)
            .await
    }

    async fn generate_pack_with_bases_mode(
        &self,
        object_ids: &[ObjectId],
        thin_bases: &[ObjectId],
        allow_dense_selected_assembly: bool,
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        let operation = self
            .operation(OperationKind::UploadPack, cancellation)
            .await?;
        let maximum = usize::try_from(operation.max_logical_objects()).unwrap_or(usize::MAX);
        let capacity = object_ids.len().min(maximum);
        let mut unique = Vec::with_capacity(capacity);
        let mut seen = HashSet::with_capacity(capacity);
        let result = if object_ids.len() > capacity {
            Err(Error::LimitExceeded {
                limit: "pack object count",
                actual: object_ids.len() as u64,
                maximum: operation.max_logical_objects(),
            })
        } else {
            let mut result = Ok(());
            for oid in object_ids {
                if seen.insert(*oid) {
                    if unique.len() >= maximum {
                        result = Err(Error::LimitExceeded {
                            limit: "pack object count",
                            actual: unique.len().saturating_add(1) as u64,
                            maximum: operation.max_logical_objects(),
                        });
                        break;
                    }
                    unique.push(*oid);
                }
            }
            result.map(|()| unique)
        };
        let result = match result {
            Ok(unique) => {
                match try_reuse_single_pack(self, &operation, &unique, cancellation).await {
                    Ok(Some(pack)) => Ok(pack),
                    Ok(None) => {
                        if thin_bases.is_empty() {
                            match Self::try_consolidate_complete_pack(
                                self,
                                &operation,
                                &unique,
                                cancellation,
                            )
                            .await?
                            {
                                Some(pack) => Ok(pack),
                                None => {
                                    let assembled = if allow_dense_selected_assembly {
                                        Self::try_assemble_selected_pack(
                                            self,
                                            &operation,
                                            &unique,
                                            cancellation,
                                        )
                                        .await?
                                    } else {
                                        None
                                    };
                                    match assembled {
                                        Some(pack) => Ok(pack),
                                        None => match Self::try_repack_selected_pack(
                                            self,
                                            &operation,
                                            &unique,
                                            cancellation,
                                        )
                                        .await?
                                        {
                                            Some(pack) => Ok(pack),
                                            None => {
                                                generate_pack_with_operation(
                                                    &operation,
                                                    &unique,
                                                    thin_bases,
                                                    None,
                                                    "packed_entries",
                                                    cancellation,
                                                )
                                                .await
                                            }
                                        },
                                    }
                                }
                            }
                        } else {
                            generate_pack_with_operation(
                                &operation,
                                &unique,
                                thin_bases,
                                None,
                                "packed_entries",
                                cancellation,
                            )
                            .await
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        operation.finish(result).await
    }

    async fn try_assemble_selected_pack(
        repository: &RemoteGitRepository,
        operation: &crate::OperationContext,
        object_ids: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<Option<GeneratedPack>> {
        let (inventory_objects, inventory_bytes) =
            repository
                .state
                .inventory
                .values()
                .fold((0_u64, 0_u64), |(objects, bytes), pack| {
                    (
                        objects.saturating_add(pack.object_count),
                        bytes.saturating_add(pack.pack_size),
                    )
                });
        if !Self::selected_pack_repack_candidate(
            inventory_objects,
            object_ids.len(),
            SELECTED_PACK_REPACK_MIN_OBJECTS,
        ) || inventory_bytes > operation.max_fetched_bytes()
        {
            return Ok(None);
        }

        // REF_DELTA names its base by OID, so the base may be emitted in a
        // later batch. OFS_DELTA is rewritten by PackWriter before it leaves
        // this path, making a dependency sort unnecessary for dense reads.
        let selected_objects = object_ids.iter().copied().collect::<HashSet<_>>();
        let pack = generate_pack_with_operation(
            operation,
            object_ids,
            &[],
            Some(&selected_objects),
            "selected_packed_entries",
            cancellation,
        )
        .await?;
        Ok(Some(pack))
    }

    async fn try_consolidate_complete_pack(
        repository: &RemoteGitRepository,
        operation: &crate::OperationContext,
        object_ids: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<Option<GeneratedPack>> {
        let inventory = repository
            .state
            .inventory
            .values()
            .copied()
            .collect::<Vec<_>>();
        let inventory_objects = inventory
            .iter()
            .fold(0_u64, |total, pack| total.saturating_add(pack.object_count));
        let inventory_bytes = inventory
            .iter()
            .fold(0_u64, |total, pack| total.saturating_add(pack.pack_size));
        if !Self::complete_pack_consolidation_candidate(
            inventory.len(),
            inventory_objects,
            object_ids.len(),
        ) {
            return Ok(None);
        }
        if inventory_bytes > operation.max_fetched_bytes() {
            return Ok(None);
        }

        let started = Instant::now();
        let source_pack_count = inventory.len();
        let workspace = tempfile::tempdir().map_err(io_error)?;
        let download_dir = workspace.path().join("source-packs");
        std::fs::create_dir_all(&download_dir).map_err(io_error)?;
        let source_download_started = Instant::now();
        let sources =
            download_repack_sources(operation, inventory, &download_dir, cancellation).await?;
        let source_download_ms = source_download_started.elapsed().as_millis() as u64;

        let concat_sources = sources.clone();
        let concatenated = tokio::task::spawn_blocking(move || {
            crab_git::repack::concatenate_complete_pack_inventory(&concat_sources)
        })
        .await
        .map_err(|source| Error::DecodeTask { source })?;
        let (repacked, strategy) = match concatenated {
            Ok(repacked) => (repacked, "complete_pack_concatenation"),
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    error_debug = ?error,
                    "complete pack concatenation was not usable; falling back to Git consolidation"
                );
                let repack_sources = sources;
                let repacked = tokio::task::spawn_blocking(move || {
                    crab_git::repack::consolidate_pack_suffix(&repack_sources)
                })
                .await
                .map_err(|source| Error::DecodeTask { source })?
                .map_err(|source| Error::ResponsePackConsolidation { source })?;
                (repacked, "complete_pack_consolidation")
            }
        };
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let generated = repacked.packs().first().ok_or(Error::InternalInvariant {
            invariant: "complete pack consolidation produced no pack",
        })?;
        let locations = crab_git::pack_locator::PackLocationIter::open(
            generated.index_path(),
            generated.reverse_index_path(),
            generated.pack_size,
        )
        .map_err(|source| Error::ResponsePackConsolidation {
            source: crab_git::repack::RepackError::from(source),
        })?;
        let mut requested = object_ids.iter().copied().collect::<HashSet<_>>();
        for location in locations {
            let location = location.map_err(|source| Error::ResponsePackConsolidation {
                source: crab_git::repack::RepackError::from(source),
            })?;
            if !requested.remove(&location.oid) {
                return Err(Error::Corrupt {
                    stage: crate::CorruptionStage::Inventory,
                });
            }
        }
        if !requested.is_empty() {
            return Err(Error::Corrupt {
                stage: crate::CorruptionStage::Inventory,
            });
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }

        let size = generated.pack_size;
        operation
            .charge(BudgetDimension::ResponseBytes, size)
            .await?;
        let checksum = decode_hex::<20>(&generated.git_sha1).ok_or(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        })?;
        let content_hash = generated.pack_hash;
        let destination = NamedTempFile::new().map_err(io_error)?;
        let destination_path = destination.path().to_owned();
        let destination = destination.into_temp_path();
        std::fs::remove_file(&destination_path).map_err(io_error)?;
        std::fs::rename(generated.pack_path(), &destination_path).map_err(io_error)?;
        let file = std::fs::File::open(&destination_path).map_err(io_error)?;
        let file = NamedTempFile::from_parts(file, destination);
        drop(repacked);
        drop(workspace);

        let pack = GeneratedPack {
            file: Arc::new(file),
            size,
            checksum,
            content_hash,
            object_count: u32::try_from(object_ids.len()).map_err(|_| Error::LimitExceeded {
                limit: "pack object count",
                actual: object_ids.len() as u64,
                maximum: u32::MAX as u64,
            })?,
        };
        pack.verify_checksum()?;
        tracing::info!(
            target: "crab_remote_git::telemetry",
            telemetry_event = "pack_generation",
            strategy,
            source_pack_count,
            object_count = pack.object_count,
            source_download_ms,
            response_bytes = size,
            pack_generation_ms = started.elapsed().as_millis() as u64,
            "remote Git response pack consolidated from complete pack inventory"
        );
        Ok(Some(pack))
    }

    async fn try_repack_selected_pack(
        repository: &RemoteGitRepository,
        operation: &crate::OperationContext,
        object_ids: &[ObjectId],
        cancellation: &CancellationToken,
    ) -> Result<Option<GeneratedPack>> {
        let inventory = repository
            .state
            .inventory
            .values()
            .copied()
            .collect::<Vec<_>>();
        let inventory_objects = inventory
            .iter()
            .fold(0_u64, |total, pack| total.saturating_add(pack.object_count));
        let inventory_bytes = inventory
            .iter()
            .fold(0_u64, |total, pack| total.saturating_add(pack.pack_size));
        if !Self::selected_pack_repack_candidate(
            inventory_objects,
            object_ids.len(),
            SELECTED_PACK_REPACK_MIN_OBJECTS,
        ) || inventory_bytes > operation.max_fetched_bytes()
        {
            return Ok(None);
        }

        let started = Instant::now();
        let source_pack_count = inventory.len();
        let workspace = tempfile::tempdir().map_err(io_error)?;
        let download_dir = workspace.path().join("source-packs");
        std::fs::create_dir_all(&download_dir).map_err(io_error)?;
        let source_download_started = Instant::now();
        let sources =
            download_repack_sources(operation, inventory, &download_dir, cancellation).await?;
        let source_download_ms = source_download_started.elapsed().as_millis() as u64;
        let selected_oids = object_ids.to_vec();
        let repacked = tokio::task::spawn_blocking(move || {
            crab_git::repack::repack_selected_objects(&sources, &selected_oids)
        })
        .await
        .map_err(|source| Error::DecodeTask { source })?
        .map_err(|source| Error::ResponsePackConsolidation { source })?;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let generated = repacked.packs().first().ok_or(Error::InternalInvariant {
            invariant: "selected-object repack produced no pack",
        })?;
        let size = generated.pack_size;
        operation
            .charge(BudgetDimension::ResponseBytes, size)
            .await?;
        let checksum = decode_hex::<20>(&generated.git_sha1).ok_or(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        })?;
        let content_hash = generated.pack_hash;
        let destination = NamedTempFile::new().map_err(io_error)?;
        let destination_path = destination.path().to_owned();
        let destination = destination.into_temp_path();
        std::fs::remove_file(&destination_path).map_err(io_error)?;
        std::fs::rename(generated.pack_path(), &destination_path).map_err(io_error)?;
        let file = std::fs::File::open(&destination_path).map_err(io_error)?;
        let file = NamedTempFile::from_parts(file, destination);
        let object_count = generated.object_count;
        drop(repacked);
        drop(workspace);

        let pack = GeneratedPack {
            file: Arc::new(file),
            size,
            checksum,
            content_hash,
            object_count: u32::try_from(object_count).map_err(|_| Error::LimitExceeded {
                limit: "pack object count",
                actual: object_count,
                maximum: u32::MAX as u64,
            })?,
        };
        pack.verify_checksum()?;
        tracing::info!(
            target: "crab_remote_git::telemetry",
            telemetry_event = "pack_generation",
            strategy = "selected_object_repack",
            source_pack_count,
            object_count = pack.object_count,
            source_bytes = inventory_bytes,
            source_download_ms,
            response_bytes = size,
            pack_generation_ms = started.elapsed().as_millis() as u64,
            "remote Git response pack repacked from selected objects"
        );
        Ok(Some(pack))
    }

    fn complete_pack_consolidation_candidate(
        pack_count: usize,
        inventory_objects: u64,
        selected_objects: usize,
    ) -> bool {
        pack_count > 1
            && selected_objects >= COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS
            && inventory_objects == selected_objects as u64
    }

    fn selected_pack_repack_candidate(
        inventory_objects: u64,
        selected_objects: usize,
        minimum_objects: usize,
    ) -> bool {
        let selected_objects = u64::try_from(selected_objects).unwrap_or(u64::MAX);
        selected_objects >= u64::try_from(minimum_objects).unwrap_or(u64::MAX)
            && selected_objects < inventory_objects
            && selected_objects.saturating_mul(2) >= inventory_objects
    }

    /// Reuse or publish one immutable generation- and authorization-bound pack.
    ///
    /// Callers must use this only for no-have requests. Incremental fetches
    /// retain their request-local negotiation and are never artifact cached.
    pub async fn generate_pack_cached(
        &self,
        object_ids: &[ObjectId],
        cache_key: GeneratedPackCacheKey,
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        self.generate_pack_cached_mode(object_ids, cache_key, false, cancellation)
            .await
    }

    /// Reuse or publish a response pack for a catalog-exact dense filter.
    ///
    /// Callers must restrict this entry point to no-have requests using only
    /// `blob:none` or `object:type` catalog filters without shallow state.
    pub async fn generate_pack_cached_with_dense_selection(
        &self,
        object_ids: &[ObjectId],
        cache_key: GeneratedPackCacheKey,
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        self.generate_pack_cached_mode(object_ids, cache_key, true, cancellation)
            .await
    }

    /// Reuse or publish one self-contained response pack for an exact request.
    ///
    /// Coordination happens before `producer` is polled, so identical processes do not repeat
    /// object planning. The request key must bind every planning semantic and callers must
    /// produce a verified self-contained pack.
    pub async fn generate_pack_request_cached<E, Fut>(
        &self,
        cache_key: GeneratedPackRequestCacheKey,
        producer: Fut,
        cancellation: &CancellationToken,
    ) -> std::result::Result<GeneratedPack, GeneratedPackRequestCacheError<E>>
    where
        Fut: Future<Output = std::result::Result<GeneratedPack, E>>,
    {
        produce_request_cached_pack(self, cache_key, producer, cancellation).await
    }

    async fn generate_pack_cached_mode(
        &self,
        object_ids: &[ObjectId],
        cache_key: GeneratedPackCacheKey,
        allow_dense_selected_assembly: bool,
        cancellation: &CancellationToken,
    ) -> Result<GeneratedPack> {
        if !cache_key.matches_selection(object_ids) {
            return Err(Error::InternalInvariant {
                invariant: "generated pack cache key does not match object selection",
            });
        }
        if let Some(pack) =
            load_cached_pack(self, cache_key.hex(), Some(object_ids.len()), cancellation).await?
        {
            record_generated_pack_cache(self, crate::CacheOutcome::Hit, 1);
            return Ok(pack);
        }
        record_generated_pack_cache(self, crate::CacheOutcome::Miss, 1);
        let repository = self.clone();
        let object_ids = object_ids.to_vec();
        let runtime = Arc::clone(&self.state.runtime);
        let generated = runtime
            .generate_pack_singleflight(
                cache_key,
                self.state.options,
                cancellation,
                move |background_cancellation| async move {
                    produce_cached_pack(
                        &repository,
                        &object_ids,
                        cache_key,
                        allow_dense_selected_assembly,
                        &background_cancellation,
                    )
                    .await
                },
            )
            .await?;
        Ok(generated.as_ref().clone())
    }
}

async fn download_repack_sources(
    operation: &crate::OperationContext,
    inventory: Vec<GitPackInventoryEntry>,
    download_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<Vec<crab_git::repack::RepackSource>> {
    download_repack_sources_with(
        inventory,
        download_dir.to_owned(),
        cancellation,
        SOURCE_PACK_DOWNLOAD_CONCURRENCY,
        |pack, path| async move {
            operation
                .download_pack_to_path(pack.pack_id, pack.pack_size, &path)
                .await
        },
    )
    .await
}

async fn download_repack_sources_with<F, Fut>(
    inventory: Vec<GitPackInventoryEntry>,
    download_dir: PathBuf,
    cancellation: &CancellationToken,
    max_concurrency: usize,
    download: F,
) -> Result<Vec<crab_git::repack::RepackSource>>
where
    F: Fn(GitPackInventoryEntry, PathBuf) -> Fut + Sync,
    Fut: Future<Output = Result<()>> + Send,
{
    let concurrency = inventory.len().min(max_concurrency.max(1)).max(1);
    let mut sources = stream::iter(inventory.into_iter().enumerate().map(|(index, pack)| {
        let path = download_dir.join(format!("pack-{index}-{}.pack", pack.pack_id));
        let download = &download;
        async move {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            std::fs::File::create(&path).map_err(io_error)?;
            download(pack, path.clone()).await?;
            Ok::<_, Error>((
                index,
                crab_git::repack::RepackSource {
                    canonical_id: pack.pack_id.to_string(),
                    path,
                    size: pack.pack_size,
                    object_count: pack.object_count,
                },
            ))
        }
    }))
    .buffer_unordered(concurrency)
    .try_collect::<Vec<_>>()
    .await?;
    sources.sort_unstable_by_key(|(index, _)| *index);
    Ok(sources.into_iter().map(|(_, source)| source).collect())
}

fn generated_pack_cache_key(
    identity: &crate::RepositoryIdentity,
    git_validation_digest: &str,
    authorization_digest: [u8; 32],
    request_digest: [u8; 32],
    object_ids: &[ObjectId],
    thin_pack: bool,
) -> GeneratedPackCacheKey {
    let mut hash = blake3::Hasher::new();
    hash.update(b"crab.generated-pack.request\0");
    hash.update(&GENERATED_PACK_CACHE_VERSION.to_be_bytes());
    identity.hash_cache_identity(&mut hash);
    hash.update(git_validation_digest.as_bytes());
    hash.update(&authorization_digest);
    hash.update(&request_digest);
    hash.update(&[u8::from(thin_pack)]);
    let selection_digest = selected_object_digest(object_ids);
    hash.update(&selection_digest);
    GeneratedPackCacheKey {
        digest: *hash.finalize().as_bytes(),
        selection_digest,
    }
}

fn generated_pack_request_cache_key(
    identity: &crate::RepositoryIdentity,
    git_validation_digest: &str,
    authorization_digest: [u8; 32],
    request_digest: [u8; 32],
) -> GeneratedPackRequestCacheKey {
    let mut hash = blake3::Hasher::new();
    hash.update(b"crab.generated-pack.preplanned-request.v1\0");
    hash.update(&GENERATED_PACK_CACHE_VERSION.to_be_bytes());
    identity.hash_cache_identity(&mut hash);
    hash.update(git_validation_digest.as_bytes());
    hash.update(&authorization_digest);
    hash.update(&request_digest);
    GeneratedPackRequestCacheKey {
        digest: *hash.finalize().as_bytes(),
    }
}

fn selected_object_digest(object_ids: &[ObjectId]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"crab.generated-pack.selection.v1\0");
    hash.update(&(object_ids.len() as u64).to_be_bytes());
    // The response bytes are order-independent for cache identity. Sorting
    // only this digest lets concurrent clients with the same dense selection
    // share one immutable artifact even when their catalog traversal order
    // differs.
    let mut sorted = object_ids.to_vec();
    sorted.sort_unstable();
    for oid in sorted {
        hash.update(oid.as_bytes());
    }
    *hash.finalize().as_bytes()
}

async fn produce_cached_pack(
    repository: &RemoteGitRepository,
    object_ids: &[ObjectId],
    cache_key: GeneratedPackCacheKey,
    allow_dense_selected_assembly: bool,
    cancellation: &CancellationToken,
) -> Result<GeneratedPack> {
    let Some(provider) = repository.generated_pack_lease_provider.as_ref() else {
        return produce_cached_pack_without_lease(
            repository,
            object_ids,
            cache_key,
            allow_dense_selected_assembly,
            cancellation,
        )
        .await;
    };
    let deadline =
        tokio::time::Instant::now() + repository.state.options.operation_limits().max_duration;
    let resource = format!("generated-pack-{}", cache_key.hex());
    let mut recorded_waiter = false;
    let mut lease_wait_attempt = 0_usize;
    loop {
        if let Some(pack) = load_cached_pack(
            repository,
            cache_key.hex(),
            Some(object_ids.len()),
            cancellation,
        )
        .await?
        {
            return Ok(pack);
        }
        let acquire = provider.try_acquire(&resource, GENERATED_PACK_LEASE_TTL);
        let lock = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(Error::Cancelled),
            () = tokio::time::sleep_until(deadline) => {
                return Err(Error::Timeout { operation: "upload-pack" });
            }
            result = acquire => match result {
                Ok(GeneratedPackLeaseAttempt::Acquired(lock)) => lock,
                Ok(GeneratedPackLeaseAttempt::Held) => {
                    if !recorded_waiter {
                        record_generated_pack_cache(repository, crate::CacheOutcome::Coalesced, 1);
                        recorded_waiter = true;
                    }
                    let delay = generated_pack_lease_poll(lease_wait_attempt);
                    lease_wait_attempt = lease_wait_attempt.saturating_add(1);
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Err(Error::Cancelled),
                        () = tokio::time::sleep_until(deadline) => {
                            return Err(Error::Timeout { operation: "upload-pack" });
                        }
                        () = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
                Err(source) => {
                    tracing::warn!(
                        generated_pack_resource = %resource,
                        error = %source,
                        error_debug = ?source,
                        "generated response-pack lease attempt failed"
                    );
                    return Err(Error::GeneratedPackLease { source });
                }
            }
        };
        return produce_cached_pack_under_lease(
            repository,
            object_ids,
            cache_key,
            allow_dense_selected_assembly,
            lock,
            cancellation,
        )
        .await;
    }
}

fn generated_pack_lease_poll(attempt: usize) -> Duration {
    // A long response-pack build must not turn every waiting client into a
    // descriptor/lease poller, while the five-second cap keeps completion
    // detection responsive after the producer publishes its artifact.
    let exponent = attempt.min(5) as u32;
    let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
    GENERATED_PACK_LEASE_POLL_INITIAL
        .saturating_mul(multiplier)
        .min(GENERATED_PACK_LEASE_POLL_MAX)
}

async fn produce_cached_pack_under_lease(
    repository: &RemoteGitRepository,
    object_ids: &[ObjectId],
    cache_key: GeneratedPackCacheKey,
    allow_dense_selected_assembly: bool,
    mut lock: Box<dyn GeneratedPackLease>,
    cancellation: &CancellationToken,
) -> Result<GeneratedPack> {
    let producer = async {
        if let Some(pack) = load_cached_pack(
            repository,
            cache_key.hex(),
            Some(object_ids.len()),
            cancellation,
        )
        .await?
        {
            return Ok(pack);
        }
        let generated = repository
            .generate_pack_with_bases_mode(
                object_ids,
                &[],
                allow_dense_selected_assembly,
                cancellation,
            )
            .await?;
        publish_cached_pack(
            repository,
            cache_key.hex(),
            object_ids.len(),
            &generated,
            cancellation,
        )
        .await?;
        Ok(generated)
    };
    tokio::pin!(producer);
    let mut renewal = tokio::time::interval(GENERATED_PACK_LEASE_RENEWAL);
    renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    renewal.tick().await;
    let result = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break Err(Error::Cancelled),
            result = &mut producer => break result,
            _ = renewal.tick() => {
                if let Err(source) = lock.renew().await {
                    break Err(Error::GeneratedPackLease { source });
                }
            }
        }
    };
    if lock.release().await.is_err() {
        tracing::warn!(
            telemetry_event = "generated_pack_lease_release",
            "generated response-pack lease release failed"
        );
    }
    result
}

async fn produce_cached_pack_without_lease(
    repository: &RemoteGitRepository,
    object_ids: &[ObjectId],
    cache_key: GeneratedPackCacheKey,
    allow_dense_selected_assembly: bool,
    cancellation: &CancellationToken,
) -> Result<GeneratedPack> {
    if let Some(pack) = load_cached_pack(
        repository,
        cache_key.hex(),
        Some(object_ids.len()),
        cancellation,
    )
    .await?
    {
        return Ok(pack);
    }
    let generated = repository
        .generate_pack_with_bases_mode(object_ids, &[], allow_dense_selected_assembly, cancellation)
        .await?;
    publish_cached_pack(
        repository,
        cache_key.hex(),
        object_ids.len(),
        &generated,
        cancellation,
    )
    .await?;
    Ok(generated)
}

async fn produce_request_cached_pack<E, Fut>(
    repository: &RemoteGitRepository,
    cache_key: GeneratedPackRequestCacheKey,
    producer: Fut,
    cancellation: &CancellationToken,
) -> std::result::Result<GeneratedPack, GeneratedPackRequestCacheError<E>>
where
    Fut: Future<Output = std::result::Result<GeneratedPack, E>>,
{
    if let Some(pack) = load_cached_pack(repository, cache_key.hex(), None, cancellation)
        .await
        .map_err(GeneratedPackRequestCacheError::Cache)?
    {
        record_generated_pack_cache(repository, crate::CacheOutcome::Hit, 1);
        return Ok(pack);
    }
    record_generated_pack_cache(repository, crate::CacheOutcome::Miss, 1);

    let Some(provider) = repository.generated_pack_lease_provider.as_ref() else {
        let generated = producer
            .await
            .map_err(GeneratedPackRequestCacheError::Producer)?;
        let object_count = usize::try_from(generated.object_count()).unwrap_or(usize::MAX);
        publish_cached_pack(
            repository,
            cache_key.hex(),
            object_count,
            &generated,
            cancellation,
        )
        .await
        .map_err(GeneratedPackRequestCacheError::Cache)?;
        return Ok(generated);
    };

    let deadline =
        tokio::time::Instant::now() + repository.state.options.operation_limits().max_duration;
    let resource = format!("generated-pack-{}", cache_key.hex());
    let mut recorded_waiter = false;
    let mut lease_wait_attempt = 0_usize;
    let mut producer = Some(producer);
    loop {
        if let Some(pack) = load_cached_pack(repository, cache_key.hex(), None, cancellation)
            .await
            .map_err(GeneratedPackRequestCacheError::Cache)?
        {
            record_generated_pack_cache(repository, crate::CacheOutcome::Hit, 1);
            return Ok(pack);
        }
        let acquire = provider.try_acquire(&resource, GENERATED_PACK_LEASE_TTL);
        let lock = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(GeneratedPackRequestCacheError::Cache(Error::Cancelled));
            }
            () = tokio::time::sleep_until(deadline) => {
                return Err(GeneratedPackRequestCacheError::Cache(Error::Timeout {
                    operation: "upload-pack",
                }));
            }
            result = acquire => match result {
                Ok(GeneratedPackLeaseAttempt::Acquired(lock)) => lock,
                Ok(GeneratedPackLeaseAttempt::Held) => {
                    if !recorded_waiter {
                        record_generated_pack_cache(repository, crate::CacheOutcome::Coalesced, 1);
                        recorded_waiter = true;
                    }
                    let delay = generated_pack_lease_poll(lease_wait_attempt);
                    lease_wait_attempt = lease_wait_attempt.saturating_add(1);
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => {
                            return Err(GeneratedPackRequestCacheError::Cache(Error::Cancelled));
                        }
                        () = tokio::time::sleep_until(deadline) => {
                            return Err(GeneratedPackRequestCacheError::Cache(Error::Timeout {
                                operation: "upload-pack",
                            }));
                        }
                        () = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
                Err(source) => {
                    return Err(GeneratedPackRequestCacheError::Cache(
                        Error::GeneratedPackLease { source },
                    ));
                }
            }
        };
        let producer = producer.take().ok_or_else(|| {
            GeneratedPackRequestCacheError::Cache(Error::InternalInvariant {
                invariant: "request pack producer was consumed before lease acquisition",
            })
        })?;
        return produce_request_cached_pack_under_lease(
            repository,
            cache_key,
            producer,
            lock,
            cancellation,
        )
        .await;
    }
}

async fn produce_request_cached_pack_under_lease<E, Fut>(
    repository: &RemoteGitRepository,
    cache_key: GeneratedPackRequestCacheKey,
    producer: Fut,
    mut lock: Box<dyn GeneratedPackLease>,
    cancellation: &CancellationToken,
) -> std::result::Result<GeneratedPack, GeneratedPackRequestCacheError<E>>
where
    Fut: Future<Output = std::result::Result<GeneratedPack, E>>,
{
    let work = async {
        if let Some(pack) = load_cached_pack(repository, cache_key.hex(), None, cancellation)
            .await
            .map_err(GeneratedPackRequestCacheError::Cache)?
        {
            return Ok(pack);
        }
        let generated = producer
            .await
            .map_err(GeneratedPackRequestCacheError::Producer)?;
        let object_count = usize::try_from(generated.object_count()).unwrap_or(usize::MAX);
        publish_cached_pack(
            repository,
            cache_key.hex(),
            object_count,
            &generated,
            cancellation,
        )
        .await
        .map_err(GeneratedPackRequestCacheError::Cache)?;
        Ok(generated)
    };
    tokio::pin!(work);
    let mut renewal = tokio::time::interval(GENERATED_PACK_LEASE_RENEWAL);
    renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    renewal.tick().await;
    let result = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                break Err(GeneratedPackRequestCacheError::Cache(Error::Cancelled));
            }
            result = &mut work => break result,
            _ = renewal.tick() => {
                if let Err(source) = lock.renew().await {
                    break Err(GeneratedPackRequestCacheError::Cache(
                        Error::GeneratedPackLease { source },
                    ));
                }
            }
        }
    };
    if lock.release().await.is_err() {
        tracing::warn!(
            telemetry_event = "generated_pack_lease_release",
            "generated response-pack lease release failed"
        );
    }
    result
}

fn record_generated_pack_cache(
    repository: &RemoteGitRepository,
    cache: crate::CacheOutcome,
    value: u64,
) {
    repository
        .state
        .runtime
        .metrics()
        .record(crate::MetricObservation {
            kind: crate::MetricKind::Cache,
            value,
            duration: None,
            outcome: None,
            cache: Some(cache),
        });
    tracing::info!(
        target: "crab_remote_git::telemetry",
        telemetry_event = "generated_pack_cache",
        cache_event = ?cache,
        value,
        "generated response-pack cache event"
    );
}

async fn load_cached_pack(
    repository: &RemoteGitRepository,
    request_hash: String,
    expected_objects: Option<usize>,
    cancellation: &CancellationToken,
) -> Result<Option<GeneratedPack>> {
    let descriptor_path = repository
        .state
        .layout
        .generated_pack_descriptor_path(&request_hash);
    let (bytes, _) = match tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(Error::Cancelled),
        result = repository.state.store.get_with_etag_bounded(
            &descriptor_path,
            GENERATED_PACK_DESCRIPTOR_MAX_BYTES,
        ) => result,
    } {
        Ok(value) => value,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(descriptor) = decode_generated_pack_descriptor(&bytes)? else {
        return Ok(None);
    };
    if !generated_pack_descriptor_matches_request(
        &descriptor,
        &request_hash,
        expected_objects,
        repository
            .state
            .options
            .operation_limits()
            .max_logical_objects,
    ) {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    let maximum = repository
        .state
        .options
        .operation_limits()
        .max_response_bytes;
    if descriptor.size > maximum {
        return Err(Error::LimitExceeded {
            limit: "pack response bytes",
            actual: descriptor.size,
            maximum,
        });
    }
    let checksum = decode_hex::<20>(&descriptor.checksum).ok_or(Error::Corrupt {
        stage: crate::CorruptionStage::PackEntry,
    })?;
    let content_hash = decode_hex::<32>(&descriptor.content_hash).ok_or(Error::Corrupt {
        stage: crate::CorruptionStage::PackEntry,
    })?;
    let artifact_path = repository
        .state
        .layout
        .generated_pack_artifact_path(&descriptor.content_hash);
    let file = NamedTempFile::new().map_err(io_error)?;
    let downloaded = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(Error::Cancelled),
        result = repository
            .state
            .store
            .download_to_path_bounded(&artifact_path, file.path(), descriptor.size) => result?,
    };
    if downloaded != descriptor.size {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    let path = file.path().to_owned();
    let token = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        inspect_cached_pack(
            &path,
            descriptor.size,
            descriptor.object_count,
            checksum,
            content_hash,
            &token,
        )
    })
    .await
    .map_err(|source| Error::DecodeTask { source })??;
    Ok(Some(GeneratedPack {
        file: Arc::new(file),
        size: descriptor.size,
        checksum,
        content_hash,
        object_count: descriptor.object_count,
    }))
}

fn generated_pack_descriptor_matches_request(
    descriptor: &GeneratedPackDescriptor,
    request_hash: &str,
    expected_objects: Option<usize>,
    max_logical_objects: u64,
) -> bool {
    descriptor.version == GENERATED_PACK_CACHE_VERSION
        && descriptor.request_hash == request_hash
        && descriptor.content_hash.len() == 64
        && descriptor.checksum.len() == 40
        && descriptor.size >= 32
        && expected_objects.is_none_or(|expected| {
            descriptor.selection_object_count == u64::try_from(expected).unwrap_or(u64::MAX)
        })
        && u64::from(descriptor.object_count) >= descriptor.selection_object_count
        && u64::from(descriptor.object_count) <= max_logical_objects
}

async fn publish_cached_pack(
    repository: &RemoteGitRepository,
    request_hash: String,
    selection_object_count: usize,
    generated: &GeneratedPack,
    cancellation: &CancellationToken,
) -> Result<()> {
    // Every `GeneratedPack` constructor validates the complete file before it
    // escapes generation or cache loading. Rehashing it here would add another
    // repository-sized read on every cold cache miss before the multipart upload.
    let content_hash = generated.content_hash_hex();
    let artifact_path = repository
        .state
        .layout
        .generated_pack_artifact_path(&content_hash);
    repository
        .state
        .store
        .put_multipart_file_retry(
            &artifact_path,
            generated.path(),
            generated.size,
            generated.content_hash,
            GENERATED_PACK_UPLOAD_PART_BYTES,
            cancellation,
            None,
        )
        .await?;
    let descriptor = serde_json::json!({
        "version": GENERATED_PACK_CACHE_VERSION,
        "request_hash": request_hash.clone(),
        "content_hash": content_hash,
        "checksum": generated.checksum_hex(),
        "size": generated.size,
        "object_count": generated.object_count,
        "selection_object_count": selection_object_count,
    });
    let bytes = serde_json::to_vec(&descriptor).map_err(|_| Error::InternalInvariant {
        invariant: "generated pack descriptor serialization failed",
    })?;
    if bytes.len() as u64 > GENERATED_PACK_DESCRIPTOR_MAX_BYTES {
        return Err(Error::InternalInvariant {
            invariant: "generated pack descriptor exceeds its fixed bound",
        });
    }
    let descriptor_path = repository
        .state
        .layout
        .generated_pack_descriptor_path(&request_hash);
    match repository
        .state
        .store
        .create_strict(&descriptor_path, bytes.into())
        .await
    {
        Ok(()) => Ok(()),
        Err(crab_storage::StorageError::StateConflict { .. }) => {
            match load_cached_pack(
                repository,
                request_hash,
                Some(selection_object_count),
                cancellation,
            )
            .await?
            {
                Some(_) => Ok(()),
                None => Err(Error::Corrupt {
                    stage: crate::CorruptionStage::PackEntry,
                }),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn decode_generated_pack_descriptor(bytes: &[u8]) -> Result<Option<GeneratedPackDescriptor>> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).map_err(|_| Error::Corrupt {
        stage: crate::CorruptionStage::PackEntry,
    })?;
    let object = value.as_object().ok_or(Error::Corrupt {
        stage: crate::CorruptionStage::PackEntry,
    })?;
    let number = |name: &str| object.get(name).and_then(serde_json::Value::as_u64);
    let string = |name: &str| object.get(name).and_then(serde_json::Value::as_str);
    let corrupt = || Error::Corrupt {
        stage: crate::CorruptionStage::PackEntry,
    };
    let version = u32::try_from(number("version").ok_or_else(corrupt)?).map_err(|_| corrupt())?;
    if version != GENERATED_PACK_CACHE_VERSION {
        return Ok(None);
    }
    if object.len() != 7 {
        return Err(corrupt());
    }
    Ok(Some(GeneratedPackDescriptor {
        version,
        request_hash: string("request_hash").ok_or_else(corrupt)?.to_owned(),
        content_hash: string("content_hash").ok_or_else(corrupt)?.to_owned(),
        checksum: string("checksum").ok_or_else(corrupt)?.to_owned(),
        size: number("size").ok_or_else(corrupt)?,
        object_count: u32::try_from(number("object_count").ok_or_else(corrupt)?)
            .map_err(|_| corrupt())?,
        selection_object_count: number("selection_object_count").ok_or_else(corrupt)?,
    }))
}

fn inspect_cached_pack(
    path: &std::path::Path,
    expected_size: u64,
    expected_objects: u32,
    expected_checksum: [u8; 20],
    expected_content_hash: [u8; 32],
    cancellation: &CancellationToken,
) -> Result<()> {
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() != expected_size {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(io_error)?;
    let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let object_count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if &header[..4] != b"PACK" || !matches!(version, 2 | 3) || object_count != expected_objects {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut sha1 = Sha1::new();
    let mut blake3 = blake3::Hasher::new();
    let mut chunk = [0u8; 1024 * 1024];
    {
        let mut body = Read::by_ref(&mut file).take(expected_size - 20);
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let read = body.read(&mut chunk).map_err(io_error)?;
            if read == 0 {
                break;
            }
            sha1.update(&chunk[..read]);
            blake3.update(&chunk[..read]);
        }
    }
    let mut trailer = [0u8; 20];
    file.read_exact(&mut trailer).map_err(io_error)?;
    blake3.update(&trailer);
    let actual_sha1: [u8; 20] = sha1.finalize().into();
    if actual_sha1 != trailer
        || trailer != expected_checksum
        || blake3.finalize().as_bytes() != &expected_content_hash
    {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    Ok(())
}

fn decode_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N.saturating_mul(2) {
        return None;
    }
    let mut output = [0u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        let start = index.saturating_mul(2);
        *byte = u8::from_str_radix(value.get(start..start + 2)?, 16).ok()?;
    }
    Some(output)
}

async fn try_reuse_single_pack(
    repository: &RemoteGitRepository,
    operation: &crate::OperationContext,
    object_ids: &[ObjectId],
    cancellation: &CancellationToken,
) -> Result<Option<GeneratedPack>> {
    let Some(inventory) = repository.single_pack_inventory() else {
        return Ok(None);
    };
    if inventory.object_count != object_ids.len() as u64 {
        return Ok(None);
    }
    if inventory.pack_size > operation.max_response_bytes() {
        return Err(Error::LimitExceeded {
            limit: "pack response bytes",
            actual: inventory.pack_size,
            maximum: operation.max_response_bytes(),
        });
    }
    let Some(expected_checksum) = operation
        .single_pack_checksum_for_exact_objects(inventory.pack_id, object_ids)
        .await?
    else {
        return Ok(None);
    };

    let started = Instant::now();
    let file = NamedTempFile::new().map_err(io_error)?;
    operation
        .download_pack_to_path(inventory.pack_id, inventory.pack_size, file.path())
        .await?;
    let path = file.path().to_owned();
    let token = cancellation.clone();
    let (checksum, content_hash) = tokio::task::spawn_blocking(move || {
        inspect_reused_pack(
            &path,
            inventory.pack_id,
            inventory.pack_size,
            inventory.object_count,
            expected_checksum,
            &token,
        )
    })
    .await
    .map_err(|source| Error::DecodeTask { source })??;
    operation
        .charge(BudgetDimension::ResponseBytes, inventory.pack_size)
        .await?;
    let object_count = u32::try_from(inventory.object_count).map_err(|_| Error::LimitExceeded {
        limit: "pack object count",
        actual: inventory.object_count,
        maximum: u32::MAX as u64,
    })?;
    tracing::info!(
        target: "crab_remote_git::telemetry",
        telemetry_event = "pack_generation",
        strategy = "canonical_pack",
        object_count,
        copied_entries = object_count,
        converted_deltas = 0u64,
        materialized_entries = 0u64,
        source_bytes = inventory.pack_size,
        response_bytes = inventory.pack_size,
        pack_generation_ms = started.elapsed().as_millis() as u64,
        "remote Git response pack reused"
    );
    Ok(Some(GeneratedPack {
        file: Arc::new(file),
        size: inventory.pack_size,
        checksum,
        content_hash,
        object_count,
    }))
}

fn inspect_reused_pack(
    path: &std::path::Path,
    pack_id: crab_xet::hash::MerkleHash,
    expected_size: u64,
    expected_objects: u64,
    expected_checksum: [u8; 20],
    cancellation: &CancellationToken,
) -> Result<([u8; 20], [u8; 32])> {
    if expected_size < 32 {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    let mut file = std::fs::File::open(path).map_err(io_error)?;
    if file.metadata().map_err(io_error)?.len() != expected_size {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::Inventory,
        });
    }
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(io_error)?;
    let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let object_count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if &header[..4] != b"PACK"
        || !matches!(version, 2 | 3)
        || u64::from(object_count) != expected_objects
    {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }

    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut sha1 = Sha1::new();
    let mut blake3 = blake3::Hasher::new();
    let mut chunk = [0u8; 1024 * 1024];
    {
        let mut body = Read::by_ref(&mut file).take(expected_size - 20);
        loop {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let read = body.read(&mut chunk).map_err(io_error)?;
            if read == 0 {
                break;
            }
            sha1.update(&chunk[..read]);
            blake3.update(&chunk[..read]);
        }
    }
    let mut trailer = [0u8; 20];
    file.read_exact(&mut trailer).map_err(io_error)?;
    blake3.update(&trailer);
    let actual_sha1: [u8; 20] = sha1.finalize().into();
    let actual_blake3 = *blake3.finalize().as_bytes();
    if actual_sha1 != trailer
        || trailer != expected_checksum
        || actual_blake3
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            != pack_id.to_string()
    {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::PackEntry,
        });
    }
    Ok((trailer, actual_blake3))
}

async fn generate_pack_with_operation(
    operation: &crate::OperationContext,
    object_ids: &[ObjectId],
    thin_bases: &[ObjectId],
    selected_objects: Option<&HashSet<ObjectId>>,
    strategy: &'static str,
    cancellation: &CancellationToken,
) -> Result<GeneratedPack> {
    let started = Instant::now();
    let object_count = u32::try_from(object_ids.len()).map_err(|_| Error::LimitExceeded {
        limit: "pack object count",
        actual: object_ids.len() as u64,
        maximum: u32::MAX as u64,
    })?;
    let mut writer = PackWriter::new(object_count, operation.max_response_bytes())?;
    let thin_bases = thin_bases.iter().copied().collect::<HashSet<_>>();
    let mut emitted = HashSet::with_capacity(object_ids.len());
    let mut stats = PackAssemblyStats::default();
    // Dense catalog responses resolve the full OID set once, allowing the
    // locator to choose one bounded scan instead of repeating point waves for
    // every pack assembly batch. Range reads remain batch-sized below.
    let locator_plan = if selected_objects.is_some() {
        Some(operation.lookup_packed_entry_locators(object_ids).await?)
    } else {
        None
    };
    for (batch_index, batch) in object_ids.chunks(OBJECT_BATCH_SIZE).enumerate() {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let entries = match locator_plan.as_ref() {
            Some(locators) => {
                let start = batch_index.saturating_mul(OBJECT_BATCH_SIZE);
                let end = start.saturating_add(batch.len());
                let locators = locators.get(start..end).ok_or(Error::InternalInvariant {
                    invariant: "dense pack locator plan does not match object batches",
                })?;
                operation
                    .read_packed_entries_with_locators(batch, locators)
                    .await?
            }
            None => operation.read_packed_entries(batch).await?,
        };
        // Selected dense responses preserve REF_DELTA dependencies by object
        // ID. The conservative path still orders entries for its historical
        // OFS_DELTA handling and thin-pack behavior.
        let entries = if selected_objects.is_none() {
            order_packed_entries(entries)?
        } else {
            entries
        };
        let materialize = entries
            .iter()
            .filter_map(|entry| {
                let materialize = selected_objects.map_or_else(
                    || {
                        entry.base_oid.is_some_and(|base| {
                            !emitted.contains(&base) && !thin_bases.contains(&base)
                        })
                    },
                    |selected| should_materialize_selected_entry(entry, selected, &thin_bases),
                );
                if selected_objects.is_none() {
                    emitted.insert(entry.oid);
                }
                materialize.then_some(entry.oid)
            })
            .collect::<Vec<_>>();
        let objects = operation
            .materialize_packed_entries(entries.clone(), &materialize)
            .await?;
        let materialized = objects
            .into_iter()
            .map(|object| (object.oid, object))
            .collect::<HashMap<_, _>>();
        let batch_cancellation = cancellation.clone();
        let (next_writer, batch_stats) = tokio::task::spawn_blocking(move || {
            writer.write_entries(entries, materialized, &batch_cancellation)
        })
        .await
        .map_err(|source| Error::DecodeTask { source })??;
        writer = next_writer;
        stats.add(batch_stats);
    }

    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let finish_cancellation = cancellation.clone();
    let pack = tokio::task::spawn_blocking(move || writer.finish(&finish_cancellation))
        .await
        .map_err(|source| Error::DecodeTask { source })??;
    let size = pack.size;
    operation
        .charge(BudgetDimension::ResponseBytes, size)
        .await?;
    tracing::info!(
        target: "crab_remote_git::telemetry",
        telemetry_event = "pack_generation",
        strategy,
        object_count,
        copied_entries = stats.copied_entries,
        converted_deltas = stats.converted_deltas,
        materialized_entries = stats.materialized_entries,
        source_bytes = stats.source_bytes,
        response_bytes = size,
        pack_generation_ms = started.elapsed().as_millis() as u64,
        "remote Git response pack generated"
    );
    Ok(pack)
}

fn should_materialize_selected_entry(
    entry: &crate::reader::RemoteGitPackedEntry,
    selected: &HashSet<ObjectId>,
    thin_bases: &HashSet<ObjectId>,
) -> bool {
    entry
        .base_oid
        .is_some_and(|base| !selected.contains(&base) && !thin_bases.contains(&base))
}

fn order_packed_entries(
    entries: Vec<crate::reader::RemoteGitPackedEntry>,
) -> Result<Vec<crate::reader::RemoteGitPackedEntry>> {
    let positions = entries
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.oid, position))
        .collect::<HashMap<_, _>>();
    if positions.len() != entries.len() {
        return Err(Error::InternalInvariant {
            invariant: "pack assembler received duplicate objects",
        });
    }
    let mut dependencies = vec![0u8; entries.len()];
    let mut dependents = vec![Vec::new(); entries.len()];
    for (position, entry) in entries.iter().enumerate() {
        let Some(base) = entry
            .base_oid
            .and_then(|base| positions.get(&base).copied())
        else {
            continue;
        };
        dependencies[position] = 1;
        dependents[base].push(position);
    }
    let mut ready = dependencies
        .iter()
        .enumerate()
        .filter_map(|(position, dependencies)| (*dependencies == 0).then_some(Reverse(position)))
        .collect::<BinaryHeap<_>>();
    let mut order = Vec::with_capacity(entries.len());
    while let Some(Reverse(position)) = ready.pop() {
        order.push(position);
        for dependent in &dependents[position] {
            dependencies[*dependent] = dependencies[*dependent].saturating_sub(1);
            if dependencies[*dependent] == 0 {
                ready.push(Reverse(*dependent));
            }
        }
    }
    if order.len() != entries.len() {
        return Err(Error::Corrupt {
            stage: crate::CorruptionStage::Delta,
        });
    }
    let mut entries = entries.into_iter().map(Some).collect::<Vec<_>>();
    order
        .into_iter()
        .map(|position| {
            entries
                .get_mut(position)
                .and_then(Option::take)
                .ok_or(Error::InternalInvariant {
                    invariant: "pack assembler dependency order is invalid",
                })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
struct PackAssemblyStats {
    copied_entries: u64,
    converted_deltas: u64,
    materialized_entries: u64,
    source_bytes: u64,
}

impl PackAssemblyStats {
    fn add(&mut self, other: Self) {
        self.copied_entries = self.copied_entries.saturating_add(other.copied_entries);
        self.converted_deltas = self.converted_deltas.saturating_add(other.converted_deltas);
        self.materialized_entries = self
            .materialized_entries
            .saturating_add(other.materialized_entries);
        self.source_bytes = self.source_bytes.saturating_add(other.source_bytes);
    }
}

struct PackWriter {
    file: NamedTempFile,
    hash: Sha1,
    content_hash: blake3::Hasher,
    object_count: u32,
    max_bytes: u64,
}

impl PackWriter {
    fn new(object_count: u32, max_bytes: u64) -> Result<Self> {
        if max_bytes < 20 {
            return Err(Error::LimitExceeded {
                limit: "pack response bytes",
                actual: 20,
                maximum: max_bytes,
            });
        }
        let mut file = NamedTempFile::new().map_err(io_error)?;
        let mut hash = Sha1::new();
        let mut content_hash = blake3::Hasher::new();
        {
            let mut sink = HashingWriter {
                file: file.as_file_mut(),
                hash: &mut hash,
                content_hash: &mut content_hash,
                written: 0,
                max_bytes: Some(max_bytes - 20),
            };
            sink.write_all(b"PACK").map_err(io_error)?;
            sink.write_all(&2u32.to_be_bytes()).map_err(io_error)?;
            sink.write_all(&object_count.to_be_bytes())
                .map_err(io_error)?;
        }
        Ok(Self {
            file,
            hash,
            content_hash,
            object_count,
            max_bytes,
        })
    }

    fn write_entries(
        mut self,
        entries: Vec<crate::reader::RemoteGitPackedEntry>,
        mut materialized: HashMap<ObjectId, RemoteGitObject>,
        cancellation: &CancellationToken,
    ) -> Result<(Self, PackAssemblyStats)> {
        let written = self.file.stream_position().map_err(io_error)?;
        let mut sink = HashingWriter {
            file: self.file.as_file_mut(),
            hash: &mut self.hash,
            content_hash: &mut self.content_hash,
            written,
            max_bytes: Some(self.max_bytes - 20),
        };
        let mut stats = PackAssemblyStats::default();
        for entry in entries {
            if cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            stats.source_bytes = stats.source_bytes.saturating_add(entry.bytes.len() as u64);
            let result = if let Some(object) = materialized.remove(&entry.oid) {
                stats.materialized_entries = stats.materialized_entries.saturating_add(1);
                write_object(&mut sink, &object)
            } else {
                match (entry.header, entry.base_oid) {
                    (_, None) | (Header::RefDelta { .. }, Some(_)) => {
                        stats.copied_entries = stats.copied_entries.saturating_add(1);
                        sink.write_all(&entry.bytes)
                    }
                    (Header::OfsDelta { .. }, Some(base_oid)) => {
                        stats.converted_deltas = stats.converted_deltas.saturating_add(1);
                        Header::RefDelta { base_id: base_oid }
                            .write_to(entry.decompressed_size, &mut sink)
                            .and_then(|_| sink.write_all(&entry.bytes[entry.header_size..]))
                            .map(|_| ())
                    }
                    (Header::Commit | Header::Tree | Header::Blob | Header::Tag, Some(_)) => {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "base pack entry unexpectedly names a delta base",
                        ))
                    }
                }
            };
            result.map_err(|error| map_pack_write_error(error, self.max_bytes))?;
        }
        if !materialized.is_empty() {
            return Err(Error::InternalInvariant {
                invariant: "pack assembler did not consume every materialized object",
            });
        }
        Ok((self, stats))
    }

    fn finish(mut self, cancellation: &CancellationToken) -> Result<GeneratedPack> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let checksum: [u8; 20] = self.hash.finalize().into();
        self.content_hash.update(&checksum);
        let content_hash = *self.content_hash.finalize().as_bytes();
        let body_size = self.file.as_file().metadata().map_err(io_error)?.len();
        self.file
            .as_file_mut()
            .write_all(&checksum)
            .and_then(|_| self.file.as_file_mut().flush())
            .map_err(io_error)?;
        let size = self.file.as_file().metadata().map_err(io_error)?.len();
        if size > self.max_bytes || body_size.saturating_add(20) > self.max_bytes {
            return Err(Error::LimitExceeded {
                limit: "pack response bytes",
                actual: size.max(body_size.saturating_add(20)),
                maximum: self.max_bytes,
            });
        }
        self.file
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(io_error)?;
        let pack = GeneratedPack {
            file: Arc::new(self.file),
            size,
            checksum,
            content_hash,
            object_count: self.object_count,
        };
        // HashingWriter covered every header and entry write. Re-scanning this
        // file would add a second repository-sized read to every assembled
        // response; cache loads and repack outputs retain independent checks.
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(pack)
    }
}

fn write_object(sink: &mut HashingWriter<'_>, object: &RemoteGitObject) -> io::Result<()> {
    let type_code = match object.kind {
        gix_object::Kind::Commit => 1,
        gix_object::Kind::Tree => 2,
        gix_object::Kind::Blob => 3,
        gix_object::Kind::Tag => 4,
    };
    write_pack_header(sink, type_code, object.data.len() as u64)?;
    let mut encoder = ZlibEncoder::new(sink, Compression::default());
    encoder.write_all(&object.data)?;
    let _ = encoder.finish()?;
    Ok(())
}

fn map_pack_write_error(error: io::Error, maximum: u64) -> Error {
    if error.kind() == io::ErrorKind::FileTooLarge {
        Error::LimitExceeded {
            limit: "pack response bytes",
            actual: maximum.saturating_add(1),
            maximum,
        }
    } else {
        io_error(error)
    }
}

fn write_pack_header(writer: &mut impl Write, type_code: u8, size: u64) -> io::Result<()> {
    let mut value = size;
    let mut byte = (value & 0x0f) as u8 | (type_code << 4);
    value >>= 4;
    if value != 0 {
        byte |= 0x80;
    }
    writer.write_all(&[byte])?;
    while value != 0 {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        writer.write_all(&[byte])?;
    }
    Ok(())
}

struct HashingWriter<'a> {
    file: &'a mut std::fs::File,
    hash: &'a mut Sha1,
    content_hash: &'a mut blake3::Hasher,
    written: u64,
    max_bytes: Option<u64>,
}

impl Write for HashingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.max_bytes.is_some_and(|maximum| {
            u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum.saturating_sub(self.written)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "generated pack exceeds its response limit",
            ));
        }
        self.file.write_all(bytes)?;
        self.hash.update(bytes);
        self.content_hash.update(bytes);
        self.written = self.written.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    band: Option<u8>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let payload_len = bytes.len() + band.map_or(0, |_| 1);
    let length = payload_len + 4;
    if length > 0xffff {
        return Err(Error::LimitExceeded {
            limit: "packet-line bytes",
            actual: length as u64,
            maximum: 0xffff,
        });
    }
    write_all_cancellable(writer, format!("{length:04x}").as_bytes(), cancellation).await?;
    if let Some(band) = band {
        write_all_cancellable(writer, &[band], cancellation).await?;
    }
    write_all_cancellable(writer, bytes, cancellation).await?;
    Ok(())
}

async fn write_all_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Error::Cancelled),
        result = writer.write_all(bytes) => result.map_err(io_error),
    }
}

fn io_error(error: io::Error) -> Error {
    Error::Metadata(crab_metadata::error::MetadataError::Io { source: error })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use bytes::Bytes;
    use crab_xet::hash::MerkleHash;

    #[test]
    fn generated_pack_key_binds_repository_identity_and_manifest_digest() {
        let identity =
            crate::RepositoryIdentity::new("memory", "org/repo", 1).expect("repository identity");
        let moved_identity = crate::RepositoryIdentity::new("memory", "org/repo", 2)
            .expect("moved repository identity");
        let objects = [ObjectId::from([1; 20])];
        let key = |identity, validation_digest| {
            generated_pack_cache_key(
                identity,
                validation_digest,
                [2; 32],
                [3; 32],
                &objects,
                false,
            )
        };

        let base = key(&identity, "validation-a");
        assert_ne!(base, key(&moved_identity, "validation-a"));
        assert_ne!(base, key(&identity, "validation-b"));
    }

    #[test]
    fn preplanned_pack_key_binds_request_authorization_and_repository_generation() {
        let identity =
            crate::RepositoryIdentity::new("memory", "org/repo", 1).expect("repository identity");
        let moved_identity = crate::RepositoryIdentity::new("memory", "org/repo", 2)
            .expect("moved repository identity");
        let key = |identity, validation, authorization, request| {
            generated_pack_request_cache_key(identity, validation, authorization, request)
        };

        let base = key(&identity, "validation-a", [2; 32], [3; 32]);
        assert_ne!(base, key(&moved_identity, "validation-a", [2; 32], [3; 32]));
        assert_ne!(base, key(&identity, "validation-b", [2; 32], [3; 32]));
        assert_ne!(base, key(&identity, "validation-a", [4; 32], [3; 32]));
        assert_ne!(base, key(&identity, "validation-a", [2; 32], [5; 32]));
    }

    #[test]
    fn generated_pack_cache_key_separates_descriptor_versions() {
        let identity =
            crate::RepositoryIdentity::new("memory", "org/repo", 1).expect("repository identity");
        let objects = [ObjectId::from([1; 20])];
        let current =
            generated_pack_cache_key(&identity, "validation", [2; 32], [3; 32], &objects, false);

        let selection_digest = selected_object_digest(&objects);
        let mut previous_hash = blake3::Hasher::new();
        previous_hash.update(b"crab.generated-pack.request.v1\0");
        identity.hash_cache_identity(&mut previous_hash);
        previous_hash.update(b"validation");
        previous_hash.update(&[2; 32]);
        previous_hash.update(&[3; 32]);
        previous_hash.update(&[0]);
        previous_hash.update(&selection_digest);
        let previous = GeneratedPackCacheKey {
            digest: *previous_hash.finalize().as_bytes(),
            selection_digest,
        };

        assert_ne!(current, previous);
    }

    #[test]
    fn generated_pack_lease_poll_backoff_is_bounded() {
        assert_eq!(
            generated_pack_lease_poll(0),
            GENERATED_PACK_LEASE_POLL_INITIAL
        );
        assert_eq!(generated_pack_lease_poll(1), Duration::from_millis(500));
        assert_eq!(generated_pack_lease_poll(4), Duration::from_secs(4));
        assert_eq!(generated_pack_lease_poll(5), GENERATED_PACK_LEASE_POLL_MAX);
        assert_eq!(
            generated_pack_lease_poll(usize::MAX),
            GENERATED_PACK_LEASE_POLL_MAX
        );
    }

    #[test]
    fn generated_pack_selection_digest_is_order_independent() {
        let identity =
            crate::RepositoryIdentity::new("memory", "org/repo", 1).expect("repository identity");
        let first = ObjectId::from([1; 20]);
        let second = ObjectId::from([2; 20]);
        let key = |objects: &[ObjectId]| {
            generated_pack_cache_key(&identity, "validation", [2; 32], [3; 32], objects, false)
        };

        assert_eq!(key(&[first, second]), key(&[second, first]));
    }

    #[test]
    fn complete_pack_consolidation_requires_the_full_large_multi_pack_inventory() {
        assert!(RemoteGitRepository::complete_pack_consolidation_candidate(
            2,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS as u64,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS,
        ));
        assert!(!RemoteGitRepository::complete_pack_consolidation_candidate(
            1,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS as u64,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS,
        ));
        assert!(!RemoteGitRepository::complete_pack_consolidation_candidate(
            2,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS as u64,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS - 1,
        ));
        assert!(!RemoteGitRepository::complete_pack_consolidation_candidate(
            2,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS as u64 + 1,
            COMPLETE_PACK_CONSOLIDATION_MIN_OBJECTS,
        ));
    }

    #[test]
    fn selected_pack_repack_requires_a_large_dense_filter() {
        assert!(RemoteGitRepository::selected_pack_repack_candidate(
            200_000, 100_000, 100_000,
        ));
        assert!(!RemoteGitRepository::selected_pack_repack_candidate(
            200_000, 99_999, 100_000,
        ));
        assert!(!RemoteGitRepository::selected_pack_repack_candidate(
            200_000, 200_000, 100_000,
        ));
        assert!(!RemoteGitRepository::selected_pack_repack_candidate(
            200_000, 100_000, 100_001,
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_pack_downloads_are_bounded_and_restore_inventory_order() {
        let workspace = tempfile::tempdir().expect("source download workspace");
        let inventory = (0..8)
            .map(|index| GitPackInventoryEntry {
                pack_id: MerkleHash::from_hex(&format!("{:064x}", index + 1))
                    .expect("pack identity"),
                object_count: index + 1,
                pack_size: 1,
            })
            .collect::<Vec<_>>();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let cancellation = CancellationToken::new();
        let sources = download_repack_sources_with(
            inventory,
            workspace.path().to_owned(),
            &cancellation,
            3,
            {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                move |pack, path| {
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        tokio::fs::write(&path, pack.pack_id.as_bytes())
                            .await
                            .map_err(io_error)?;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    }
                }
            },
        )
        .await
        .expect("bounded source downloads");

        assert_eq!(maximum.load(Ordering::SeqCst), 3);
        assert_eq!(
            sources
                .iter()
                .map(|source| source.object_count)
                .collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        assert!(sources.iter().all(|source| source.path.is_file()));
    }

    fn packed_entry(oid: u8, base_oid: Option<u8>) -> crate::reader::RemoteGitPackedEntry {
        let oid = ObjectId::from([oid; 20]);
        let base_oid = base_oid.map(|base| ObjectId::from([base; 20]));
        crate::reader::RemoteGitPackedEntry {
            oid,
            pack_offset: 0,
            header: base_oid.map_or(Header::Blob, |base_id| Header::RefDelta { base_id }),
            decompressed_size: 1,
            header_size: 1,
            base_oid,
            bytes: Bytes::from_static(&[0, 0]),
        }
    }

    #[test]
    fn pack_entries_are_ordered_base_first_across_a_deep_chain() {
        let entries = vec![
            packed_entry(3, Some(2)),
            packed_entry(2, Some(1)),
            packed_entry(1, None),
        ];

        let ordered = order_packed_entries(entries).expect("acyclic delta chain");

        assert_eq!(
            ordered
                .into_iter()
                .map(|entry| entry.oid)
                .collect::<Vec<_>>(),
            vec![
                ObjectId::from([1; 20]),
                ObjectId::from([2; 20]),
                ObjectId::from([3; 20]),
            ]
        );
    }

    #[test]
    fn selected_pack_reuses_a_base_that_is_read_after_its_delta() {
        let selected = [ObjectId::from([1; 20]), ObjectId::from([2; 20])]
            .into_iter()
            .collect::<HashSet<_>>();
        let entry = packed_entry(2, Some(1));

        assert!(!should_materialize_selected_entry(
            &entry,
            &selected,
            &HashSet::new(),
        ));
    }

    #[test]
    fn selected_pack_materializes_a_delta_base_outside_the_selection() {
        let selected = [ObjectId::from([2; 20])]
            .into_iter()
            .collect::<HashSet<_>>();
        let entry = packed_entry(2, Some(1));

        assert!(should_materialize_selected_entry(
            &entry,
            &selected,
            &HashSet::new(),
        ));
    }

    #[test]
    fn pack_writer_accepts_a_ref_delta_before_its_base() {
        let base_data = b"hello world";
        let target_data = b"hello world!";
        let base_oid = blob_oid(base_data);
        let target_oid = blob_oid(target_data);
        let delta = [0x0b, 0x0c, 0x90, 0x0b, 0x01, b'!'];
        let target = valid_packed_entry(
            target_oid,
            Header::RefDelta { base_id: base_oid },
            &delta,
            Some(base_oid),
        );
        let base = valid_packed_entry(base_oid, Header::Blob, base_data, None);
        let cancellation = CancellationToken::new();
        let writer = PackWriter::new(2, 1024).expect("pack header fits response bound");
        let (writer, stats) = writer
            .write_entries(vec![target, base], HashMap::new(), &cancellation)
            .expect("forward REF delta is writable");
        let generated = writer.finish(&cancellation).expect("finish generated pack");

        assert_strict_pack(&generated);
        assert_eq!(stats.copied_entries, 2);
        assert_eq!(stats.materialized_entries, 0);
    }

    #[test]
    fn pack_writer_rewrites_an_ofs_delta_before_its_base() {
        let base_data = b"hello world";
        let target_data = b"hello world!";
        let base_oid = blob_oid(base_data);
        let target_oid = blob_oid(target_data);
        let delta = [0x0b, 0x0c, 0x90, 0x0b, 0x01, b'!'];
        let target = valid_packed_entry(
            target_oid,
            Header::OfsDelta { base_distance: 1 },
            &delta,
            Some(base_oid),
        );
        let base = valid_packed_entry(base_oid, Header::Blob, base_data, None);
        let cancellation = CancellationToken::new();
        let writer = PackWriter::new(2, 1024).expect("pack header fits response bound");
        let (writer, stats) = writer
            .write_entries(vec![target, base], HashMap::new(), &cancellation)
            .expect("forward OFS delta is writable");
        let generated = writer.finish(&cancellation).expect("finish generated pack");

        assert_strict_pack(&generated);
        assert_eq!(stats.converted_deltas, 1);
        assert_eq!(stats.materialized_entries, 0);
    }

    #[test]
    fn pack_writer_finish_returns_the_checksums_streamed_during_writes() {
        let data = b"streamed checksum proof";
        let oid = blob_oid(data);
        let entry = valid_packed_entry(oid, Header::Blob, data, None);
        let cancellation = CancellationToken::new();
        let writer = PackWriter::new(1, 1024).expect("pack header fits response bound");
        let (writer, _) = writer
            .write_entries(vec![entry], HashMap::new(), &cancellation)
            .expect("write pack entry");
        let generated = writer.finish(&cancellation).expect("finish generated pack");
        let bytes = std::fs::read(generated.path()).expect("read generated pack");

        let checksum: [u8; 20] = Sha1::digest(&bytes[..bytes.len() - 20]).into();
        assert_eq!(generated.checksum, checksum);
        assert_eq!(generated.content_hash, *blake3::hash(&bytes).as_bytes());
    }

    fn assert_strict_pack(pack: &GeneratedPack) {
        let output = std::process::Command::new("git")
            .args(["index-pack", "--strict", "--stdin"])
            .stdin(std::process::Stdio::from(
                std::fs::File::open(pack.path()).expect("open generated pack"),
            ))
            .output()
            .expect("run strict index-pack");
        assert!(
            output.status.success(),
            "strict index-pack failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn blob_oid(data: &[u8]) -> ObjectId {
        let mut object = format!("blob {}\0", data.len()).into_bytes();
        object.extend_from_slice(data);
        ObjectId::from(<[u8; 20]>::from(Sha1::digest(object)))
    }

    fn valid_packed_entry(
        oid: ObjectId,
        header: Header,
        data: &[u8],
        base_oid: Option<ObjectId>,
    ) -> crate::reader::RemoteGitPackedEntry {
        let mut bytes = Vec::new();
        let header_size = header
            .write_to(data.len() as u64, &mut bytes)
            .expect("write pack entry header");
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("compress pack entry");
        bytes.extend(encoder.finish().expect("finish pack entry compression"));
        crate::reader::RemoteGitPackedEntry {
            oid,
            pack_offset: 12,
            header,
            decompressed_size: data.len() as u64,
            header_size,
            base_oid,
            bytes: Bytes::from(bytes),
        }
    }

    #[test]
    fn pack_entry_dependency_cycles_fail_closed() {
        let entries = vec![packed_entry(1, Some(2)), packed_entry(2, Some(1))];

        assert!(matches!(
            order_packed_entries(entries),
            Err(Error::Corrupt {
                stage: crate::CorruptionStage::Delta
            })
        ));
    }

    #[test]
    fn pack_object_contains_raw_git_payload_without_loose_header() {
        let mut file = NamedTempFile::new().expect("temporary pack object");
        let mut hash = Sha1::new();
        let mut content_hash = blake3::Hasher::new();
        {
            let mut sink = HashingWriter {
                file: file.as_file_mut(),
                hash: &mut hash,
                content_hash: &mut content_hash,
                written: 0,
                max_bytes: None,
            };
            let object = RemoteGitObject {
                oid: ObjectId::from_hex(b"0000000000000000000000000000000000000000")
                    .expect("object ID"),
                kind: gix_object::Kind::Blob,
                data: Bytes::from_static(b"hello"),
            };
            write_object(&mut sink, &object).expect("write pack object");
        }
        file.as_file_mut().flush().expect("flush pack object");

        let bytes = std::fs::read(file.path()).expect("read pack object");
        assert_eq!(
            bytes[0], 0x35,
            "blob header must encode a five-byte payload"
        );
        let mut decoder = flate2::read::ZlibDecoder::new(&bytes[1..]);
        let mut payload = Vec::new();
        decoder
            .read_to_end(&mut payload)
            .expect("decode pack object");
        assert_eq!(payload, b"hello");
    }

    #[test]
    fn pack_header_uses_seven_bit_continuation_groups() {
        let mut header = Vec::new();
        write_pack_header(&mut header, 3, 4096).expect("write large pack header");
        assert_eq!(header, [0xb0, 0x80, 0x02]);
    }

    #[test]
    fn pack_writer_rejects_response_limit_before_writing_an_unbounded_temp_pack() {
        let mut writer = PackWriter::new(1, 32).expect("pack header fits the response bound");
        let object = RemoteGitObject {
            oid: ObjectId::from_hex(b"0000000000000000000000000000000000000000")
                .expect("object ID"),
            kind: gix_object::Kind::Blob,
            data: Bytes::from(vec![b'x'; 128]),
        };
        let written = writer.file.stream_position().expect("pack position");
        let maximum = writer.max_bytes;
        let mut sink = HashingWriter {
            file: writer.file.as_file_mut(),
            hash: &mut writer.hash,
            content_hash: &mut writer.content_hash,
            written,
            max_bytes: Some(maximum - 20),
        };
        let result =
            write_object(&mut sink, &object).map_err(|error| map_pack_write_error(error, maximum));
        assert!(matches!(result, Err(Error::LimitExceeded { .. })));
    }

    #[test]
    fn generation_batch_covers_the_default_operation_object_bound() {
        let maximum = usize::try_from(crate::OperationLimits::default().max_logical_objects)
            .expect("default logical-object bound fits usize");

        assert!(OBJECT_BATCH_SIZE >= maximum);
    }

    #[test]
    fn generated_pack_cache_allows_delta_bases_beyond_the_selected_objects() {
        let descriptor = GeneratedPackDescriptor {
            version: GENERATED_PACK_CACHE_VERSION,
            request_hash: "request".to_owned(),
            content_hash: "a".repeat(64),
            checksum: "b".repeat(40),
            size: 64,
            object_count: 5,
            selection_object_count: 3,
        };

        assert!(generated_pack_descriptor_matches_request(
            &descriptor,
            "request",
            Some(3),
            10,
        ));
        assert!(!generated_pack_descriptor_matches_request(
            &descriptor,
            "request",
            Some(5),
            10,
        ));
        assert!(generated_pack_descriptor_matches_request(
            &descriptor,
            "request",
            None,
            10,
        ));
    }

    #[test]
    fn generated_pack_cache_rejects_an_artifact_smaller_than_the_selection() {
        let descriptor = GeneratedPackDescriptor {
            version: GENERATED_PACK_CACHE_VERSION,
            request_hash: "request".to_owned(),
            content_hash: "a".repeat(64),
            checksum: "b".repeat(40),
            size: 64,
            object_count: 2,
            selection_object_count: 3,
        };

        assert!(!generated_pack_descriptor_matches_request(
            &descriptor,
            "request",
            Some(3),
            10,
        ));
    }

    #[test]
    fn generated_pack_cache_treats_an_older_descriptor_as_a_miss() {
        let bytes = serde_json::json!({
            "version": 1,
            "request_hash": "request",
            "content_hash": "a".repeat(64),
            "checksum": "b".repeat(40),
            "size": 64,
            "object_count": 3,
        });

        assert!(
            decode_generated_pack_descriptor(&serde_json::to_vec(&bytes).unwrap())
                .unwrap()
                .is_none()
        );
    }
}
