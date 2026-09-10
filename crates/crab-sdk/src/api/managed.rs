//! Managed-service configuration and repository administration.

pub use crate::managed_impl::{
    ManagedOptions as Options, ManagedPage as Page, ManagedRepositories as Managed,
    ManagedRepository as RepositoryInfo, ManagedRepositoryState as RepositoryState,
};

impl crate::Client {
    /// Connect to the selected managed service and validate its capabilities.
    pub fn managed(&self) -> crate::operation::Request<'_, Managed, crate::operation::Options> {
        self.managed_repositories()
    }
}
