//! Populate only the ancestry needed by mirror comparison, without remote writes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use crab_metadata::manifest_store::RepositorySnapshot;
use crab_remote_git::{OperationContext, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use gix_object::{Find as _, FindHeader as _, Write as _};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error(transparent)]
    Remote(#[from] crab_remote_git::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid Git object id")]
    Oid(#[from] gix_hash::decode::Error),
    #[error("invalid Git ancestry object")]
    Decode(#[from] gix_object::decode::Error),
    #[error("local Git object access failed")]
    Local(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("mirror ancestry exceeds its object or byte budget")]
    Limit,
    #[error("local Git object does not match {0}")]
    Identity(gix_hash::ObjectId),
    #[error("mirror ancestry worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

pub(super) async fn load_changed_history(
    cache: Arc<crab_cache::lifecycle::CacheUseGuard>,
    source: &BTreeMap<String, String>,
    snapshot: &RepositorySnapshot,
    layout: StoreLayout<Store>,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let roots = source
        .iter()
        .filter_map(|(name, oid)| {
            snapshot
                .journal
                .refs
                .get(name)
                .filter(|other| *other != oid)
        })
        .map(|oid| gix_hash::ObjectId::from_hex(oid.as_bytes()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if roots.is_empty() {
        return Ok(());
    }
    let bucket = layout.store().bucket_identity();
    let provider = format!("{:?}:{}:{}", bucket.cloud, bucket.host, bucket.container);
    let identity = RepositoryIdentity::new(provider, layout.repo_prefix().to_owned(), 1)?;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let options = RepositoryOptions::default();
    let opened = OperationContext::from_snapshot(
        layout,
        snapshot,
        identity,
        Arc::clone(&runtime),
        options,
        cancel,
    )
    .await;
    let result = match opened {
        Ok(operation) => {
            let executor = tokio::runtime::Handle::current();
            // Git inflation, parsing, and loose-object writes stay off Tokio's
            // executor. The worker is joined before releasing cache ownership.
            tokio::task::spawn_blocking(move || {
                executor.block_on(async move {
                    let result =
                        populate(&cache.path().join("objects"), roots, &operation, options).await;
                    let close = operation.finish(Ok(())).await;
                    result.and(close.map_err(Error::Remote))
                })
            })
            .await
            .map_err(Error::Worker)
            .and_then(|result| result)
        }
        Err(error) => Err(Error::Remote(error)),
    };
    runtime.shutdown().await;
    result
}

async fn populate(
    objects_dir: &Path,
    mut pending: BTreeSet<gix_hash::ObjectId>,
    operation: &OperationContext,
    options: RepositoryOptions,
) -> Result<(), Error> {
    let odb = gix_odb::at(objects_dir)?;
    let mut visited = BTreeSet::new();
    let mut buffer = Vec::new();
    let mut bytes = 0_u64;
    while let Some(oid) = pending.pop_first() {
        if operation.cancellation().is_cancelled() {
            return Err(crab_remote_git::Error::Cancelled.into());
        }
        if !visited.insert(oid) {
            continue;
        }
        if visited.len().saturating_add(pending.len()) > 2_000_000 {
            return Err(Error::Limit);
        }
        let header = odb.try_header(&oid).map_err(Error::Local)?;
        let (kind, data) = if let Some(header) = header {
            if header.size > options.object_limits().max_object_bytes {
                return Err(Error::Limit);
            }
            let object = odb
                .try_find(&oid, &mut buffer)
                .map_err(Error::Local)?
                .ok_or(Error::Identity(oid))?;
            (object.kind, object.data)
        } else {
            let object = operation.read_object(oid).await?;
            let written = odb
                .write_buf(object.kind, &object.data)
                .map_err(Error::Local)?;
            if written != oid {
                return Err(Error::Identity(oid));
            }
            buffer.clear();
            buffer.extend_from_slice(&object.data);
            (object.kind, buffer.as_slice())
        };
        bytes = bytes.saturating_add(data.len() as u64);
        if bytes > options.operation_limits().max_inflated_bytes {
            return Err(Error::Limit);
        }
        let actual = gix_object::compute_hash(gix_hash::Kind::Sha1, kind, data)
            .map_err(|error| Error::Local(Box::new(error)))?;
        if actual != oid {
            return Err(Error::Identity(oid));
        }
        // A cached object is not proof that all of its parents were imported:
        // an earlier inspection may have been interrupted midway through a walk.
        match kind {
            gix_object::Kind::Commit => {
                let commit = gix_object::CommitRef::from_bytes(data, gix_hash::Kind::Sha1)?;
                for parent in commit.parents() {
                    if !visited.contains(&parent) {
                        pending.insert(parent);
                    }
                }
            }
            gix_object::Kind::Tag => {
                let tag = gix_object::TagRef::from_bytes(data, gix_hash::Kind::Sha1)?;
                pending.insert(tag.target());
            }
            gix_object::Kind::Blob | gix_object::Kind::Tree => {}
        }
    }
    Ok(())
}
