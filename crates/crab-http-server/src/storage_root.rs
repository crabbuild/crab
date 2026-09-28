use crab_git::url::{ObjectUrl, UrlForm};
use crab_storage::{
    StaticEnvStoreTarget, StaticEnvStoreUrlForm, StaticEnvStoreUrlParts, StorageProviderKind,
    Store, build_static_env_target_store, static_env_target_selection_for_provider,
};

use crate::{Error, Result, StorageConfig};

#[derive(Clone)]
pub(crate) struct StorageRoot {
    pub store: Store,
    pub prefix: String,
    pub provider_namespace: String,
    pub bucket_label: String,
}

impl StorageRoot {
    pub(crate) fn cell_store(&self) -> cellule_store::Store {
        // Cellule owns its transport policy, while repository and Cell objects
        // must still use the same configured cloud client and storage root.
        let identity = self.store.bucket_identity();
        let cloud = match identity.cloud {
            StorageProviderKind::S3 => cellule_store::StorageProviderKind::S3,
            StorageProviderKind::Gcs => cellule_store::StorageProviderKind::Gcs,
            StorageProviderKind::Azure => cellule_store::StorageProviderKind::Azure,
            StorageProviderKind::Local => cellule_store::StorageProviderKind::Local,
        };
        let mut store = cellule_store::Store::new(self.store.inner().clone()).with_bucket_identity(
            cellule_store::BucketIdentity::new(
                cloud,
                identity.host.clone(),
                identity.container.clone(),
            ),
        );
        if let Some(identity) = self.store.target_identity() {
            store = store.with_target_identity(*identity);
        }
        if let Some(scope) = self.store.storage_scope() {
            store = store.with_storage_scope(cellule_store::StorageScope {
                repo_prefix: scope.repo_prefix.clone(),
                global_prefix: scope.global_prefix.clone(),
                source_repo: scope.source_repo.clone(),
                scope_hash: scope.scope_hash.clone(),
            });
        }
        store
    }

    pub(crate) fn build(config: &StorageConfig) -> Result<Self> {
        let parsed = ObjectUrl::parse(&config.url)
            .map_err(|_| Error::Config("storage.url must be a valid raw cloud URL"))?;
        if parsed.form != UrlForm::Raw
            || parsed.cloud == StorageProviderKind::Local
            || parsed.prefix.is_empty()
        {
            return Err(Error::Config(
                "storage.url must use s3://, gs://, or az:// with a nonempty root prefix",
            ));
        }
        let selection = static_env_target_selection_for_provider(
            StaticEnvStoreUrlParts {
                provider: parsed.cloud,
                form: StaticEnvStoreUrlForm::Raw,
                bucket: &parsed.bucket,
                prefix: &parsed.prefix,
            },
            parsed.cloud,
            "",
        )?;
        let bucket_label = match &selection.target {
            StaticEnvStoreTarget::Bucket { bucket, .. } => bucket.clone(),
            StaticEnvStoreTarget::AzureAccountContainer { container, .. } => container.clone(),
        };
        let store = build_static_env_target_store(selection.target)?;
        let identity = store.bucket_identity();
        let provider = match identity.cloud {
            StorageProviderKind::S3 => "s3",
            StorageProviderKind::Gcs => "gcs",
            StorageProviderKind::Azure => "azure",
            StorageProviderKind::Local => "local",
        };
        Ok(Self {
            store,
            prefix: selection.repo_prefix,
            provider_namespace: format!("{provider}:{}:{}", identity.host, identity.container),
            bucket_label,
        })
    }

    pub(crate) fn repository_prefix(&self, relative: &str) -> Result<String> {
        let relative = crab_git::url::normalize_repository_prefix(relative)
            .map_err(|_| Error::Config("repository prefix is invalid"))?;
        Ok(format!("{}/{}", self.prefix, relative))
    }

    pub(crate) fn path(&self, relative: &str) -> object_store::path::Path {
        object_store::path::Path::from(format!("{}/{}", self.prefix, relative))
    }
}

#[cfg(test)]
impl StorageRoot {
    pub(crate) fn memory(store: Store, prefix: &str) -> Self {
        Self {
            store,
            prefix: prefix.to_owned(),
            provider_namespace: "memory:test".to_owned(),
            bucket_label: "memory".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::{memory::InMemory, path::Path};

    use super::*;

    #[tokio::test]
    async fn cellule_and_repository_stores_share_object_storage() {
        let root = StorageRoot::memory(Store::new(Arc::new(InMemory::new())), "shared");
        let path = Path::from("shared/cell-control");
        root.store
            .put(&path, Bytes::from_static(b"root"))
            .await
            .unwrap();

        let (body, _) = root.cell_store().get_with_etag(&path).await.unwrap();
        assert_eq!(body, Bytes::from_static(b"root"));
    }
}
