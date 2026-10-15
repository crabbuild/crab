//! Selected dependency closure from captured checkpoint roots and ordered deltas.
use super::*;
use crate::capsule_protocol::PackRange;
use crate::pages::{PageCache, PageWork};
use crab_storage::Store;
use std::collections::BTreeSet;

#[cfg(test)]
mod tests;

/// Snapshot-bound catalog queries with one cache and aggregate byte budget.
///
/// The owner retains the captured objects against GC. Missing keys return an
/// empty selection; missing pages and conflicting dependencies return errors.
#[derive(Debug)]
pub struct PointerCatalogLookup {
    checkpoint: Option<CheckpointSource>,
    deltas: Vec<PointerCatalog>,
    cache: PageCache,
    maximum: u64,
}

#[derive(Debug)]
struct CheckpointSource {
    path: object_store::path::Path,
    range: PackRange,
    descriptor: Descriptor,
}

impl PointerCatalogLookup {
    pub(in crate::capsule_protocol) fn checkpoint(
        path: object_store::path::Path,
        range: PackRange,
        descriptor: Descriptor,
        maximum: u64,
    ) -> Result<Self> {
        descriptor.validate(range.length())?;
        if range.offset().checked_add(range.length()).is_none() {
            return Err(corrupt("catalog source range overflows"));
        }
        Ok(Self {
            checkpoint: Some(CheckpointSource {
                path,
                range,
                descriptor,
            }),
            deltas: Vec::new(),
            cache: PageCache::default(),
            maximum,
        })
    }

    /// Capture publication-ordered deltas after the checkpoint represented here.
    ///
    /// Deltas must be the selected, deduplicated capsules of the same view.
    /// This performs no mutable-root reads or dependency expansion.
    pub fn with_deltas(mut self, deltas: Vec<PointerCatalog>) -> Result<Self> {
        for delta in &deltas {
            delta.validate(false, true)?;
        }
        self.deltas.extend(deltas);
        Ok(self)
    }

    /// Start a lookup for a captured view that has no checkpoint catalog.
    #[must_use]
    pub fn empty(maximum: u64) -> Self {
        Self {
            checkpoint: None,
            deltas: Vec::new(),
            cache: PageCache::default(),
            maximum,
        }
    }

    fn work(&self) -> PageWork {
        PageWork {
            maximum: self.maximum,
            bytes: 0,
            seen: BTreeSet::new(),
        }
    }

    async fn rows(
        &self,
        store: &Store,
        root: Option<&PageRef>,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let Some(source) = &self.checkpoint else {
            return Ok(vec![None; keys.len()]);
        };
        let Some(root) = root else {
            return Ok(vec![None; keys.len()]);
        };
        self.cache
            .lookup(
                store,
                &source.path,
                0,
                source.range.offset()..source.range.offset() + source.descriptor.body_bytes,
                root,
                kind,
                keys,
                work,
            )
            .await
    }

