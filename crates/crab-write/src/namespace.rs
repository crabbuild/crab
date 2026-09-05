use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use crab_coordination::{CoordinationError, GIT_REF_NAMESPACE_RESOURCE, PushLockAcquireContext};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::WriteError;

/// Serialize ref-name changes while retaining the operation's publication outcome.
///
/// Hold edited ref leases before entering, then capture a fresh snapshot and
/// validate its final namespace inside `operation`. The supplied token includes
/// caller cancellation and lease-renewal failure; check it before publication.
/// Await completion without dropping this future. An operation must report its
/// known commit outcome: cleanup or late renewal failure cannot reject a commit.
/// Existing-ref updates need only their per-ref leases and do not enter this gate.
pub async fn with_ref_namespace<T, E, F, Fut>(
    store: &Store,
    layout: &StoreLayout<Store>,
    ttl: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> std::result::Result<T, E>
where
    E: From<WriteError>,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let deadline = Instant::now()
        .checked_add(ttl.saturating_mul(2))
        .ok_or_else(|| {
            E::from(WriteError::Internal(
                "namespace lease deadline overflow".into(),
            ))
        })?;
    let mut context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    let mut attempt = 0;
    let mut lease = loop {
        if cancel.is_cancelled() {
            return Err(E::from(WriteError::Cancelled));
        }
        match context
            .acquire_internal(layout.repo_prefix(), GIT_REF_NAMESPACE_RESOURCE, ttl)
            .await
        {
            Ok(lease) => break lease,
            Err(error @ CoordinationError::PushLockHeld { .. }) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(E::from(WriteError::from(error)));
                }
                let delay = crate::journal::push_lock_wait_delay(attempt, remaining);
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    () = cancel.cancelled() => return Err(E::from(WriteError::Cancelled)),
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(E::from(WriteError::from(error))),
        }
    };
    let scoped = cancel.child_token();
    let mut outcome = None;
    let renewal = crab_coordination::while_renewing(&mut lease, Some(&scoped), async {
        outcome = Some(operation(scoped.clone()).await);
        Ok::<(), CoordinationError>(())
    })
    .await;
    let release = lease.release().await;
    // The inner result records whether the active marker/CAS committed. A late
    // lease or cleanup error must not turn an accepted write into a rejection.
    if let Err(error) = renewal {
        warn!(%error, "ref namespace renewal failed; preserving publication outcome");
    }
    if let Err(error) = release {
        warn!(%error, "ref namespace release failed; preserving publication outcome");
    }
    outcome.ok_or_else(|| {
        E::from(WriteError::Internal(
            "namespace operation did not complete".into(),
        ))
    })?
}
