//! Versioned metadata contracts for request-minimal repository publication.

mod capsule;
mod checkpoint;
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
pub use root::{
    CapsulePointer, CheckpointPointer, GcFence, MAX_CAPSULE_FRONTIER, MAX_ROOT_BYTES,
    RepositoryRoot, RootRecord,
};
pub use run::{CapsuleRun, MAX_CAPSULES_PER_RUN};
#[cfg(feature = "storage")]
pub use store::{RootSnapshot, create_root, load_root};
pub use transaction::{CapsuleRefEdit, CapsuleTransaction};
