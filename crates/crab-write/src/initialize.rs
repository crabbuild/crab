use crab_metadata::manifests::Manifest;
use crab_storage::{Store, StoreLayout};

use crate::{Result, WriteError};

/// Publish an initial import after its immutable content and Git proof are uploaded.
///
/// The caller must validate the candidate, authorize this unplanned direct write,
/// and hold ref leases and GC fences. Returns None if admission became stale.
/// Await completion: a failed manifest CAS response may have committed remotely.
pub async fn publish_initial_manifest(
    store: &Store,
    layout: &StoreLayout<Store>,
    current: &crab_metadata::manifest_store::RepositorySnapshot,
    manifest: &Manifest,
    ttl: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<String>> {
    crate::with_ref_namespace(store, layout, ttl, cancel, |cancel| async move {
        if cancel.is_cancelled() {
            return Err(WriteError::Cancelled);
        }
        let fresh = crab_metadata::manifest_store::read_repository_snapshot(store, layout).await?;
        // Uploads can overlap another first import or journal create. The shared
        // namespace gate prevents either path from crossing this recheck.
        if fresh.manifest_etag != current.manifest_etag
            || !fresh.journal.refs.is_empty()
            || !fresh.journal.transactions.is_empty()
            || !fresh.journal.visible_heads.is_empty()
        {
            return Ok(None);
        }
        if cancel.is_cancelled() {
            return Err(WriteError::Cancelled);
        }
        Ok(Some(
            crab_metadata::manifest_store::write_manifest_cas(
                store,
                layout,
                manifest,
                &fresh.manifest_etag,
            )
            .await?,
        ))
    })
    .await
}
