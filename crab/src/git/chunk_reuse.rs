//! Capsule-rooted chunk reuse for add and staged publication.

use super::xorb_proof::{
    cached_remote_xorb_index_matches, committed_origin_receipt_matches_remote,
    record_remote_xorb_index, record_remote_xorb_payload_proof, remote_xorb_index,
};
use crate::core::config::DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::{StoreLayout, store::Store};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::format::XorbRef;
use futures_util::StreamExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

const GLOBAL_CHUNK_LOOKUP_BUDGET: Duration = Duration::from_mins(2);
const ADD_REMOTE_CLASSIFIER_INITIAL_CAPACITY: usize = 65_536;

pub(super) struct ChunkVerifier {
    head_check_concurrency: usize,
    store: Store,
    caching_store: Option<crab_cache_store::CachingStore>,
    router: StoreLayout,
    pub(super) cancel: CancellationToken,
}

impl ChunkVerifier {
    pub(super) fn new(
        head_check_concurrency: usize,
        store: Store,
        caching_store: Option<crab_cache_store::CachingStore>,
        router: StoreLayout,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            head_check_concurrency,
            store,
            caching_store,
            router,
            cancel,
        }
    }
    pub(super) async fn revalidate(
        &self,
        planned: &HashMap<MerkleHash, crab_staging::push_plan::ExistingChunkCandidate>,
    ) -> Result<VerifiedRemoteCandidates> {
        if planned.is_empty() {
            return Ok(VerifiedRemoteCandidates::default());
        }
        let store = &self.store;
        let exact_candidates = planned
            .iter()
            .map(|(chunk_hash, candidate)| (*chunk_hash, candidate.placement_id))
            .collect::<Vec<_>>();
        let deadline = tokio::time::Instant::now() + GLOBAL_CHUNK_LOOKUP_BUDGET;
        let (capsule, capsule_indexed, capsule_matched) =
            tokio::time::timeout_at(deadline, async {
                let candidates_layout = crab_storage::StoreLayout::with_global_prefix(
                    store.as_storage().clone(),
                    self.router.repo_prefix().to_owned(),
                    self.router.global_prefix().to_owned(),
                );
                let mut candidates =
                    crab_metadata::capsule_protocol::CapsuleChunkIndex::new(&candidates_layout)
                        .candidates_by_id(
                            store.as_storage(),
                            &exact_candidates,
                            crab_metadata::capsule_protocol::MAX_CATALOG_BYTES,
                        )
                        .await?;
                let indexed = candidates.placements.len();
                candidates.placements.retain(|hash, placement| {
                    planned
                        .get(&MerkleHash::from(*hash))
                        .is_some_and(|candidate| {
                            candidate.placement_id == placement.placement_id()
                                && candidate.origin_proof_id == placement.origin_proof_id
                                && candidate.xorb_ref
                                    == XorbRef {
                                        xorb_hash: MerkleHash::from(placement.xorb_hash),
                                        chunk_index: placement.chunk_index,
                                        uncompressed_size: placement.uncompressed_size,
                                    }
                        })
                });
                let matched = candidates.placements.len();
                let verified = self.verify_capsule_chunk_candidates(candidates).await?;
                Ok::<_, CrabError>((verified, indexed, matched))
            })
            .await
            .map_err(|_| {
                CrabError::NetworkTransient(object_store::Error::Generic {
                    store: "capsule chunk index",
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "staged capsule-proof revalidation budget exhausted",
                    )),
                })
            })??;
        debug!(
            planned = planned.len(),
            indexed = capsule_indexed,
            matched = capsule_matched,
            capsule_source_verified = capsule.candidates.len(),
            payload_verified = capsule.candidates.len(),
            "revalidated staged remote placement proofs"
        );
        Ok(capsule)
    }

    pub(super) async fn lookup_capsule_chunks_for_add(
        &self,
        hashes: &[MerkleHash],
        index: &crab_metadata::capsule_protocol::CapsuleChunkIndex,
    ) -> Result<HashMap<MerkleHash, crab_staging::push_plan::ExistingChunkCandidate>> {
        let store = &self.store;
        let candidates = index
            .candidates(
                store.as_storage(),
                hashes,
                crab_metadata::capsule_protocol::MAX_CATALOG_BYTES,
            )
            .await?;
        self.verify_capsule_chunk_candidates(candidates)
            .await
            .map(|verified| verified.candidates)
    }

    async fn verify_capsule_chunk_candidates(
        &self,
        candidates: crab_metadata::capsule_protocol::CapsuleChunkCandidates,
    ) -> Result<VerifiedRemoteCandidates> {
        #[derive(Default)]
        struct SourceCandidates<'a> {
            shards: HashSet<&'a str>,
            placements: Vec<&'a crab_metadata::receipts::CommittedChunkPlacement>,
        }
        let store = &self.store;
        let mut by_repo: BTreeMap<&str, SourceCandidates<'_>> = BTreeMap::new();
        for placement in candidates.placements.values() {
            if let Some(anchor) = candidates.anchors.get(&placement.source_anchor_id) {
                let source = by_repo.entry(&anchor.source_repo_prefix).or_default();
                source.shards.insert(&anchor.source_shard_hash);
                source.placements.push(placement);
            }
        }
        let mut refs = HashMap::new();
        let mut origins = HashMap::new();
        let mut proofs = HashMap::new();
        let mut bindings: HashMap<
            MerkleHash,
            (
                crab_metadata::capsule_protocol::XorbCatalogEntry,
                crab_metadata::receipts::OriginReceipt,
            ),
        > = HashMap::new();
        let mut retained_bytes = 0_u64;
        for (repo, source) in by_repo {
            check_cancelled(&self.cancel)?;
            let layout = crab_storage::StoreLayout::with_global_prefix(
                store.as_storage().clone(),
                repo.to_owned(),
                self.router.global_prefix().to_owned(),
            );
            let root = match crab_metadata::capsule_protocol::load_root(&layout).await {
                Ok(root) => root,
                Err(crab_metadata::error::MetadataError::Storage {
                    source: crab_storage::StorageError::NotFound { .. },
                }) => continue,
                Err(error) => return Err(error.into()),
            };
            let lookup = crab_metadata::capsule_protocol::load_pointer_catalog_lookup_from_root(
                &layout, &root,
            )
            .await?;
            let hashes = source
                .shards
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            // Validate and release one source closure before opening the next;
            // otherwise each source could retain its entire format byte budget.
            let catalog = lookup.shards(layout.store(), &hashes).await?;
            for placement in source.placements {
                let Some(anchor) = candidates.anchors.get(&placement.source_anchor_id) else {
                    continue;
                };
                let xorb_hash = MerkleHash::from(placement.xorb_hash);
                let path = self.router.xorb_path(&xorb_hash);
                if anchor
                    .validate_placement(placement, &catalog, path.as_ref())
                    .is_err()
                {
                    continue;
                }
                let entry = catalog.xorbs().get(&xorb_hash.hex()).ok_or_else(|| {
                    CrabError::Internal(
                        "validated capsule placement lost its descriptor".to_owned(),
                    )
                })?;
                if let Some((prior, origin)) = bindings.get(&xorb_hash) {
                    // Observation time may differ; physical assertions for the
                    // same immutable xorb cannot disagree or certify one another.
                    if prior != entry
                        || origin.etag != anchor.origin.etag
                        || origin.object_version != anchor.origin.object_version
                        || origin.payload_digest != anchor.origin.payload_digest
                        || origin.size != anchor.origin.size
                    {
                        return Err(CrabError::CorruptObject {
                            path: path.to_string(),
                            reason: "conflicting canonical xorb assertions".to_owned(),
                        });
                    }
                } else {
                    let metadata_bytes = std::mem::size_of_val(entry.chunks()) as u64
                        + entry
                            .chunks()
                            .iter()
                            .map(|chunk| chunk.hash().len() as u64)
                            .sum::<u64>()
                        + entry.body_digest().len() as u64
                        + serde_json::to_vec(&anchor.origin)
                            .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
                            .len() as u64;
                    retained_bytes =
                        retained_bytes.checked_add(metadata_bytes).ok_or_else(|| {
                            CrabError::Internal("selected xorb metadata size overflows".to_owned())
                        })?;
                    if retained_bytes > crab_metadata::capsule_protocol::MAX_CATALOG_BYTES {
                        return Err(CrabError::CorruptObject {
                            path: "capsule chunk candidates".to_owned(),
                            reason: "selected xorb metadata exceeds the catalog byte limit"
                                .to_owned(),
                        });
                    }
                    bindings.insert(xorb_hash, (entry.clone(), anchor.origin.clone()));
                }
                let hash = MerkleHash::from(placement.chunk_hash);
                refs.insert(
                    hash,
                    XorbRef {
                        xorb_hash,
                        chunk_index: placement.chunk_index,
                        uncompressed_size: placement.uncompressed_size,
                    },
                );
                origins.insert(xorb_hash, anchor.origin.clone());
                proofs.insert(hash, (placement.placement_id(), placement.origin_proof_id));
            }
        }
        // Source catalogs establish commitment. Receipts belong to the trusted
        // scope's canonical global index; fresh provider identity binds their
        // verified bytes, while cache or staged IDs alone cannot certify reuse.
        let verified = self.verify_origins(&refs, &origins).await?;
        let accepted_xorbs = verified
            .values()
            .map(|reference| reference.xorb_hash)
            .collect::<HashSet<_>>();
        bindings.retain(|hash, _| accepted_xorbs.contains(hash));
        let candidates = verified
            .into_iter()
            .filter_map(|(hash, xorb_ref)| {
                let (placement_id, origin_proof_id) = proofs.get(&hash).copied()?;
                Some((
                    hash,
                    crab_staging::push_plan::ExistingChunkCandidate {
                        xorb_ref,
                        placement_id,
                        origin_proof_id,
                    },
                ))
            })
            .collect();
        Ok(VerifiedRemoteCandidates {
            candidates,
            xorbs: bindings,
        })
    }
    pub(super) async fn verify_origins(
        &self,
        refs: &HashMap<MerkleHash, XorbRef>,
        origins: &HashMap<MerkleHash, crab_metadata::receipts::OriginReceipt>,
    ) -> Result<HashMap<MerkleHash, XorbRef>> {
        let mut by_xorb = HashMap::<MerkleHash, Vec<(MerkleHash, XorbRef)>>::new();
        for (hash, reference) in refs {
            by_xorb
                .entry(reference.xorb_hash)
                .or_default()
                .push((*hash, *reference));
        }
        let cache = self
            .caching_store
            .as_ref()
            .map(|store| Arc::clone(store.local_cache()));
        let results = futures_util::stream::iter(by_xorb.into_iter().map(|(hash, entries)| {
            let cache = cache.clone();
            async move {
                check_cancelled(&self.cancel)?;
                let path = self.router.xorb_path(&hash);
                let meta = match self.store.head(&path).await {
                    Ok(meta) => meta,
                    Err(CrabError::NotFound { .. }) => return Ok(Vec::new()),
                    Err(error) => return Err(error),
                };
                let cached = cache
                    .as_deref()
                    .and_then(|cache| cached_remote_xorb_index_matches(cache, &hash, &meta));
                let index = match cached {
                    Some(index) => index,
                    None => match remote_xorb_index(&self.store, &path, meta.size).await {
                        Ok(index) if index.hash == hash => {
                            if let Some(cache) = cache.as_deref() {
                                record_remote_xorb_index(cache, &hash, &meta, &index);
                            }
                            index
                        }
                        Ok(_) | Err(CrabError::CorruptObject { .. }) => return Ok(Vec::new()),
                        Err(error) => return Err(error),
                    },
                };
                // Cached validity cannot certify a replacement. The rooted
                // assertion must bind this exact fresh provider identity before
                // any selected placement becomes reconstruction authority.
                if !origins.get(&hash).is_some_and(|origin| {
                    committed_origin_receipt_matches_remote(origin, &hash, &path, &index, &meta)
                }) {
                    return Ok(Vec::new());
                }
                let accepted = entries
                    .into_iter()
                    .filter(|(chunk, reference)| {
                        usize::try_from(reference.chunk_index)
                            .ok()
                            .and_then(|index_value| index.chunks.get(index_value))
                            .is_some_and(|metadata| {
                                metadata.hash == *chunk
                                    && metadata.uncompressed_len == reference.uncompressed_size
                            })
                    })
                    .collect::<Vec<_>>();
                if !accepted.is_empty()
                    && let Some(cache) = cache.as_deref()
                {
                    record_remote_xorb_payload_proof(cache, &hash, &index, &meta);
                }
                Ok(accepted)
            }
        }))
        .buffer_unordered(self.head_check_concurrency.max(1))
        .collect::<Vec<Result<Vec<(MerkleHash, XorbRef)>>>>()
        .await;
        check_cancelled(&self.cancel)?;
        let mut verified = HashMap::new();
        for result in results {
            verified.extend(result?);
        }
        Ok(verified)
    }
}

