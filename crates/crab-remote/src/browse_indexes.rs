//! Background publication of derived indexes, separate from per-ref authority.

use std::{sync::Arc, time::Duration};

use crab_metadata::capsule_protocol::{
    BrowseIndexes, MAX_BROWSE_INDEXES_BYTES, load_root, retain_browse_indexes,
};
use crab_read::capsule_protocol::{
    CapsuleReadLimits, open_view_from_root_with_metadata, read_activity_from_root,
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

/// Publish graph and physical readiness, then complete attribution, for one captured state.
///
/// This is opt-in background work, never a prerequisite for ref publication. A
/// superseded snapshot is not published; readers independently reject stale records.
/// Returns the verified complete record only after owning and completing the pass.
pub async fn ensure(
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Option<BrowseIndexes>, Error> {
    let mut ready = None;
    crab_write::generation::with_generation_owner::<_, Error>(
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
            let view = open_view_from_root_with_metadata(layout, root, limits).await?;
            let snapshot = view.git_snapshot()?;
            if snapshot.manifest.refs.is_empty() {
                return Ok(());
            }
            let repository = view
                .git_repository_from_store(
                    layout.clone(),
                    identity.clone(),
                    runtime.clone(),
                    options,
                    max_bytes,
                    cancel,
                )
                .await?;
            let mut indexes = crab_write::generation::browse::build_graph(
                layout,
                &repository,
                &snapshot,
                previous.as_ref(),
                options,
                cancel,
            )
            .await?;
            let physical_ready = match indexes.object_index_hash() {
                Some(hash) => verify_object_index(layout, &snapshot, hash, max_bytes, options, cancel).await?,
                None => false,
            };
            if !physical_ready {
                let write = view.build_object_index(layout, max_bytes, cancel).await?;
                let hash = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(crab_read::ReadError::Cancelled.into()),
                    result = write.upload(layout.store(), layout) => result?,
                };
                let reader = crab_metadata::git_object_locator::PagedGitObjectIndex::load(
                    layout.store(), layout, &hash, &snapshot.manifest_etag, &snapshot.manifest,
                    options.object_limits().max_pack_index_bytes,
                ).await?;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(crab_read::ReadError::Cancelled.into()),
                    result = reader.verify_complete(layout.store(), &snapshot.manifest, max_bytes) => result?,
                }
                indexes = indexes.with_object_index(hash)?;
            }
            // Lifetime publication precedes query readiness. If the later CAS
            // loses, extra retained data is safe; the opposite order can strand readers.
            retain_browse_indexes(
                layout, &indexes, snapshot.manifest.generation, view.layered_checkpoint(),
                view.capsule_run_pointers(),
            ).await?;
            if previous.as_ref() == Some(&indexes) && indexes.path_state_hash().is_some() {
                ready = Some(indexes);
                return Ok(());
            }
            let etag = if previous.as_ref() == Some(&indexes) {
                etag
            } else {
                match publish_current(layout, &indexes, etag, cancel).await? {
                    Some(etag) => Some(etag),
                    None => return Ok(()),
                }
            };
            // Bind the just-verified physical pages to this same view before path
            // construction. Reopening a fresh root could mix index and ref identities.
            let repository = view.clone().with_browse_indexes(Some(indexes.clone()))
                .git_repository_from_store(layout.clone(), identity.clone(), runtime, options, max_bytes, cancel)
                .await?;
            let complete = crab_write::generation::browse::build_path_state(
                layout,
                &repository,
                &snapshot,
                &indexes,
                options,
                cancel,
            )
            .await?;
            retain_browse_indexes(
                layout, &complete, snapshot.manifest.generation, view.layered_checkpoint(),
                view.capsule_run_pointers(),
            ).await?;
            if publish_current(layout, &complete, etag, cancel)
                .await?
                .is_some()
            {
                ready = Some(complete);
            }
            Ok(())
        },
    )
    .await?;
    Ok(ready)
}

