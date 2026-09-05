use crab_storage::{StorageProviderKind, StoreLayout, build_static_env_store};

use crate::{Config, Error, Result};

/// Initialize or adopt every repository prefix in a validated server config.
///
/// The bucket must already exist. A nonempty prefix without Crab's canonical
/// layout fails closed and no existing repository metadata is overwritten.
///
/// # Errors
///
/// Returns the first repository's credential, storage, or canonical metadata
/// failure after previously initialized entries have reached durable state.
pub async fn initialize_repositories(config: &Config) -> Result<()> {
    for repository in &config.repositories {
        let store = build_static_env_store(&repository.bucket, StorageProviderKind::S3)?;
        let layout = StoreLayout::new(store.clone(), repository.prefix.clone());
        let head = format!("refs/heads/{}", repository.default_branch);
        crab_write::initialize::initialize_repository(&store, &layout, &head)
            .await
            .map_err(Error::Initialization)?;
        tracing::info!(
            owner = %repository.owner,
            repository = %repository.name,
            head,
            "repository initialized"
        );
    }
    Ok(())
}
