//! Bounded, filesystem-free Git repository reads from Crab object storage.

mod budget;
mod cache;
mod commit_graph;
mod delta;
mod error;
mod metrics;
mod objects;
mod operation;
mod pack;
mod path;
mod reader;
mod refs;
mod repository;
mod revision;
mod runtime;
mod snapshot;
mod state;
mod traversal;
mod visibility;

pub use budget::BudgetDimension;
pub use crab_metadata::git_visibility::{GitCatalogVisibilityIndex, GitVisibilityIndex};
pub use error::{
    BlameUnsupportedReason, CorruptionStage, CursorError, DeltaCorruption, Error,
    InflatedEntryError, PathError, RepositoryDiagnostic, RepositoryStateError, Result,
    RevisionError,
};
pub use metrics::{
    CacheOutcome, MetricKind, MetricObservation, MetricOutcome, NoopMetrics, RemoteGitMetrics,
};
pub use objects::{
    AnnotatedTag, Blob, BlobMetadata, Commit, ContentClassification, EntryKind, EntryMode,
    Signature, SignatureHeader, TreeEntry,
};
pub use operation::{OperationContext, OperationKind, ShallowClosureSelection};
pub use pack::{
    GENERATED_PACK_CACHE_VERSION, GeneratedPack, GeneratedPackCacheKey, GeneratedPackLease,
    GeneratedPackLeaseAttempt,
    GeneratedPackLeaseError, GeneratedPackLeaseProvider, GeneratedPackRequestCacheError,
    GeneratedPackRequestCacheKey,
};
pub use path::GitPath;
pub use reader::{RemoteGitObject, RemoteGitObjectMetadata};
pub use refs::{HeadReference, RepositoryRef, RepositoryRefs};
pub use repository::RemoteGitRepository;
pub use repository::{ObjectLimits, OperationLimits, RepositoryIdentity, RepositoryOptions};
pub use revision::{ResolvedRevision, Revision};
pub use runtime::{RemoteGitRuntime, RuntimeOptions, RuntimeSnapshot};
pub use snapshot::RemoteGitSnapshot;
pub use traversal::{
    ArchiveEntry, ArchiveStream, Blame, BlameRange, ChangeKind, Comparison, Diff,
    DiffClassification, DiffHunk, DirectoryMetadata, HistoryTraversal, Page, PageCursor,
    PageRequest, PathHistoryEntry, Submodule, Symlink, TreeChange,
};
