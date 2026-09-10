//! Remote repository reads and publication without a local worktree.

#[cfg(feature = "write")]
use std::path::PathBuf;

pub use crate::archive::{ArchiveEvent, ArchiveStream, ContentMode};
pub use crate::client::{Reference, References, RepositoryCapability as Capability, Snapshot};
#[cfg(feature = "content")]
pub use crate::content::ContentStream;
pub use crate::read_types::{
    Blame, BlameRange, Commit, Diff, DiffClassification, DiffHunk, EntryMode, HistoryTraversal,
    Page, PageCursor, PageRequest, Signature, SignatureHeader, TreeEntry,
};

#[cfg(feature = "write")]
pub mod write {
    //! Remote commit and atomic ref-publication contracts.

    pub use crate::client::mutation::PreparedMutation;
    pub use crate::mutation::{
        CommitIdentity, CommitOptions, CommitReceipt, FileEdit, MutationOutcome, Readiness,
        RecoveryToken, RefBatch, RefRejection, RefUpdate,
    };
}

/// Borrowed operations for one opened remote repository generation.
#[derive(Clone, Copy)]
pub struct Remote<'a> {
    pub(crate) owner: &'a crate::client::RemoteRepository,
}

impl<'a> Remote<'a> {
    /// Inspect implemented operation families without I/O or refreshing state.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        self.owner.capabilities()
    }

    /// Return this handle's pinned refs without refreshing their generation.
    pub fn refs(&self) -> crate::operation::Request<'a, References, crate::operation::Options> {
        self.owner.refs()
    }

    /// Capture the current generation without changing this repository or its snapshots.
    pub fn refresh(
        &self,
    ) -> crate::operation::Request<'a, crate::Repository, crate::operation::Options> {
        self.owner.refresh().map(crate::Repository::from_remote)
    }

    /// Resolve a reachable commit from this repository's captured generation.
    pub fn snapshot(
        &self,
        revision: crate::Revision,
    ) -> crate::operation::Request<'a, Snapshot, crate::operation::Options> {
        self.owner.snapshot(revision)
    }

    /// Prepare a commit and branch update without a local worktree or Git executable.
    #[cfg(feature = "write")]
    pub fn prepare_commit(
        &self,
        commit: write::CommitOptions,
        edits: Vec<write::FileEdit>,
        scratch: PathBuf,
    ) -> crate::operation::Request<'a, write::PreparedMutation, crate::operation::Options> {
        let owner = self.owner;
        crate::Request::new(move |options| {
            Box::pin(owner.prepare_commit(commit, edits, scratch, options))
        })
    }

    /// Prepare an atomic ref batch using already available remote objects.
    #[cfg(feature = "write")]
    pub fn prepare_ref_update(
        &self,
        batch: write::RefBatch,
        scratch: PathBuf,
    ) -> crate::operation::Request<'a, write::PreparedMutation, crate::operation::Options> {
        let owner = self.owner;
        crate::Request::new(move |options| {
            Box::pin(owner.prepare_ref_update(batch, scratch, options))
        })
    }

    /// Reconcile historical commitment after verifying this repository binding.
    #[cfg(feature = "write")]
    pub fn reconcile(
        &self,
        token: write::RecoveryToken,
    ) -> crate::operation::Request<'a, write::MutationOutcome, crate::operation::Options> {
        let owner = self.owner;
        crate::Request::new(move |options| Box::pin(owner.reconcile(token, options)))
    }
}
