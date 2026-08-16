//! Global `chunk_index_db`: three-tier cache-first committed candidates.
//!
//! The store wraps one remote [`Db`] (the globally shared
//! `chunk_index_db`) plus local cache tiers — in-memory `ChunkIndex`
//! and, when available, on-disk `PersistentChunkIndex`. Every `get`
//! runs cache-first; remote hits warm all available local tiers so the
//! next lookup for the same chunk short-circuits in nanoseconds.
//!
//! Immutable receipt history remains the repair source. A rebuildable
//! point-readable head per chunk avoids one remote prefix scan per cache miss;
//! every returned candidate still needs manifest, GC-root, and origin proof.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;
use crate::metadata::metadb::db::Db;
use crate::metadata::metadb::transaction::{DbTarget, Transaction};
use bytes::Bytes;
use crab_metadata::chunk_index::ChunkIndex;
use crab_metadata::key_codec::{
    PREFIX_CONTENT, decode_content_key, encode_committed_chunk_head_key,
    encode_committed_chunk_key, encode_content_key, encode_origin_proof_key,
    encode_source_anchor_key,
};
use crab_metadata::persistent_chunk_index::PersistentChunkIndex;
use crab_metadata::receipts::{
    CommittedChunkPlacement, CommittedChunkReceipt, OriginReceipt, SourceAnchor,
};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::format::XorbRef;

/// Logical database label used in error payloads and structured logs.
pub(crate) const DB_LABEL: &str = "chunk_index_db";
// Keep large add/push dedup lookups from materializing every cache miss
// as one SQLite or SlateDB request while preserving batch amortization.
const GET_BATCH_TIER_CHUNK_LIMIT: usize = 4096;

/// Compact remote candidates with shared proof records retained once.
#[derive(Debug, Default)]
pub(crate) struct CommittedChunkCandidateBatch {
    pub(crate) placements: HashMap<MerkleHash, CommittedChunkPlacement>,
    pub(crate) origin_proofs: HashMap<[u8; 32], OriginReceipt>,
    pub(crate) source_anchors: HashMap<[u8; 32], SourceAnchor>,
}

/// Cheap-cloneable three-tier owning accessor over the global
/// `chunk_index_db`.
///
/// Constructed by [`MetaDb::chunk_index`] once the remote [`Db`]
/// handle and both local cache tiers have been initialised.
///
/// [`MetaDb::chunk_index`]: crate::metadata::metadb::MetaDb::chunk_index
#[derive(Clone)]
pub struct ChunkIndexStore {
    /// Remote SlateDB. The only place the global chunk_index_db lives.
    db: Arc<Db>,

    /// In-memory hot tier. The existing `ChunkIndex` is synchronous,
    /// so it lives behind a blocking `Mutex`.
    memory: Arc<Mutex<ChunkIndex>>,

    /// On-disk warm tier. Optional so diagnostics and degraded cache
    /// opens can still use memory + remote SlateDB.
    persistent: Option<Arc<PersistentChunkIndex>>,

    /// Optional metrics sink. When populated, local-cache counters
    /// (hit / miss / lazy_fill) are bumped for each `get` /
    /// `get_batch` call. The remote `Db` owns its own metrics hook
    /// for the SlateDB-level counters.
    metrics: Option<Arc<Metrics>>,
}

impl ChunkIndexStore {
    /// Wrap session-owned tiers for point-only access.
    pub fn new(
        db: Arc<Db>,
        memory: Arc<Mutex<ChunkIndex>>,
        persistent: Arc<PersistentChunkIndex>,
    ) -> Self {
        Self::new_with_optional_persistent(db, memory, Some(persistent))
    }

    /// Wrap session-owned tiers when the on-disk tier is unavailable.
    pub(crate) fn new_with_optional_persistent(
        db: Arc<Db>,
        memory: Arc<Mutex<ChunkIndex>>,
        persistent: Option<Arc<PersistentChunkIndex>>,
    ) -> Self {
        Self {
            db,
            memory,
            persistent,
            metrics: None,
        }
    }

    /// Attach a metrics sink to the store for cache-tier counters.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Look up a single chunk hash through all three tiers.
    ///
    /// Cache-first: in-memory → persistent, when available → remote.
    /// Remote hits warm all available local tiers before returning.
    /// A lock-poisoned in-memory
    /// mutex is mapped to [`CrabError::Internal`] so the caller can
    /// surface it rather than silently missing.
    pub async fn get(&self, chunk_hash: &MerkleHash) -> Result<Option<XorbRef>> {
        self.get_batch(std::slice::from_ref(chunk_hash))
            .await
            .map(|mut values| values.pop().flatten())
    }

    /// Batch lookup with the same three-tier flow.
    ///
    /// Passes:
    ///   1. In-memory mirror — every input.
    ///   2. Persistent on-disk index, when available — remaining misses;
    ///      warms in-memory on hit.
    ///   3. Committed receipt-prefix scans for the final misses; warms both
    ///      local tiers with candidate placements.
    ///
    /// Returns a `Vec` aligned with the input.
    pub async fn get_batch(&self, chunk_hashes: &[MerkleHash]) -> Result<Vec<Option<XorbRef>>> {
        self.get_batch_with_candidates(chunk_hashes)
            .await
            .map(|(placements, _)| placements)
    }

