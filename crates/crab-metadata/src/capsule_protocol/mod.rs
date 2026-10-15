//! Versioned metadata contracts for capsule-protocol repository publication.

use bstr::ByteSlice;

mod browse;
#[cfg(feature = "storage")]
mod browse_retention;
mod capsule;
#[cfg(feature = "storage")]
mod captured_visibility;
#[cfg(feature = "storage")]
mod catalog_capture;
mod chunk_candidates;
mod history;
mod layered;
#[cfg(feature = "storage")]
mod plan;
mod pointer;
mod ref_head;
#[cfg(feature = "storage")]
mod ref_view;
mod root;
mod run;
#[cfg(feature = "storage")]
mod source_retention;
#[cfg(feature = "storage")]
mod store;
mod transaction;
mod transaction_record;
mod visibility;

#[cfg(feature = "storage")]
pub use browse::load_browse_indexes;
pub use browse::{BrowseIndexes, MAX_BROWSE_INDEXES_BYTES};
#[cfg(feature = "remote-index")]
pub use browse_retention::verify_retained_browse_indexes;
#[cfg(feature = "storage")]
pub use browse_retention::{
    RetainedBrowseIndexes, load_retained_browse_indexes, prune_retained_browse_indexes,
    retain_browse_indexes,
};
pub use capsule::{
    Capsule, CapsuleGitPack, CapsuleGitPackDescriptor, CapsuleSection, CapsuleSectionKind,
    CapsuleSectionLocation,
};
#[cfg(feature = "storage")]
pub use captured_visibility::{
    CapturedVisibility, VisibilityReconciliation, apply_transaction_visibility_index,
    visibility_is_ready,
};
#[cfg(feature = "storage")]
pub use catalog_capture::load_pointer_catalog_lookup_from_root;
pub use history::{
    HistorySegment, HistorySegmentPointer, HistorySegmentState, MAX_HISTORY_CHAIN_BYTES,
    MAX_HISTORY_CHAIN_SEGMENTS, MAX_HISTORY_SEGMENT_BYTES,
};
#[cfg(feature = "storage")]
pub use layered::LayeredVisibilityReader;
pub use layered::{
    LayeredCheckpoint, LayeredObjectMember, LayeredVisibilitySnapshot, PackLayer,
    PackMemberDescriptor, PackRange, PackSourceDescriptor, PackSourceKind, source_catalog_digest,
    visibility_object_set_digest,
};
#[cfg(feature = "storage")]
pub use plan::{
    CapsulePlanReceipt, ensure_capsule_plan_unattempted, prepare_capsule_plan,
    publish_capsule_plan_receipt, publish_capsule_plan_repair_receipt, read_capsule_plan_intent,
    resolve_capsule_plan_receipt,
};
#[cfg(feature = "storage")]
pub use pointer::PointerCatalogLookup;
pub use pointer::{
    FileCatalogEntry, MAX_CATALOG_BYTES, PointerCatalog, PointerCatalogAssembly, ShardCatalogEntry,
    XorbCatalogEntry, XorbChunkEntry,
};
pub use ref_head::{
    CAPSULE_REF_COMPACTION_FAN_IN, CAPSULE_REF_ROLLUP_WINDOW, CapsuleRefHead, CapsuleRefState,
    MAX_CAPSULE_REF_FRONTIER, MAX_CAPSULE_REF_HEADS, capsule_ref_name_from_key,
    capsule_ref_name_key,
};
#[cfg(feature = "storage")]
pub use ref_view::{
    RefControlFrontiers, VisibleRefHeads, apply_control_refs, materialize_visible_ref_heads,
    order_ref_controls, select_ref_control_frontiers, select_ref_journal_controls,
};
pub use root::{
    CapsulePointer, CheckpointPointer, GcFence, MAX_CAPSULE_FRONTIER, MAX_ROOT_BYTES,
    RepositoryRoot, RootRecord,
};
pub use run::{
    CapsuleControl, CapsuleControlLocation, CapsuleRun, CapsuleRunAdmission, CapsuleRunControl,
    MAX_CAPSULES_PER_RUN,
};
#[cfg(feature = "storage")]
pub use run::{CapsuleRunAdmissionReader, RunCatalogSource};
#[cfg(feature = "storage")]
pub use source_retention::{
    RetainedCapsuleSources, load_retained_capsule_sources, prune_retained_capsule_sources,
    retain_capsule_sources,
};
#[cfg(feature = "storage")]
pub use store::{
    RootSnapshot, capture_ref_heads, create_root, load_capsule_run, load_capsule_run_control,
    load_capsule_run_git_control, load_history_chain, load_history_segment,
    load_layered_checkpoint, load_layered_checkpoint_control, load_pointer_catalog,
    load_pointer_catalog_from_root, load_root, resolve_referenced_activations,
};
pub use transaction::{CapsuleRefEdit, CapsuleTransaction};
pub use transaction_record::{
    CapsuleTransactionRecord, CapsuleTransactionStatus, MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
};
pub use visibility::CapsuleVisibilityDelta;
#[cfg(feature = "storage")]
pub use visibility::{CapsuleVisibilityReader, VisibilityChange, VisibilityEditHeader};

fn valid_ref_name(name: &str) -> bool {
    gix_validate::reference::name_partial(name.as_bytes().as_bstr()).is_ok()
}

fn valid_ref_namespace<'a>(names: impl IntoIterator<Item = &'a str>) -> bool {
    let names = names.into_iter().collect::<std::collections::BTreeSet<_>>();
    names.iter().all(|name| {
        name.match_indices('/')
            .all(|(index, _)| !names.contains(&name[..index]))
    })
}

pub use chunk_candidates::{
    CapsuleChunkAnchor, CapsuleChunkCandidates, capsule_chunk_anchor_key, capsule_chunk_head_key,
};

#[cfg(feature = "remote-index")]
pub(crate) use chunk_candidates::capsule_chunk_candidate_rows;
