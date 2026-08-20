//! Repository-wide serialization for destructive maintenance operations.

use std::time::Duration;

use crab_coordination::PushLock;
use tokio_util::sync::CancellationToken;

use crate::coordination::heartbeat::LockHeartbeat;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::store::Store;

const MAINTENANCE_LOCK_TTL: Duration = Duration::from_mins(5);

/// Renewable lease shared by destructive repository maintenance commands.
pub(crate) struct RepositoryMaintenanceLease {
    lock: PushLock,
    heartbeat: LockHeartbeat,
}

impl RepositoryMaintenanceLease {
    /// Acquire the repository maintenance lease and start renewing it.
    pub async fn acquire(store: &Store, prefix: &str, cancel: &CancellationToken) -> Result<Self> {
        check_cancelled(cancel)?;
        let lock = PushLock::acquire_internal(
            store.inner(),
            prefix,
            crab_coordination::REPOSITORY_MAINTENANCE_RESOURCE,
            MAINTENANCE_LOCK_TTL,
        )
        .await
        .map_err(CrabError::from)?;
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock.path().to_owned(),
            lock.holder().to_owned(),
            lock.ttl(),
            lock.ttl() / 3,
            cancel.clone(),
        );
        Ok(Self { lock, heartbeat })
    }

    /// Stop renewal and release the holder-checked lease.
    pub async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lock.release().await.map_err(CrabError::from)
    }
}

/// Ordered set of repository maintenance leases for bucket-wide GC.
pub(crate) struct RepositoryMaintenanceLeases {
    leases: Vec<RepositoryMaintenanceLease>,
}

impl RepositoryMaintenanceLeases {
    /// Acquire each repository lease in sorted order, releasing partial work on failure.
    pub(crate) async fn acquire(
        store: &Store,
        prefixes: impl IntoIterator<Item = String>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let mut prefixes = prefixes.into_iter().collect::<Vec<_>>();
        prefixes.sort_unstable();
        prefixes.dedup();
        let mut leases = Vec::with_capacity(prefixes.len());
        for prefix in prefixes {
            let lease = match RepositoryMaintenanceLease::acquire(store, &prefix, cancel).await {
                Ok(lease) => lease,
                Err(error) => {
                    let _ = Self { leases }.release().await;
                    return Err(error);
                }
            };
            leases.push(lease);
        }
        Ok(Self { leases })
    }

    /// Release all leases in reverse acquisition order.
    pub(crate) async fn release(mut self) -> Result<()> {
        let mut first_error = None;
        while let Some(lease) = self.leases.pop() {
            if let Err(error) = lease.release().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn multi_repo_acquisition_releases_partial_leases_on_contention() {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let held = PushLock::acquire_internal_default(
            store.inner(),
            "org/b",
            crab_coordination::REPOSITORY_MAINTENANCE_RESOURCE,
        )
        .await
        .unwrap();

        let result = RepositoryMaintenanceLeases::acquire(
            &store,
            ["org/b".to_owned(), "org/a".to_owned()],
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(CrabError::PushLockHeld { .. })));

        let released_partial = PushLock::acquire_internal_default(
            store.inner(),
            "org/a",
            crab_coordination::REPOSITORY_MAINTENANCE_RESOURCE,
        )
        .await
        .unwrap();
        released_partial.release().await.unwrap();
        held.release().await.unwrap();
    }
}
