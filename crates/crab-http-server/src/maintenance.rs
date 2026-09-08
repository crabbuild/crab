use std::{sync::Arc, time::Duration};

use crab_remote_git::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use crab_write::{Result, WriteError};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const LEASE_TTL: Duration = Duration::from_secs(60);
const PASS_BUDGET: Duration = Duration::from_secs(3 * 60);

async fn publish(
    store: &Store,
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<()> {
    crab_write::generation::ensure_readable(
        store, layout, identity, runtime, options, LEASE_TTL, cancel,
    )
    .await
}

pub(crate) async fn run(
    store: Store,
    layout: StoreLayout<Store>,
    identity: RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    admission: Arc<Semaphore>,
    parent: CancellationToken,
) -> Result<()> {
    let cancel = parent.child_token();
    let operation = async {
        let _permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(WriteError::Cancelled),
            permit = admission.acquire_owned() => permit.map_err(|_| WriteError::Cancelled)?,
        };
        publish(&store, &layout, &identity, runtime, options, &cancel).await
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
