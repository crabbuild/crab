//! Background publication of derived indexes, separate from per-ref authority.

use std::{sync::Arc, time::Duration};

use crab_metadata::capsule_protocol::{BrowseIndexes, MAX_BROWSE_INDEXES_BYTES, load_root};
use crab_read::capsule_protocol::{
    CapsuleReadLimits, open_view_from_root_with_control, read_activity_from_root,
};
use crab_remote_git::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::{ETag, StorageError, Store, StoreLayout};
use tokio_util::sync::CancellationToken;

/// Failure while building or publishing derived browse indexes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("browse-index read failed")]
    Read(#[from] crab_read::ReadError),
    #[error("browse-index metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("browse-index storage failed")]
    Storage(#[from] StorageError),
    #[error("browse-index construction failed")]
    Write(#[from] crab_write::WriteError),
    #[error("browse-index coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
}

/// Publish complete indexes only for the exact state captured under maintenance ownership.
///
/// This is opt-in background work, never a prerequisite for ref publication. A
/// superseded snapshot is not published; readers independently reject stale records.
pub async fn ensure(
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    crab_write::generation::with_generation_owner(
        layout.store(),
        layout,
        Duration::from_secs(60),
        cancel,
        async {
            let (previous, etag) = previous_record(layout).await?;
            let root = load_root(layout).await?;
            let limits = CapsuleReadLimits {
                max_capsule_bytes: max_bytes,
                max_frontier_bytes: max_bytes,
            };
            let view = open_view_from_root_with_control(layout, root, limits).await?;
            let snapshot = view.git_snapshot()?;
            if snapshot.manifest.refs.is_empty() {
                return Ok(());
            }
            let repository = view
                .git_repository_from_store(
                    layout.clone(),
                    identity.clone(),
                    runtime,
                    options,
                    max_bytes,
                    cancel,
                )
                .await?;
            let indexes = crab_write::generation::browse::build(
                layout,
                &repository,
                &snapshot,
                previous.as_ref(),
                options,
                cancel,
            )
            .await?;
            if previous.as_ref() == Some(&indexes) {
                return Ok(());
            }
            let current = load_root(layout).await?;
            let activity = read_activity_from_root(layout, &current).await?;
            if activity.state_digest() != indexes.state_digest() || cancel.is_cancelled() {
                return Ok(());
            }
            let path = layout.capsule_browse_indexes_path();
            let bytes = indexes.encode()?;
            match etag {
                Some(etag) => {
                    layout.store().update(&path, bytes, etag).await?;
                }
                None => layout.store().create_strict(&path, bytes).await?,
            }
            Ok(())
        },
    )
    .await
}

async fn previous_record(
    layout: &StoreLayout<Store>,
) -> Result<(Option<BrowseIndexes>, Option<ETag>), Error> {
    let path = layout.capsule_browse_indexes_path();
    match layout
        .store()
        .get_with_etag_bounded(&path, MAX_BROWSE_INDEXES_BYTES)
        .await
    {
        // A malformed derived record is rebuildable; its CAS token still prevents
        // a repair pass from overwriting a concurrently published replacement.
        Ok((bytes, etag)) => Ok((BrowseIndexes::decode(&bytes).ok(), Some(etag))),
        Err(StorageError::NotFound { .. }) => Ok((None, None)),
        Err(StorageError::CorruptObject { .. }) => {
            let meta = layout.store().head(&path).await?;
            Ok((
                None,
                Some(ETag {
                    e_tag: meta.e_tag,
                    version: meta.version,
                }),
            ))
        }
        Err(error) => Err(error.into()),
    }
}
