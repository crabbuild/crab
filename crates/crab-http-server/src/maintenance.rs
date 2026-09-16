use std::{sync::Arc, time::Duration};

use crab_storage::{Store, StoreLayout};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const CAPSULE_THRESHOLD: u32 = 32;
pub(crate) const FOREGROUND_CAPSULE_THRESHOLD: u32 = 56;
const PASS_BUDGET: Duration = Duration::from_secs(3 * 60);
const CHECKPOINT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("repository checkpoint cancelled")]
    Cancelled,
    #[error("repository checkpoint read failed")]
    Read(#[from] crab_read::ReadError),
    #[error("repository Git-pack consolidation failed")]
    Consolidation(#[from] crab_remote::checkpoint::CheckpointError),
    #[error("repository checkpoint metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("repository checkpoint publication failed")]
    Write(#[from] crab_write::WriteError),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

async fn publish(layout: &StoreLayout<Store>, cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let view = crab_read::capsule_protocol::open_view(
        layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    let capsule_count = view
        .capsule_run_pointers()
        .iter()
        .try_fold(0_u32, |total, pointer| {
            total.checked_add(pointer.capsule_count())
        })
        .ok_or(crab_metadata::error::MetadataError::Internal(
            "capsule checkpoint count overflowed".to_owned(),
        ))?;
    if capsule_count < CAPSULE_THRESHOLD {
        return Ok(());
    }
    let packs = crab_remote::checkpoint::consolidate_git_packs(
        &view,
        CHECKPOINT_BYTES,
        CHECKPOINT_BYTES,
        cancel,
    )
    .await?
    .into_packs();
    if packs.is_empty() {
        return Ok(());
    }
    let visibility = crab_metadata::capsule_protocol::CapsuleVisibilitySnapshot::from_index(
        &view.git_visibility_index()?,
    )?;
    let checkpoint = crab_metadata::capsule_protocol::Checkpoint::build_with_catalogs(
        view.root().root().generation(),
        view.root().digest(),
        packs,
        view.pointer_catalog()?,
        Some(visibility),
    )?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let result = if view.visible_ref_transactions().is_empty() {
        crab_write::capsule_protocol::publish_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &checkpoint,
        )
        .await
    } else {
        crab_write::capsule_protocol::publish_ref_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &checkpoint,
            view.refs().clone(),
            view.peeled_refs().clone(),
            view.visible_ref_transactions().clone(),
        )
        .await
    };
    match result {
        Ok(_) | Err(crab_write::WriteError::CapsuleRootChanged { .. }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) async fn run(
    layout: StoreLayout<Store>,
    admission: Arc<Semaphore>,
    parent: CancellationToken,
) -> Result<()> {
    let cancel = parent.child_token();
    let operation = async {
        let _permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Error::Cancelled),
            permit = admission.acquire_owned() => permit.map_err(|_| Error::Cancelled)?,
        };
        publish(&layout, &cancel).await
    };
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result,
        () = tokio::time::sleep(PASS_BUDGET) => {
            // Cancellation is cooperative; dropping publication here would leak
            // catalog handles or release admission while writes are still running.
            cancel.cancel();
            operation.await
        }
    }
}
