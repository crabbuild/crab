//! Preview Rust SDK for remote, local, and managed Crab repository workflows.
//!
//! Open existing repositories through [`Client::open`] and select the
//! repository's [`remote`] or [`local`] interface. The API is unpublished while
//! mandatory backend and release qualification gates run.

#[cfg(feature = "remote")]
mod archive;
mod builder;
#[cfg(feature = "remote")]
mod client;
#[cfg(feature = "remote")]
mod content;
mod error;
#[cfg(feature = "local")]
#[path = "local.rs"]
mod local_impl;
#[cfg(feature = "managed")]
#[path = "managed.rs"]
mod managed_impl;
#[cfg(feature = "write")]
mod mutation;
#[cfg(feature = "remote")]
mod operation_options;
#[cfg(feature = "remote")]
mod progress;
#[cfg(feature = "remote")]
mod read_limits;
#[cfg(feature = "remote")]
mod read_types;
#[cfg(feature = "remote")]
mod remote_error;
#[cfg(feature = "remote")]
mod repository;
#[cfg(feature = "remote")]
mod request;
#[cfg(feature = "remote")]
mod runtime;
mod store_options;
#[cfg(feature = "remote")]
mod stream;
mod value;

#[cfg(feature = "local")]
#[path = "api/local.rs"]
pub mod local;
#[cfg(feature = "managed")]
#[path = "api/managed.rs"]
pub mod managed;
#[cfg(feature = "remote")]
#[path = "api/operation.rs"]
pub mod operation;
#[cfg(feature = "remote")]
#[path = "api/remote.rs"]
pub mod remote;
#[path = "api/storage.rs"]
pub mod storage;

pub use builder::ClientBuilder;
#[cfg(feature = "remote")]
pub use client::Client;
pub use error::{Error, ErrorKind, Result};
#[cfg(feature = "write")]
pub use mutation::WritePolicy;
#[cfg(feature = "remote")]
pub use repository::{OpenOptions, Repository, RepositoryMode};
pub use value::{GitPath, HashAlgorithm, ObjectId, RepositoryLocator, Revision};

// Internal paths stay concise while the only public paths are the task
// namespaces above.
#[cfg(feature = "remote")]
pub(crate) use archive::{ArchiveStream, ContentMode};
#[cfg(feature = "remote")]
pub(crate) use client::ReadOptions;
#[cfg(all(feature = "write", test))]
pub(crate) use client::RepositoryCapability;
#[cfg(feature = "content")]
pub(crate) use content::ContentCache;
#[cfg(feature = "remote")]
pub(crate) use content::ContentStream;
#[cfg(feature = "remote")]
pub(crate) use error::OperationId;
#[cfg(feature = "local")]
pub(crate) use local_impl::{LocalExecutionPolicy, LocalRepository, LocalTools};
#[cfg(feature = "managed")]
pub(crate) use managed_impl::ManagedOptions;
#[cfg(any(feature = "local", all(feature = "write", test)))]
pub(crate) use mutation::CommitIdentity;
#[cfg(feature = "write")]
pub(crate) use mutation::{
    CommitOptions, CommitReceipt, FileEdit, MutationOutcome, Readiness, RecoveryToken, RefBatch,
    RefRejection, RefUpdate,
};
#[cfg(all(feature = "remote", test))]
pub(crate) use operation_options::Cancellation;
#[cfg(feature = "remote")]
pub(crate) use operation_options::OperationOptions;
#[cfg(all(feature = "remote", test))]
pub(crate) use progress::ProgressEvent;
#[cfg(feature = "remote")]
pub(crate) use progress::{Progress, ProgressUpdate};
#[cfg(feature = "remote")]
pub(crate) use read_limits::ReadLimits;
#[cfg(feature = "write")]
pub(crate) use read_types::EntryMode;
#[cfg(feature = "remote")]
pub(crate) use read_types::{Blame, Commit, Diff, HistoryTraversal, Page, PageRequest, TreeEntry};
#[cfg(feature = "remote")]
pub(crate) use request::Request;
pub(crate) use store_options::DirectStoreOptions;
