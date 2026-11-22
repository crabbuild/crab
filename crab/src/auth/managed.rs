use crab_auth::managed::TransferOperation;
use crab_auth::token_cache::expand_token_cache_path;
use crab_auth_store::ManagedRepositoryResolver;
use crab_storage::cdn::CdnConsumer;
use tokio_util::sync::CancellationToken;

use super::{build_store_for_consumer, open_repository_root};
use crate::core::config::Config;
use crate::core::error::Result;
use crate::storage::store::Store;

/// Store and physical repository prefix selected for one repository locator.
pub struct RepositoryStore {
    pub store: Store,
    pub repository_prefix: String,
    pub capsule_root: crab_metadata::capsule_protocol::RootSnapshot,
    pub view_retention: Option<crab_auth::crab_auth_client::ViewRetention>,
}

/// Resolves a direct or managed repository into the canonical store abstraction.
pub async fn build_repository_store(
    config: &Config,
    locator: crab_git::RepositoryLocator,
    operation: TransferOperation,
    consumer: CdnConsumer,
    cancel: &CancellationToken,
) -> Result<RepositoryStore> {
    match locator {
        crab_git::RepositoryLocator::Direct(repository) => {
            let repository_prefix = repository.repo_prefix.clone();
            let canonical_url = format!("crab://{}/{}", repository.bucket, repository_prefix);
            let resolved = build_store_for_consumer(
                config,
                crab_git::url::CrabUrl::from(repository),
                transfer_operation_name(operation, consumer),
                consumer,
                cancel,
            )
            .await?;
            let capsule_root =
                open_repository_root(&resolved.store, &repository_prefix, &canonical_url).await?;
            Ok(RepositoryStore {
                store: resolved.store,
                repository_prefix,
                capsule_root,
                view_retention: resolved.view_retention,
            })
        }
        crab_git::RepositoryLocator::Managed(repository) => {
            crate::core::cdn::reject_managed_qualification()?;
            let canonical_url = repository.canonical_url();
            let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
            let managed = ManagedRepositoryResolver::new(cache_dir)
                .with_cdn_consumer(consumer)
                .resolve(&repository, operation, cancel)
                .await?;
            let store = Store::from_storage(managed.store);
            let capsule_root =
                open_repository_root(&store, &managed.repository_prefix, &canonical_url).await?;
            Ok(RepositoryStore {
                store,
                repository_prefix: managed.repository_prefix,
                capsule_root,
                view_retention: None,
            })
        }
    }
}

pub(super) fn transfer_operation_name(
    operation: TransferOperation,
    consumer: CdnConsumer,
) -> &'static str {
    if consumer == CdnConsumer::Mount {
        return "mount";
    }
    match operation {
        TransferOperation::Clone => "clone",
        TransferOperation::Fetch => "fetch",
        TransferOperation::Hydrate => "hydrate",
        TransferOperation::PushUpload => "push-upload",
    }
}
