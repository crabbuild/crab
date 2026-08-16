use crab_auth::managed::TransferOperation;
use crab_auth::token_cache::expand_token_cache_path;
use crab_auth_store::ManagedRepositoryResolver;
use tokio_util::sync::CancellationToken;

use super::build_store;
use crate::core::config::Config;
use crate::core::error::Result;
use crate::storage::store::Store;

/// Store and physical repository prefix selected for one repository locator.
pub struct RepositoryStore {
    pub store: Store,
    pub repository_prefix: String,
}

/// Resolves a direct or managed repository into the canonical store abstraction.
pub async fn build_repository_store(
    config: &Config,
    locator: crab_git::RepositoryLocator,
    operation: TransferOperation,
    cancel: &CancellationToken,
) -> Result<RepositoryStore> {
    match locator {
        crab_git::RepositoryLocator::Direct(repository) => {
            let repository_prefix = repository.repo_prefix.clone();
            let store = build_store(
                config,
                crab_git::url::CrabUrl::from(repository),
                transfer_operation_name(operation),
                cancel,
            )
            .await?;
            Ok(RepositoryStore {
                store,
                repository_prefix,
            })
        }
        crab_git::RepositoryLocator::Managed(repository) => {
            let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
            let managed = ManagedRepositoryResolver::new(cache_dir)
                .resolve(&repository, operation, cancel)
                .await?;
            Ok(RepositoryStore {
                store: Store::from_storage(managed.store),
                repository_prefix: managed.repository_prefix,
            })
        }
    }
}

fn transfer_operation_name(operation: TransferOperation) -> &'static str {
    match operation {
        TransferOperation::Clone => "clone",
        TransferOperation::Fetch => "fetch",
        TransferOperation::Hydrate => "hydrate",
        TransferOperation::PushUpload => "push-upload",
    }
}
