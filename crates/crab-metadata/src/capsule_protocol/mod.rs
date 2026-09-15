//! Versioned metadata contracts for capsule-protocol repository publication.

use bstr::ByteSlice;

mod capsule;
mod checkpoint;
mod pointer;
mod root;
mod run;
#[cfg(feature = "storage")]
mod store;
mod transaction;
mod visibility;

pub use capsule::{
    Capsule, CapsuleGitPack, CapsuleGitPackDescriptor, CapsuleSection, CapsuleSectionKind,
    CapsuleSectionLocation,
};
pub use checkpoint::Checkpoint;
pub use pointer::{
    FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
};
pub use root::{
    CapsulePointer, CheckpointPointer, GcFence, MAX_CAPSULE_FRONTIER, MAX_ROOT_BYTES,
    RepositoryRoot, RootRecord,
};
pub use run::{CapsuleRun, MAX_CAPSULES_PER_RUN};
#[cfg(feature = "storage")]
pub use store::{RootSnapshot, create_root, load_pointer_catalog, load_root};
pub use transaction::{CapsuleRefEdit, CapsuleTransaction};
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
