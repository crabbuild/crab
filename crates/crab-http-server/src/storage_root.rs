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