    /// Batch candidate lookup plus compact shared remote proof records.
    pub(crate) async fn get_batch_with_candidates(
        &self,
        chunk_hashes: &[MerkleHash],
    ) -> Result<(Vec<Option<XorbRef>>, CommittedChunkCandidateBatch)> {
        let (placements, candidates, _) = self
            .get_batch_with_candidates_inner(chunk_hashes, None)
            .await?;
        Ok((placements, candidates))
    }

    /// Resolve local candidates first, then consult remote receipt heads only
    /// when the remaining miss set fits the caller's proof budget.
    pub(crate) async fn get_batch_with_candidates_bounded(
        &self,
        chunk_hashes: &[MerkleHash],
        remote_candidate_limit: usize,
    ) -> Result<(Vec<Option<XorbRef>>, CommittedChunkCandidateBatch, usize)> {
        self.get_batch_with_candidates_inner(chunk_hashes, Some(remote_candidate_limit))
            .await
    }

    async fn get_batch_with_candidates_inner(
        &self,
        chunk_hashes: &[MerkleHash],
        remote_candidate_limit: Option<usize>,
    ) -> Result<(Vec<Option<XorbRef>>, CommittedChunkCandidateBatch, usize)> {
        if chunk_hashes.is_empty() {
            return Ok((Vec::new(), CommittedChunkCandidateBatch::default(), 0));
        }

        let mut results: Vec<Option<XorbRef>> = vec![None; chunk_hashes.len()];

        // Pass 1: in-memory. Single lock acquisition covers the whole
        // input, so the batch cost is one mutex-uncontested lock not
        // N of them.
        let mut persistent_todo: Vec<usize> = Vec::new();
        let mut memory_hits: u64 = 0;
        {
            let guard = self.memory.lock().map_err(|_| {
                CrabError::Internal(String::from("in-memory chunk index mutex poisoned"))
            })?;
            for (idx, hash) in chunk_hashes.iter().enumerate() {
                if let Some(xorb_ref) = guard.get(hash) {
                    results[idx] = Some(*xorb_ref);
                    memory_hits += 1;
                } else {
                    persistent_todo.push(idx);
                }
            }
        }

        if persistent_todo.is_empty() {
            if let Some(m) = self.metrics.as_ref() {
                m.add_metadb_chunk_index_cache_hits(memory_hits);
            }
            return Ok((results, CommittedChunkCandidateBatch::default(), 0));
        }

        let mut remote_todo: Vec<usize> = Vec::new();
        let mut persistent_hit_count = 0u64;
        if let Some(persistent) = &self.persistent {
            // Pass 2: persistent on-disk index.
            for batch in persistent_todo.chunks(GET_BATCH_TIER_CHUNK_LIMIT) {
                let persistent_hashes: Vec<MerkleHash> =
                    batch.iter().map(|&idx| chunk_hashes[idx]).collect();
                let persistent_results = persistent.get_batch(&persistent_hashes)?;
                if persistent_results.len() != batch.len() {
                    return Err(CrabError::Internal(format!(
                        "persistent chunk-index batch returned {} entries for {} hashes",
                        persistent_results.len(),
                        batch.len()
                    )));
                }
                let mut persistent_hits = Vec::new();
                for (&idx, hit) in batch.iter().zip(persistent_results) {
                    let hash = chunk_hashes[idx];
                    match hit {
                        Some(xorb_ref) => {
                            results[idx] = Some(xorb_ref);
                            persistent_hit_count += 1;
                            persistent_hits.push((hash, xorb_ref));
                        }
                        None => remote_todo.push(idx),
                    }
                }
                if !persistent_hits.is_empty() {
                    insert_many(&self.memory, &persistent_hits)?;
                }
            }
        } else {
            remote_todo = persistent_todo;
        }
        let cache_hits = memory_hits + persistent_hit_count;

        if remote_todo.is_empty() {
            if let Some(m) = self.metrics.as_ref() {
                m.add_metadb_chunk_index_cache_hits(cache_hits);
            }
            return Ok((results, CommittedChunkCandidateBatch::default(), 0));
        }

        // Pass 3: generation/GC receipt candidates for remaining misses.
        // Legacy unversioned remote rows are intentionally not a read path.
        let remote_miss_count = remote_todo.len() as u64;
        if let Some(m) = self.metrics.as_ref() {
            m.add_metadb_chunk_index_cache_hits(cache_hits);
            m.add_metadb_chunk_index_cache_misses(remote_miss_count);
        }
        if remote_candidate_limit.is_some_and(|limit| remote_todo.len() > limit) {
            return Ok((
                results,
                CommittedChunkCandidateBatch::default(),
                remote_todo.len(),
            ));
        }
        let remote_hashes = remote_todo
            .iter()
            .map(|&idx| chunk_hashes[idx])
            .collect::<Vec<_>>();
        let candidates = self.get_committed_candidates_batch(&remote_hashes).await?;
        let mut lazy_fills = 0u64;
        let mut warm_entries: Vec<(MerkleHash, XorbRef)> =
            Vec::with_capacity(GET_BATCH_TIER_CHUNK_LIMIT);
        for &idx in &remote_todo {
            if let Some(placement) = candidates.placements.get(&chunk_hashes[idx]) {
                let xorb_ref = XorbRef {
                    xorb_hash: MerkleHash::from(placement.xorb_hash),
                    chunk_index: placement.chunk_index,
                    uncompressed_size: placement.uncompressed_size,
                };
                results[idx] = Some(xorb_ref);
                warm_entries.push((chunk_hashes[idx], xorb_ref));
            }

            if warm_entries.len() == GET_BATCH_TIER_CHUNK_LIMIT {
                lazy_fills += warm_entries.len() as u64;
                if let Some(persistent) = &self.persistent {
                    persistent.insert_batch(&warm_entries)?;
                }
                insert_many(&self.memory, &warm_entries)?;
                warm_entries.clear();
            }
        }
        if !warm_entries.is_empty() {
            lazy_fills += warm_entries.len() as u64;
            if let Some(persistent) = &self.persistent {
                persistent.insert_batch(&warm_entries)?;
            }
            insert_many(&self.memory, &warm_entries)?;
        }
        if lazy_fills > 0
            && let Some(m) = self.metrics.as_ref()
        {
            m.add_metadb_chunk_index_cache_lazy_fills(lazy_fills);
        }

        Ok((results, candidates, 0))
    }

