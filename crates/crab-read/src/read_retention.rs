use std::{future::Future, sync::Arc};

use crab_coordination::{
    GcReadBarrier,
    read_retention::{ReadRetentionLease, RetainedReadRoot},
    read_retention_runtime::{ReadRetentionPin, ReadRetentionSession, RetainedRead},
};
use crab_metadata::capsule_protocol::{PointerCatalogLookup, load_root, store_read_capture};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;

use crate::capsule_protocol::{CapsuleReadLimits, open_git_view_from_root};
use crate::{ReadError, Result};

/// Capture and retain the authenticated pointer catalog before admitting reads.
///
/// The caller supplies a fresh owner and a trusted native store authorized to
/// publish reader records. Keep the session alive until readers have drained.
pub async fn retain_pointer_catalog(
    layout: &StoreLayout<Store>,
    owner: &str,
    maximum: u64,
    session: &ReadRetentionSession,
) -> Result<(PointerCatalogLookup, RetainedRead)> {
    let admission = session.capture()?;
    let store = layout.store().inner();
    let repository = GcReadBarrier::observe(Arc::clone(store), layout.repo_prefix()).await?;
    let global = GcReadBarrier::observe(Arc::clone(store), layout.global_prefix()).await?;
    let root = load_root(layout).await?;
    let view = open_git_view_from_root(
        layout,
        root,
        CapsuleReadLimits {
            max_capsule_bytes: maximum,
            max_frontier_bytes: maximum,
        },
    )
    .await?;
    let catalog = view.pointer_catalog_lookup(layout, maximum)?;
    let (hash, size) = store_read_capture(
        layout,
        owner,
        view.root(),
        view.capsule_run_pointers(),
        &catalog,
    )
    .await?;
    let lease = ReadRetentionLease::create(
        Arc::clone(store),
        layout.read_retention_claim_path(owner),
        RetainedReadRoot::new(hash, size)?,
    )
    .await?;
    let retention = admission.retain(lease)?;
    // A sweep that crossed capture may have omitted this claim. Drop its owner
    // so it drains and releases, without ever admitting the rejected view.
    repository.validate().await?;
    global.validate().await?;
    // Barrier validation can outlast renewal. Require a healthy claim at handoff.
    let _ready = retention.pin()?;
    Ok((catalog, retention))
}

pub(crate) fn read<'a, T: Send + 'a, F>(
    retained: Option<&'a RetainedRead>,
    cancel: &'a CancellationToken,
    operation: impl FnOnce(Option<Arc<ReadRetentionPin>>, CancellationToken) -> F + Send + 'a,
) -> futures_util::future::BoxFuture<'a, Result<T>>
where
    F: Future<Output = Result<T>> + Send + 'a,
{
    // Keep provider future types behind the lifetime boundary. Mount callbacks
    // otherwise inherit the full transport and reconstruction state machines.
    Box::pin(async move {
        let Some(retained) = retained else {
            return operation(None, cancel.clone()).await;
        };
        let pin = retained.pin()?;
        let operation_cancel = cancel.child_token();
        let _cancel_on_drop = operation_cancel.clone().drop_guard();
        let operation = operation(Some(Arc::clone(&pin)), operation_cancel.clone());
        tokio::pin!(operation);
        let outcome = tokio::select! {
            biased;
            () = pin.cancellation().cancelled() => {
                // Cancellation requests a stop; it cannot release ownership while
                // the reconstructor or its blocking writer is still draining.
                operation_cancel.cancel();
                operation.await
            }
            outcome = &mut operation => outcome,
        };
        match outcome {
            Err(error) if !matches!(error, ReadError::Cancelled) => Err(error),
            outcome => {
                pin.ensure_valid()?;
                outcome
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use crab_coordination::read_retention::load_read_retention_root;
    use crab_metadata::capsule_protocol::{RepositoryRoot, RetainedReadCapture, RootRecord};
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn retained_pointer_catalog_keeps_its_projected_capture_until_shutdown() {
        let repository_id = "a".repeat(64);
        let owner = "b".repeat(64);
        let repo = format!(
            "org/repo/acl-views/v2/{}/1-{}",
            "c".repeat(64),
            "d".repeat(64)
        );
        let layout = StoreLayout::with_global_prefix(
            Store::new(Arc::new(InMemory::new())),
            repo.clone(),
            format!("{repo}/.crab"),
        );
        let initial =
            RootRecord::encode(RepositoryRoot::initial(&repository_id, "refs/heads/main").unwrap())
                .unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_root_path(), initial.bytes().clone())
            .await
            .unwrap();

        let session = ReadRetentionSession::default();
        let (_catalog, retained) = retain_pointer_catalog(&layout, &owner, u64::MAX, &session)
            .await
            .unwrap();
        let claim_path = layout.read_retention_claim_path(&owner);
        let native = Arc::clone(layout.store().inner());
        let claim = load_read_retention_root(&native, &claim_path)
            .await
            .unwrap()
            .unwrap();
        let mut capture =
            RetainedReadCapture::open(layout, &owner, &repository_id, claim.hash(), claim.size())
                .await
                .unwrap();
        assert!(capture.next_runs().await.unwrap().is_none());
        assert!(retained.pin().is_ok());

        session.shutdown().await.unwrap();
        assert!(retained.pin().is_err());
        assert!(
            load_read_retention_root(&native, &claim_path)
                .await
                .unwrap()
                .is_none()
        );
    }
}
