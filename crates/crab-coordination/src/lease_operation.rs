//! Lease renewal for operations that must drain before their lock is released.
use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::{CoordinationError, PushLock};

/// Renews a lease while awaiting an operation, preserving its primary error.
///
/// Renewal failure signals `failure_cancel`, then continues polling the operation
/// until it finishes. Callers must cooperate with that token and release the lock
/// afterwards. This future must be awaited to completion, including cancellation;
/// dropping it neither drains the operation nor releases the borrowed lock.
/// A completed operation need not wait for a pending backend renewal retry.
pub async fn while_renewing<T, E>(
    lock: &mut PushLock,
    failure_cancel: Option<&CancellationToken>,
    operation: impl Future<Output = std::result::Result<T, E>>,
) -> std::result::Result<T, E>
where
    E: From<CoordinationError>,
{
    let renewal_interval = (lock.ttl() / 3).max(Duration::from_secs(1));
    let mut ticker = tokio::time::interval(renewal_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    tokio::pin!(operation);
    let mut renewal_error = None;
    loop {
        tokio::select! {
            biased;
            result = &mut operation => {
                return match result {
                    Err(error) => Err(error),
                    Ok(value) => match renewal_error {
                        Some(error) => Err(E::from(error)),
                        None => Ok(value),
                    },
                };
            }
            _ = ticker.tick(), if renewal_error.is_none() => {
                // A backend CAS may consume its full retry deadline. Keep polling completed
                // maintenance so a successful operation can release a still-valid lease.
                let renewal = lock.renew();
                tokio::pin!(renewal);
                tokio::select! {
                    biased;
                    result = &mut renewal => {
                        if let Err(error) = result {
                            if let Some(failure_cancel) = failure_cancel {
                                failure_cancel.cancel();
                            }
                            renewal_error = Some(error);
                        }
                    }
                    result = &mut operation => {
                        return match result {
                            Err(error) => Err(error),
                            Ok(value) => match renewal_error {
                                Some(error) => Err(E::from(error)),
                                None => Ok(value),
                            },
                        };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GIT_MANIFEST_RESOURCE, PushLockPayload};
    use bytes::Bytes;
    use object_store::{ObjectStore, ObjectStoreExt, path::Path};
    use std::sync::Arc;

    #[tokio::test]
    async fn lost_lease_drains_operation_and_preserves_primary_error() {
        for fail_operation in [false, true] {
            let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
            let mut lease = PushLock::acquire_internal(
                &store,
                "draining-owner",
                GIT_MANIFEST_RESOURCE,
                Duration::from_secs(3),
            )
            .await
            .unwrap();
            let replacement = PushLockPayload::new("replacement", u64::MAX, 60);
            store
                .put(
                    &Path::from(lease.path()),
                    Bytes::from(serde_json::to_vec(&replacement).unwrap()).into(),
                )
                .await
                .unwrap();
            let cancel = CancellationToken::new();
            let mut drained = false;
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                while_renewing(&mut lease, Some(&cancel), async {
                    cancel.cancelled().await;
                    tokio::task::yield_now().await;
                    drained = true;
                    if fail_operation {
                        return Err(CoordinationError::Configuration {
                            key: "operation".into(),
                            origin: "primary".into(),
                        });
                    }
                    Ok(())
                }),
            )
            .await
            .unwrap();
            assert!(drained);
            if fail_operation {
                assert!(
                    matches!(result, Err(CoordinationError::Configuration { key, .. }) if key == "operation")
                );
            } else {
                assert!(matches!(
                    result,
                    Err(CoordinationError::PushLockHeld { .. })
                ));
            }
            lease.release().await.unwrap();
            let payload: PushLockPayload = serde_json::from_slice(
                &store
                    .get(&Path::from(
                        crate::internal_lock_path("draining-owner", GIT_MANIFEST_RESOURCE).unwrap(),
                    ))
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(payload, replacement);
        }
    }
}
