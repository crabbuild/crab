use std::collections::{BTreeMap, BTreeSet};

use crab_storage::{ETag, Store, StoreLayout};
use futures_util::{StreamExt, TryStreamExt};
use object_store::ObjectMeta;

use crate::capsule_protocol::{
    CapsuleControl, CapsuleRefHead, CapsuleRun, CapsuleRunControl, CapsuleSectionKind,
    HistorySegment, HistorySegmentPointer, LayeredCheckpoint, MAX_CAPSULE_REF_HEADS,
    MAX_HISTORY_SEGMENT_BYTES, MAX_ROOT_BYTES, PointerCatalog, RootRecord, capsule_ref_name_key,
};
use crate::error::MetadataError;
use crate::error::Result;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ref_capture_tests;

#[cfg(test)]
mod catalog_order_tests;

/// A verified stored root and the provider token protecting its next update.
#[derive(Debug, Clone)]
pub struct RootSnapshot {
    record: RootRecord,
    etag: ETag,
}

impl RootSnapshot {
    /// Return the immutable generation and ref state captured by this snapshot.
    #[must_use]
    pub fn record(&self) -> &RootRecord {
        &self.record
    }

    /// Return the opaque provider token required for a root CAS update.
    #[must_use]
    pub fn etag(&self) -> &ETag {
        &self.etag
    }

