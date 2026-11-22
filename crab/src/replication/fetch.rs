//! Shared replica selection for Git fetch and its diagnostics.

use std::future::Future;

use crab_metadata::capsule_protocol::RootSnapshot;
use tokio_util::sync::CancellationToken;

use super::{ReadSource, ReadStoreSelection};
use crate::{
    core::{
        config::Config,
        error::{CrabError, Result},
    },
    git::url::CrabUrl,
    storage::{StoreLayout, store::Store},
};

pub(crate) async fn select_fetch_replica(
    config: Config,
    url: CrabUrl,
    cancel: CancellationToken,
) -> Result<ReadStoreSelection> {
    super::select_read_store(&config, &url, "fetch", &cancel).await
}

pub(crate) async fn primary_store_for_batch(
    primary_store: &Store,
    prefix: &str,
    primary_root: Option<&RootSnapshot>,
    cancel: &CancellationToken,
) -> Result<ReadStoreSelection> {
    let router = StoreLayout::new(primary_store.clone(), prefix.to_owned());
    let capsule_root = match primary_root {
        Some(root) => root.clone(),
        None => {
            let layout = crab_storage::StoreLayout::with_global_prefix(
                primary_store.as_storage().clone(),
                router.repo_prefix().to_owned(),
                router.global_prefix().to_owned(),
            );
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
                result = crab_write::capsule_protocol::open_root(&layout) => result?,
            }
        }
    };
    Ok(ReadStoreSelection {
        view_retention: None,
        store: primary_store.clone(),
        router,
        source: ReadSource::Primary,
        capsule_root,
    })
}

pub(crate) async fn read_store_for_batch_with_selector<F, Fut>(
    primary_store: &Store,
    prefix: &str,
    primary_root: Option<&RootSnapshot>,
    remote_url: Option<&str>,
    config: &Config,
    cancel: &CancellationToken,
    select_read: F,
) -> Result<ReadStoreSelection>
where
    F: FnOnce(Config, CrabUrl, CancellationToken) -> Fut,
    Fut: Future<Output = Result<ReadStoreSelection>>,
{
    // Managed grants already bind this store to one physical repository view.
    // Re-parsing the logical URL as a direct bucket would bypass that scope.
    if primary_store.storage_scope().is_some() {
        return primary_store_for_batch(primary_store, prefix, primary_root, cancel).await;
    }
    if config
        .replication
        .as_ref()
        .is_none_or(|replication| !replication.has_read_replicas())
    {
        return primary_store_for_batch(primary_store, prefix, primary_root, cancel).await;
    }
    let Some(url) = remote_url else {
        return primary_store_for_batch(primary_store, prefix, primary_root, cancel).await;
    };
    let Ok(parsed) = CrabUrl::parse(url) else {
        return primary_store_for_batch(primary_store, prefix, primary_root, cancel).await;
    };

    match select_read(config.clone(), parsed, cancel.clone()).await {
        Ok(selection) => {
            if let ReadSource::Replica { name } = &selection.source {
                tracing::debug!(replica = %name, "selected read replica for fetch");
            }
            Ok(selection)
        }
        Err(error) if matches!(&error, CrabError::Configuration { key, .. } if key == "cdn") => {
            Err(error)
        }
        // Readiness has its own primary fallback. A rejected authorization
        // after selection cannot open the same operation with primary authority.
        Err(
            error @ (CrabError::AuthFailed { .. }
            | CrabError::AuthExpired { .. }
            | CrabError::Forbidden { .. }
            | CrabError::NoCredentials
            | CrabError::Cancelled),
        ) => Err(error),
        Err(error) if crab_storage::store::cdn_read_rejection(&error).is_some() => Err(error),
        Err(error) => {
            tracing::debug!(error = %error, "replica read selection failed; using primary");
            primary_store_for_batch(primary_store, prefix, primary_root, cancel).await
        }
    }
}