    /// Read immutable committed receipts for each queried chunk hash.
    ///
    /// Receipts are returned as candidates. The push planner still validates
    /// source-manifest membership, the GC registry, and canonical-origin
    /// object state before treating one as current proof.
    pub(crate) async fn get_committed_candidates_batch(
        &self,
        chunk_hashes: &[MerkleHash],
    ) -> Result<CommittedChunkCandidateBatch> {
        let mut out = CommittedChunkCandidateBatch::default();
        if chunk_hashes.is_empty() {
            return Ok(out);
        }

        let head_keys = chunk_hashes
            .iter()
            .map(|hash| Bytes::copy_from_slice(&encode_committed_chunk_head_key(hash)))
            .collect::<Vec<_>>();
        let heads = self.db.get_batch(&head_keys).await?;
        let mut proof_ids = Vec::new();
        let mut anchor_ids = Vec::new();
        let mut seen_proofs = HashSet::new();
        let mut seen_anchors = HashSet::new();
        for (index, raw) in heads.into_iter().enumerate() {
            let placement = raw
                .map(|raw| decode_committed_placement(chunk_hashes[index], raw.as_ref(), None))
                .transpose()?;
            if let Some(placement) = placement.as_ref() {
                if seen_proofs.insert(placement.origin_proof_id) {
                    proof_ids.push(placement.origin_proof_id);
                }
                if seen_anchors.insert(placement.source_anchor_id) {
                    anchor_ids.push(placement.source_anchor_id);
                }
            }
            if let Some(placement) = placement {
                out.placements.insert(chunk_hashes[index], placement);
            }
        }

        let mut record_keys = proof_ids
            .iter()
            .map(|id| Bytes::copy_from_slice(&encode_origin_proof_key(id)))
            .collect::<Vec<_>>();
        record_keys.extend(
            anchor_ids
                .iter()
                .map(|id| Bytes::copy_from_slice(&encode_source_anchor_key(id))),
        );
        let mut record_values = self.db.get_batch(&record_keys).await?.into_iter();
        let mut proofs = HashMap::with_capacity(proof_ids.len());
        for id in proof_ids {
            let raw = record_values
                .next()
                .flatten()
                .ok_or_else(|| missing_receipt_record("origin proof", id))?;
            proofs.insert(id, decode_origin_receipt(id, raw.as_ref())?);
        }
        let mut anchors = HashMap::with_capacity(anchor_ids.len());
        for id in anchor_ids {
            let raw = record_values
                .next()
                .flatten()
                .ok_or_else(|| missing_receipt_record("source anchor", id))?;
            anchors.insert(id, decode_source_anchor(id, raw.as_ref())?);
        }

        for placement in out.placements.values() {
            if !proofs.contains_key(&placement.origin_proof_id) {
                return Err(missing_receipt_record(
                    "origin proof",
                    placement.origin_proof_id,
                ));
            }
            if !anchors.contains_key(&placement.source_anchor_id) {
                return Err(missing_receipt_record(
                    "source anchor",
                    placement.source_anchor_id,
                ));
            }
        }
        out.origin_proofs = proofs;
        out.source_anchors = anchors;
        Ok(out)
    }

