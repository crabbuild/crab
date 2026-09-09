//! Preview Rust SDK for remote, local, and managed Crab repository workflows.
//!
//! The API is implemented but unpublished while mandatory backend and release
//! qualification gates run.

#[cfg(feature = "remote")]
mod archive;
#[cfg(feature = "remote")]
pub use archive::{ArchiveEvent, ArchiveStream, ContentMode};
#[cfg(feature = "remote")]
mod client;
#[cfg(feature = "managed")]
mod managed;
#[cfg(feature = "managed")]
pub use managed::{
    ManagedOptions, ManagedPage, ManagedRepositories, ManagedRepository, ManagedRepositoryState,
};
#[cfg(feature = "local")]
mod local;
#[cfg(feature = "local")]
pub use local::{
    CheckoutOptions, CloneOptions, ConflictState, FetchDepth, FetchOptions, FetchOutcome,
    HydrationState, IntegrationId, IntegrationKind, LocalCommitOptions, LocalConfiguration,
    LocalExecutionPolicy, LocalPushOutcome, LocalPushRecoveryToken, LocalRepository, LocalSnapshot,
    LocalStatus, LocalTools, PreparedPush, PullMode, PullOptions, PullOutcome, PushOptions,
    PushRefspec, StageOutcome, StatusEntry,
};
#[cfg(feature = "write")]
mod mutation;
#[cfg(feature = "write")]
pub use client::mutation::PreparedMutation;
#[cfg(feature = "write")]
pub use mutation::{
    CommitIdentity, CommitOptions, CommitReceipt, FileEdit, MutationOutcome, Readiness,
    RecoveryToken, RefBatch, RefRejection, RefUpdate, WritePolicy,
};
#[cfg(feature = "remote")]
mod request;
#[cfg(feature = "remote")]
pub use request::Request;
mod store_options;
pub use store_options::{AzureOptions, DirectStoreOptions, GcsOptions, S3Options};
mod builder;
pub use builder::ClientBuilder;
#[cfg(feature = "remote")]
mod content;
mod error;
#[cfg(feature = "remote")]
mod read_limits;
#[cfg(feature = "remote")]
mod read_types;
#[cfg(feature = "remote")]
pub use read_limits::ReadLimits;
#[cfg(feature = "remote")]
mod operation_options;
#[cfg(feature = "remote")]
mod progress;
#[cfg(feature = "remote")]
pub use progress::{Progress, ProgressEvent, ProgressReceiver, ProgressUpdate};
#[cfg(feature = "remote")]
mod remote_error;
#[cfg(feature = "remote")]
mod runtime;
#[cfg(feature = "remote")]
pub use operation_options::{Cancellation, OperationOptions};
#[cfg(feature = "remote")]
mod stream;
#[cfg(feature = "content")]
pub use content::ContentCache;
#[cfg(feature = "remote")]
pub use content::ContentStream;

#[cfg(feature = "remote")]
pub use client::{
    Client, ReadOptions, Reference, References, RemoteRepository, RepositoryCapability, Snapshot,
};
#[cfg(feature = "remote")]
pub use read_types::{
    Blame, BlameRange, Commit, Diff, DiffClassification, DiffHunk, EntryMode, HistoryTraversal,
    Page, PageCursor, PageRequest, Signature, SignatureHeader, TreeEntry,
};
mod value;

pub use error::{Error, ErrorKind, OperationId, Result};
pub use value::{GitPath, HashAlgorithm, ObjectId, RepositoryLocator, Revision};
