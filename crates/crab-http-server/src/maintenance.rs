use std::{sync::Arc, time::Duration};

use crab_coordination::{
    CoordinationError, GIT_GENERATION_OWNER_RESOURCE, GcFenceHeartbeat, GcFenceLease,
    PushLockAcquireContext,
};
use crab_remote_git::{RemoteGitRuntime, RepositoryIdentity, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use crab_write::{Result, WriteError};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const LEASE_TTL: Duration = Duration::from_secs(60);
const PASS_BUDGET: Duration = Duration::from_secs(3 * 60);

struct WriterFence {
    lease: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl WriterFence {
    async fn acquire(store: &Store, domain: &str, cancel: &CancellationToken) -> Result<Self> {
        if cancel.is_cancelled() {
            return Err(WriteError::Cancelled);
        }
        let lease = GcFenceLease::acquire_writer(store.inner(), domain, LEASE_TTL).await?;
        let heartbeat = GcFenceHeartbeat::spawn(&lease, cancel.clone(), LEASE_TTL / 3);
        Ok(Self { lease, heartbeat })
    }

    async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lease.release().await.map_err(Into::into)
    }
}

async fn publish(
    store: &Store,
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    let mut owner = match context
        .try_acquire_internal(
            layout.repo_prefix(),
            GIT_GENERATION_OWNER_RESOURCE,
            LEASE_TTL,
        )
        .await
    {
        Ok(owner) => owner,
        // Another server or CLI owner is already responsible for publication.
        Err(CoordinationError::PushLockHeld { .. }) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let result = crab_coordination::while_renewing(&mut owner, Some(cancel), async {
        let global = WriterFence::acquire(store, layout.global_prefix(), cancel).await?;
        let repo = match WriterFence::acquire(store, layout.repo_prefix(), cancel).await {
            Ok(repo) => repo,
            Err(error) => {
                let _ = global.release().await;
                return Err(error);
            }
        };
        let mut result = async {
            let (manifest, _) = crab_metadata::manifest_store::read_manifest(store, layout).await?;
            let Some(manifest) = crab_write::generation::make_readable(
                store,
                layout,
                LEASE_TTL,
                manifest.pusher,
                cancel,
            )
            .await?
            else {
                return Ok(());
            };
            crab_write::generation::maintain_commit_graph(
                store, layout, &manifest, identity, runtime, options, cancel,
            )
            .await?;
            Ok(())
        }
        .await;
        // Release both domains even if publication or an earlier release failed.
        for fence in [repo, global] {
            result = result.and(fence.release().await);
        }
        result
    })
    .await;
    result.and(owner.release().await.map_err(Into::into))
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