    /// Bind a successful root CAS result to its exact predecessor snapshot.
    pub fn committed_successor(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        let generation = self
            .record
            .root()
            .generation()
            .checked_add(1)
            .ok_or_else(|| contract_error("root generation overflowed"))?;
        if record.root().generation() != generation
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().ref_epoch() != self.record.root().ref_epoch()
        {
            return Err(contract_error(
                "committed root does not directly extend its CAS snapshot",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a checkpoint-only root replacement to its exact CAS predecessor.
    pub fn committed_checkpoint(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() != self.record.root().ref_epoch()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().head() != self.record.root().head()
            || !record.root().capsule_frontier().is_empty()
            || record.root().checkpoint().is_none()
        {
            return Err(contract_error(
                "committed checkpoint root does not replace its exact CAS snapshot",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a checkpoint that folds visible per-ref heads into a new root generation.
    pub fn committed_ref_checkpoint(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        let generation = self
            .record
            .root()
            .generation()
            .checked_add(1)
            .ok_or_else(|| contract_error("root generation overflowed"))?;
        if record.root().generation() != generation
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() != self.record.root().ref_epoch()
            || record.root().head() != self.record.root().head()
            || !record.root().capsule_frontier().is_empty()
            || record.root().checkpoint().is_none()
        {
            return Err(contract_error(
                "committed ref checkpoint does not extend its exact CAS snapshot",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a HEAD-only root replacement to its exact CAS predecessor.
    pub fn committed_head(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() != self.record.root().ref_epoch()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().checkpoint() != self.record.root().checkpoint()
            || record.root().history() != self.record.root().history()
            || record.root().capsule_frontier() != self.record.root().capsule_frontier()
            || record.root().compacted_ref_transactions()
                != self.record.root().compacted_ref_transactions()
            || record.root().gc_fence() != self.record.root().gc_fence()
        {
            return Err(contract_error(
                "committed HEAD root changed published repository state",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a GC fence transition that preserves all logical repository state.
    pub fn committed_maintenance(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() != self.record.root().ref_epoch()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().head() != self.record.root().head()
            || record.root().checkpoint() != self.record.root().checkpoint()
            || record.root().history() != self.record.root().history()
            || record.root().capsule_frontier() != self.record.root().capsule_frontier()
            || record.root().compacted_ref_transactions()
                != self.record.root().compacted_ref_transactions()
        {
            return Err(contract_error(
                "committed maintenance root changed logical repository state",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a fenced history-frontier replacement to its exact CAS predecessor.
    pub fn committed_history(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if self.record.root().gc_fence().is_none()
            || record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() != self.record.root().ref_epoch()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().head() != self.record.root().head()
            || record.root().checkpoint() != self.record.root().checkpoint()
            || record.root().history() == self.record.root().history()
            || record.root().capsule_frontier() != self.record.root().capsule_frontier()
            || record.root().compacted_ref_transactions()
                != self.record.root().compacted_ref_transactions()
            || record.root().gc_fence() != self.record.root().gc_fence()
        {
            return Err(contract_error(
                "committed history root changed non-history repository state",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind a restore fence that preserves state while retiring old ref heads.
    pub fn committed_restore_fence(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        if record.root().generation() != self.record.root().generation()
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() == self.record.root().ref_epoch()
            || record.root().refs() != self.record.root().refs()
            || record.root().peeled_refs() != self.record.root().peeled_refs()
            || record.root().head() != self.record.root().head()
            || record.root().checkpoint() != self.record.root().checkpoint()
            || record.root().history() != self.record.root().history()
            || !record.root().capsule_frontier().is_empty()
            || !record.root().compacted_ref_transactions().is_empty()
            || record.root().gc_fence().is_none()
        {
            return Err(contract_error(
                "committed restore fence changed repository state",
            ));
        }
        Ok(Self { record, etag })
    }

    /// Bind an atomic historical restore to its fenced CAS predecessor.
    pub fn committed_restore(&self, record: RootRecord, etag: ETag) -> Result<Self> {
        let generation = self
            .record
            .root()
            .generation()
            .checked_add(1)
            .ok_or_else(|| contract_error("root generation overflowed"))?;
        if self.record.root().gc_fence().is_none()
            || record.root().generation() != generation
            || record.root().parent_root_digest() != Some(self.record.digest())
            || record.root().repository_id() != self.record.root().repository_id()
            || record.root().ref_epoch() != self.record.root().ref_epoch()
            || record.root().checkpoint().is_none()
            || record.root().history() != self.record.root().history()
            || !record.root().capsule_frontier().is_empty()
            || !record.root().compacted_ref_transactions().is_empty()
            || record.root().gc_fence() != self.record.root().gc_fence()
        {
            return Err(contract_error(
                "committed restore does not replace its exact fenced snapshot",
            ));
        }
        Ok(Self { record, etag })
    }
}

/// Create the first root at an empty v2 publication key.
pub async fn create_root(router: &StoreLayout<Store>, record: RootRecord) -> Result<RootSnapshot> {
    if record.root().generation() != 0 {
        return Err(contract_error(
            "initial stored root must be generation zero",
        ));
    }
    let etag = router
        .store()
        .create_strict_with_etag(&router.capsule_root_path(), record.bytes().clone())
        .await?;
    Ok(RootSnapshot { record, etag })
}

/// Load and verify the single root used by readers and publication CAS.
pub async fn load_root(router: &StoreLayout<Store>) -> Result<RootSnapshot> {
    let (bytes, etag) = router
        .store()
        .get_with_etag_bounded(&router.capsule_root_path(), MAX_ROOT_BYTES)
        .await?;
    Ok(RootSnapshot {
        record: RootRecord::decode(bytes)?,
        etag,
    })
}

/// Load and verify one immutable history segment against its authenticated pointer.
pub async fn load_history_segment(
    router: &StoreLayout<Store>,
    pointer: &HistorySegmentPointer,
) -> Result<HistorySegment> {
    let path = router.capsule_history_segment_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size().min(MAX_HISTORY_SEGMENT_BYTES))
        .await?;
    let segment = HistorySegment::decode(bytes)?;
    let actual = segment.pointer()?;
    if &actual != pointer {
        return Err(corrupt(
            &path,
            "history segment does not match its authenticated pointer",
        ));
    }
    Ok(segment)
}

/// Load an authenticated newest-to-oldest history chain within caller bounds.
pub async fn load_history_chain(
    router: &StoreLayout<Store>,
    newest: &HistorySegmentPointer,
    max_segments: usize,
    max_bytes: u64,
) -> Result<Vec<HistorySegment>> {
    if max_segments == 0 || max_bytes == 0 {
        return Err(contract_error("history chain bounds must be non-zero"));
    }
    let mut pointer = Some(newest.clone());
    let mut hashes = BTreeSet::new();
    let mut total_bytes = 0_u64;
    let mut segments = Vec::new();
    while let Some(current) = pointer {
        if segments.len() == max_segments {
            return Err(contract_error("history chain exceeds its segment limit"));
        }
        if !hashes.insert(current.hash().to_owned()) {
            return Err(contract_error("history chain is cyclic"));
        }
        total_bytes = total_bytes
            .checked_add(current.size())
            .ok_or_else(|| contract_error("history chain byte count overflowed"))?;
        if total_bytes > max_bytes {
            return Err(contract_error("history chain exceeds its byte limit"));
        }
        let segment = load_history_segment(router, &current).await?;
        pointer = segment.previous().cloned();
        segments.push(segment);
    }
    Ok(segments)
}

/// Load and verify a checkpoint's visibility body and external catalog commitment.
///
/// Catalog pages remain cold until lookup or explicit full materialization.
pub async fn load_layered_checkpoint(
    router: &StoreLayout<Store>,
    pointer: &super::CheckpointPointer,
    maximum_bytes: u64,
) -> Result<LayeredCheckpoint> {
    let format_limit = super::layered::checkpoint_maximum_bytes(pointer.format())
        .ok_or_else(|| contract_error("checkpoint pointer format is unsupported"))?;
    let checkpoint_limit = maximum_bytes.min(format_limit);
    if pointer.size() > checkpoint_limit {
        return Err(MetadataError::CheckpointSize {
            bytes: pointer.size(),
            maximum: checkpoint_limit,
        });
    }
    let path = router.capsule_checkpoint_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let pointer = pointer.clone();
    super::decode::run(move || {
        let checkpoint = LayeredCheckpoint::decode(bytes)?;
        if !checkpoint.matches_pointer(&pointer)? {
            return Err(corrupt(
                &path,
                "layered checkpoint does not match its authenticated pointer",
            ));
        }
        let total = checkpoint.encoded_metadata_bytes()?;
        if total > maximum_bytes {
            return Err(MetadataError::CheckpointSize {
                bytes: total,
                maximum: maximum_bytes,
            });
        }
        Ok(checkpoint)
    })
    .await
}

/// Load and authenticate only the footer of one layered checkpoint.
pub async fn load_layered_checkpoint_control(
    router: &StoreLayout<Store>,
    pointer: &super::CheckpointPointer,
) -> Result<LayeredCheckpoint> {
    let format_limit = super::layered::checkpoint_maximum_bytes(pointer.format())
        .ok_or_else(|| contract_error("checkpoint pointer format is unsupported"))?;
    super::root::validate_checkpoint_pointer(pointer)?;
    if pointer.size() > format_limit
        || pointer.control_size() > super::layered::MAX_CHECKPOINT_FOOTER_BYTES as u64 + 48
    {
        return Err(corrupt(
            &router.capsule_checkpoint_path(pointer.hash()),
            "checkpoint control exceeds format byte bounds",
        ));
    }
    // A checkpoint without catalog or visibility sections is entirely footer.
    // Its valid control range starts at zero; only an empty range is invalid.
    if pointer.control_size() == 0 {
        return Err(corrupt(
            &router.capsule_checkpoint_path(pointer.hash()),
            "layered checkpoint pointer does not name a control suffix",
        ));
    }
    let path = router.capsule_checkpoint_path(pointer.hash());
    let bytes = router
        .store()
        .range_get(&path, pointer.control_offset()..pointer.size())
        .await?;
    let checkpoint = LayeredCheckpoint::decode_control(
        bytes,
        pointer.size(),
        pointer.control_offset(),
        pointer.hash(),
        pointer.footer_hash(),
    )?;
    if !checkpoint.matches_pointer(pointer)? {
        return Err(corrupt(
            &path,
            "layered checkpoint control does not match its authenticated pointer",
        ));
    }
    Ok(checkpoint)
}

/// Load and verify one immutable capsule run against its authenticated pointer.
pub async fn load_capsule_run(
    router: &StoreLayout<Store>,
    pointer: &super::CapsulePointer,
) -> Result<CapsuleRun> {
    load_run(router, pointer).await
}

/// Load one run's authenticated footer and control sections without reading its Git payload.
pub async fn load_capsule_run_control(
    router: &StoreLayout<Store>,
    pointer: &super::CapsulePointer,
) -> Result<(CapsuleRunControl, Vec<CapsuleControl>)> {
    super::root::validate_capsule_pointer(pointer)?;
    let path = router.capsule_path(pointer.hash());
    let suffix = router
        .store()
        .range_get(&path, pointer.control_offset()..pointer.size())
        .await?;
    let control = CapsuleRunControl::decode_suffix(
        suffix,
        pointer.size(),
        pointer.hash(),
        pointer.level(),
        pointer.transaction_ids(),
        pointer.newest_base_root_digest(),
    )?;
    if control.footer_hash() != pointer.footer_hash() {
        return Err(corrupt(
            &path,
            "capsule pointer footer hash does not match the authenticated run suffix",
        ));
    }
    let detached = load_detached_control_sections(router, &path, &control).await?;
    super::decode::run(move || {
        let capsules = control.materialize_capsules_with_external_controls(&detached)?;
        Ok((control, capsules))
    })
    .await
}

/// Load complete reference controls while validating admission counts through authenticated pages.
///
/// OID admission stays undecoded; large admission regions remain paged.
/// Reference reconciliation still validates complete visibility edits.
pub async fn load_capsule_run_git_control(
    router: &StoreLayout<Store>,
    pointer: &super::CapsulePointer,
) -> Result<(CapsuleRunControl, Vec<CapsuleControl>)> {
    load_run_git_control(router, pointer, None).await
}

// Catalog capture can coalesce small visibility bodies within admitted headroom.
// Git views retain selected paging; both paths keep payloads cold and authenticate pages.
pub(super) async fn load_run_git_control(
    router: &StoreLayout<Store>,
    pointer: &super::CapsulePointer,
    read_ahead: Option<&std::sync::atomic::AtomicU64>,
) -> Result<(CapsuleRunControl, Vec<CapsuleControl>)> {
    super::root::validate_capsule_pointer(pointer)?;
    let path = router.capsule_path(pointer.hash());
    // A publication leaf can share one control read with its small admission
    // pages. Compacted histories retain selected-member paging.
    let prefetch = pointer.level() == 0 && pointer.control_size() <= crate::pages::READ_AHEAD_BYTES;
    let start = if prefetch {
        pointer.control_offset()
    } else {
        pointer
            .size()
            .checked_sub(CapsuleRunControl::trailer_bytes())
            .ok_or_else(|| corrupt(&path, "capsule run is shorter than its trailer"))?
    };
    let body = router
        .store()
        .range_get(&path, start..pointer.size())
        .await?;
    if body.len() as u64 != pointer.size() - start {
        return Err(corrupt(&path, "capsule control read is truncated"));
    }
    let trailer_start = body
        .len()
        .saturating_sub(CapsuleRunControl::trailer_bytes() as usize);
    let range = CapsuleRunControl::footer_range(&body[trailer_start..], pointer)?;
    let (footer, admission) = if prefetch {
        let offset = usize::try_from(range.start - start)
            .map_err(|_| corrupt(&path, "capsule footer offset cannot be represented"))?;
        let admission_start = usize::try_from(pointer.control_offset() - start)
            .map_err(|_| corrupt(&path, "capsule admission offset cannot be represented"))?;
        (
            body.slice(offset..),
            Some(body.slice(admission_start..offset)),
        )
    } else {
        (router.store().range_get(&path, range).await?, None)
    };
    let control = CapsuleRunControl::decode_footer(footer, pointer)?;
    if let Some(mut reader) =
        control.admission_reader(router, super::run::MAX_RUN_FOOTER_BYTES as u64)?
    {
        if let Some(body) = admission {
            reader.prefetch(body)?;
        }
        let counts = reader.counts(router.store()).await?;
        if counts.len() != control.git_packs().len()
            || counts
                .iter()
                .zip(control.git_packs())
                .any(|(count, member)| *count != member.object_count())
        {
            return Err(corrupt(
                &path,
                "capsule member counts do not match its authenticated pack directory",
            ));
        }
    }
    let capsules = control.materialize_capsules_with_paged_visibility(router, read_ahead)?;
    Ok((control, capsules))
}

async fn load_detached_control_sections(
    router: &StoreLayout<Store>,
    path: &object_store::path::Path,
    control: &CapsuleRunControl,
) -> Result<BTreeMap<(String, CapsuleSectionKind), bytes::Bytes>> {
    let ranges = control
        .capsule_locations()
        .iter()
        .flat_map(|location| location.detached_controls())
        .collect::<Vec<_>>();
    futures_util::stream::iter(ranges.into_iter().map(|(hash, kind, range)| async move {
        let end = range
            .offset()
            .checked_add(range.length())
            .ok_or_else(|| corrupt(path, "detached capsule control range overflows"))?;
        let bytes = router.store().range_get(path, range.offset()..end).await?;
        if bytes.len() as u64 != range.length()
            || blake3::hash(&bytes).to_hex().as_str() != range.blake3()
        {
            return Err(corrupt(
                path,
                "detached capsule control range does not match its commitment",
            ));
        }
        Ok(((hash, kind), bytes))
    }))
    .buffer_unordered(8)
    .try_collect()
    .await
}

/// Load the complete authenticated pointer catalog named by one v2 root.
pub async fn load_pointer_catalog(router: &StoreLayout<Store>) -> Result<PointerCatalog> {
    let snapshot = load_root(router).await?;
    load_pointer_catalog_from_root(router, &snapshot).await
}

/// Load the complete pointer catalog from an already verified v2 root.
pub async fn load_pointer_catalog_from_root(
    router: &StoreLayout<Store>,
    snapshot: &RootSnapshot,
) -> Result<PointerCatalog> {
    super::catalog_capture::materialize_from_root(router, snapshot).await
}

/// Capture one stable, epoch-filtered ref-head view and its committed activations.
///
/// Rejects mismatched object identities, activation records and uncommitted base
/// heads. Concurrent changes retry with bounded backoff on a Tokio runtime.
pub async fn capture_ref_heads(
    router: &StoreLayout<Store>,
    root: &super::RepositoryRoot,
) -> Result<(Vec<CapsuleRefHead>, BTreeSet<String>)> {
    // Read-only callers need heads and activations without retaining provider
    // receipts. Both APIs share capture and authority validation.
    let snapshot = capture_ref_head_snapshot(router, root).await?;
    Ok((snapshot.heads, snapshot.active))
}

/// Load individually observed ref heads for push discovery.
///
/// This is not a stable reader snapshot. Publishers must resolve activations,
/// re-read edited heads, validate expected old values and publish through CAS.
pub async fn load_ref_heads_for_push(
    router: &StoreLayout<Store>,
    root: &super::RepositoryRoot,
) -> Result<Vec<CapsuleRefHead>> {
    let origin = super::ref_head::cache::origin(router);
    let cached = load_checkpoint_ref_heads(router, root, origin.as_deref()).await?;
    let objects = list_ref_head_objects(router).await?;
    Ok(load_ref_head_bodies(router, &objects, &cached)
        .await?
        .into_iter()
        .map(|(_, head, _)| head)
        .collect())
}

/// Stable ref-head bodies and provider tokens; activation outcomes are never persisted.
pub struct RefHeadSnapshot {
    heads: Vec<CapsuleRefHead>,
    active: BTreeSet<String>,
    identities: Vec<ETag>,
    repository_id: String,
    ref_epoch: String,
    origin: Option<String>,
}

impl RefHeadSnapshot {
    /// Return the validated current-epoch heads in canonical ref-name order.
    #[must_use]
    pub fn heads(&self) -> &[CapsuleRefHead] {
        &self.heads
    }

    /// Return freshly resolved committed activations for this capture only.
    #[must_use]
    pub fn active(&self) -> &BTreeSet<String> {
        &self.active
    }

    /// Encode a bounded checkpoint cache without preserving activation outcomes.
    pub fn checkpoint_cache(&self) -> Result<Option<super::RefHeadCache>> {
        let Some(origin) = &self.origin else {
            return Ok(None);
        };
        super::RefHeadCache::capture(
            origin.clone(),
            &self.repository_id,
            &self.ref_epoch,
            self.heads
                .iter()
                .zip(&self.identities)
                .map(|(head, identity)| {
                    (head, identity.e_tag.as_deref(), identity.version.as_deref())
                }),
        )
    }
}

/// Capture a stable inventory, reusing only checkpoint bodies with matching physical identities.
pub async fn capture_ref_head_snapshot(
    router: &StoreLayout<Store>,
    root: &super::RepositoryRoot,
) -> Result<RefHeadSnapshot> {
    // Retain only the preceding attempt's bodies. A provider identity from the
    // next listing must still match before a body can be reused. Empty tokens
    // and S3's mutable null version do not identify content; always GET those
    // bodies again unless an ETag identifies them. Activations are never cached.
    let origin = super::ref_head::cache::origin(router);
    let mut previous = load_checkpoint_ref_heads(router, root, origin.as_deref()).await?;
    let mut before = list_ref_head_objects(router).await?;
    for attempt in 0_u32..32 {
        // Independent writers need not settle within eight short probes. Keep
        // a finite retry bound and reuse the preceding final inventory; success
        // still needs matching identities and freshly resolved activations.
        if attempt > 0 {
            let delay = std::time::Duration::from_millis(10_u64 << (attempt - 1).min(4));
            tokio::time::sleep(delay).await;
        }
        let loaded = load_ref_head_bodies(router, &before, &previous).await?;
        let matched = loaded
            .iter()
            .all(|(object, _, etag)| listed_version_matches(object, etag));
        previous = loaded
            .into_iter()
            .map(|(object, head, etag)| (object.location, (head, etag)))
            .collect();
        if !matched {
            before = list_ref_head_objects(router).await?;
            continue;
        }
        // Restore retires old ref heads without deleting them. Filter before
        // resolving activations: retired records cannot authorize catalog data
        // or require dependencies that no longer belong to this root's epoch.
        let mut heads = previous
            .values()
            .map(|(head, _)| head)
            .filter(|head| head.ref_epoch() == root.ref_epoch())
            .cloned()
            .collect::<Vec<_>>();
        heads.sort_unstable_by(|left, right| left.ref_name().cmp(right.ref_name()));
        let active = resolve_referenced_activations(router, &heads).await?;
        let after = list_ref_head_objects(router).await?;
        if before != after {
            before = after;
            continue;
        }
        for head in &heads {
            let visible = head.visible(&active);
            let base_oid = root.refs().get(head.ref_name()).map(String::as_str);
            if visible.transaction_id().is_none() && visible.oid() != base_oid {
                return Err(corrupt(
                    &router.capsule_ref_head_path(&capsule_ref_name_key(head.ref_name())),
                    "capsule ref head without a transaction differs from the compacted root",
                ));
            }
        }
        let identities = heads
            .iter()
            .map(|head| {
                previous
                    .remove(&router.capsule_ref_head_path(&capsule_ref_name_key(head.ref_name())))
                    .map(|(_, identity)| identity)
                    .ok_or_else(|| contract_error("captured head has no provider receipt"))
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(RefHeadSnapshot {
            heads,
            active,
            identities,
            repository_id: root.repository_id().to_owned(),
            ref_epoch: root.ref_epoch().to_owned(),
            origin,
        });
    }
    Err(contract_error(
        "capsule ref snapshot changed during every bounded capture attempt",
    ))
}

type RefHeadBodies = BTreeMap<object_store::path::Path, (CapsuleRefHead, ETag)>;

async fn load_checkpoint_ref_heads(
    router: &StoreLayout<Store>,
    root: &super::RepositoryRoot,
    origin: Option<&str>,
) -> Result<RefHeadBodies> {
    let cached = match (
        root.checkpoint()
            .and_then(|pointer| pointer.ref_heads.as_ref()),
        origin,
    ) {
        (Some(cache), Some(origin)) if cache.origin == origin => {
            let checkpoint = root
                .checkpoint()
                .ok_or_else(|| contract_error("ref-head cache has no checkpoint"))?;
            cache.validate(checkpoint.control_offset())?;
            let range = &cache.range;
            let bytes = router
                .store()
                .range_get(
                    &router.capsule_checkpoint_path(checkpoint.hash()),
                    range.offset()..range.offset() + range.length(),
                )
                .await?;
            cache.verify(bytes)?.heads(router, root)?
        }
        _ => BTreeMap::new(),
    };
    Ok(cached)
}

async fn load_ref_head_bodies(
    router: &StoreLayout<Store>,
    objects: &[ObjectMeta],
    cached: &RefHeadBodies,
) -> Result<Vec<(ObjectMeta, CapsuleRefHead, ETag)>> {
    futures_util::stream::iter(objects.iter().cloned().map(|object| {
        let cached = cached
            .get(&object.location)
            .filter(|(_, etag)| {
                super::ref_head::cache::identifies_body(
                    object.e_tag.as_deref(),
                    object.version.as_deref(),
                ) && listed_version_matches(&object, etag)
            })
            .cloned();
        async move {
            let (head, etag) = match cached {
                Some(cached) => cached,
                None => {
                    let (bytes, etag) = router.store().get_with_etag(&object.location).await?;
                    let head = CapsuleRefHead::decode(&bytes)?;
                    if router.capsule_ref_head_path(&capsule_ref_name_key(head.ref_name()))
                        != object.location
                    {
                        return Err(corrupt(
                            &object.location,
                            "capsule ref-head key does not match its ref name",
                        ));
                    }
                    (head, etag)
                }
            };
            Ok((object, head, etag))
        }
    }))
    .buffer_unordered(32)
    .try_collect()
    .await
}

async fn list_ref_head_objects(router: &StoreLayout<Store>) -> Result<Vec<ObjectMeta>> {
    let mut objects = router
        .store()
        .list_prefix_bounded(&router.capsule_ref_heads_prefix(), MAX_CAPSULE_REF_HEADS)
        .await?
        .ok_or_else(|| contract_error("capsule ref-head limit exceeded"))?;
    objects.sort_unstable_by(|left, right| left.location.cmp(&right.location));
    Ok(objects)
}

fn listed_version_matches(object: &ObjectMeta, etag: &ETag) -> bool {
    object
        .e_tag
        .as_ref()
        .is_none_or(|listed| etag.e_tag.as_ref() == Some(listed))
        && object
            .version
            .as_ref()
            .is_none_or(|listed| etag.version.as_ref() == Some(listed))
}

/// Validate referenced activation records and select those committed for these heads.
///
/// Callers filter retired epochs before resolving activations so stale heads
/// cannot introduce authority or demand retired dependencies.
pub async fn resolve_referenced_activations(
    router: &StoreLayout<Store>,
    heads: &[CapsuleRefHead],
) -> Result<BTreeSet<String>> {
    let referenced = heads
        .iter()
        .filter_map(CapsuleRefHead::prepared_activation_id)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    futures_util::stream::iter(referenced.into_iter().map(|activation_id| async move {
        let path = router.capsule_transaction_path(&activation_id);
        let (body, _) = router
            .store()
            .get_with_etag_bounded(&path, super::MAX_CAPSULE_TRANSACTION_RECORD_BYTES)
            .await?;
        let record = super::CapsuleTransactionRecord::decode(&body)?;
        if record.activation_id() != activation_id {
            return Err(corrupt(
                &path,
                "transaction record does not match its activation key",
            ));
        }
        if heads.iter().any(|head| {
            head.prepared_activation_id() == Some(activation_id.as_str())
                && head
                    .visible(&BTreeSet::from([activation_id.clone()]))
                    .transaction_id()
                    != Some(record.transaction_id())
        }) {
            return Err(corrupt(
                &path,
                "transaction record does not match its prepared ref heads",
            ));
        }
        Ok::<_, MetadataError>((
            activation_id,
            record.status() == super::CapsuleTransactionStatus::Committed,
        ))
    }))
    .buffer_unordered(32)
    .try_filter_map(
        |(activation_id, committed)| async move { Ok(committed.then_some(activation_id)) },
    )
    .try_collect()
    .await
}

async fn load_run(
    router: &StoreLayout<Store>,
    pointer: &super::CapsulePointer,
) -> Result<CapsuleRun> {
    super::root::validate_capsule_pointer(pointer)?;
    let path = router.capsule_path(pointer.hash());
    // Complete views still need resident capsule bytes. Keep their transport
    // on the shared range engine so a large seed cannot serialize the clone.
    let bytes = router
        .store()
        .get_immutable_bounded(&path, pointer.size())
        .await?;
    let pointer = pointer.clone();
    super::decode::run(move || {
        let run = CapsuleRun::decode(bytes)?;
        if run.hash() != pointer.hash()
            || run.bytes().len() as u64 != pointer.size()
            || run.level() != pointer.level()
            || run.transaction_ids() != pointer.transaction_ids()
            || run.newest_base_root_digest() != pointer.newest_base_root_digest()
            || run.control_offset() != pointer.control_offset()
            || run.control_size() != pointer.control_size()
            || run.footer_hash() != pointer.footer_hash()
        {
            return Err(corrupt(
                &path,
                "capsule run does not match its authenticated pointer",
            ));
        }
        Ok(run)
    })
    .await
}

fn corrupt(path: &object_store::path::Path, reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: path.to_string(),
        reason: reason.into(),
    }
}

fn contract_error(reason: impl Into<String>) -> crate::error::MetadataError {
    crate::error::MetadataError::CapsuleContract {
        record: "stored root",
        reason: reason.into(),
    }
}
