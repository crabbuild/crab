use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use crab_storage::{ETag, Store, StoreLayout};
use serde::{Deserialize, Serialize};

use super::{
    BrowseIndexes, CapsulePointer, CheckpointPointer, LayeredCheckpoint, MAX_BROWSE_INDEXES_BYTES,
    MAX_HISTORY_CHAIN_BYTES, MAX_HISTORY_CHAIN_SEGMENTS, MAX_HISTORY_SEGMENT_BYTES, PackSourceKind,
};
use crate::error::{MetadataError, Result};
use crate::validation::{corrupt_object, validate_content_hash};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    version: u32,
    hash: String,
    roots: usize,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    generation: u64,
    indexes: BrowseIndexes,
    previous: Option<String>,
    checkpoint: Option<CheckpointPointer>,
    catalog: Option<String>,
    sources: BTreeMap<String, PackSourceKind>,
}

/// Immutable capture of browse pages and the original Git sources used by their readers.
#[derive(Debug)]
pub struct RetainedBrowseIndexes {
    hash: String,
    payload: Payload,
}

impl RetainedBrowseIndexes {
    /// Return this immutable retention record's identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the source generation used by explicit history pruning.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.payload.generation
    }

    /// Return captured index dependencies without granting current visibility.
    #[must_use]
    pub fn indexes(&self) -> &BrowseIndexes {
        &self.payload.indexes
    }

    /// Return original source bodies, including a captured checkpoint when present.
    #[must_use]
    pub fn source_paths(&self, layout: &StoreLayout<Store>) -> Vec<object_store::path::Path> {
        self.payload
            .checkpoint
            .iter()
            .map(|pointer| layout.capsule_checkpoint_path(pointer.hash()))
            .chain(
                self.payload
                    .catalog
                    .iter()
                    .map(|hash| layout.capsule_catalog_path(hash)),
            )
            .chain(self.payload.sources.iter().map(|(hash, kind)| match kind {
                PackSourceKind::CapsuleRun => layout.capsule_path(hash),
                PackSourceKind::PackLayer => layout.capsule_pack_layer_path(hash),
            }))
            .collect()
    }
}

impl Payload {
    fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|source| MetadataError::BrowseIndexRecord { source })?;
        if bytes.len() as u64 > MAX_HISTORY_SEGMENT_BYTES {
            return Err(corrupt("retained browse record exceeds byte limit"));
        }
        Ok(bytes.into())
    }

    fn validate(&self) -> Result<()> {
        if self.version != 2 {
            return Err(corrupt("unsupported retained browse version"));
        }
        self.indexes.encode()?;
        if let Some(pointer) = &self.checkpoint {
            super::root::validate_checkpoint_pointer(pointer)?;
        } else if self.catalog.is_some() {
            return Err(corrupt("retained catalog has no checkpoint owner"));
        }
        for hash in self
            .previous
            .iter()
            .chain(self.catalog.iter())
            .chain(self.sources.keys())
        {
            validate_content_hash(hash, "retention dependency", "browse retention")?;
        }
        Ok(())
    }
}