#[derive(Debug, Default)]
pub(crate) struct VerifiedRemoteCandidates {
    pub candidates: HashMap<MerkleHash, crab_staging::push_plan::ExistingChunkCandidate>,
    pub xorbs: HashMap<
        MerkleHash,
        (
            crab_metadata::capsule_protocol::XorbCatalogEntry,
            crab_metadata::receipts::OriginReceipt,
        ),
    >,
}
impl VerifiedRemoteCandidates {
    pub(super) fn into_refs(self) -> HashMap<MerkleHash, XorbRef> {
        self.candidates
            .into_iter()
            .map(|(hash, candidate)| (hash, candidate.xorb_ref))
            .collect()
    }
}

/// Revalidate staged placement IDs against current Capsule roots and origins.
pub(crate) async fn revalidate_staged_remote_chunks(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    planned: &HashMap<MerkleHash, crab_staging::push_plan::ExistingChunkCandidate>,
    cancel: &CancellationToken,
) -> Result<VerifiedRemoteCandidates> {
    if planned.is_empty() {
        return Ok(VerifiedRemoteCandidates::default());
    }
    check_cancelled(cancel)?;
    let store = Store::from_storage(layout.store().clone());
    let router = StoreLayout::with_global_prefix(
        store.clone(),
        layout.repo_prefix().to_owned(),
        layout.global_prefix().to_owned(),
    );
    let verifier = ChunkVerifier::new(
        DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY,
        store,
        None,
        router,
        cancel.clone(),
    );
    let verified = Box::pin(verifier.revalidate(planned)).await?;
    check_cancelled(cancel)?;
    Ok(verified)
}

