//! Verified packed hint runs under one conditional global root.
use super::*;
use crate::pages::{Kind, PageCache, PageWork, Rows};
use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use object_store::path::Path;
use std::collections::{BTreeMap, BTreeSet};

mod filter;
mod migration;
mod run;
pub use migration::migrate_capsule_chunk_candidates;
use run::{Run, merge, row_value};

const ROOT_BYTES: u64 = 64 * 1024;
const LEVELS: usize = 32;
const READ_ATTEMPTS: usize = 3;
const WRITE_ATTEMPTS: usize = crab_storage::cas::DEFAULT_MAX_ATTEMPTS as usize;
const KIND: Kind = Kind::CapsuleChunkHints;
type Request = (MerkleHash, [u8; 33], Option<[u8; 32]>);

#[derive(Deserialize)]
struct Version {
    version: u32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    version: u32,
    sequence: u64,
    levels: Vec<Option<Run>>,
}

struct RunCache {
    run: Run,
    layer: usize,
    pages: PageCache,
}

impl RunCache {
    fn new(run: &Run, layer: usize) -> Self {
        Self {
            run: run.clone(),
            layer,
            pages: PageCache::default(),
        }
    }
}

impl Root {
    fn decode(body: &[u8]) -> Result<Self> {
        let version: Version = serde_json::from_slice(body)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if version.version != 2 {
            return Err(corrupt(
                "capsule hint index requires crab doctor --migrate-chunk-index",
            ));
        }
        let root: Self = serde_json::from_slice(body)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if root.sequence == 0
            || root.levels.is_empty()
            || root.levels.len() > LEVELS
            || root.levels.last().is_none_or(Option::is_none)
        {
            return Err(corrupt("capsule hint root shape is invalid"));
        }
        for (level, run) in root.levels.iter().enumerate() {
            if let Some(run) = run {
                run.validate()?;
                if run.level() != level {
                    return Err(corrupt("capsule hint run is in the wrong size tier"));
                }
            }
        }
        Ok(root)
    }

    fn encode(&self) -> Result<Bytes> {
        let bytes = serde_json::to_vec(self)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if bytes.len() as u64 > ROOT_BYTES {
            return Err(corrupt("capsule hint root exceeds its byte bound"));
        }
        Ok(Bytes::from(bytes))
    }
}

/// Bounded global hints whose immutable runs never authorize payload reuse.
#[derive(Debug)]
pub struct CapsuleChunkIndex {
    root: Path,
    prefix: Path,
}

impl CapsuleChunkIndex {
    /// Bind hint identities to one caller-selected global scope.
    #[must_use]
    pub fn new(layout: &StoreLayout<Store>) -> Self {
        Self {
            root: layout.capsule_chunk_index_path(),
            prefix: layout.capsule_chunk_pages_prefix(),
        }
    }

    /// Report an advertised root without loading its immutable dependencies.
    pub async fn available(&self, store: &Store, maximum_bytes: u64) -> Result<bool> {
        Ok(self
            .load(store, &mut PageWork::new(maximum_bytes, 0))
            .await?
            .is_some())
    }

    /// Read current chunk heads and their immutable source anchors.
    pub async fn candidates(
        &self,
        store: &Store,
        hashes: &[MerkleHash],
        maximum_bytes: u64,
    ) -> Result<CapsuleChunkCandidates> {
        admit_requests(hashes.len(), maximum_bytes)?;
        let requests = hashes
            .iter()
            .map(|hash| (*hash, capsule_chunk_head_key(&(*hash).into()), None))
            .collect::<Vec<_>>();
        self.read(store, &requests, maximum_bytes).await
    }

    /// Read exact add-time placements independently of later head replacement.
    pub async fn candidates_by_id(
        &self,
        store: &Store,
        candidates: &[(MerkleHash, [u8; 32])],
        maximum_bytes: u64,
    ) -> Result<CapsuleChunkCandidates> {
        admit_requests(candidates.len(), maximum_bytes)?;
        let requests = candidates
            .iter()
            .map(|(hash, id)| {
                (
                    *hash,
                    crate::key_codec::capsule_chunk_placement_key(id),
                    Some(*id),
                )
            })
            .collect::<Vec<_>>();
        self.read(store, &requests, maximum_bytes).await
    }

