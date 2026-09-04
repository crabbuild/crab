//! Low-dependency Git contracts and repository mechanics for Crab.

pub mod delta;
pub mod discover;
#[cfg(feature = "facade")]
pub mod facade;
pub mod filter_attr_cache;
pub mod incoming_pack;
pub mod lfs_pointer;
pub mod odb_adapter;
pub mod pack;
pub mod pack_locator;
pub mod pointer_detect;
pub mod pointer_ref;
pub mod push_state;
pub mod ref_resolve;
pub mod refname;
pub mod reject_reason;
pub mod repack;
pub mod tag;
pub mod url;
pub mod walk;
pub mod worktree;

pub use filter_attr_cache::{FilterAttrCache, FilterEntry, FilterKind, collect_all_entries};
pub use lfs_pointer::{
    LFS_VERSION_URL, LfsExtension, LfsPointer, LfsPointerError, MAX_LFS_POINTER_SIZE,
};
pub use odb_adapter::{CrabOdb, NoopXorbResolver, OdbError, XorbBlobResolver};
pub use pack::{
    InstalledPack, PackError, initialize_bare_git_dir, install_pack_file_from_path,
    object_kinds_from_git_dir, verify_pack_sha1,
};
pub use pack_locator::{
    PackKindMetadataIter, PackLocationIter, PackLocatorError, PackObjectLocation,
    decode_pack_kind_metadata, decode_pack_kind_metadata_iter, encode_pack_kind_metadata,
    max_pack_index_size, pack_kind_metadata_size, pack_reverse_index_size,
    validate_pack_kind_metadata, write_pack_reverse_index,
};
pub use pointer_detect::{PointerKind, classify};
pub use pointer_ref::{PointerRefError, resolve_pointer_commit, resolve_pointer_ref};
pub use push_state::PushState;
pub use refname::{RefNameError, validate_push_refname};
pub use reject_reason::FetchRejectReason;
pub use tag::{AnnotatedTagRef, TagPeelError, annotated_tag_refs_at, peeled_tag_refs_at};
pub use url::{
    AzureStorageTarget, Cloud, CrabUrl, DirectRepository, ManagedRepository, ObjectUrl,
    RepositoryLocator, RepositoryUrl, UrlError, UrlForm, normalize_repository_bucket,
    normalize_repository_prefix,
};
pub use walk::{
    PointerBlob, ReachableSet, WalkError, walk_reachable, walk_reachable_bounded,
    walk_reachable_by_ref, walk_reachable_by_ref_bounded,
};