/// Classify add-time reuse through current Capsule and origin proofs.
pub(crate) struct AddRemoteChunkClassifier {
    verifier: ChunkVerifier,
    remote_chunk_index:
        tokio::sync::OnceCell<Option<crab_metadata::capsule_protocol::CapsuleChunkIndex>>,
    candidate_cache: Option<Arc<crate::cache::add_remote_candidates::AddRemoteCandidateCache>>,
    candidate_cache_hits: std::sync::atomic::AtomicU64,
    candidate_cache_misses: std::sync::atomic::AtomicU64,
}

impl AddRemoteChunkClassifier {
    #[must_use]
    pub(crate) fn new(
        head_check_concurrency: usize,
        store: Store,
        caching_store: Option<crab_cache_store::CachingStore>,
        router: StoreLayout,
        cancel: CancellationToken,
    ) -> Self {
        let verifier = ChunkVerifier::new(
            head_check_concurrency,
            store.clone(),
            caching_store,
            router.clone(),
            cancel,
        );
        let cache_path = crate::cache::add_remote_candidate_cache_path(
            &crate::cache::default_cache_root(),
            &store.bucket_identity(),
            router.global_prefix(),
        );
        let candidate_cache =
            match crate::cache::add_remote_candidates::AddRemoteCandidateCache::open(&cache_path) {
                Ok(cache) => Some(Arc::new(cache)),
                Err(error) => {
                    warn!(
                        error = %error,
                        path = %cache_path.display(),
                        "add remote candidate cache unavailable; using remote proof lookup"
                    );
                    None
                }
            };
        Self {
            verifier,
            remote_chunk_index: tokio::sync::OnceCell::new(),
            candidate_cache,
            candidate_cache_hits: std::sync::atomic::AtomicU64::new(0),
            candidate_cache_misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(crate) fn report_cache_stats(&self) {
        let cache_hits = self
            .candidate_cache_hits
            .load(std::sync::atomic::Ordering::Relaxed);
        let cache_misses = self
            .candidate_cache_misses
            .load(std::sync::atomic::Ordering::Relaxed);
        if cache_hits > 0 || cache_misses > 0 {
            debug!(
                cache_hits,
                cache_misses, "add remote candidate cache summary"
            );
        }
    }

    #[must_use]
    pub(crate) fn shard_hint_scope(&self) -> crate::cache::shard_hints::ShardHintScope {
        crate::cache::shard_hints::ShardHintScope::new(
            &self.verifier.router.store().bucket_identity(),
            self.verifier.router.global_prefix(),
        )
    }
    #[cfg(test)]
    pub(super) fn with_test_cache(
        store: Store,
        router: StoreLayout,
        candidate_cache: Option<Arc<crate::cache::add_remote_candidates::AddRemoteCandidateCache>>,
    ) -> Self {
        Self {
            verifier: ChunkVerifier::new(
                DEFAULT_PUSH_HEAD_CHECK_CONCURRENCY,
                store,
                None,
                router,
                CancellationToken::new(),
            ),
            remote_chunk_index: tokio::sync::OnceCell::new(),
            candidate_cache,
            candidate_cache_hits: std::sync::atomic::AtomicU64::new(0),
            candidate_cache_misses: std::sync::atomic::AtomicU64::new(0),
        }
    }
    #[cfg(test)]
    pub(super) fn verifier_for_test(&self) -> &ChunkVerifier {
        &self.verifier
    }
}

#[async_trait::async_trait]
impl crab_staging::push_plan::ExistingChunkLookup for AddRemoteChunkClassifier {
    async fn lookup_existing_candidates(
        &self,
        chunks: &[(MerkleHash, u64)],
    ) -> crab_staging::Result<Vec<Option<crab_staging::push_plan::ExistingChunkCandidate>>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let initial_capacity = chunks.len().min(ADD_REMOTE_CLASSIFIER_INITIAL_CAPACITY);
        let mut unique_chunks = Vec::with_capacity(initial_capacity);
        let mut seen = HashSet::with_capacity(initial_capacity);
        for (chunk_hash, _) in chunks {
            if seen.insert(*chunk_hash) {
                unique_chunks.push(*chunk_hash);
            }
        }
        drop(seen);

        // Most first pushes are all misses; grow only when a positive candidate
        // exists instead of reserving buckets for the whole repository.
        let mut candidates = HashMap::new();
        let mut misses = Vec::with_capacity(unique_chunks.len());
        if let Some(cache) = &self.candidate_cache {
            match cache.memory_get_batch(&unique_chunks) {
                Ok(cached) => {
                    for chunk_hash in &unique_chunks {
                        match cached.get(chunk_hash).copied() {
                            Some(Some(candidate)) => {
                                self.candidate_cache_hits
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                candidates.insert(*chunk_hash, candidate);
                            }
                            Some(None) => {
                                self.candidate_cache_hits
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            None => misses.push(*chunk_hash),
                        }
                    }
                }
                Err(error) => {
                    warn!(error = %error, "add remote candidate memory cache lookup failed");
                    misses = unique_chunks;
                }
            }
        } else {
            misses = unique_chunks;
        }

        if !misses.is_empty()
            && let Some(cache) = &self.candidate_cache
        {
            let mut remote_misses = Vec::new();
            let lookup_hashes = Arc::new(std::mem::take(&mut misses));
            let persistent_lookup = {
                let cache = Arc::clone(cache);
                let hashes = Arc::clone(&lookup_hashes);
                tokio::task::spawn_blocking(move || cache.load_persistent(hashes.as_slice())).await
            };
            match persistent_lookup {
                Ok(Ok(persisted)) => {
                    if persisted.is_empty() {
                        // Preserve the cold-cache all-miss fast path without
                        // reserving this repository-sized buffer for warm hits.
                        remote_misses.reserve(lookup_hashes.len());
                    }
                    for &chunk_hash in lookup_hashes.iter() {
                        match persisted.get(&chunk_hash).copied() {
                            Some(Some(candidate)) => {
                                self.candidate_cache_hits
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                candidates.insert(chunk_hash, candidate);
                            }
                            Some(None) => {
                                self.candidate_cache_hits
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                // A persisted negative is advisory: it only avoids
                                // a duplicate lookup, and push still revalidates
                                // every candidate it does receive.
                            }
                            None => remote_misses.push(chunk_hash),
                        }
                    }
                }
                Ok(Err(error)) => {
                    warn!(error = %error, "add remote candidate persistent cache lookup failed");
                    remote_misses.extend(lookup_hashes.iter().copied());
                }
                Err(error) => {
                    warn!(error = %error, "add remote candidate persistent cache task failed");
                    remote_misses.extend(lookup_hashes.iter().copied());
                }
            }
            misses = remote_misses;
        }

        self.candidate_cache_misses
            .fetch_add(misses.len() as u64, std::sync::atomic::Ordering::Relaxed);
        if !misses.is_empty() {
            let fetched = async {
                check_cancelled(&self.verifier.cancel)?;
                let chunk_index = self
                    .remote_chunk_index
                    .get_or_try_init(|| async {
                        let store = &self.verifier.store;
                        let layout = crab_storage::StoreLayout::with_global_prefix(
                            store.as_storage().clone(),
                            self.verifier.router.repo_prefix().to_owned(),
                            self.verifier.router.global_prefix().to_owned(),
                        );
                        let index =
                            crab_metadata::capsule_protocol::CapsuleChunkIndex::new(&layout);
                        Ok::<_, CrabError>(
                            index
                                .available(
                                    layout.store(),
                                    crab_metadata::capsule_protocol::MAX_CATALOG_BYTES,
                                )
                                .await?
                                .then_some(index),
                        )
                    })
                    .await?;
                check_cancelled(&self.verifier.cancel)?;
                // An absent index cannot supply remote candidates during this add.
                // Packing locally remains correct even if another writer creates
                // the index later; transient open failures are still retried.
                if let Some(chunk_index) = chunk_index {
                    tokio::time::timeout(
                        GLOBAL_CHUNK_LOOKUP_BUDGET,
                        self.verifier
                            .lookup_capsule_chunks_for_add(&misses, chunk_index),
                    )
                    .await
                    .map_err(|_| {
                        CrabError::NetworkTransient(object_store::Error::Generic {
                            store: "capsule chunk index",
                            source: Box::new(std::io::Error::new(
                                std::io::ErrorKind::TimedOut,
                                "add remote classification proof budget exhausted",
                            )),
                        })
                    })?
                } else {
                    Ok(HashMap::new())
                }
            }
            .await
            .map_err(|error| match error {
                CrabError::Cancelled => crab_staging::StagingError::Cancelled,
                error => crab_staging::StagingError::Io(std::io::Error::other(error)),
            })?;
            // Keep index absence in the session's OnceCell: persisting one miss
            // per chunk makes cache writes scale with the entire new file.
            let cache = self
                .candidate_cache
                .as_ref()
                .filter(|_| matches!(self.remote_chunk_index.get(), Some(Some(_))));
            let mut updates = Vec::with_capacity(misses.len());
            for chunk_hash in misses {
                let candidate = fetched.get(&chunk_hash).copied();
                if cache.is_some() {
                    updates.push((chunk_hash, candidate));
                }
                if let Some(candidate) = candidate {
                    candidates.insert(chunk_hash, candidate);
                }
            }
            if let Some(cache) = cache {
                if let Err(error) = cache.memory_insert_batch(&updates) {
                    warn!(error = %error, "add remote candidate memory cache update failed");
                }
                let cache = Arc::clone(cache);
                match tokio::task::spawn_blocking(move || cache.persist_unique_results(&updates))
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        warn!(error = %error, "add remote candidate persistent cache update failed");
                    }
                    Err(error) => {
                        warn!(error = %error, "add remote candidate persistent cache task failed");
                    }
                }
            }
        }
        Ok(chunks
            .iter()
            .map(|(chunk_hash, size)| {
                candidates
                    .get(chunk_hash)
                    .copied()
                    .filter(|candidate| u64::from(candidate.xorb_ref.uncompressed_size) == *size)
            })
            .collect())
    }
}
