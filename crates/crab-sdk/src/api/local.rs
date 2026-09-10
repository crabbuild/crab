//! Local Git worktree configuration and workflows.

use std::path::{Path, PathBuf};

pub use crate::local_impl::{
    CheckoutOptions, CloneOptions, ConflictState, FetchDepth, FetchOptions, FetchOutcome,
    HydrationState, IntegrationId, IntegrationKind, LocalCommitOptions as CommitOptions,
    LocalConfiguration as Configuration, LocalExecutionPolicy as ExecutionPolicy,
    LocalPushOutcome as PushOutcome, LocalPushRecoveryToken as PushRecoveryToken,
    LocalSnapshot as Snapshot, LocalStatus as Status, LocalTools as Tools, PreparedPush, PullMode,
    PullOptions, PullOutcome, PushOptions, PushRefspec, StageOutcome, StatusEntry,
};

/// Client configuration required by local repository workflows.
#[derive(Clone)]
pub struct Options {
    pub(crate) tools: Tools,
    pub(crate) execution_policy: ExecutionPolicy,
}

impl Options {
    /// Use exact Git and Crab executable paths for every local child process.
    #[must_use]
    pub fn new(tools: Tools) -> Self {
        Self {
            tools,
            execution_policy: ExecutionPolicy::default(),
        }
    }

    /// Select whether repository-provided hooks and executable drivers may run.
    #[must_use]
    pub fn with_execution_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.execution_policy = policy;
        self
    }
}

/// Borrowed operations for one opened local Git worktree.
#[derive(Clone, Copy)]
pub struct Local<'a> {
    pub(crate) owner: &'a crate::LocalRepository,
}

impl<'a> Local<'a> {
    /// Return the canonical working-tree root.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.owner.path()
    }

    /// Return the canonical Git common directory shared by linked worktrees.
    #[must_use]
    pub fn common_directory(&self) -> &Path {
        self.owner.common_directory()
    }

    /// Resolve a local revision without contacting its remote.
    pub fn snapshot(
        &self,
        revision: &str,
    ) -> crate::operation::Request<'a, Snapshot, crate::operation::Options> {
        self.owner.snapshot(revision)
    }

    /// Fetch configured refspecs without integrating HEAD.
    pub fn fetch(
        &self,
        options: FetchOptions,
    ) -> crate::operation::Request<'a, FetchOutcome, crate::operation::Options> {
        self.owner.fetch(options)
    }

    /// Read staged, working-tree, conflict, and untracked state.
    pub fn status(&self) -> crate::operation::Request<'a, Status, crate::operation::Options> {
        self.owner.status()
    }

    /// Stage exact repository-relative paths through Crab's canonical content path.
    pub fn stage(
        &self,
        paths: Vec<PathBuf>,
    ) -> crate::operation::Request<'a, StageOutcome, crate::operation::Options> {
        self.owner.stage(paths)
    }

    /// Commit the current index with explicit author and committer metadata.
    pub fn commit(
        &self,
        options: CommitOptions,
    ) -> crate::operation::Request<'a, crate::ObjectId, crate::operation::Options> {
        self.owner.commit(options)
    }

    /// Switch revisions while preserving local changes by default.
    pub fn checkout(
        &self,
        revision: &str,
        options: CheckoutOptions,
    ) -> crate::operation::Request<'a, crate::ObjectId, crate::operation::Options> {
        self.owner.checkout(revision, options)
    }

    /// Fetch and integrate the selected branch, returning conflicts as state.
    pub fn pull(
        &self,
        options: PullOptions,
    ) -> crate::operation::Request<'a, PullOutcome, crate::operation::Options> {
        self.owner.pull(options)
    }

    /// Continue the currently conflicted merge or rebase.
    pub fn continue_integration(
        &self,
        id: IntegrationId,
    ) -> crate::operation::Request<'a, PullOutcome, crate::operation::Options> {
        self.owner.continue_integration(id)
    }

    /// Abort the currently conflicted merge or rebase.
    pub fn abort_integration(
        &self,
        id: IntegrationId,
    ) -> crate::operation::Request<'a, crate::ObjectId, crate::operation::Options> {
        self.owner.abort_integration(id)
    }

    /// Hydrate exact paths, or all tracked paths when the selection is empty.
    pub fn hydrate(
        &self,
        paths: Vec<PathBuf>,
    ) -> crate::operation::Request<'a, (), crate::operation::Options> {
        self.owner.hydrate(paths)
    }

    /// Replace hydrated content with pointers for exact paths, or all paths when empty.
    pub fn dehydrate(
        &self,
        paths: Vec<PathBuf>,
    ) -> crate::operation::Request<'a, (), crate::operation::Options> {
        self.owner.dehydrate(paths)
    }

    /// Warm content for exact paths, or all reachable HEAD content when empty.
    pub fn prefetch_content(
        &self,
        paths: Vec<PathBuf>,
    ) -> crate::operation::Request<'a, (), crate::operation::Options> {
        self.owner.prefetch_content(paths)
    }

    /// Prepare an exact atomic push and durable historical recovery identity.
    pub fn prepare_push(
        &self,
        options: PushOptions,
    ) -> crate::operation::Request<'a, PreparedPush, crate::operation::Options> {
        self.owner.prepare_push(options)
    }
}