    async fn read(
        &self,
        store: &Store,
        requests: &[Request],
        maximum: u64,
    ) -> Result<CapsuleChunkCandidates> {
        if requests.is_empty() {
            return Ok(CapsuleChunkCandidates::default());
        }
        let mut work = PageWork::new(maximum, 0);
        for attempt in 0..READ_ATTEMPTS {
            let Some((root, _)) = self.load(store, &mut work).await? else {
                return Ok(CapsuleChunkCandidates::default());
            };
            match self
                .records(store, &root, requests, &mut work, attempt)
                .await
            {
                Err(
                    error @ MetadataError::Storage {
                        source: StorageError::NotFound { .. },
                    },
                ) => {
                    let current = self.load(store, &mut work).await?;
                    if current.as_ref().is_none_or(|(current, _)| current == &root) {
                        return Err(error);
                    }
                    // Floating hints may lose superseded runs to GC. Recapture
                    // the entire query; readers must never require a write grant.
                    if attempt + 1 == READ_ATTEMPTS {
                        return Err(StorageError::StateConflict {
                            path: self.root.to_string(),
                        }
                        .into());
                    }
                }
                result => return result,
            }
        }
        Err(StorageError::StateConflict {
            path: self.root.to_string(),
        }
        .into())
    }

    async fn lookup(
        &self,
        store: &Store,
        root: &Root,
        keys: &[Vec<u8>],
        caches: &mut Vec<Option<RunCache>>,
        work: &mut PageWork,
        attempt: usize,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let mut output: Vec<Option<(u64, Vec<u8>)>> = vec![None; keys.len()];
        caches.resize_with(root.levels.len(), || None);
        for (index, run) in root.levels.iter().enumerate() {
            let Some(run) = run else {
                continue;
            };
            // A root retry does not change immutable runs in unrelated tiers.
            // Keep their cache/source ID; replace both when routing changes.
            // One cache per tier retains the existing bounded working set.
            let cache =
                caches[index].get_or_insert_with(|| RunCache::new(run, attempt * LEVELS + index));
            if cache.run != *run {
                *cache = RunCache::new(run, attempt * LEVELS + index);
            }
            let path = run.path(&self.prefix);
            let possible = run
                .filter_candidates(store, &path, &cache.pages, cache.layer, keys, work)
                .await?;
            let positions = possible
                .iter()
                .enumerate()
                .filter_map(|(index, possible)| possible.then_some(index))
                .collect::<Vec<_>>();
            if positions.is_empty() {
                continue;
            }
            let selected_keys = positions
                .iter()
                .map(|index| keys[*index].clone())
                .collect::<Vec<_>>();
            let values = cache
                .pages
                .lookup(
                    store,
                    &path,
                    cache.layer,
                    0..run.bytes,
                    &run.records,
                    KIND,
                    &selected_keys,
                    work,
                )
                .await?;
            for (position, value) in positions.into_iter().zip(values) {
                let key = &keys[position];
                let selected = &mut output[position];
                let Some(value) = value else {
                    continue;
                };
                let (sequence, raw) = row_value(key, &value, root.sequence)?;
                work.admit_materialized((key.len() + value.len() + 96) as u64)?;
                if let Some((prior_sequence, prior)) = selected {
                    if (key[0] != 0x06 || *prior_sequence == sequence) && prior.as_slice() != raw {
                        return Err(corrupt("capsule hint identity has conflicting values"));
                    }
                    if *prior_sequence >= sequence {
                        continue;
                    }
                }
                *selected = Some((sequence, raw.to_vec()));
            }
        }
        Ok(output
            .into_iter()
            .map(|value| value.map(|(_, bytes)| bytes))
            .collect())
    }

    async fn records(
        &self,
        store: &Store,
        root: &Root,
        requests: &[Request],
        work: &mut PageWork,
        attempt: usize,
    ) -> Result<CapsuleChunkCandidates> {
        let mut caches = Vec::new();
        let keys = requests
            .iter()
            .map(|(_, key, _)| key.to_vec())
            .collect::<Vec<_>>();
        let values = self
            .lookup(store, root, &keys, &mut caches, work, attempt)
            .await?;
        let mut output = CapsuleChunkCandidates::default();
        let mut anchors = BTreeSet::new();
        for ((hash, _, expected), value) in requests.iter().zip(values) {
            let Some(value) = value else {
                continue;
            };
            let placement = CommittedChunkPlacement::decode(&value)?;
            if placement.chunk_hash != <[u8; 32]>::from(*hash)
                || expected.is_some_and(|id| placement.placement_id() != id)
            {
                return Err(corrupt(
                    "capsule placement differs from its requested identity",
                ));
            }
            if let Some(previous) = output
                .placements
                .insert(placement.chunk_hash, placement.clone())
                && previous != placement
            {
                return Err(corrupt(
                    "one query requests conflicting placements for a chunk",
                ));
            }
            anchors.insert(placement.source_anchor_id);
        }
        let keys = anchors
            .iter()
            .map(|id| capsule_chunk_anchor_key(id).to_vec())
            .collect::<Vec<_>>();
        let values = self
            .lookup(store, root, &keys, &mut caches, work, attempt)
            .await?;
        for (id, value) in anchors.into_iter().zip(values) {
            let value =
                value.ok_or_else(|| corrupt("capsule placement source anchor is absent"))?;
            output
                .anchors
                .insert(id, CapsuleChunkAnchor::decode(&id, &value)?);
        }
        Ok(output)
    }

