//! Repository-wide serialization for destructive maintenance operations.

use std::time::Duration;

use crab_coordination::{GcFenceHeartbeat, GcFenceLease, PushLock};
use tokio_util::sync::CancellationToken;

use crate::coordination::heartbeat::LockHeartbeat;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::store::Store;

const MAINTENANCE_LOCK_TTL: Duration = Duration::from_mins(5);
const GC_FENCE_TTL: Duration = crab_coordination::DEFAULT_GC_FENCE_TTL;

/// Renewable exclusive fence for one repo or bucket GC domain.
pub(crate) struct GcSweepLease {
    fence: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

/// Renewable shared admission for bucket-global administrative writers such
/// as ref-registry repair. These operations have no single repository domain.
pub(crate) struct GcGlobalWriterLease {
    fence: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl GcGlobalWriterLease {
    pub(crate) async fn acquire(
        store: &Store,
        domain: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let fence = GcFenceLease::acquire_writer(store.inner(), domain, GC_FENCE_TTL)
            .await
            .map_err(CrabError::from)?;
        let heartbeat = GcFenceHeartbeat::spawn(&fence, cancel.clone(), GC_FENCE_TTL / 3);
        Ok(Self { fence, heartbeat })
    }

    pub(crate) async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.fence.release().await.map_err(CrabError::from)
    }
}

/// Renewable shared writer admission for one global and one repository domain.
pub(crate) struct GcWriterLeases {
    global: GcFenceLeaseWithHeartbeat,
    repo: GcFenceLeaseWithHeartbeat,
    operation_cancel: CancellationToken,
}

impl GcWriterLeases {
    /// Acquire global content admission before repository admission.
    pub(crate) async fn acquire(
        store: &Store,
        global_domain: &str,
        repo_domain: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let global = GcFenceLease::acquire_writer(store.inner(), global_domain, GC_FENCE_TTL)
            .await
            .map_err(CrabError::from)?;
        let global_heartbeat = GcFenceHeartbeat::spawn(&global, cancel.clone(), GC_FENCE_TTL / 3);
        let repo = match GcFenceLease::acquire_writer(store.inner(), repo_domain, GC_FENCE_TTL)
            .await
            .map_err(CrabError::from)
        {
            Ok(repo) => repo,
            Err(error) => {
                global_heartbeat.stop().await;
                let _ = global.release().await;
                return Err(error);
            }
        };
        let repo_heartbeat = GcFenceHeartbeat::spawn(&repo, cancel.clone(), GC_FENCE_TTL / 3);
        // Keep the heartbeat handles alive by wrapping them in the private
        // lease type below; both domains must renew for the whole publication.
        Ok(Self {
            global: GcFenceLeaseWithHeartbeat {
                lease: global,
                heartbeat: global_heartbeat,
            },
            repo: GcFenceLeaseWithHeartbeat {
                lease: repo,
                heartbeat: repo_heartbeat,
            },
            operation_cancel: cancel.clone(),
        })
    }

    /// Stop both heartbeats and release repository before global admission.
    pub(crate) async fn release(self) -> Result<()> {
        let cancelled = self.operation_cancel.is_cancelled();
        let repo_result = self.repo.release().await;
        let global_result = self.global.release().await;
        match repo_result {
            Err(error) => Err(error),
            Ok(()) => match global_result {
                Err(error) => Err(error),
                Ok(()) if cancelled => Err(CrabError::Cancelled),
                Ok(()) => Ok(()),
            },
        }
    }
}

struct GcFenceLeaseWithHeartbeat {
    lease: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl GcFenceLeaseWithHeartbeat {
    async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lease.release().await.map_err(CrabError::from)
    }
}

impl GcSweepLease {
    /// Acquire an exclusive sweep fence and renew it until release.
    pub(crate) async fn acquire(
        store: &Store,
        domain: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let fence = GcFenceLease::acquire_sweep(store.inner(), domain, GC_FENCE_TTL)
            .await
            .map_err(CrabError::from)?;
        let heartbeat = GcFenceHeartbeat::spawn(&fence, cancel.clone(), GC_FENCE_TTL / 3);
        Ok(Self { fence, heartbeat })
    }

    /// Stop renewal and release the exclusive fence.
    pub(crate) async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.fence.release().await.map_err(CrabError::from)
    }
}

/// Renewable lease shared by destructive repository maintenance commands.
pub(crate) struct RepositoryMaintenanceLease {
    lock: PushLock,
    heartbeat: LockHeartbeat,
    gc_writer: GcWriterLeases,
}

impl RepositoryMaintenanceLease {
    /// Acquire the repository maintenance lease and the GC writer fence.
    pub async fn acquire(
        store: &Store,
        global_domain: &str,
        prefix: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let gc_writer = GcWriterLeases::acquire(store, global_domain, prefix, cancel).await?;
        let lock = match PushLock::acquire_internal(
            store.inner(),
            prefix,
            crab_coordination::REPOSITORY_MAINTENANCE_RESOURCE,
            MAINTENANCE_LOCK_TTL,
        )
        .await
        {
            Ok(lock) => lock,
            Err(error) => {
                let _ = gc_writer.release().await;
                return Err(CrabError::from(error));
            }
        };
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock.path().to_owned(),
            lock.holder().to_owned(),
            lock.ttl(),
            lock.ttl() / 3,
            cancel.clone(),
        );
        Ok(Self {
            lock,
            heartbeat,
            gc_writer,
        })
    }

    /// Stop renewal and release the holder-checked lease.
    pub async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        let lock_result = self.lock.release().await.map_err(CrabError::from);
        let gc_result = self.gc_writer.release().await;
        match lock_result {
            Err(error) => Err(error),
            Ok(()) => gc_result,
        }
    }
}
