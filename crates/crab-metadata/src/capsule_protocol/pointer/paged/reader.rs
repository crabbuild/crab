//! Selected dependency closure from captured checkpoint and run roots.
use super::*;
use crate::capsule_protocol::run::catalog::{CatalogOrigin, CatalogValue};
use crate::capsule_protocol::{PackRange, RunCatalogSource};
use crate::pages::{PageCache, PageWork};
use crab_storage::Store;
use std::collections::BTreeSet;
use std::sync::Arc;

#[cfg(test)]
mod tests;

/// Snapshot-bound catalog queries with one cache and aggregate byte budget.
///
/// The owner retains the captured objects against GC. Missing keys return an
/// empty selection; missing pages and conflicting dependencies return errors.
#[derive(Debug)]
pub struct PointerCatalogLookup {
    checkpoint: Option<CheckpointSource>,
    runs: Vec<RunCatalogSource>,
    ranks: BTreeMap<String, usize>,
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
            runs: Vec::new(),
            ranks: BTreeMap::new(),
            cache: PageCache::default(),
            maximum,
        })
    }

    /// Start a lookup for a captured view that has no checkpoint catalog.
    #[must_use]
    pub fn empty(maximum: u64) -> Self {
        Self {
            checkpoint: None,
            runs: Vec::new(),
            ranks: BTreeMap::new(),
            cache: PageCache::default(),
            maximum,
        }
    }

    /// Capture physical runs and the exact selected logical-capsule publication order.
    ///
    /// Unselected occurrences contribute neither file precedence nor conflicts.
    pub fn with_run_sources(
        mut self,
        runs: Vec<RunCatalogSource>,
        order: &[String],
    ) -> Result<Self> {
        if !self.runs.is_empty() || !self.ranks.is_empty() {
            return Err(corrupt("run catalog sources have already been captured"));
        }
        for (index, hash) in order.iter().enumerate() {
            crate::validation::validate_content_hash(
                hash,
                "catalog capsule",
                "capsule-protocol pointer catalog",
            )?;
            if self.ranks.insert(hash.clone(), index + 1).is_some() {
                return Err(corrupt("selected catalog capsules are not unique"));
            }
        }
        self.runs = runs;
        Ok(self)
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
    ) -> Result<Vec<Vec<CatalogValue>>> {
        let mut output = (0..keys.len()).map(|_| Vec::new()).collect::<Vec<_>>();
        if let (Some(source), Some(root)) = (&self.checkpoint, root) {
            let origin = Arc::new(CatalogOrigin {
                path: source.path.clone(),
                range: source.range.offset()..source.range.offset() + source.descriptor.body_bytes,
                layer: 0,
            });
            let rows = self
                .cache
                .lookup(
                    store,
                    &origin.path,
                    origin.layer,
                    origin.range.clone(),
                    root,
                    kind,
                    keys,
                    work,
                )
                .await?;
            for (result, row) in output.iter_mut().zip(rows) {
                if let Some(bytes) = row {
                    result.push(CatalogValue {
                        origin: origin.clone(),
                        bytes,
                        identity: None,
                        rank: 0,
                    });
                }
            }
        }
        for (index, run) in self.runs.iter().enumerate() {
            let rows = run
                .lookup(store, &self.cache, index + 1, &self.ranks, kind, keys, work)
                .await?;
            for (result, rows) in output.iter_mut().zip(rows) {
                result.extend(rows);
            }
        }
        Ok(output)
    }

    async fn scan(
        &self,
        store: &Store,
        origin: &CatalogOrigin,
        root: Option<&PageRef>,
        kind: Kind,
        work: &mut PageWork,
    ) -> Result<Rows> {
        let Some(root) = root else {
            return Ok(Vec::new());
        };
        self.cache
            .scan(
                store,
                &origin.path,
                origin.layer,
                origin.range.clone(),
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
            for value in row {
                let row: XorbRow = from_json(&value.bytes)?;
                let chunks = self
                    .scan(
                        store,
                        &value.origin,
                        Some(&row.chunks),
                        Kind::CatalogXorbChunks,
                        work,
                    )
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
                let entry = XorbCatalogEntry::new(row.encoded_size, hex(&row.body_digest)?, chunks);
                verify_identity(&value, Kind::CatalogXorbs, &json(&entry)?)?;
                catalog.insert_xorb(hash.clone(), entry)?;
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
        self.dependencies(store, hashes, &[], &[]).await
    }

    /// Select files and explicit shard/xorb identities with their complete closure.
    ///
    /// All keys and dependency pages share one query budget. Absent requested
    /// identities are misses; a present file or shard with missing dependencies fails.
    pub async fn dependencies(
        &self,
        store: &Store,
        files: &[String],
        shards: &[String],
        xorbs: &[String],
    ) -> Result<PointerCatalog> {
        let mut work = self.work();
        let hashes = requested(files, &mut work)?;
        let mut shards = requested(shards, &mut work)?;
        let mut xorbs = requested(xorbs, &mut work)?;
        let root = self
            .checkpoint
            .as_ref()
            .and_then(|s| s.descriptor.files.as_ref());
        let rows = self
            .rows(store, root, Kind::CatalogFiles, &keys(&hashes)?, &mut work)
            .await?;
        let mut catalog = PointerCatalog::new();
        for (hash, row) in hashes.iter().zip(rows) {
            let mut selected: Option<(usize, FileCatalogEntry)> = None;
            for value in row {
                verify_identity(&value, Kind::CatalogFiles, &value.bytes)?;
                if value.bytes.len() != 40 {
                    return Err(corrupt("catalog file row has an invalid byte length"));
                }
                let size = u64::from_be_bytes(
                    value.bytes[..8]
                        .try_into()
                        .map_err(|_| corrupt("file size is truncated"))?,
                );
                let entry = FileCatalogEntry::new(size, hex(&value.bytes[8..])?);
                if let Some((rank, old)) = &selected
                    && (old.size != entry.size || (*rank == value.rank && old != &entry))
                {
                    return Err(corrupt("file has conflicting declared sizes or ordering"));
                }
                if selected.as_ref().is_none_or(|(rank, _)| *rank < value.rank) {
                    selected = Some((value.rank, entry));
                }
            }
            let selected = selected.map(|(_, entry)| entry);
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
        for (hash, row) in shards.iter().zip(rows) {
            for value in row {
                let row: ShardRow = from_json(&value.bytes)?;
                let closure = self
                    .scan(
                        store,
                        &value.origin,
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
                let entry = ShardCatalogEntry::new(row.encoded_size, closure);
                verify_identity(&value, Kind::CatalogShards, &json(&entry)?)?;
                catalog.insert_shard(hash.clone(), entry)?;
            }
        }
        for file in catalog.files.values() {
            catalog
                .shards
                .get(&file.shard_hash)
                .ok_or_else(|| corrupt("selected file references an absent shard"))?;
        }
        xorbs.extend(
            catalog
                .shards
                .values()
                .flat_map(|entry| entry.xorb_hashes.iter().cloned()),
        );
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
            for row in row {
                verify_identity(&row, Kind::CatalogChunks, &row.bytes)?;
                let value = row.bytes;
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
                let candidate = (xorb, index, size);
                let placement = selected
                    .entry(hash.clone())
                    .or_insert_with(|| candidate.clone());
                if candidate < *placement {
                    *placement = candidate;
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

fn verify_identity(value: &CatalogValue, kind: Kind, normalized: &[u8]) -> Result<()> {
    if value
        .identity
        .is_some_and(|identity| identity != semantic_identity(kind, normalized))
    {
        return Err(corrupt(
            "run catalog value differs from its semantic commitment",
        ));
    }
    Ok(())
}