    /// Persist committed chunk receipts in the transaction's v2 namespace.
    pub fn save_committed_receipts(
        &self,
        txn: &mut Transaction,
        entries: &[(MerkleHash, CommittedChunkReceipt)],
    ) -> Result<()> {
        let mut heads: HashMap<MerkleHash, (u64, CommittedChunkPlacement)> = HashMap::new();
        let mut persisted_proofs = HashSet::new();
        let mut persisted_anchors = HashSet::new();
        for (chunk_hash, receipt) in entries {
            if MerkleHash::from(receipt.chunk_hash) != *chunk_hash {
                return Err(CrabError::Internal(
                    "committed chunk receipt hash does not match transaction key".to_owned(),
                ));
            }
            receipt
                .validate(receipt.committed_generation, receipt.shard_index_hash)
                .map_err(CrabError::from)?;
            let source = receipt.source_anchor();
            let placement = receipt.compact_placement();
            let proof_id = placement.origin_proof_id;
            let anchor_id = placement.source_anchor_id;
            if persisted_proofs.insert(proof_id) {
                let value = serde_json::to_vec(&receipt.origin).map_err(|error| {
                    CrabError::Internal(format!("origin proof serialize failed: {error}"))
                })?;
                txn.put(
                    DbTarget::ChunkIndex,
                    Bytes::copy_from_slice(&encode_origin_proof_key(&proof_id)),
                    Bytes::from(value),
                );
            }
            if persisted_anchors.insert(anchor_id) {
                let value = serde_json::to_vec(&source).map_err(|error| {
                    CrabError::Internal(format!("source anchor serialize failed: {error}"))
                })?;
                txn.put(
                    DbTarget::ChunkIndex,
                    Bytes::copy_from_slice(&encode_source_anchor_key(&anchor_id)),
                    Bytes::from(value),
                );
            }
            let key = encode_committed_chunk_key(chunk_hash, &placement.placement_id());
            let value = placement.encode().map_err(CrabError::from)?;
            txn.put(
                DbTarget::ChunkIndex,
                Bytes::copy_from_slice(&key),
                Bytes::copy_from_slice(&value),
            );
            let replace = heads.get(chunk_hash).is_none_or(|(generation, prior)| {
                receipt
                    .committed_generation
                    .cmp(generation)
                    .then_with(|| placement.placement_id().cmp(&prior.placement_id()))
                    .is_gt()
            });
            if replace {
                heads.insert(*chunk_hash, (receipt.committed_generation, placement));
            }
        }
        for (chunk_hash, (_, placement)) in heads {
            let value = placement.encode().map_err(CrabError::from)?;
            txn.put(
                DbTarget::ChunkIndex,
                Bytes::copy_from_slice(&encode_committed_chunk_head_key(&chunk_hash)),
                Bytes::copy_from_slice(&value),
            );
        }
        Ok(())
    }

    /// Tombstone selected immutable receipt rows after their proof is stale.
    pub fn delete_committed_receipts(
        &self,
        txn: &mut Transaction,
        entries: &[(MerkleHash, [u8; 32])],
    ) {
        for (chunk_hash, receipt_id) in entries {
            txn.delete(
                DbTarget::ChunkIndex,
                Bytes::copy_from_slice(&encode_committed_chunk_key(chunk_hash, receipt_id)),
            );
            txn.delete(
                DbTarget::ChunkIndex,
                Bytes::copy_from_slice(&encode_committed_chunk_head_key(chunk_hash)),
            );
        }
    }

    /// Record a delete into the transaction.
    pub(crate) fn delete_legacy(&self, txn: &mut Transaction, chunk_hash: &MerkleHash) {
        let key = Bytes::copy_from_slice(&encode_content_key(chunk_hash));
        txn.delete(DbTarget::ChunkIndex, key);
    }

    /// Return one bounded batch of legacy unversioned keys.
    pub(crate) async fn legacy_keys_batch(&self, limit: usize) -> Result<Vec<Bytes>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut rows = self.db.scan_prefix(&[PREFIX_CONTENT]).await?;
        let mut keys = Vec::with_capacity(limit);
        while keys.len() < limit {
            let Some(row) = rows.next().await.map_err(|source| {
                CrabError::from(crate::core::error::MetaDbError::Read {
                    db: DB_LABEL.to_owned(),
                    prefix: String::from("<legacy-content>"),
                    source,
                })
            })?
            else {
                break;
            };
            decode_content_key(&row.key)
                .map_err(|error| super::map_value_codec_error(error, DB_LABEL, &row.key))?;
            keys.push(row.key);
        }
        Ok(keys)
    }

    /// Tombstone a bounded legacy-key batch after a complete rebuild.
    pub(crate) fn delete_legacy_keys(&self, txn: &mut Transaction, keys: &[Bytes]) {
        for key in keys {
            txn.delete(DbTarget::ChunkIndex, key.clone());
        }
    }

    /// Evict stale candidates from both local acceleration tiers.
    pub fn remove_local_candidates(&self, chunk_hashes: &[MerkleHash]) -> Result<()> {
        if let Some(persistent) = &self.persistent {
            persistent.remove_batch(chunk_hashes)?;
        }
        let mut memory = self.memory.lock().map_err(|_| {
            CrabError::Internal(String::from("in-memory chunk index mutex poisoned"))
        })?;
        for chunk_hash in chunk_hashes {
            memory.remove(chunk_hash);
        }
        Ok(())
    }

    /// Warm local cache tiers with a freshly-uploaded shard.
    ///
    /// Called by the push pipeline in step 9b in parallel with the
    /// remote SlateDB commit: once the shard is durable on S3, its
    /// new chunks are registered in the on-disk `PersistentChunkIndex`
    /// and the in-memory `ChunkIndex` under the shard-hash marker so
    /// the next classify_chunks round treats them as class-A without
    /// any remote reads.
    ///
    /// Failures are surfaced via [`Result`] so the caller can log
    /// them at `warn!` without aborting the push — remote state is
    /// authoritative.
    pub async fn warm_local_shard(
        &self,
        shard_hash: MerkleHash,
        entries: &[(MerkleHash, XorbRef)],
    ) -> Result<()> {
        if let Some(persistent) = &self.persistent {
            persistent.install_shard(shard_hash, entries)?;
        }
        let mut guard = self.memory.lock().map_err(|_| {
            CrabError::Internal(String::from("in-memory chunk index mutex poisoned"))
        })?;
        guard.install_shard(shard_hash, entries);
        Ok(())
    }
}

