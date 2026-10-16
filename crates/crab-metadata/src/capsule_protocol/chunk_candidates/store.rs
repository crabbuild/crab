//! Immutable paged capsule hints under one conditional global root.
use super::*;
use crate::pages::object::{Node, ObjectPages, Reference};
use crate::pages::{Kind, PageWork, Rows};
use bytes::Bytes;
use crab_storage::{StorageError, Store, StoreLayout};
use object_store::path::Path;
use std::collections::{BTreeMap, BTreeSet};

const ROOT_BYTES: u64 = 4096;
const READ_ATTEMPTS: usize = 3;
const WRITE_ATTEMPTS: usize = crab_storage::cas::DEFAULT_MAX_ATTEMPTS as usize;
const KIND: Kind = Kind::CapsuleChunkHints;
type Request = (MerkleHash, [u8; 33], Option<[u8; 32]>);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    version: u32,
    records: Reference,
}

/// Bounded global capsule-hint access without legacy database initialization.
///
/// Each lookup captures a current root. Hints do not authorize payload reuse;
/// callers still prove current source membership and exact physical origin.
#[derive(Debug)]
pub struct CapsuleChunkIndex {
    root: Path,
    pages: ObjectPages,
}

impl CapsuleChunkIndex {
    /// Bind page identities to one caller-selected global scope.
    #[must_use]
    pub fn new(layout: &StoreLayout<Store>) -> Self {
        Self {
            root: layout.capsule_chunk_index_path(),
            pages: ObjectPages::new(layout.capsule_chunk_pages_prefix()),
        }
    }

    /// Report an advertised root without loading its page dependencies.
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
                .records(store, &root.records, requests, &mut work)
                .await
            {
                Err(
                    error @ MetadataError::Storage {
                        source: StorageError::NotFound { .. },
                    },
                ) => {
                    let current = self.load(store, &mut work).await?;
                    if current
                        .as_ref()
                        .is_none_or(|(current, _)| current.records == root.records)
                    {
                        return Err(error);
                    }
                    // These heads are floating advisory discovery, not a pinned
                    // repository view. A superseded root can lose old pages to GC;
                    // restart the entire query without requiring writes from readers.
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

    async fn records(
        &self,
        store: &Store,
        root: &Reference,
        requests: &[Request],
        work: &mut PageWork,
    ) -> Result<CapsuleChunkCandidates> {
        let keys = requests
            .iter()
            .map(|(_, key, _)| key.to_vec())
            .collect::<Vec<_>>();
        let values = self
            .pages
            .lookup(store, Some(root), KIND, &keys, work)
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
            .pages
            .lookup(store, Some(root), KIND, &keys, work)
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
        let body = match store
            .get_with_etag_bounded(&self.root, ROOT_BYTES.min(work.maximum))
            .await
        {
            Ok(body) => body,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        work.admit_object(*blake3::hash(&body.0).as_bytes(), body.0.len() as u64)?;
        let root: Root = serde_json::from_slice(&body.0)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        if root.version != 1 {
            return Err(corrupt("capsule hint root version is unsupported"));
        }
        root.records.validate(KIND)?;
        Ok(Some((root, body.1)))
    }

    /// Authenticate a current root and every immutable page before maintenance.
    ///
    /// The caller holds global sweep admission through marking and deletion.
    /// The root digest participates in the caller's plan revalidation.
    pub async fn objects(
        &self,
        store: &Store,
        maximum_bytes: u64,
    ) -> Result<(Option<String>, Vec<(Path, u64)>)> {
        let mut work = PageWork::new(maximum_bytes, 0);
        let Some((root, _)) = self.load(store, &mut work).await? else {
            return Ok((None, Vec::new()));
        };
        let mut pending = vec![root.records.clone()];
        let mut objects = BTreeMap::new();
        while let Some((reference, node)) = self
            .pages
            .next_page(store, &mut pending, KIND, &mut work)
            .await?
        {
            if let Node::Leaf(rows) = node.as_ref() {
                for (key, value) in rows {
                    validate_row(key, value)?;
                }
            }
            objects.insert(self.pages.path(&reference.hash), u64::from(reference.bytes));
        }
        let body = serde_json::to_vec(&root)
            .map_err(|source| MetadataError::PointerCatalogRecord { source })?;
        Ok((
            Some(blake3::hash(&body).to_hex().to_string()),
            objects.into_iter().collect(),
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
        let mut work = PageWork::new(maximum, 0);
        let keys = rows.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
        for attempt in 0..WRITE_ATTEMPTS {
            if cancelled() {
                return Err(MetadataError::CapsuleHintCancelled);
            }
            let snapshot = self.load(store, &mut work).await?;
            let root = snapshot.as_ref().map(|(root, _)| &root.records);
            let previous = self
                .pages
                .lookup(store, root, KIND, &keys, &mut work)
                .await?;
            for ((key, incoming), previous) in rows.iter().zip(previous) {
                if let Some(previous) = previous {
                    validate_row(key, &previous)?;
                    if key[0] != 0x06 && previous != *incoming {
                        return Err(corrupt(
                            "immutable capsule hint identity has conflicting bytes",
                        ));
                    }
                }
            }
            let Some(records) = self
                .pages
                .edit(store, root, KIND, rows.clone(), &mut work)
                .await?
            else {
                return Err(corrupt("nonempty capsule hints produced no root"));
            };
            if root == Some(&records) {
                return Ok(());
            }
            let body = Bytes::from(
                serde_json::to_vec(&Root {
                    version: 1,
                    records,
                })
                .map_err(|source| MetadataError::PointerCatalogRecord { source })?,
            );
            if body.len() as u64 > ROOT_BYTES {
                return Err(corrupt("capsule hint root exceeds its byte bound"));
            }
            // Every edited immutable page is read-back verified by ObjectPages
            // before the conditional root transition can advertise it.
            if cancelled() {
                return Err(MetadataError::CapsuleHintCancelled);
            }
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