impl Head {
    fn append(previous: Option<&Self>, bytes: &Bytes) -> Result<Self> {
        let value = Self {
            version: 1,
            hash: blake3::hash(bytes).to_hex().to_string(),
            roots: previous.map_or(0, |head| head.roots).saturating_add(1),
            bytes: previous
                .map_or(0, |head| head.bytes)
                .saturating_add(bytes.len() as u64),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        validate_content_hash(&self.hash, "retained root", "browse retention")?;
        if self.version != 1
            || self.roots == 0
            || self.roots > MAX_HISTORY_CHAIN_SEGMENTS
            || self.bytes == 0
            || self.bytes > MAX_HISTORY_CHAIN_BYTES
        {
            return Err(corrupt(
                "retained browse head exceeds version or resource bounds",
            ));
        }
        Ok(())
    }
}

async fn load_head(layout: &StoreLayout<Store>) -> Result<Option<(Head, ETag)>> {
    let (bytes, etag) = match layout
        .store()
        .get_with_etag_bounded(
            &layout.browse_retained_head_path(),
            MAX_BROWSE_INDEXES_BYTES,
        )
        .await
    {
        Ok(value) => value,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let head: Head = serde_json::from_slice(&bytes)
        .map_err(|source| MetadataError::BrowseIndexRecord { source })?;
    head.validate()?;
    Ok(Some((head, etag)))
}

async fn load_payload(
    layout: &StoreLayout<Store>,
    hash: &str,
    maximum: u64,
) -> Result<(Payload, u64)> {
    validate_content_hash(hash, "retained root", "browse retention")?;
    let expected = *blake3::Hash::from_hex(hash)
        .map_err(|_| corrupt("invalid retained browse hash"))?
        .as_bytes();
    let bytes = layout
        .store()
        .verify_bounded(
            &layout.browse_retained_root_path(hash),
            &expected,
            maximum.min(MAX_HISTORY_SEGMENT_BYTES),
        )
        .await?;
    let payload: Payload = serde_json::from_slice(&bytes)
        .map_err(|source| MetadataError::BrowseIndexRecord { source })?;
    payload.validate()?;
    if let Some(pointer) = &payload.checkpoint {
        // A retention record cannot omit an owned dependency from GC marking.
        // Authenticate the checkpoint footer before trusting its captured paths.
        let checkpoint = super::load_layered_checkpoint_control(layout, pointer).await?;
        let catalog = checkpoint.catalog().map(|value| value.hash());
        if payload.catalog.as_deref() != catalog
            || checkpoint
                .sources()
                .iter()
                .any(|source| payload.sources.get(source.object_hash()) != Some(&source.kind()))
        {
            return Err(corrupt("retained dependencies differ from checkpoint"));
        }
    }
    Ok((payload, bytes.len() as u64))
}

async fn publish_head(layout: &StoreLayout<Store>, head: &Head, etag: Option<ETag>) -> Result<()> {
    let bytes =
        serde_json::to_vec(head).map_err(|source| MetadataError::BrowseIndexRecord { source })?;
    let path = layout.browse_retained_head_path();
    match etag {
        Some(etag) => layout.store().update(&path, bytes.into(), etag).await?,
        None => {
            layout
                .store()
                .create_strict_with_etag(&path, bytes.into())
                .await?
        }
    };
    Ok(())
}

/// Retain original dependencies before publishing replacement browse readiness.
///
/// The caller holds generation ownership and both GC writer fences. Lifetime
/// CAS precedes readiness CAS: a failed readiness publication may retain extra
/// data, but a crash can never advertise unretained pages or source ranges.
pub async fn retain_browse_indexes(
    layout: &StoreLayout<Store>,
    indexes: &BrowseIndexes,
    generation: u64,
    checkpoint: Option<&LayeredCheckpoint>,
    run_pointers: &[CapsulePointer],
) -> Result<()> {
    let previous = load_head(layout).await?;
    let mut sources = BTreeMap::new();
    let checkpoint_sources = checkpoint
        .into_iter()
        .flat_map(LayeredCheckpoint::sources)
        .map(|source| (source.object_hash().to_owned(), source.kind()));
    let capsule_sources = run_pointers
        .iter()
        .map(|pointer| (pointer.hash().to_owned(), PackSourceKind::CapsuleRun));
    // Ref-only runs carry visibility/catalog pages despite having no Git
    // members. Captures own those lazy reads as well as physical pack ranges.
    for (hash, kind) in checkpoint_sources.chain(capsule_sources) {
        if sources.insert(hash, kind).is_some_and(|old| old != kind) {
            return Err(corrupt("retained source has conflicting kinds"));
        }
    }
    let mut payload = Payload {
        version: 2,
        generation,
        indexes: indexes.clone(),
        previous: previous.as_ref().map(|(head, _)| head.hash.clone()),
        checkpoint: checkpoint.map(LayeredCheckpoint::pointer).transpose()?,
        catalog: checkpoint
            .and_then(LayeredCheckpoint::catalog)
            .map(|value| value.hash().to_owned()),
        sources,
    };
    if let Some((head, _)) = &previous {
        let (old, _) = load_payload(layout, &head.hash, head.bytes).await?;
        if old.generation > generation {
            return Err(corrupt("retained browse generation moved backwards"));
        }
        payload.previous = old.previous.clone();
        if payload == old {
            return Ok(());
        }
        payload.previous = Some(head.hash.clone());
    }
    let bytes = payload.encode()?;
    let head = Head::append(previous.as_ref().map(|(head, _)| head), &bytes)?;
    layout
        .store()
        .put_if_absent_verified(&layout.browse_retained_root_path(&head.hash), bytes)
        .await?;
    publish_head(layout, &head, previous.map(|(_, etag)| etag)).await
}

async fn load_chain(
    layout: &StoreLayout<Store>,
    head: &Head,
) -> Result<Vec<RetainedBrowseIndexes>> {
    let mut next = Some(head.hash.clone());
    let mut seen = BTreeSet::new();
    let mut records: Vec<RetainedBrowseIndexes> = Vec::new();
    let mut remaining = head.bytes;
    while let Some(hash) = next {
        if records.len() == head.roots || !seen.insert(hash.clone()) {
            return Err(corrupt("retained browse count or cycle mismatch"));
        }
        let (payload, bytes) = load_payload(layout, &hash, remaining).await?;
        remaining = remaining
            .checked_sub(bytes)
            .ok_or_else(|| corrupt("retained browse chain exceeds byte limit"))?;
        if records
            .last()
            .is_some_and(|newer| newer.generation() < payload.generation)
        {
            return Err(corrupt("retained browse generations are not ordered"));
        }
        next = payload.previous.clone();
        records.push(RetainedBrowseIndexes { hash, payload });
    }
    if remaining != 0 || records.len() != head.roots {
        return Err(corrupt("retained browse accounting mismatch"));
    }
    Ok(records)
}

/// Authenticate retained captures for maintenance; foreground lookup never walks this chain.
pub async fn load_retained_browse_indexes(
    layout: &StoreLayout<Store>,
) -> Result<Vec<RetainedBrowseIndexes>> {
    match load_head(layout).await? {
        Some((head, _)) => load_chain(layout, &head).await,
        None => Ok(Vec::new()),
    }
}

/// Verify retained index/source integrity against stable lifetime and readiness heads.
///
/// Maintenance decodes complete graph/path indexes and streams original source
/// and physical-index bodies. Retain the returned capture and recheck it after
/// later phases. This proves retained bytes, never ref visibility.
#[cfg(feature = "remote-index")]
pub async fn verify_retained_browse_indexes(
    layout: &StoreLayout<Store>,
    maximum_bytes: u64,
) -> Result<BrowseRetentionCapture> {
    let head = load_head(layout).await?;
    let current = super::load_browse_indexes(layout).await?;
    let records = match &head {
        Some((head, _)) => load_chain(layout, head).await?,
        None => Vec::new(),
    };
    if current
        .as_ref()
        .is_some_and(|current| !records.iter().any(|record| record.indexes() == current))
    {
        return Err(corrupt("published browse indexes have no retained capture"));
    }
    let mut sources = BTreeSet::new();
    for record in records {
        let graph = crate::split_commit_graph::load_split_commit_graph(
            layout.store(),
            layout,
            record.indexes().commit_graph_hash(),
            maximum_bytes.min(crate::split_commit_graph::DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES),
        )
        .await?;
        if graph.descriptor.generation != record.generation() {
            return Err(corrupt(
                "retained graph generation differs from its capture",
            ));
        }
        if let Some(hash) = record.indexes().path_state_hash() {
            crate::path_state::load_path_state(
                layout.store(),
                layout,
                hash,
                &graph,
                maximum_bytes.min(crate::path_state::DEFAULT_MAX_PATH_STATE_BYTES),
            )
            .await?;
        }
        if let Some(hash) = record.indexes().object_index_hash() {
            crate::git_object_locator::PagedGitObjectIndex::verify_retained(
                layout.store(),
                layout,
                hash,
                record.indexes().state_digest(),
                &graph.descriptor,
                maximum_bytes,
            )
            .await?;
        }
        // Captures may share original multi-gigabyte sources. Authenticate each
        // immutable body once, including ref-only sources absent from current history.
        for path in record.source_paths(layout) {
            if !sources.insert(path.clone()) {
                continue;
            }
            let size = layout.store().head(&path).await?.size;
            if size > maximum_bytes {
                return Err(corrupt("retained source exceeds byte limit"));
            }
            let hash = path
                .filename()
                .and_then(|hash| blake3::Hash::from_hex(hash).ok())
                .ok_or_else(|| corrupt("retained source path has no content identity"))?;
            layout
                .store()
                .verify_size_and_hash(&path, size, hash.as_bytes())
                .await?;
        }
    }
    // Browse maintenance can change lifetime without moving a Git ref. A
    // concurrent prune/rebuild must yield a retry, never a clean mixed capture.
    let capture = BrowseRetentionCapture { head, current };
    if !capture.is_current(layout).await? {
        return Err(corrupt(
            "browse retention changed during verification; retry fsck",
        ));
    }
    Ok(capture)
}

/// Verified browse lifetime and readiness authority retained across maintenance phases.
#[cfg(feature = "remote-index")]
#[derive(Debug)]
pub struct BrowseRetentionCapture {
    head: Option<(Head, ETag)>,
    current: Option<BrowseIndexes>,
}

#[cfg(feature = "remote-index")]
impl BrowseRetentionCapture {
    /// Revalidate control identity without decoding indexes or original sources again.
    ///
    /// Invalid records return an error; valid concurrent movement returns false.
    pub async fn is_current(&self, layout: &StoreLayout<Store>) -> Result<bool> {
        Ok(load_head(layout).await? == self.head
            && super::load_browse_indexes(layout).await? == self.current)
    }
}

/// Trim captured generations after authoritative history pruning commits.
///
/// The caller holds the exclusive repository GC lease. Failure leaves extra
/// retention and is retryable; trimming before history CAS could strand readers.
pub async fn prune_retained_browse_indexes(
    layout: &StoreLayout<Store>,
    minimum_generation: u64,
) -> Result<()> {
    let Some((head, etag)) = load_head(layout).await? else {
        return Ok(());
    };
    let current = super::load_browse_indexes(layout).await?;
    let records = load_chain(layout, &head).await?;
    let keep = |position: usize, record: &RetainedBrowseIndexes| {
        position == 0
            || record.generation() >= minimum_generation
            || current.as_ref() == Some(record.indexes())
    };
    if records
        .iter()
        .enumerate()
        .all(|(position, record)| keep(position, record))
    {
        return Ok(());
    }
    let mut previous: Option<Head> = None;
    for (position, record) in records.into_iter().enumerate().rev() {
        if !keep(position, &record) {
            continue;
        }
        let payload = Payload {
            previous: previous.as_ref().map(|head| head.hash.clone()),
            ..record.payload
        };
        let bytes = payload.encode()?;
        let head = Head::append(previous.as_ref(), &bytes)?;
        layout
            .store()
            .put_if_absent_verified(&layout.browse_retained_root_path(&head.hash), bytes)
            .await?;
        previous = Some(head);
    }
    let head = previous.ok_or_else(|| corrupt("pruning removed current browse retention"))?;
    publish_head(layout, &head, Some(etag)).await
}

fn corrupt(reason: &str) -> MetadataError {
    corrupt_object("browse retention", reason)
}

#[cfg(test)]
mod tests;
