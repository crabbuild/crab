//! Versioned metadata contracts for capsule-protocol repository publication.

use bstr::ByteSlice;

mod capsule;
mod checkpoint;
mod history;
mod layered;
#[cfg(feature = "storage")]
mod plan;
mod pointer;
mod ref_head;
mod root;
mod run;
#[cfg(feature = "storage")]
mod store;
mod transaction;
mod transaction_record;
mod visibility;

pub use capsule::{
    Capsule, CapsuleGitPack, CapsuleGitPackDescriptor, CapsuleSection, CapsuleSectionKind,
    CapsuleSectionLocation,
};
pub use checkpoint::{Checkpoint, CheckpointControl};
pub use history::{
    HistorySegment, HistorySegmentPointer, HistorySegmentState, MAX_HISTORY_CHAIN_BYTES,
    MAX_HISTORY_CHAIN_SEGMENTS, MAX_HISTORY_SEGMENT_BYTES,
};
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
pub use pointer::{
    FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
pub use ref_head::{
    CAPSULE_REF_COMPACTION_FAN_IN, CapsuleRefHead, CapsuleRefState, MAX_CAPSULE_REF_FRONTIER,
    MAX_CAPSULE_REF_HEADS, capsule_ref_name_from_key, capsule_ref_name_key,
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
pub use store::{
    RootSnapshot, create_root, load_capsule_run, load_capsule_run_control, load_checkpoint,
    load_checkpoint_control, load_history_chain, load_history_segment, load_layered_checkpoint,
    load_layered_checkpoint_control, load_pointer_catalog, load_pointer_catalog_from_root,
    load_root,
};
pub use transaction::{CapsuleRefEdit, CapsuleTransaction};
pub use transaction_record::{
    CapsuleTransactionRecord, CapsuleTransactionStatus, MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
};
pub use visibility::{CapsuleVisibilityDelta, CapsuleVisibilitySnapshot};

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
