use std::{sync::Arc, time::Duration};

use crab_remote_git::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use crab_write::{Result, WriteError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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

pub(crate) struct ProjectionContext {
    pub(crate) repository_id: Uuid,
    pub(crate) router: crate::cells::RepositoryCellRouter,
    pub(crate) metrics: crate::metrics::Metrics,
}

#[expect(
    clippy::too_many_arguments,
    reason = "maintenance keeps publication, admission, cancellation, and projection ownership explicit"
)]
pub(crate) async fn run_with_projection(
    store: Store,
    layout: StoreLayout<Store>,
    identity: RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    admission: Arc<Semaphore>,
    initial_permit: Option<OwnedSemaphorePermit>,
    parent: CancellationToken,
    projection: Option<ProjectionContext>,
) -> Result<()> {
    let cancel = parent.child_token();
    let operation = async {
        let _permit = match initial_permit {
            Some(permit) => permit,
            None => tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(WriteError::Cancelled),
                permit = admission.acquire_owned() => permit.map_err(|_| WriteError::Cancelled)?,
            },
        };
        let result = publish(
            &store,
            &layout,
            &identity,
            Arc::clone(&runtime),
            options,
            &cancel,
        )
        .await;
        if result.is_ok()
            && !cancel.is_cancelled()
            && let Some(projection) = projection
        {
            let repository_id = projection.repository_id;
            if let Err(error) = crate::projection::reconcile(
                &store, &layout, &identity, runtime, options, projection, &cancel,
            )
            .await
            {
                tracing::warn!(
                    repository_id = %repository_id,
                    error = ?error,
                    "Git projection reconciliation was deferred"
                );
            }
        }
        result
    };
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result,
        () = tokio::time::sleep(PASS_BUDGET) => {
            // Cancellation is cooperative; dropping publication here would leak
            // catalog handles or release admission while writes are still running.
            cancel.cancel();
            finish_budgeted_pass(operation.await, parent.is_cancelled())
        }
    }
}

fn finish_budgeted_pass(result: Result<()>, parent_cancelled: bool) -> Result<()> {
    match result {
        Err(WriteError::Cancelled | WriteError::RemoteGit(crab_remote_git::Error::Cancelled))
            if !parent_cancelled =>
        {
            Ok(())
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_cancellation_is_retryable_but_parent_cancellation_is_terminal() {
        assert!(finish_budgeted_pass(Err(WriteError::Cancelled), false).is_ok());
        assert!(
            finish_budgeted_pass(
                Err(WriteError::RemoteGit(crab_remote_git::Error::Cancelled)),
                false,
            )
            .is_ok()
        );
        assert!(matches!(
            finish_budgeted_pass(Err(WriteError::Cancelled), true),
            Err(WriteError::Cancelled)
        ));
    }
}
