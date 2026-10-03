use std::{future::Future, sync::Arc, time::Duration};

use crab_remote_git::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    #[error("repository browse indexing failed")]
    Browse(#[from] crab_remote::browse_indexes::Error),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy)]
pub(crate) enum Pass {
    ForegroundCheckpoint,
    BackgroundMaintenance,
}

async fn publish(
    layout: &StoreLayout<Store>,
    pass: Pass,
    cancel: &CancellationToken,
) -> Result<()> {
    let result = match pass {
        Pass::ForegroundCheckpoint => {
            crab_remote::checkpoint::publish_capsule_checkpoint(
                layout,
                CAPSULE_THRESHOLD,
                CHECKPOINT_BYTES,
                cancel,
            )
            .await
        }
        Pass::BackgroundMaintenance => {
            crab_remote::checkpoint::maintain_capsule_repository(
                layout,
                CAPSULE_THRESHOLD,
                CHECKPOINT_BYTES,
                cancel,
            )
            .await
        }
    };
    match result {
        Ok(_) => Ok(()),
        Err(crab_remote::checkpoint::CheckpointError::Cancelled) => Err(Error::Cancelled),
        Err(error) => Err(Error::Checkpoint(error)),
    }
}

pub(crate) struct ProjectionContext {
    pub(crate) repository_id: Uuid,
    pub(crate) router: Option<crate::cells::RepositoryCellRouter>,
    pub(crate) metrics: crate::metrics::Metrics,
    pub(crate) identity: RepositoryIdentity,
    pub(crate) runtime: Arc<RemoteGitRuntime>,
    pub(crate) options: RepositoryOptions,
}

pub(crate) async fn run(
    layout: StoreLayout<Store>,
    admission: Arc<Semaphore>,
    initial_permit: Option<OwnedSemaphorePermit>,
    parent: CancellationToken,
    pass: Pass,
    projection: Option<ProjectionContext>,
) -> Result<()> {
    let has_projection = projection.is_some();
    let cancel = parent.child_token();
    let operation = async {
        let _permit = match initial_permit {
            Some(permit) => permit,
            None => tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Error::Cancelled),
                permit = admission.acquire_owned() => permit.map_err(|_| Error::Cancelled)?,
            },
        };
        let result = publish(&layout, pass, &cancel).await;
        if result.is_ok()
            && !cancel.is_cancelled()
            && let Some(projection) = projection
        {
            let indexed = crab_remote::browse_indexes::ensure(
                &layout,
                &projection.identity,
                Arc::clone(&projection.runtime),
                projection.options,
                CHECKPOINT_BYTES,
                &cancel,
            )
            .await;
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            indexed?;
            let repository_id = projection.repository_id;
            if let Err(error) = crate::projection::reconcile(&layout, projection, &cancel).await {
                tracing::warn!(
                    repository_id = %repository_id,
                    error = ?error,
                    "Git projection reconciliation was deferred"
                );
            }
        }
        result
    };
    await_pass(
        operation,
        cancel.clone(),
        &parent,
        has_projection,
        PASS_BUDGET,
        pass,
    )
    .await
}

async fn await_pass<F>(
    operation: F,
    cancel: CancellationToken,
    parent: &CancellationToken,
    has_projection: bool,
    catalog_budget: Duration,
    pass: Pass,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::pin!(operation);
    if has_projection {
        // A projection is one immutable rebuild. Cancelling it on a short
        // wall-clock budget discards the staged epoch and makes a large
        // repository restart from the first commit on every retry. Shutdown
        // still cancels the child token and awaits cleanup before returning.
        return tokio::select! {
            result = &mut operation => result,
            () = parent.cancelled() => {
                cancel.cancel();
                operation.await
            }
        };
    }
    tokio::select! {
        result = &mut operation => result,
        () = tokio::time::sleep(catalog_budget) => {
            // Cancellation is cooperative; dropping publication here would leak
            // catalog handles or release admission while writes are still running.
            cancel.cancel();
            finish_budgeted_pass(operation.await, parent.is_cancelled(), pass)
        }
    }
}

fn finish_budgeted_pass(result: Result<()>, parent_cancelled: bool, pass: Pass) -> Result<()> {
    match result {
        Err(Error::Cancelled)
            if !parent_cancelled && matches!(pass, Pass::BackgroundMaintenance) =>
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
        assert!(
            finish_budgeted_pass(Err(Error::Cancelled), false, Pass::BackgroundMaintenance).is_ok()
        );
        assert!(matches!(
            finish_budgeted_pass(Err(Error::Cancelled), true, Pass::BackgroundMaintenance),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            finish_budgeted_pass(Err(Error::Cancelled), false, Pass::ForegroundCheckpoint),
            Err(Error::Cancelled)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn projection_pass_is_not_cut_off_by_catalog_budget() {
        let parent = CancellationToken::new();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            await_pass(
                async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(())
                },
                parent.child_token(),
                &parent,
                true,
                Duration::from_millis(1),
                Pass::BackgroundMaintenance,
            ),
        )
        .await;
        assert!(matches!(result, Ok(Ok(()))));
    }
}
