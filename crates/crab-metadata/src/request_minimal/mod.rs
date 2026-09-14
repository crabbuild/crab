//! Versioned metadata contracts for request-minimal repository publication.

mod capsule;
mod root;
#[cfg(feature = "storage")]
mod store;
mod transaction;

pub use capsule::{Capsule, CapsuleSection, CapsuleSectionKind, CapsuleSectionLocation};
pub use root::{
    CapsulePointer, CheckpointPointer, MAX_CAPSULE_FRONTIER, MAX_ROOT_BYTES, RepositoryRoot,
    RootRecord,
};
#[cfg(feature = "storage")]
pub use store::{RootSnapshot, create_root, load_root};
pub use transaction::{CapsuleRefEdit, CapsuleTransaction};