/// Bulk write-through insert. Holds the mutex once for the whole
/// batch to amortise the lock overhead when warming from a fan-out.
fn decode_committed_placement(
    chunk_hash: MerkleHash,
    value: &[u8],
    expected_receipt_id: Option<[u8; 32]>,
) -> Result<CommittedChunkPlacement> {
    let placement =
        CommittedChunkPlacement::decode(value).map_err(|error| CrabError::CorruptObject {
            path: format!("{DB_LABEL}:committed-chunk"),
            reason: format!("committed chunk placement decode failed: {error}"),
        })?;
    if MerkleHash::from(placement.chunk_hash) != chunk_hash
        || expected_receipt_id.is_some_and(|expected| placement.placement_id() != expected)
    {
        return Err(CrabError::CorruptObject {
            path: format!("{DB_LABEL}:committed-chunk"),
            reason: "committed chunk key does not match its placement".to_owned(),
        });
    }
    Ok(placement)
}

fn decode_origin_receipt(id: [u8; 32], value: &[u8]) -> Result<OriginReceipt> {
    let receipt: OriginReceipt =
        serde_json::from_slice(value).map_err(|error| CrabError::CorruptObject {
            path: format!("{DB_LABEL}:origin-proof"),
            reason: format!("origin proof decode failed: {error}"),
        })?;
    if receipt.proof_id() != id {
        return Err(CrabError::CorruptObject {
            path: format!("{DB_LABEL}:origin-proof"),
            reason: "origin proof key does not match its value".to_owned(),
        });
    }
    Ok(receipt)
}

fn decode_source_anchor(id: [u8; 32], value: &[u8]) -> Result<SourceAnchor> {
    let anchor: SourceAnchor =
        serde_json::from_slice(value).map_err(|error| CrabError::CorruptObject {
            path: format!("{DB_LABEL}:source-anchor"),
            reason: format!("source anchor decode failed: {error}"),
        })?;
    anchor.validate().map_err(CrabError::from)?;
    if anchor.anchor_id() != id {
        return Err(CrabError::CorruptObject {
            path: format!("{DB_LABEL}:source-anchor"),
            reason: "source anchor key does not match its value".to_owned(),
        });
    }
    Ok(anchor)
}

fn missing_receipt_record(kind: &str, _id: [u8; 32]) -> CrabError {
    CrabError::CorruptObject {
        path: format!("{DB_LABEL}:{kind}"),
        reason: format!("compact committed chunk references missing {kind}"),
    }
}