async fn publish_current(
    layout: &StoreLayout<Store>,
    indexes: &BrowseIndexes,
    etag: Option<ETag>,
    cancel: &CancellationToken,
) -> Result<Option<ETag>, Error> {
    let current = load_root(layout).await?;
    let activity = read_activity_from_root(layout, &current).await?;
    if activity.state_digest() != indexes.state_digest() || cancel.is_cancelled() {
        return Ok(None);
    }
    let path = layout.capsule_browse_indexes_path();
    let bytes = indexes.encode()?;
    // Both phases carry the observed CAS token. An expired builder cannot
    // replace a successor; readers also reject records for superseded refs.
    match etag {
        Some(etag) => layout.store().update(&path, bytes, etag).await.map(Some),
        None => layout
            .store()
            .create_strict_with_etag(&path, bytes)
            .await
            .map(Some),
    }
    .map_err(Into::into)
}

async fn verify_object_index(
    layout: &StoreLayout<Store>,
    snapshot: &crab_metadata::manifest_store::RepositorySnapshot,
    hash: &str,
    max_bytes: u64,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<bool, Error> {
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(crab_read::ReadError::Cancelled.into()),
        result = async {
            let index = crab_metadata::git_object_locator::PagedGitObjectIndex::load(
                layout.store(), layout, hash, &snapshot.manifest_etag, &snapshot.manifest,
                options.object_limits().max_pack_index_bytes,
            ).await?;
            index.verify_complete(layout.store(), &snapshot.manifest, max_bytes).await
        } => result,
    };
    // Only the elected maintenance owner repairs derived corruption. Foreground
    // lookup propagates these errors; provider/auth failures never trigger rebuilds.
    match result {
        Ok(()) => Ok(true),
        Err(
            crab_metadata::error::MetadataError::CorruptObject { .. }
            | crab_metadata::error::MetadataError::GitObjectIndexRecord { .. }
            | crab_metadata::error::MetadataError::Storage {
                source: StorageError::NotFound { .. } | StorageError::CorruptObject { .. },
            },
        ) => Ok(false),
        Err(error) => Err(error.into()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crab_metadata::capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleTransaction, load_browse_indexes,
    };

    async fn fixture() -> StoreLayout<Store> {
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "browse-publication".to_owned(),
        );
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        layout
    }

    #[tokio::test]
    async fn expired_builder_cannot_replace_same_state_complete_publication() {
        let layout = fixture().await;
        let root = load_root(&layout).await.unwrap();
        let activity = read_activity_from_root(&layout, &root).await.unwrap();
        let partial =
            BrowseIndexes::new(activity.state_digest().to_owned(), "2".repeat(64), None).unwrap();
        let cancel = CancellationToken::new();
        let first = publish_current(&layout, &partial, None, &cancel)
            .await
            .unwrap()
            .unwrap();
        let complete = BrowseIndexes::new(
            partial.state_digest().to_owned(),
            "2".repeat(64),
            Some("3".repeat(64)),
        )
        .unwrap();
        publish_current(&layout, &complete, Some(first.clone()), &cancel)
            .await
            .unwrap();
        let stale = publish_current(&layout, &partial, Some(first), &cancel).await;
        assert!(matches!(
            stale,
            Err(Error::Storage(StorageError::StateConflict { .. }))
        ));
        assert_eq!(load_browse_indexes(&layout).await.unwrap(), Some(complete));
    }

    #[tokio::test]
    async fn ref_only_publication_fences_a_captured_graph_and_cancelled_builder() {
        let layout = fixture().await;
        let root = load_root(&layout).await.unwrap();
        let activity = read_activity_from_root(&layout, &root).await.unwrap();
        let partial =
            BrowseIndexes::new(activity.state_digest().to_owned(), "2".repeat(64), None).unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            publish_current(&layout, &partial, None, &cancelled)
                .await
                .unwrap()
                .is_none()
        );
        // This exercises control publication, independently of the producer's
        // Git dependency proof. No derived record can confer ref authority.
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/tags/tip",
                None,
                Some("4".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();
        assert!(
            publish_current(&layout, &partial, None, &CancellationToken::new())
                .await
                .unwrap()
                .is_none()
        );
        assert!(load_browse_indexes(&layout).await.unwrap().is_none());
    }
}