    async fn scan(
        &self,
        store: &Store,
        root: Option<&PageRef>,
        kind: Kind,
        work: &mut PageWork,
    ) -> Result<Rows> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        let source = self
            .checkpoint
            .as_ref()
            .ok_or_else(|| corrupt("catalog page has no source"))?;
        self.cache
            .scan(
                store,
                &source.path,
                0,
                source.range.offset()..source.range.offset() + source.descriptor.body_bytes,
                root,
                kind,
                work,
            )
            .await
    }

    async fn selected_xorbs(
        &self,
        store: &Store,
        hashes: &BTreeSet<String>,
        work: &mut PageWork,
    ) -> Result<PointerCatalog> {
        let keys = keys(hashes)?;
        let root = self
            .checkpoint
            .as_ref()
            .and_then(|s| s.descriptor.xorbs.as_ref());
        let rows = self
            .rows(store, root, Kind::CatalogXorbs, &keys, work)
            .await?;
        let mut catalog = PointerCatalog::new();
        for (hash, row) in hashes.iter().zip(rows) {
            if let Some(row) = row {
                let row: XorbRow = from_json(&row)?;
                let chunks = self
                    .scan(store, Some(&row.chunks), Kind::CatalogXorbChunks, work)
                    .await?;
                let chunks = chunks
                    .into_iter()
                    .enumerate()
                    .map(|(position, (key, value))| {
                        if key != (position as u32).to_be_bytes() || value.len() != 36 {
                            return Err(corrupt("xorb chunk row has invalid position or size"));
                        }
                        let size = u32::from_be_bytes(
                            value[32..]
                                .try_into()
                                .map_err(|_| corrupt("chunk size is truncated"))?,
                        );
                        Ok(XorbChunkEntry::new(hex(&value[..32])?, size))
                    })
                    .collect::<Result<_>>()?;
                catalog.insert_xorb(
                    hash.clone(),
                    XorbCatalogEntry::new(row.encoded_size, hex(&row.body_digest)?, chunks),
                )?;
            }
            // Every matching immutable descriptor participates, even when a
            // later source supplies the placement or file binding selected.
            for delta in &self.deltas {
                if let Some(entry) = delta.xorbs.get(hash) {
                    admit(
                        work,
                        72u64.saturating_add((entry.chunks.len() as u64).saturating_mul(36)),
                    )?;
                    catalog.insert_xorb(hash.clone(), entry.clone())?;
                }
            }
        }
        catalog.validate(true, true)?;
        Ok(catalog)
    }

    /// Select xorb descriptors without decoding unrelated catalog records.
    pub async fn xorbs(&self, store: &Store, hashes: &[String]) -> Result<PointerCatalog> {
        let mut work = self.work();
        let hashes = requested(hashes, &mut work)?;
        self.selected_xorbs(store, &hashes, &mut work).await
    }

    /// Select requested files and their complete shard/xorb dependency closure.
    pub async fn files(&self, store: &Store, hashes: &[String]) -> Result<PointerCatalog> {
        let mut work = self.work();
        let hashes = requested(hashes, &mut work)?;
        let root = self
            .checkpoint
            .as_ref()
            .and_then(|s| s.descriptor.files.as_ref());
        let rows = self
            .rows(store, root, Kind::CatalogFiles, &keys(&hashes)?, &mut work)
            .await?;
        let mut catalog = PointerCatalog::new();
        let mut shards = BTreeSet::new();
        for (hash, row) in hashes.iter().zip(rows) {
            let mut selected = row
                .map(|value| {
                    if value.len() != 40 {
                        return Err(corrupt("catalog file row has an invalid byte length"));
                    }
                    let size = u64::from_be_bytes(
                        value[..8]
                            .try_into()
                            .map_err(|_| corrupt("file size is truncated"))?,
                    );
                    Ok(FileCatalogEntry::new(size, hex(&value[8..])?))
                })
                .transpose()?;
            for delta in &self.deltas {
                if let Some(entry) = delta.files.get(hash) {
                    if selected.as_ref().is_some_and(|old| old.size != entry.size) {
                        return Err(corrupt("file has conflicting declared sizes"));
                    }
                    admit(&mut work, 40)?;
                    selected = Some(entry.clone());
                }
            }
            if let Some(entry) = selected {
                shards.insert(entry.shard_hash.clone());
                catalog.insert_file(hash.clone(), entry)?;
            }
        }
        let root = self
            .checkpoint
            .as_ref()
            .and_then(|s| s.descriptor.shards.as_ref());
        let rows = self
            .rows(store, root, Kind::CatalogShards, &keys(&shards)?, &mut work)
            .await?;
        let mut xorbs = BTreeSet::new();
        for (hash, row) in shards.iter().zip(rows) {
            if let Some(row) = row {
                let row: ShardRow = from_json(&row)?;
                let closure = self
                    .scan(
                        store,
                        row.xorbs.as_ref(),
                        Kind::CatalogShardXorbs,
                        &mut work,
                    )
                    .await?;
                let closure = closure
                    .into_iter()
                    .map(|(key, value)| {
                        if !value.is_empty() {
                            return Err(corrupt("shard xorb row has unexpected data"));
                        }
                        hex(&key)
                    })
                    .collect::<Result<_>>()?;
                catalog.insert_shard(
                    hash.clone(),
                    ShardCatalogEntry::new(row.encoded_size, closure),
                )?;
            }
            for delta in &self.deltas {
                if let Some(entry) = delta.shards.get(hash) {
                    admit(
                        &mut work,
                        8u64.saturating_add((entry.xorb_hashes.len() as u64).saturating_mul(32)),
                    )?;
                    catalog.insert_shard(hash.clone(), entry.clone())?;
                }
            }
            let entry = catalog
                .shards
                .get(hash)
                .ok_or_else(|| corrupt("selected file references an absent shard"))?;
            xorbs.extend(entry.xorb_hashes.iter().cloned());
        }
        let selected = self.selected_xorbs(store, &xorbs, &mut work).await?;
        for (hash, entry) in selected.xorbs {
            catalog.insert_xorb(hash, entry)?;
        }
        catalog.validate(true, true)?;
        Ok(catalog)
    }

    /// Select the lowest-xorb placement for each requested chunk and its xorb closure.
    ///
    /// The selected xorb's ordered recipe must agree with the reverse index.
    pub async fn chunks(&self, store: &Store, hashes: &[String]) -> Result<PointerCatalog> {
        let mut work = self.work();
        let hashes = requested(hashes, &mut work)?;
        let root = self
            .checkpoint
            .as_ref()
            .and_then(|s| s.descriptor.chunks.as_ref());
        let rows = self
            .rows(store, root, Kind::CatalogChunks, &keys(&hashes)?, &mut work)
            .await?;
        let mut selected = BTreeMap::new();
        for (hash, row) in hashes.iter().zip(rows) {
            if let Some(value) = row {
                if value.len() != 40 {
                    return Err(corrupt(
                        "catalog chunk placement has an invalid byte length",
                    ));
                }
                let xorb = hex(&value[..32])?;
                let index = u32::from_be_bytes(
                    value[32..36]
                        .try_into()
                        .map_err(|_| corrupt("chunk position is truncated"))?,
                );
                let size = u32::from_be_bytes(
                    value[36..]
                        .try_into()
                        .map_err(|_| corrupt("chunk size is truncated"))?,
                );
                selected.insert(hash.clone(), (xorb, index, size));
            }
        }
        // Deltas are still materialized by the view owner. This scan is bounded
        // to the post-checkpoint suffix; run-level indexing replaces it next.
        for delta in &self.deltas {
            for (xorb, entry) in &delta.xorbs {
                for (index, chunk) in entry.chunks.iter().enumerate() {
                    if !hashes.contains(&chunk.hash) {
                        continue;
                    }
                    let candidate = (xorb.clone(), index as u32, chunk.uncompressed_size);
                    let placement = selected
                        .entry(chunk.hash.clone())
                        .or_insert_with(|| candidate.clone());
                    if candidate < *placement {
                        *placement = candidate;
                    }
                }
            }
        }
        let xorbs = selected.values().map(|(xorb, _, _)| xorb.clone()).collect();
        let catalog = self.selected_xorbs(store, &xorbs, &mut work).await?;
        for (hash, (xorb, index, size)) in selected {
            let entry = catalog
                .xorbs
                .get(&xorb)
                .ok_or_else(|| corrupt("chunk placement references an absent xorb"))?;
            let chunk = entry
                .chunks
                .get(index as usize)
                .ok_or_else(|| corrupt("chunk placement references an absent xorb position"))?;
            let first = entry.chunks.iter().position(|chunk| chunk.hash == hash);
            if chunk.hash != hash
                || chunk.uncompressed_size != size
                || first != Some(index as usize)
            {
                return Err(corrupt("chunk placement differs from its xorb recipe"));
            }
        }
        Ok(catalog)
    }
}

fn keys(hashes: &BTreeSet<String>) -> Result<Vec<Vec<u8>>> {
    hashes
        .iter()
        .map(|value| hash(value).map(|key| key.to_vec()))
        .collect()
}

fn admit(work: &mut PageWork, bytes: u64) -> Result<()> {
    work.bytes = work
        .bytes
        .checked_add(bytes)
        .ok_or_else(|| corrupt("catalog query byte count overflows"))?;
    if work.bytes > work.maximum {
        return Err(corrupt("catalog query exceeds its byte limit"));
    }
    Ok(())
}

fn requested(hashes: &[String], work: &mut PageWork) -> Result<BTreeSet<String>> {
    admit(work, (hashes.len() as u64).saturating_mul(96))?;
    for value in hashes {
        hash(value)?;
    }
    Ok(hashes.iter().cloned().collect())
}