    async fn load(
        &self,
        store: &Store,
        work: &mut PageWork,
    ) -> Result<Option<(Root, crab_storage::store::ETag)>> {
        let (body, etag) = match store
            .get_with_etag_bounded(&self.root, ROOT_BYTES.min(work.maximum))
            .await
        {
            Ok(body) => body,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        work.admit_object(*blake3::hash(&body).as_bytes(), body.len() as u64)?;
        Ok(Some((Root::decode(&body)?, etag)))
    }

    /// Authenticate the complete physical hint closure before GC.
    ///
    /// The caller holds global sweep admission; hints are not payload roots.
    pub async fn objects(
        &self,
        store: &Store,
        maximum_bytes: u64,
    ) -> Result<(Option<String>, Vec<(Path, u64)>)> {
        let mut work = PageWork::new(maximum_bytes, 0);
        let Some((root, _)) = self.load(store, &mut work).await? else {
            return Ok((None, Vec::new()));
        };
        let mut objects = Vec::new();
        let mut identities = BTreeMap::new();
        for run in root.levels.iter().flatten() {
            for (key, value) in run
                .read(store, &self.prefix, root.sequence, &mut work)
                .await?
            {
                let (sequence, raw) = row_value(&key, &value, root.sequence)?;
                let sequence = if key[0] == 0x06 { sequence } else { 0 };
                let digest = *blake3::hash(raw).as_bytes();
                if let Some(prior) = identities.insert((key, sequence), digest)
                    && prior != digest
                {
                    return Err(corrupt(
                        "capsule hint closure contains conflicting identities",
                    ));
                }
            }
            objects.push((run.path(&self.prefix), run.bytes));
        }
        Ok((
            Some(blake3::hash(&root.encode()?).to_hex().to_string()),
            objects,
        ))
    }

    async fn publish(
        &self,
        store: &Store,
        rows: Rows,
        maximum: u64,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        for (key, value) in &rows {
            validate_row(key, value)?;
        }
        let keys = rows.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
        let mut work = PageWork::new(maximum, 0);
        let mut caches = Vec::new();
        for attempt in 0..WRITE_ATTEMPTS {
            if cancelled() {
                return Err(MetadataError::CapsuleHintCancelled);
            }
            let snapshot = self.load(store, &mut work).await?;
            let previous = if let Some((root, _)) = &snapshot {
                self.lookup(store, root, &keys, &mut caches, &mut work, attempt)
                    .await?
            } else {
                vec![None; rows.len()]
            };
            let sequence = snapshot
                .as_ref()
                .map_or(Some(1), |(root, _)| root.sequence.checked_add(1))
                .ok_or_else(|| corrupt("capsule hint sequence overflows"))?;
            let mut versioned = BTreeMap::new();
            for ((key, value), prior) in rows.iter().zip(previous) {
                if key[0] != 0x06 && prior.as_ref().is_some_and(|prior| prior != value) {
                    return Err(corrupt(
                        "immutable capsule hint identity has conflicting bytes",
                    ));
                }
                if prior.as_ref() == Some(value) {
                    continue;
                }
                let mut encoded = sequence.to_le_bytes().to_vec();
                encoded.extend_from_slice(value);
                versioned.insert(key.clone(), encoded);
            }
            if versioned.is_empty() {
                return Ok(());
            }
            let mut levels = snapshot
                .as_ref()
                .map_or_else(Vec::new, |(root, _)| root.levels.clone());
            let (run, body) = loop {
                let (run, body) = Run::build(&versioned)?;
                let level = run.level();
                if level >= LEVELS {
                    return Err(corrupt("capsule hint run exceeds its size tiers"));
                }
                if levels.len() <= level {
                    levels.resize(level + 1, None);
                }
                let Some(prior) = levels[level].take() else {
                    break (run, body);
                };
                // Row sequences survive compaction: a large new publication must
                // not let an older head shadow a newer head in an unmerged tier.
                let prior = prior
                    .read(store, &self.prefix, sequence - 1, &mut work)
                    .await?;
                merge(&mut versioned, prior, sequence)?;
                if cancelled() {
                    return Err(MetadataError::CapsuleHintCancelled);
                }
            };
            work.admit_object(run.hash, run.bytes)?;
            if cancelled() {
                return Err(MetadataError::CapsuleHintCancelled);
            }
            store
                .put_if_absent_verified(&run.path(&self.prefix), body)
                .await?;
            let level = run.level();
            levels[level] = Some(run);
            while levels.last().is_some_and(Option::is_none) {
                levels.pop();
            }
            let root = Root {
                version: 2,
                sequence,
                levels,
            };
            // Verify the whole packed run before advertising its page references.
            // Global GC admission remains held through this root CAS by callers.
            if cancelled() {
                return Err(MetadataError::CapsuleHintCancelled);
            }
            let body = root.encode()?;
            let result = match snapshot {
                Some((_, etag)) => store.update(&self.root, body, etag).await.map(|_| ()),
                None => store.create_strict(&self.root, body).await,
            };
            match result {
                Ok(()) => return Ok(()),
                Err(StorageError::StateConflict { .. }) if attempt + 1 < WRITE_ATTEMPTS => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(StorageError::StateConflict {
            path: self.root.to_string(),
        }
        .into())
    }
}

fn admit_requests(count: usize, maximum: u64) -> Result<()> {
    if (count as u64)
        .checked_mul(std::mem::size_of::<Request>() as u64)
        .is_none_or(|bytes| bytes > maximum)
    {
        return Err(corrupt("capsule hint query exceeds its request byte bound"));
    }
    Ok(())
}

fn validate_row(key: &[u8], value: &[u8]) -> Result<()> {
    let identity: [u8; 32] = key
        .get(1..)
        .and_then(|key| key.try_into().ok())
        .ok_or_else(|| corrupt("capsule hint key length differs"))?;
    match key.first() {
        Some(0x07) => {
            CapsuleChunkAnchor::decode(&identity, value)?;
        }
        Some(0x06 | 0x08) => {
            let placement = CommittedChunkPlacement::decode(value)?;
            let expected = if key[0] == 0x06 {
                placement.chunk_hash
            } else {
                placement.placement_id()
            };
            if expected != identity {
                return Err(corrupt("capsule hint placement key differs"));
            }
        }
        _ => return Err(corrupt("capsule hint key class is unsupported")),
    }
    Ok(())
}

/// Publish rebuildable hints only after ref visibility, under global writer admission.
///
/// Existing exact placement IDs remain discoverable after head replacement.
/// A failure cannot revoke a Git publication that the caller already committed.
pub async fn write_capsule_chunk_candidates(
    layout: &StoreLayout<Store>,
    catalog: &super::super::PointerCatalog,
    origins: &HashMap<MerkleHash, OriginReceipt>,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<()> {
    if cancelled() {
        return Err(MetadataError::CapsuleHintCancelled);
    }
    let maximum = super::super::MAX_CATALOG_BYTES;
    super::super::pointer::admit_catalog_rows(std::slice::from_ref(catalog), maximum)?;
    let mut source_shards = BTreeMap::new();
    for (shard_hash, shard) in catalog.shards() {
        for xorb in shard.xorb_hashes() {
            source_shards
                .entry(xorb.as_str())
                .or_insert(shard_hash.as_str());
        }
    }
    let mut rows = BTreeMap::new();
    let mut materialized = 0_u64;
    for (hash, entry) in catalog.xorbs().iter().rev() {
        let parsed =
            MerkleHash::from_hex(hash).map_err(|_| corrupt("invalid capsule xorb hash"))?;
        let Some(origin) = origins.get(&parsed) else {
            continue;
        };
        let source = source_shards
            .get(hash.as_str())
            .ok_or_else(|| corrupt("capsule xorb candidate has no source shard root"))?;
        let incoming =
            capsule_chunk_candidate_rows(layout.repo_prefix(), source, parsed, entry, origin)?;
        for (key, value) in incoming {
            materialized = materialized
                .checked_add((key.len() + value.len() + 96) as u64)
                .ok_or_else(|| corrupt("capsule hint publication byte count overflows"))?;
            if materialized > maximum {
                return Err(corrupt("capsule hint publication exceeds its byte bound"));
            }
            rows.insert(key, value);
        }
    }
    CapsuleChunkIndex::new(layout)
        .publish(
            layout.store(),
            rows.into_iter().collect(),
            maximum,
            cancelled,
        )
        .await
}

#[cfg(test)]
mod tests;
