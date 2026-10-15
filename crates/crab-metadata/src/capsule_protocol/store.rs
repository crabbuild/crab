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

/// Load and verify one immutable metadata-only layered checkpoint.
pub async fn load_layered_checkpoint(
    router: &StoreLayout<Store>,
    pointer: &super::CheckpointPointer,
) -> Result<LayeredCheckpoint> {
    if pointer.format() != super::layered::CHECKPOINT_VERSION {
        return Err(corrupt(
            &router.capsule_checkpoint_path(pointer.hash()),
            "checkpoint pointer does not name the layered checkpoint format",
        ));
    }
    let path = router.capsule_checkpoint_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let checkpoint = LayeredCheckpoint::decode(bytes)?;
    if !checkpoint.matches_pointer(pointer)? {
        return Err(corrupt(
            &path,
            "layered checkpoint does not match its authenticated pointer",
        ));
    }
    Ok(checkpoint)
}

/// Load and authenticate only the footer of one layered checkpoint.
pub async fn load_layered_checkpoint_control(
    router: &StoreLayout<Store>,
    pointer: &super::CheckpointPointer,
) -> Result<LayeredCheckpoint> {
    if pointer.format() != super::layered::CHECKPOINT_VERSION {
        return Err(corrupt(
            &router.capsule_checkpoint_path(pointer.hash()),
            "checkpoint pointer does not name the layered checkpoint format",
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
    let capsules = control.materialize_capsules_with_external_controls(&detached)?;
    Ok((control, capsules))
}

/// Load complete reference controls while validating admission counts through authenticated pages.
///
/// OID admission remains cold; reference reconciliation still validates complete visibility edits.
pub async fn load_capsule_run_git_control(
    router: &StoreLayout<Store>,
    pointer: &super::CapsulePointer,
) -> Result<(CapsuleRunControl, Vec<CapsuleControl>)> {
    super::root::validate_capsule_pointer(pointer)?;
    let path = router.capsule_path(pointer.hash());
    let start = pointer
        .size()
        .checked_sub(CapsuleRunControl::trailer_bytes())
        .ok_or_else(|| corrupt(&path, "capsule run is shorter than its trailer"))?;
    let trailer = router
        .store()
        .range_get(&path, start..pointer.size())
        .await?;
    let range = CapsuleRunControl::footer_range(&trailer, pointer)?;
    let footer = router.store().range_get(&path, range).await?;
    let control = CapsuleRunControl::decode_footer(footer, pointer)?;
    if let Some(reader) =
        control.admission_reader(router, super::run::MAX_RUN_FOOTER_BYTES as u64)?
    {
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
    let capsules = control.materialize_capsules_with_paged_visibility(router)?;
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
/// heads. Concurrent changes retry within the existing bounded capture policy.
pub async fn capture_ref_heads(
    router: &StoreLayout<Store>,
    root: &super::RepositoryRoot,
) -> Result<(Vec<CapsuleRefHead>, BTreeSet<String>)> {
    for _ in 0..8 {
        let before = list_ref_head_objects(router).await?;
        let head_router = router.clone();
        let loaded = futures_util::stream::iter(before.iter().cloned().map(move |object| {
            let router = head_router.clone();
            async move {
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
                Ok::<_, MetadataError>((head, listed_version_matches(&object, &etag)))
            }
        }))
        .buffer_unordered(32)
        .try_collect::<Vec<_>>()
        .await?;
        if loaded.iter().any(|(_, matched)| !matched) {
            continue;
        }
        // Restore retires old ref heads without deleting them. Filter before
        // resolving activations: retired records cannot authorize catalog data
        // or require dependencies that no longer belong to this root's epoch.
        let mut heads = loaded
            .into_iter()
            .map(|(head, _)| head)
            .filter(|head| head.ref_epoch() == root.ref_epoch())
            .collect::<Vec<_>>();
        heads.sort_unstable_by(|left, right| left.ref_name().cmp(right.ref_name()));
        let active = resolve_referenced_activations(router, &heads).await?;
        let after = list_ref_head_objects(router).await?;
        if before != after {
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
        return Ok((heads, active));
    }
    Err(contract_error(
        "capsule ref snapshot changed during every bounded capture attempt",
    ))
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
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
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
