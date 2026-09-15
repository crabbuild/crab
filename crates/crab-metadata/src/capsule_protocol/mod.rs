//! Versioned metadata contracts for capsule-protocol repository publication.

mod capsule;
mod checkpoint;
mod pointer;
mod root;
mod run;
#[cfg(feature = "storage")]
mod store;
mod transaction;

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
