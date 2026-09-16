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
    #[error("repository checkpoint failed")]
    Checkpoint(#[source] crab_remote::checkpoint::CheckpointError),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

async fn publish(layout: &StoreLayout<Store>, cancel: &CancellationToken) -> Result<()> {
    match crab_remote::checkpoint::publish_capsule_checkpoint(
        layout,
        CAPSULE_THRESHOLD,
        CHECKPOINT_BYTES,
        cancel,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(crab_remote::checkpoint::CheckpointError::Cancelled) => Err(Error::Cancelled),
        Err(error) => Err(Error::Checkpoint(error)),
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
