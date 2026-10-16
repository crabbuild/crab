mod inspection;

use super::*;
use crate::pages::PageWork;
use crate::pages::object::ObjectPages;
use crab_storage::Store;
use object_store::path::Path;
use std::collections::BTreeSet;

pub(in crate::capsule_protocol) struct CatalogPages {
    pub(in crate::capsule_protocol) pages: ObjectPages,
    pub(in crate::capsule_protocol) descriptor: Descriptor,
    hash: [u8; 32],
    bytes: u64,
    opened: futures_util::lock::Mutex<bool>,
}

impl std::fmt::Debug for CatalogPages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogPages")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl CatalogPages {
    pub(in crate::capsule_protocol) fn new(
        prefix: Path,
        descriptor: Descriptor,
        hash: [u8; 32],
        bytes: u64,
    ) -> Result<Self> {
        let body = descriptor.encode()?;
        if body.len() as u64 != bytes || blake3::hash(&body).as_bytes() != &hash {
            return Err(corrupt("catalog commitment differs from its descriptor"));
        }
        Ok(Self {
            pages: ObjectPages::new(prefix),
            descriptor,
            hash,
            bytes,
            opened: futures_util::lock::Mutex::new(false),
        })
    }

    async fn open(&self, store: &Store, work: &mut PageWork) -> Result<()> {
        work.admit_object(self.hash, self.bytes)?;
        let mut opened = self.opened.lock().await;
        if !*opened {
            let body = store
                .get_with_etag_bounded(&self.pages.path(&self.hash), self.bytes)
                .await?
                .0;
            if blake3::hash(&body).as_bytes() != &self.hash
                || Descriptor::decode(&body)? != self.descriptor
            {
                return Err(corrupt(
                    "stored catalog descriptor differs from its commitment",
                ));
            }
            *opened = true;
        }
        Ok(())
    }

    fn root(&self, kind: Kind) -> Result<Option<&Reference>> {
        Ok(match kind {
            Kind::CatalogFiles => self.descriptor.files.as_ref(),
            Kind::CatalogShards => self.descriptor.shards.as_ref(),
            Kind::CatalogXorbs => self.descriptor.xorbs.as_ref(),
            Kind::CatalogChunks => self.descriptor.chunks.as_ref(),
            _ => return Err(corrupt("catalog query selected a nested tree as its root")),
        })
    }

    pub(in crate::capsule_protocol) async fn rows(
        &self,
        store: &Store,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.open(store, work).await?;
        self.pages
            .lookup(store, self.root(kind)?, kind, keys, work)
            .await
    }

    pub(in crate::capsule_protocol) async fn xorb(
        &self,
        store: &Store,
        bytes: &[u8],
        work: &mut PageWork,
    ) -> Result<XorbCatalogEntry> {
        let row: XorbRow = from_json(bytes)?;
        let rows = self
            .pages
            .scan(store, Some(&row.chunks), Kind::CatalogXorbChunks, work)
            .await?;
        xorb_from_rows(row.encoded_size, row.body_digest, rows)
    }

    pub(in crate::capsule_protocol) async fn shard(
        &self,
        store: &Store,
        bytes: &[u8],
        work: &mut PageWork,
    ) -> Result<ShardCatalogEntry> {
        let row: ShardRow = from_json(bytes)?;
        let rows = self
            .pages
            .scan(store, row.xorbs.as_ref(), Kind::CatalogShardXorbs, work)
            .await?;
        shard_from_rows(row.encoded_size, rows)
    }

    pub(in crate::capsule_protocol) async fn full(
        &self,
        store: &Store,
        work: &mut PageWork,
    ) -> Result<PointerCatalog> {
        self.open(store, work).await?;
        let mut catalog = PointerCatalog::new();
        let files = self
            .pages
            .scan(
                store,
                self.root(Kind::CatalogFiles)?,
                Kind::CatalogFiles,
                work,
            )
            .await?;
        for (key, value) in files {
            catalog.insert_file(hex(&key)?, file_from_bytes(&value)?)?;
        }
        let shards = self
            .pages
            .scan(
                store,
                self.root(Kind::CatalogShards)?,
                Kind::CatalogShards,
                work,
            )
            .await?;
        for (key, value) in shards {
            catalog.insert_shard(hex(&key)?, self.shard(store, &value, work).await?)?;
        }
        let xorbs = self
            .pages
            .scan(
                store,
                self.root(Kind::CatalogXorbs)?,
                Kind::CatalogXorbs,
                work,
            )
            .await?;
        let mut expected_chunks = BTreeMap::new();
        for (key, value) in xorbs {
            let entry = self.xorb(store, &value, work).await?;
            xorb_chunks(
                &entry,
                &key.as_slice()
                    .try_into()
                    .map_err(|_| corrupt("invalid xorb key"))?,
                &mut expected_chunks,
            )?;
            catalog.insert_xorb(hex(&key)?, entry)?;
        }
        let chunks = self
            .pages
            .scan(
                store,
                self.root(Kind::CatalogChunks)?,
                Kind::CatalogChunks,
                work,
            )
            .await?;
        if chunks != expected_chunks.into_iter().collect::<Rows>() {
            return Err(corrupt("catalog reverse placements differ from xorb order"));
        }
        catalog.validate(true, true)?;
        Ok(catalog)
    }

    pub(in crate::capsule_protocol) async fn apply(
        &self,
        store: &Store,
        deltas: &[PointerCatalog],
        work: &mut PageWork,
    ) -> Result<Descriptor> {
        admit_catalog_rows(deltas, work.maximum.saturating_sub(work.bytes))?;
        self.open(store, work).await?;
        let mut delta = PointerCatalog::new();
        for piece in deltas {
            delta.merge_delta(piece)?;
        }
        let keys = |values: Vec<&String>| {
            values
                .into_iter()
                .map(|value| Ok(hash(value)?.to_vec()))
                .collect::<Result<Vec<_>>>()
        };
        let xorb_keys = keys(delta.xorbs.keys().collect())?;
        let existing = self
            .rows(store, Kind::CatalogXorbs, &xorb_keys, work)
            .await?;
        let mut new_xorbs = BTreeSet::new();
        for ((identity, incoming), existing) in delta.xorbs.iter().zip(existing) {
            match existing {
                Some(existing) if self.xorb(store, &existing, work).await? != *incoming => {
                    return Err(contract_error(format!(
                        "xorb {identity} has conflicting descriptors"
                    )));
                }
                None => {
                    new_xorbs.insert(identity.clone());
                }
                _ => (),
            }
        }
        let shard_keys = keys(delta.shards.keys().collect())?;
        let existing = self
            .rows(store, Kind::CatalogShards, &shard_keys, work)
            .await?;
        let mut new_shards = BTreeSet::new();
        for ((identity, incoming), existing) in delta.shards.iter().zip(existing) {
            match existing {
                Some(existing) if self.shard(store, &existing, work).await? != *incoming => {
                    return Err(contract_error(format!(
                        "shard {identity} has conflicting descriptors"
                    )));
                }
                None => {
                    new_shards.insert(identity.clone());
                }
                _ => (),
            }
        }
        let file_keys = keys(delta.files.keys().collect())?;
        let existing = self
            .rows(store, Kind::CatalogFiles, &file_keys, work)
            .await?;
        for ((identity, incoming), existing) in delta.files.iter().zip(existing) {
            if let Some(existing) = existing
                && file_from_bytes(&existing)?.size != incoming.size
            {
                return Err(contract_error(format!(
                    "file {identity} has conflicting declared sizes"
                )));
            }
        }
        let required_shards = delta
            .files
            .values()
            .map(|entry| entry.shard_hash.clone())
            .filter(|hash| !delta.shards.contains_key(hash))
            .collect::<BTreeSet<_>>();
        let existing = self
            .rows(
                store,
                Kind::CatalogShards,
                &keys(required_shards.iter().collect())?,
                work,
            )
            .await?;
        let mut required_xorbs = delta
            .shards
            .values()
            .flat_map(|entry| &entry.xorb_hashes)
            .cloned()
            .collect::<BTreeSet<_>>();
        for row in existing {
            let row =
                row.ok_or_else(|| contract_error("catalog delta references an absent shard"))?;
            required_xorbs.extend(self.shard(store, &row, work).await?.xorb_hashes);
        }
        required_xorbs.retain(|hash| !delta.xorbs.contains_key(hash));
        let existing = self
            .rows(
                store,
                Kind::CatalogXorbs,
                &keys(required_xorbs.iter().collect())?,
                work,
            )
            .await?;
        for row in existing {
            let row =
                row.ok_or_else(|| contract_error("catalog delta references an absent xorb"))?;
            self.xorb(store, &row, work).await?;
        }

        let mut descriptor = self.descriptor.clone();
        let mut xorbs = Vec::new();
        let mut chunks = BTreeMap::new();
        for (identity, entry) in &delta.xorbs {
            if !new_xorbs.contains(identity) {
                continue;
            }
            let identity = hash(identity)?;
            let rows = xorb_chunks(entry, &identity, &mut chunks)?;
            let root = self
                .pages
                .edit(store, None, Kind::CatalogXorbChunks, rows, work)
                .await?
                .ok_or_else(|| contract_error("catalog xorb contains no chunks"))?;
            xorbs.push((
                identity.to_vec(),
                json(&XorbRow {
                    encoded_size: entry.encoded_size,
                    body_digest: hash(&entry.body_digest)?,
                    chunks: root,
                })?,
            ));
        }
        let mut shards = Vec::new();
        for (identity, entry) in &delta.shards {
            if !new_shards.contains(identity) {
                continue;
            }
            let nested = self
                .pages
                .edit(
                    store,
                    None,
                    Kind::CatalogShardXorbs,
                    shard_xorbs(entry)?,
                    work,
                )
                .await?;
            shards.push((
                hash(identity)?.to_vec(),
                json(&ShardRow {
                    encoded_size: entry.encoded_size,
                    xorbs: nested,
                })?,
            ));
        }
        let chunk_keys = chunks.keys().cloned().collect::<Vec<_>>();
        let existing = self
            .rows(store, Kind::CatalogChunks, &chunk_keys, work)
            .await?;
        let mut changed = Vec::new();
        let mut prior = Vec::new();
        let mut prior_xorbs = BTreeSet::new();
        for ((key, incoming), existing) in chunks.into_iter().zip(existing) {
            if let Some(existing) = existing {
                if existing.len() != 40 {
                    return Err(corrupt("catalog chunk placement length differs"));
                }
                // Superseded placements remain loaded dependencies. Precedence
                // must not hide a broken recipe behind a valid new placement.
                let keep_existing = existing[..36] <= incoming[..36];
                prior_xorbs.insert(hex(&existing[..32])?);
                prior.push((key.clone(), existing));
                if keep_existing {
                    continue;
                }
            }
            changed.push((key, incoming));
        }
        let existing = self
            .rows(
                store,
                Kind::CatalogXorbs,
                &keys(prior_xorbs.iter().collect())?,
                work,
            )
            .await?;
        let mut prior_entries = BTreeMap::new();
        for (identity, row) in prior_xorbs.into_iter().zip(existing) {
            let row = row.ok_or_else(|| corrupt("prior placement has no xorb"))?;
            prior_entries.insert(identity, self.xorb(store, &row, work).await?);
        }
        for (key, placement) in prior {
            let identity = hex(&placement[..32])?;
            let entry = prior_entries
                .get(&identity)
                .ok_or_else(|| corrupt("prior placement lost its xorb"))?;
            let index = u32::from_be_bytes(
                placement[32..36]
                    .try_into()
                    .map_err(|_| corrupt("placement ordinal is truncated"))?,
            ) as usize;
            let size = u32::from_be_bytes(
                placement[36..]
                    .try_into()
                    .map_err(|_| corrupt("placement size is truncated"))?,
            );
            let hash = hex(&key)?;
            if entry
                .chunks
                .get(index)
                .is_none_or(|chunk| chunk.hash != hash || chunk.uncompressed_size != size)
                || entry.chunks.iter().position(|chunk| chunk.hash == hash) != Some(index)
            {
                return Err(corrupt(
                    "prior placement differs from its canonical xorb recipe",
                ));
            }
        }
        descriptor.files = self
            .pages
            .edit(
                store,
                descriptor.files.as_ref(),
                Kind::CatalogFiles,
                file_rows(&delta)?,
                work,
            )
            .await?;
        descriptor.shards = self
            .pages
            .edit(
                store,
                descriptor.shards.as_ref(),
                Kind::CatalogShards,
                shards,
                work,
            )
            .await?;
        descriptor.xorbs = self
            .pages
            .edit(
                store,
                descriptor.xorbs.as_ref(),
                Kind::CatalogXorbs,
                xorbs,
                work,
            )
            .await?;
        descriptor.chunks = self
            .pages
            .edit(
                store,
                descriptor.chunks.as_ref(),
                Kind::CatalogChunks,
                changed,
                work,
            )
            .await?;
        descriptor.validate()?;
        Ok(descriptor)
    }
}