fn insert_many(memory: &Arc<Mutex<ChunkIndex>>, entries: &[(MerkleHash, XorbRef)]) -> Result<()> {
    let mut guard = memory
        .lock()
        .map_err(|_| CrabError::Internal(String::from("in-memory chunk index mutex poisoned")))?;
    for (hash, xorb_ref) in entries {
        guard.insert(*hash, *xorb_ref);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::ObjectStore;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use tempfile::TempDir;

    use super::*;
    use crate::core::metrics::Metrics;

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

    fn committed_receipt(chunk_hash: MerkleHash, xorb_ref: XorbRef) -> CommittedChunkReceipt {
        CommittedChunkReceipt {
            schema_version: crab_metadata::receipts::RECEIPT_SCHEMA_VERSION,
            chunk_hash: chunk_hash.into(),
            xorb_hash: xorb_ref.xorb_hash.into(),
            chunk_index: xorb_ref.chunk_index,
            uncompressed_size: xorb_ref.uncompressed_size,
            origin: crab_metadata::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                format!(".crab/xorbs/{}", xorb_ref.xorb_hash.hex()),
                xorb_ref.xorb_hash.into(),
                [9; 32],
                1024,
                Some("etag".to_owned()),
                None,
            ),
            source_repo_prefix: "org/repo".to_owned(),
            source_shard_hash: hash_from_seed(88_001).into(),
            committed_generation: 1,
            shard_index_hash: hash_from_seed(88_002).into(),
            gc_registry_generation: 1,
        }
    }

    struct TestCtx {
        store: ChunkIndexStore,
        db: Arc<Db>,
        memory: Arc<Mutex<ChunkIndex>>,
        persistent: Arc<PersistentChunkIndex>,
        // Kept so the cache tempdir lives for the test.
        _cache_dir: TempDir,
    }

    async fn new_ctx() -> TestCtx {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let persistent =
            Arc::new(PersistentChunkIndex::open_or_create(&cache_path).expect("open sqlite"));
        let memory = Arc::new(Mutex::new(ChunkIndex::new()));
        let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = Arc::new(
            Db::open(backing, ObjectPath::from(".crab/chunk_index_db/"), DB_LABEL)
                .await
                .expect("open chunk_index_db"),
        );
        let store = ChunkIndexStore::new(
            Arc::clone(&db),
            Arc::clone(&memory),
            Arc::clone(&persistent),
        );
        TestCtx {
            store,
            db,
            memory,
            persistent,
            _cache_dir: cache_dir,
        }
    }

    async fn new_ctx_without_persistent() -> TestCtx {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("unused-chunk-index.sqlite");
        let persistent =
            Arc::new(PersistentChunkIndex::open_or_create(&cache_path).expect("open sqlite"));
        let memory = Arc::new(Mutex::new(ChunkIndex::new()));
        let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let db = Arc::new(
            Db::open(backing, ObjectPath::from(".crab/chunk_index_db/"), DB_LABEL)
                .await
                .expect("open chunk_index_db"),
        );
        let store = ChunkIndexStore::new_with_optional_persistent(
            Arc::clone(&db),
            Arc::clone(&memory),
            None,
        );
        TestCtx {
            store,
            db,
            memory,
            persistent,
            _cache_dir: cache_dir,
        }
    }

    async fn new_ctx_with_metrics() -> (TestCtx, Arc<Metrics>) {
        let cache_dir = TempDir::new().expect("tempdir");
        let cache_path = cache_dir.path().join("chunk-index.sqlite");
        let persistent =
            Arc::new(PersistentChunkIndex::open_or_create(&cache_path).expect("open sqlite"));
        let memory = Arc::new(Mutex::new(ChunkIndex::new()));
        let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let metrics = Arc::new(Metrics::new());
        let db = Arc::new(
            Db::open_with_metrics(
                backing,
                ObjectPath::from(".crab/chunk_index_db/"),
                DB_LABEL,
                Arc::clone(&metrics),
            )
            .await
            .expect("open chunk_index_db"),
        );
        let store = ChunkIndexStore::new(
            Arc::clone(&db),
            Arc::clone(&memory),
            Arc::clone(&persistent),
        )
        .with_metrics(Arc::clone(&metrics));
        (
            TestCtx {
                store,
                db,
                memory,
                persistent,
                _cache_dir: cache_dir,
            },
            metrics,
        )
    }

    /// Seed a chunk-index entry directly into the remote `chunk_index_db`
    /// via a raw WriteBatch. Used to simulate the "another client wrote
    /// this" scenario for three-tier fall-through tests.
    async fn seed_remote(db: &Db, chunk_hash: &MerkleHash, xorb_ref: &XorbRef) {
        let receipt = committed_receipt(*chunk_hash, *xorb_ref);
        let mut batch = slatedb::WriteBatch::new();
        put_receipt(&mut batch, chunk_hash, &receipt);
        db.write(batch).await.expect("seed remote");
    }

    fn put_receipt(
        batch: &mut slatedb::WriteBatch,
        chunk_hash: &MerkleHash,
        receipt: &CommittedChunkReceipt,
    ) {
        let source = receipt.source_anchor();
        let placement = receipt.compact_placement();
        batch.put(
            encode_origin_proof_key(&placement.origin_proof_id).as_slice(),
            serde_json::to_vec(&receipt.origin).expect("serialize origin proof"),
        );
        batch.put(
            encode_source_anchor_key(&placement.source_anchor_id).as_slice(),
            serde_json::to_vec(&source).expect("serialize source anchor"),
        );
        batch.put(
            encode_committed_chunk_key(chunk_hash, &placement.placement_id()).as_slice(),
            placement.encode().expect("serialize placement"),
        );
        batch.put(
            encode_committed_chunk_head_key(chunk_hash).as_slice(),
            placement.encode().expect("serialize placement head"),
        );
    }

    // --- three-tier get tests ---

    #[tokio::test]
    async fn chunk_index_get_returns_none_when_nothing_present() {
        let ctx = new_ctx().await;
        let got = ctx.store.get(&hash_from_seed(1)).await.expect("get");
        assert!(got.is_none());
        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_get_hits_in_memory_first_without_remote_io() {
        let ctx = new_ctx().await;
        let chunk = hash_from_seed(2);
        let value = xorb_ref_for(100, 0, 1024);

        // Seed ONLY the in-memory tier.
        {
            let mut guard = ctx.memory.lock().expect("lock");
            guard.insert(chunk, value);
        }

        let got = ctx.store.get(&chunk).await.expect("get");
        assert_eq!(got, Some(value));

        // Remote must not have been populated and must not contain
        // the entry.
        let raw = ctx.db.get(&encode_content_key(&chunk)).await.expect("raw");
        assert!(raw.is_none(), "tier-1 hit must not touch remote");

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_get_falls_through_to_persistent_and_warms_memory() {
        let ctx = new_ctx().await;
        let chunk = hash_from_seed(3);
        let value = xorb_ref_for(200, 1, 4096);

        ctx.persistent
            .insert(&chunk, &value)
            .expect("persistent insert");

        let got = ctx.store.get(&chunk).await.expect("get");
        assert_eq!(got, Some(value));

        // In-memory tier is now warm.
        {
            let guard = ctx.memory.lock().expect("lock");
            assert_eq!(guard.get(&chunk).copied(), Some(value));
        }

        // Remote still empty.
        let raw = ctx.db.get(&encode_content_key(&chunk)).await.expect("raw");
        assert!(raw.is_none(), "persistent hit must not touch remote");

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_get_falls_through_to_remote_and_warms_both_local_tiers() {
        let ctx = new_ctx().await;
        let chunk = hash_from_seed(4);
        let value = xorb_ref_for(300, 7, 16_384);

        seed_remote(&ctx.db, &chunk, &value).await;

        let got = ctx.store.get(&chunk).await.expect("get");
        assert_eq!(got, Some(value));

        // Both local tiers must now be warm.
        assert_eq!(
            ctx.persistent.get(&chunk).expect("persistent get"),
            Some(value),
            "persistent tier must be warmed by remote hit"
        );
        {
            let guard = ctx.memory.lock().expect("lock");
            assert_eq!(
                guard.get(&chunk).copied(),
                Some(value),
                "in-memory tier must be warmed by remote hit"
            );
        }

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_get_uses_remote_when_persistent_tier_is_unavailable() {
        let ctx = new_ctx_without_persistent().await;
        let chunk = hash_from_seed(40);
        let value = xorb_ref_for(304, 11, 8192);

        seed_remote(&ctx.db, &chunk, &value).await;

        let got = ctx.store.get(&chunk).await.expect("get");
        assert_eq!(got, Some(value));

        {
            let guard = ctx.memory.lock().expect("lock");
            assert_eq!(
                guard.get(&chunk).copied(),
                Some(value),
                "remote hit must still warm memory without persistent tier"
            );
        }
        assert_eq!(
            ctx.persistent.get(&chunk).expect("persistent get"),
            None,
            "unattached persistent tier must not be mutated"
        );

        ctx.db.close().await.expect("close");
    }

    // --- get_batch ---

    #[tokio::test]
    async fn chunk_index_get_batch_preserves_order_across_tiers() {
        let ctx = new_ctx().await;

        // Six slots:
        //   idx 0: miss everywhere
        //   idx 1: in-memory hit
        //   idx 2: persistent hit
        //   idx 3: remote hit
        //   idx 4: miss everywhere
        //   idx 5: remote hit
        let miss_a = hash_from_seed(10);
        let mem_hit = hash_from_seed(11);
        let persist_hit = hash_from_seed(12);
        let remote_hit_a = hash_from_seed(13);
        let miss_b = hash_from_seed(14);
        let remote_hit_b = hash_from_seed(15);

        let v_mem = xorb_ref_for(10, 0, 1);
        let v_persist = xorb_ref_for(20, 1, 2);
        let v_remote_a = xorb_ref_for(30, 2, 3);
        let v_remote_b = xorb_ref_for(40, 3, 4);

        {
            let mut guard = ctx.memory.lock().expect("lock");
            guard.insert(mem_hit, v_mem);
        }
        ctx.persistent
            .insert(&persist_hit, &v_persist)
            .expect("persistent insert");
        seed_remote(&ctx.db, &remote_hit_a, &v_remote_a).await;
        seed_remote(&ctx.db, &remote_hit_b, &v_remote_b).await;

        let query = vec![
            miss_a,
            mem_hit,
            persist_hit,
            remote_hit_a,
            miss_b,
            remote_hit_b,
        ];
        let got = ctx.store.get_batch(&query).await.expect("batch");

        assert_eq!(got.len(), query.len());
        assert_eq!(got[0], None, "miss");
        assert_eq!(got[1], Some(v_mem), "in-memory hit");
        assert_eq!(got[2], Some(v_persist), "persistent hit");
        assert_eq!(got[3], Some(v_remote_a), "remote hit");
        assert_eq!(got[4], None, "miss");
        assert_eq!(got[5], Some(v_remote_b), "remote hit");
        assert_eq!(
            ctx.persistent
                .get(&remote_hit_a)
                .expect("persistent remote hit a"),
            Some(v_remote_a)
        );
        assert_eq!(
            ctx.persistent
                .get(&remote_hit_b)
                .expect("persistent remote hit b"),
            Some(v_remote_b)
        );

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_get_batch_uses_remote_when_persistent_tier_is_unavailable() {
        let ctx = new_ctx_without_persistent().await;
        let miss = hash_from_seed(50);
        let remote = hash_from_seed(51);
        let value = xorb_ref_for(351, 3, 4096);
        seed_remote(&ctx.db, &remote, &value).await;

        let got = ctx
            .store
            .get_batch(&[miss, remote])
            .await
            .expect("get_batch");

        assert_eq!(got, vec![None, Some(value)]);
        {
            let guard = ctx.memory.lock().expect("lock");
            assert_eq!(guard.get(&remote).copied(), Some(value));
        }
        assert_eq!(ctx.persistent.get(&remote).expect("persistent get"), None);

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn bounded_lookup_resolves_persistent_hits_before_skipping_remote_misses() {
        let ctx = new_ctx().await;
        let local_hash = hash_from_seed(70_000);
        let local_ref = xorb_ref_for(80_000, 7, 8192);
        ctx.persistent
            .insert(&local_hash, &local_ref)
            .expect("persistent insert");
        let mut query = (0..257u64)
            .map(|index| hash_from_seed(90_000 + index))
            .collect::<Vec<_>>();
        query.push(local_hash);

        let (placements, candidates, skipped_remote) = ctx
            .store
            .get_batch_with_candidates_bounded(&query, 256)
            .await
            .expect("bounded lookup");

        assert_eq!(placements.last(), Some(&Some(local_ref)));
        assert!(candidates.placements.is_empty());
        assert_eq!(skipped_remote, 257);
        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn remote_candidate_batch_deduplicates_shared_proof_records() {
        let ctx = new_ctx_without_persistent().await;
        let xorb_hash = hash_from_seed(120_000);
        let entries = (0..128_u64)
            .map(|index| {
                let chunk_hash = hash_from_seed(121_000 + index);
                let xorb_ref = XorbRef {
                    xorb_hash,
                    chunk_index: index as u32,
                    uncompressed_size: 4096,
                };
                (chunk_hash, committed_receipt(chunk_hash, xorb_ref))
            })
            .collect::<Vec<_>>();
        let query = entries
            .iter()
            .map(|(chunk_hash, _)| *chunk_hash)
            .collect::<Vec<_>>();
        let mut txn = Transaction::new();
        ctx.store
            .save_committed_receipts(&mut txn, &entries)
            .expect("save receipts");
        let (_, batch) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        ctx.db.write(batch).await.expect("write receipts");

        let (_, candidates, skipped) = ctx
            .store
            .get_batch_with_candidates_bounded(&query, query.len())
            .await
            .expect("candidate batch");

        assert_eq!(skipped, 0);
        assert_eq!(candidates.placements.len(), query.len());
        assert_eq!(candidates.origin_proofs.len(), 1);
        assert_eq!(candidates.source_anchors.len(), 1);
        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_get_batch_uses_one_adaptive_remote_lookup_and_preserves_order() {
        let (ctx, metrics) = new_ctx_with_metrics().await;
        let total = GET_BATCH_TIER_CHUNK_LIMIT + 3;
        let entries: Vec<(MerkleHash, XorbRef)> = (0..total)
            .map(|idx| {
                let seed = 1_000 + idx as u64;
                (
                    hash_from_seed(seed),
                    xorb_ref_for(seed + 10_000, idx as u32, 4096),
                )
            })
            .collect();
        let mut batch = slatedb::WriteBatch::new();
        for (chunk_hash, xorb_ref) in &entries {
            let receipt = committed_receipt(*chunk_hash, *xorb_ref);
            put_receipt(&mut batch, chunk_hash, &receipt);
        }
        ctx.db.write(batch).await.expect("seed remote");

        let query: Vec<MerkleHash> = entries
            .iter()
            .rev()
            .map(|(chunk_hash, _)| *chunk_hash)
            .collect();
        let got = ctx.store.get_batch(&query).await.expect("batch");

        assert_eq!(got.len(), query.len());
        for ((_, expected), actual) in entries.iter().rev().zip(got.iter()) {
            assert_eq!(*actual, Some(*expected));
        }
        assert_eq!(
            metrics.snapshot().metadb_batch_get_count,
            2,
            "one head lookup and one proof-record lookup should cover all remote misses"
        );

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn committed_chunk_receipts_round_trip_as_remote_candidates() {
        let ctx = new_ctx().await;

        let entries: Vec<(MerkleHash, XorbRef)> = (0u64..6)
            .map(|i| {
                (
                    hash_from_seed(100 + i),
                    xorb_ref_for(500 + i, i as u32, 100),
                )
            })
            .collect();

        let mut txn = Transaction::new();
        let receipts = entries
            .iter()
            .map(|(chunk_hash, xorb_ref)| (*chunk_hash, committed_receipt(*chunk_hash, *xorb_ref)))
            .collect::<Vec<_>>();
        ctx.store
            .save_committed_receipts(&mut txn, &receipts)
            .expect("save receipts");
        let (_fi_batch, ci_batch) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        ctx.db.write(ci_batch).await.expect("write");

        for (chunk, expected) in &entries {
            assert_eq!(
                ctx.store.get(chunk).await.expect("candidate"),
                Some(*expected)
            );
            assert!(
                ctx.db
                    .get(&encode_content_key(chunk))
                    .await
                    .expect("legacy key")
                    .is_none(),
                "new writers must not recreate the unversioned namespace"
            );
            assert!(
                ctx.db
                    .get(&encode_committed_chunk_head_key(chunk))
                    .await
                    .expect("committed head")
                    .is_some(),
                "new writers must maintain the point-readable committed head"
            );
        }

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn warm_local_shard_without_persistent_tier_updates_memory() {
        let ctx = new_ctx_without_persistent().await;
        let shard = hash_from_seed(60);
        let chunk = hash_from_seed(61);
        let value = xorb_ref_for(361, 4, 2048);

        ctx.store
            .warm_local_shard(shard, &[(chunk, value)])
            .await
            .expect("warm");

        {
            let guard = ctx.memory.lock().expect("lock");
            assert_eq!(guard.get(&chunk).copied(), Some(value));
            assert!(guard.has_shard(&shard));
        }
        assert!(
            !ctx.persistent.has_shard(&shard).expect("persistent shard"),
            "unattached persistent tier must not be warmed"
        );

        ctx.db.close().await.expect("close");
    }

    #[tokio::test]
    async fn chunk_index_delete_removes_entry_via_transaction() {
        let ctx = new_ctx().await;
        let chunk = hash_from_seed(200);
        let value = xorb_ref_for(200, 0, 100);

        // Seed remote + local.
        seed_remote(&ctx.db, &chunk, &value).await;
        ctx.persistent.insert(&chunk, &value).expect("persistent");

        // Issue delete through the transaction surface.
        let mut txn = Transaction::new();
        ctx.store.delete_legacy(&mut txn, &chunk);
        let (_fi_batch, ci_batch) = crate::metadata::metadb::transaction::into_per_db_batches(txn);
        ctx.db.write(ci_batch).await.expect("write delete");

        // Remote must now miss.
        let raw = ctx.db.get(&encode_content_key(&chunk)).await.expect("get");
        assert!(raw.is_none(), "delete must remove the remote entry");

        ctx.db.close().await.expect("close");
    }
}
