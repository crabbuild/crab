use std::collections::{BTreeMap, BTreeSet};

use crab_storage::{ETag, Store, StoreLayout};
use object_store::path::Path;
use serde::{Deserialize, Serialize};

use super::{
    CapsulePointer, MAX_CAPSULES_PER_RUN, MAX_HISTORY_CHAIN_BYTES, MAX_HISTORY_CHAIN_SEGMENTS,
    MAX_HISTORY_SEGMENT_BYTES,
};
use crate::error::{MetadataError, Result};
use crate::validation::{corrupt_object, validate_content_hash};

const MAX_HEAD_BYTES: u64 = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Floor {
    version: u32,
    minimum_generation: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tip {
    hash: String,
    roots: usize,
    bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    version: u32,
    ref_key: String,
    minimum_generation: u64,
    tip: Option<Tip>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Segment {
    version: u32,
    ref_key: String,
    generation: u64,
    sources: Vec<CapsulePointer>,
    previous: Option<String>,
}

/// Authenticated lifetime dependencies; they never authorize ref or object visibility.
pub struct RetainedCapsuleSources {
    paths: Vec<Path>,
    runs: Vec<CapsulePointer>,
}

impl RetainedCapsuleSources {
    /// Return live history heads, immutable segments and original source bodies for GC.
    #[must_use]
    pub fn paths(&self) -> &[Path] {
        &self.paths
    }

    /// Return complete original source proofs for maintenance integrity checks.
    #[must_use]
    pub fn runs(&self) -> &[CapsulePointer] {
        &self.runs
    }
}

impl Tip {
    fn append(previous: Option<&Self>, hash: String, bytes: usize) -> Result<Self> {
        let tip = Self {
            hash,
            roots: previous.map_or(0, |tip| tip.roots).saturating_add(1),
            bytes: previous
                .map_or(0, |tip| tip.bytes)
                .saturating_add(bytes as u64),
        };
        tip.validate()?;
        Ok(tip)
    }

    fn validate(&self) -> Result<()> {
        validate_content_hash(&self.hash, "source history root", "source retention")?;
        if self.roots == 0
            || self.roots > MAX_HISTORY_CHAIN_SEGMENTS
            || self.bytes == 0
            || self.bytes > MAX_HISTORY_CHAIN_BYTES
        {
            return Err(corrupt("source history exceeds resource bounds"));
        }
        Ok(())
    }
}

impl Segment {
    fn validate(&self, head: &Head) -> Result<()> {
        if self.version != 1
            || self.ref_key != head.ref_key
            || self.generation < head.minimum_generation
            || self.sources.is_empty()
        {
            return Err(corrupt("source segment differs from lifetime head"));
        }
        let mut hashes = BTreeSet::new();
        let mut count = 0usize;
        for source in &self.sources {
            super::root::validate_capsule_pointer(source)?;
            if !hashes.insert(source.hash()) {
                return Err(corrupt("source segment repeats an immutable source"));
            }
            count = count.saturating_add(source.capsule_count() as usize);
        }
        if count > MAX_CAPSULES_PER_RUN {
            return Err(corrupt("source segment exceeds compaction intake bound"));
        }
        if let Some(hash) = &self.previous {
            validate_content_hash(hash, "source history predecessor", "source retention")?;
        }
        Ok(())
    }
}

async fn load_floor(layout: &StoreLayout<Store>) -> Result<Option<(u64, ETag)>> {
    let (bytes, etag) = match layout
        .store()
        .get_with_etag_bounded(&layout.capsule_source_floor_path(), MAX_HEAD_BYTES)
        .await
    {
        Ok(value) => value,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let floor: Floor = serde_json::from_slice(&bytes).map_err(record_error)?;
    if floor.version != 1 {
        return Err(corrupt("unsupported source lifetime floor"));
    }
    Ok(Some((floor.minimum_generation, etag)))
}

async fn admit_generation(layout: &StoreLayout<Store>, generation: u64) -> Result<u64> {
    let floor = load_floor(layout).await?.map_or(0, |(floor, _)| floor);
    if generation < floor {
        return Err(corrupt(
            "source retirement precedes explicit history pruning",
        ));
    }
    Ok(floor)
}

async fn advance_floor(layout: &StoreLayout<Store>, minimum_generation: u64) -> Result<u64> {
    let previous = load_floor(layout).await?;
    if let Some((floor, _)) = &previous
        && *floor >= minimum_generation
    {
        return Ok(*floor);
    }
    let bytes = serde_json::to_vec(&Floor {
        version: 1,
        minimum_generation,
    })
    .map_err(record_error)?;
    let path = layout.capsule_source_floor_path();
    match previous {
        Some((_, etag)) => {
            layout.store().update(&path, bytes.into(), etag).await?;
        }
        None => {
            layout
                .store()
                .create_strict_with_etag(&path, bytes.into())
                .await?;
        }
    }
    Ok(minimum_generation)
}

async fn load_head(layout: &StoreLayout<Store>, path: &Path) -> Result<Option<(Head, ETag)>> {
    let (bytes, etag) = match layout
        .store()
        .get_with_etag_bounded(path, MAX_HEAD_BYTES)
        .await
    {
        Ok(value) => value,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let head: Head = serde_json::from_slice(&bytes).map_err(record_error)?;
    validate_content_hash(&head.ref_key, "source history ref key", "source retention")?;
    if head.version != 1 || *path != layout.capsule_source_head_path(&head.ref_key) {
        return Err(corrupt("source lifetime head has an invalid identity"));
    }
    if let Some(tip) = &head.tip {
        tip.validate()?;
    }
    Ok(Some((head, etag)))
}

async fn publish_head(layout: &StoreLayout<Store>, head: &Head, etag: Option<ETag>) -> Result<()> {
    let bytes = serde_json::to_vec(head).map_err(record_error)?;
    if bytes.len() as u64 > MAX_HEAD_BYTES {
        return Err(corrupt("source head exceeds byte bound"));
    }
    let path = layout.capsule_source_head_path(&head.ref_key);
    match etag {
        Some(etag) => {
            layout.store().update(&path, bytes.into(), etag).await?;
        }
        None => {
            layout
                .store()
                .create_strict_with_etag(&path, bytes.into())
                .await?;
        }
    }
    Ok(())
}

async fn read_segment(
    layout: &StoreLayout<Store>,
    head: &Head,
    hash: &str,
    maximum: u64,
) -> Result<(Segment, usize)> {
    let expected =
        blake3::Hash::from_hex(hash).map_err(|_| corrupt("invalid source segment identity"))?;
    let bytes = layout
        .store()
        .verify_bounded(
            &layout.capsule_source_segment_path(hash),
            expected.as_bytes(),
            maximum.min(MAX_HISTORY_SEGMENT_BYTES),
        )
        .await?;
    let segment: Segment = serde_json::from_slice(&bytes).map_err(record_error)?;
    segment.validate(head)?;
    Ok((segment, bytes.len()))
}

async fn write_segment(
    layout: &StoreLayout<Store>,
    head: &Head,
    segment: &Segment,
    previous: Option<&Tip>,
) -> Result<Tip> {
    segment.validate(head)?;
    let bytes = serde_json::to_vec(segment).map_err(record_error)?;
    if bytes.len() as u64 > MAX_HISTORY_SEGMENT_BYTES {
        return Err(corrupt("source segment exceeds byte bound"));
    }
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let tip = Tip::append(previous, hash, bytes.len())?;
    layout
        .store()
        .put_if_absent_verified(&layout.capsule_source_segment_path(&tip.hash), bytes.into())
        .await?;
    Ok(tip)
}

async fn read_chain(layout: &StoreLayout<Store>, head: &Head) -> Result<Vec<(String, Segment)>> {
    let Some(tip) = &head.tip else {
        return Ok(Vec::new());
    };
    let mut next = Some(tip.hash.clone());
    let mut remaining = tip.bytes;
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some(hash) = next {
        if records.len() == tip.roots || !seen.insert(hash.clone()) {
            return Err(corrupt("source history cycle/count mismatch"));
        }
        let (segment, bytes) = read_segment(layout, head, &hash, remaining).await?;
        remaining = remaining
            .checked_sub(bytes as u64)
            .ok_or_else(|| corrupt("source history exceeds byte bound"))?;
        next = segment.previous.clone();
        records.push((hash, segment));
    }
    if remaining != 0 || records.len() != tip.roots {
        return Err(corrupt("source history accounting mismatch"));
    }
    Ok(records)
}

/// Retain replaced physical sources before a compacted ref frontier becomes visible.
///
/// Caller owns the edited ref lease and GC writer fences. Per-ref CAS prevents
/// expired publishers from overwriting newer lifetime/pruning authority.
pub async fn retain_capsule_sources(
    layout: &StoreLayout<Store>,
    ref_name: &str,
    generation: u64,
    sources: &[CapsulePointer],
) -> Result<()> {
    if sources.is_empty() {
        return Ok(());
    }
    if !ref_name.starts_with("refs/") || !super::valid_ref_name(ref_name) {
        return Err(corrupt("source retention requires a valid ref name"));
    }
    // Ref-head keys encode the full name. Lifetime heads need only a bounded
    // per-ref identity, so long ref names cannot expand these control records.
    let ref_key = blake3::hash(ref_name.as_bytes()).to_hex().to_string();
    let minimum_generation = admit_generation(layout, generation).await?;
    let path = layout.capsule_source_head_path(&ref_key);
    let previous = load_head(layout, &path).await?;
    let (mut head, etag) = match previous {
        Some((head, etag)) => (head, Some(etag)),
        None => (
            Head {
                version: 1,
                ref_key,
                minimum_generation,
                tip: None,
            },
            None,
        ),
    };
    let mut segment = Segment {
        version: 1,
        ref_key: head.ref_key.clone(),
        generation,
        sources: sources.to_vec(),
        previous: head.tip.as_ref().map(|tip| tip.hash.clone()),
    };
    if let Some(tip) = &head.tip {
        let (old, _) = read_segment(layout, &head, &tip.hash, tip.bytes).await?;
        segment.previous = old.previous.clone();
        if segment == old {
            admit_generation(layout, generation).await?;
            return Ok(());
        }
        segment.previous = Some(tip.hash.clone());
    }
    head.tip = Some(write_segment(layout, &head, &segment, head.tip.as_ref()).await?);
    publish_head(layout, &head, etag).await?;
    // A formerly unseen ref can race pruning after its first floor read.
    // Recheck before publication; GC also excludes bodies below the global floor.
    admit_generation(layout, generation).await?;
    Ok(())
}

/// Authenticate the supplied per-ref lifetime heads and their complete dependency closure.
pub async fn load_retained_capsule_sources(
    layout: &StoreLayout<Store>,
    heads: &[Path],
) -> Result<RetainedCapsuleSources> {
    let mut paths = BTreeSet::new();
    let mut runs = BTreeMap::new();
    let mut roots = 0usize;
    let mut bytes = 0u64;
    let floor = load_floor(layout).await?;
    if floor.is_some() {
        paths.insert(layout.capsule_source_floor_path());
    }
    let minimum_generation = floor.map_or(0, |(floor, _)| floor);
    for path in heads {
        if !paths.insert(path.clone()) {
            continue;
        }
        let (head, _) = load_head(layout, path)
            .await?
            .ok_or_else(|| corrupt("source lifetime head disappeared"))?;
        if head.minimum_generation > minimum_generation {
            return Err(corrupt("source head exceeds the repository pruning floor"));
        }
        if let Some(tip) = &head.tip {
            roots = roots.saturating_add(tip.roots);
            bytes = bytes.saturating_add(tip.bytes);
            if roots > MAX_HISTORY_CHAIN_SEGMENTS || bytes > MAX_HISTORY_CHAIN_BYTES {
                return Err(corrupt("aggregate source history exceeds resource bounds"));
            }
        }
        for (hash, segment) in read_chain(layout, &head).await? {
            paths.insert(layout.capsule_source_segment_path(&hash));
            if segment.generation < minimum_generation {
                continue;
            }
            for source in segment.sources {
                paths.insert(layout.capsule_path(source.hash()));
                if runs
                    .insert(source.hash().to_owned(), source.clone())
                    .is_some_and(|old| old != source)
                {
                    return Err(corrupt(
                        "source histories name conflicting immutable source proofs",
                    ));
                }
            }
        }
    }
    Ok(RetainedCapsuleSources {
        paths: paths.into_iter().collect(),
        runs: runs.into_values().collect(),
    })
}

/// Prune superseded sources only after authoritative history pruning commits.
///
/// Caller holds the exclusive repository GC lease. The persistent generation
/// floor prevents late publishers from reviving explicitly discarded captures.
pub async fn prune_retained_capsule_sources(
    layout: &StoreLayout<Store>,
    minimum_generation: u64,
) -> Result<()> {
    // Record the policy before enumerating heads. This also fences old
    // publishers whose refs have never created a source-lifetime head yet.
    let minimum_generation = advance_floor(layout, minimum_generation).await?;
    let heads = layout
        .store()
        .list_prefix(&layout.capsule_source_heads_prefix())
        .await?;
    for meta in heads {
        let (mut head, etag) = load_head(layout, &meta.location)
            .await?
            .ok_or_else(|| corrupt("source head disappeared during pruning"))?;
        if head.minimum_generation >= minimum_generation {
            continue;
        }
        let records = read_chain(layout, &head).await?;
        head.minimum_generation = minimum_generation;
        let mut tip = None;
        for (_, mut segment) in records
            .into_iter()
            .rev()
            .filter(|(_, segment)| segment.generation >= minimum_generation)
        {
            segment.previous = tip.as_ref().map(|tip: &Tip| tip.hash.clone());
            tip = Some(write_segment(layout, &head, &segment, tip.as_ref()).await?);
        }
        head.tip = tip;
        publish_head(layout, &head, Some(etag)).await?;
    }
    Ok(())
}

fn record_error(source: serde_json::Error) -> MetadataError {
    MetadataError::SourceRetentionRecord { source }
}
fn corrupt(reason: &str) -> MetadataError {
    corrupt_object("capsule source retention", reason)
}

#[cfg(test)]
mod tests;
