//! Shared object URL to [`Store`] resolution.
//!
//! Import and export both accept raw object-storage URLs. This module owns
//! the common mapping from parsed URL form to a retrying [`Store`] plus the
//! normalized object prefix the caller should read or write under.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::core::config::{Config, StorageProvider};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::url::{Cloud, CrabUrl, ObjectUrl, UrlForm};
use crate::storage::store::{BucketIdentity, Store};

/// Resolved object-store side for raw import/export operations.
#[derive(Clone)]
pub struct ResolvedObjectStore {
    /// Backing object store, wrapped with retry + metrics.
    pub store: Store,
    /// Cross-scheme identity for same-bucket detection.
    pub bucket: BucketIdentity,
    /// Object-store path prefix (no leading slash, never trailing).
    /// `""` means bucket root.
    pub prefix: String,
}

/// Resolve an [`ObjectUrl`] into a [`Store`] and normalized prefix.
///
/// Raw cloud URLs force their storage provider from the URL scheme;
/// `crab://` URLs use the configured provider. `file://` URLs build an
/// `object_store` local filesystem rooted at the URL path and return an
/// empty prefix because all subsequent paths are relative to that root.
pub async fn resolve_object_url_store(
    url: &ObjectUrl,
    config: &Config,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<ResolvedObjectStore> {
    check_cancelled(cancel)?;

    let store = match url.cloud {
        Cloud::Local => build_local_store(url)?,
        Cloud::S3 | Cloud::Gcs | Cloud::Azure => {
            build_cloud_store(url, config, operation, cancel).await?
        }
    };
    let bucket = if url.cloud == Cloud::Local {
        url.bucket_identity()
    } else {
        store.bucket_identity()
    };
    let prefix = if url.cloud == Cloud::Local {
        String::new()
    } else {
        url.prefix.clone()
    };

    Ok(ResolvedObjectStore {
        store,
        bucket,
        prefix,
    })
}

fn build_local_store(url: &ObjectUrl) -> Result<Store> {
    let fs = object_store::local::LocalFileSystem::new_with_prefix(&url.prefix).map_err(|e| {
        CrabError::Configuration {
            key: format!("failed to build local file store: {e}"),
            origin: url.prefix.clone(),
        }
    })?;
    Ok(Store::new(Arc::new(fs)).with_bucket_identity(url.bucket_identity()))
}

async fn build_cloud_store(
    url: &ObjectUrl,
    config: &Config,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<Store> {
    let mut effective_config = config.clone();
    if url.form == UrlForm::Raw {
        effective_config.auth.storage_provider = storage_provider_for_cloud(url.cloud)?;
    }

    let crab_url = CrabUrl {
        bucket: url.bucket.clone(),
        repo_path: url.prefix.clone(),
    };
    crate::auth::build_store(&effective_config, &crab_url, operation, cancel).await
}

fn storage_provider_for_cloud(cloud: Cloud) -> Result<StorageProvider> {
    StorageProvider::from_storage_provider_kind(cloud).ok_or_else(|| {
        CrabError::Internal("storage_provider_for_cloud called for local URL".into())
    })
}
