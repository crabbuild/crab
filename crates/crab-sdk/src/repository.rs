#[cfg(feature = "write")]
use std::path::PathBuf;

use crate::{Client, OperationOptions, RepositoryLocator, Result};
#[cfg(feature = "local")]
use crate::{Error, ErrorKind};

/// Selects how [`Client::open`] accesses an existing repository.
#[derive(Clone, Debug)]
pub struct OpenOptions(OpenTarget);

#[derive(Clone, Debug)]
enum OpenTarget {
    Remote(RepositoryLocator),
    #[cfg(feature = "local")]
    Local(PathBuf),
}

impl OpenOptions {
    /// Open a repository without creating a local worktree.
    #[must_use]
    pub fn remote(locator: RepositoryLocator) -> Self {
        Self(OpenTarget::Remote(locator))
    }

    /// Open an existing local Git worktree without network access or mutation.
    #[cfg(feature = "local")]
    #[must_use]
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self(OpenTarget::Local(path.into()))
    }
}

/// Access mode selected when a repository was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RepositoryMode {
    Remote,
    #[cfg(feature = "local")]
    Local,
}

/// One opened repository with a remote or local interface.
#[derive(Clone)]
pub struct Repository {
    backend: RepositoryBackend,
}

#[derive(Clone)]
enum RepositoryBackend {
    Remote(crate::client::RemoteRepository),
    #[cfg(feature = "local")]
    Local(crate::LocalRepository),
}

impl Repository {
    pub(crate) fn from_remote(owner: crate::client::RemoteRepository) -> Self {
        Self {
            backend: RepositoryBackend::Remote(owner),
        }
    }

    #[cfg(feature = "local")]
    pub(crate) fn from_local(owner: crate::LocalRepository) -> Self {
        Self {
            backend: RepositoryBackend::Local(owner),
        }
    }

    /// Return the interface selected when this repository was opened.
    #[must_use]
    pub const fn mode(&self) -> RepositoryMode {
        match &self.backend {
            RepositoryBackend::Remote(_) => RepositoryMode::Remote,
            #[cfg(feature = "local")]
            RepositoryBackend::Local(_) => RepositoryMode::Local,
        }
    }

    /// Borrow remote operations without performing I/O.
    pub fn remote(&self) -> Result<crate::remote::Remote<'_>> {
        match &self.backend {
            RepositoryBackend::Remote(owner) => Ok(crate::remote::Remote { owner }),
            #[cfg(feature = "local")]
            RepositoryBackend::Local(_) => Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "repository was opened in local mode",
            )),
        }
    }

    /// Borrow local-worktree operations without performing I/O.
    #[cfg(feature = "local")]
    pub fn local(&self) -> Result<crate::local::Local<'_>> {
        let RepositoryBackend::Local(owner) = &self.backend else {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "repository was opened in remote mode",
            ));
        };
        Ok(crate::local::Local { owner })
    }
}

impl Client {
    /// Open an existing remote repository or local Git worktree.
    pub fn open(&self, options: OpenOptions) -> crate::Request<'_, Repository, OperationOptions> {
        crate::Request::new(move |operation| {
            Box::pin(async move {
                match options.0 {
                    OpenTarget::Remote(locator) => self
                        .open_remote_with_options(locator, operation)
                        .await
                        .map(Repository::from_remote),
                    #[cfg(feature = "local")]
                    OpenTarget::Local(path) => self
                        .open_local_with_options(path, operation)
                        .await
                        .map(Repository::from_local),
                }
            })
        })
    }

    /// Resume a saved remote mutation with current credentials and scratch space.
    #[cfg(feature = "write")]
    pub fn resume_remote(
        &self,
        token: crate::remote::write::RecoveryToken,
        scratch: PathBuf,
    ) -> crate::Request<'_, crate::remote::write::MutationOutcome, OperationOptions> {
        crate::Request::new(move |operation| {
            Box::pin(self.resume_mutation(token, scratch, operation))
        })
    }

    /// Reconcile a saved remote mutation without requiring a readable repository.
    #[cfg(feature = "write")]
    pub fn reconcile_remote(
        &self,
        token: crate::remote::write::RecoveryToken,
    ) -> crate::Request<'_, crate::remote::write::MutationOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.reconcile(token, operation)))
    }

    /// Clone a remote source into a new local Git worktree.
    #[cfg(feature = "local")]
    pub fn clone_local(
        &self,
        source: RepositoryLocator,
        destination: impl Into<PathBuf>,
        options: crate::local::CloneOptions,
    ) -> crate::Request<'_, Repository, OperationOptions> {
        self.clone_repository(source, destination.into(), options)
            .map(Repository::from_local)
    }

    /// Resume an unattempted local push or reconcile its historical attempt.
    #[cfg(feature = "local")]
    pub fn resume_local_push(
        &self,
        token: crate::local::PushRecoveryToken,
        scratch: PathBuf,
    ) -> crate::Request<'_, crate::local::PushOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.resume_push(token, scratch, operation)))
    }

    /// Reconcile a restarted local push without replaying it.
    #[cfg(feature = "local")]
    pub fn reconcile_local_push(
        &self,
        token: crate::local::PushRecoveryToken,
    ) -> crate::Request<'_, crate::local::PushOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.reconcile_push(token, operation)))
    }
}
