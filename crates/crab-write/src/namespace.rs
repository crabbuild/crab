use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use crab_coordination::{CoordinationError, GIT_REF_NAMESPACE_RESOURCE, PushLockAcquireContext};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;

use crate::WriteError;

/// Serialize only ref-name changes that can participate in the same Git D/F conflict.
///
/// The first component beneath `refs/<kind>/` defines a conflict domain:
/// `refs/heads/a` conflicts with `refs/heads/a/x`, while sibling agent branches
/// `refs/heads/a` and `refs/heads/b` retain independent leases.
pub async fn with_ref_namespaces<T, E, F, Fut>(
    store: &Store,
    layout: &StoreLayout<Store>,
    ref_names: &[String],
    ttl: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> std::result::Result<T, E>
where
    E: From<WriteError>,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let resources = ref_names
        .iter()
        .map(|name| namespace_resource(name))
        .collect::<std::collections::BTreeSet<_>>();
    let deadline = Instant::now()
        .checked_add(ttl.saturating_mul(2))
        .ok_or_else(|| {
            E::from(WriteError::Internal(
                "namespace lease deadline overflow".into(),
            ))
        })?;
    let scoped = cancel.child_token();
    let mut leases = Vec::with_capacity(resources.len());
    for resource in resources {
        let mut context = PushLockAcquireContext::new(Arc::clone(store.inner()));
        let mut attempt = 0;
        let lease = loop {
            if scoped.is_cancelled() {
                release_leases(leases).await;
                return Err(E::from(WriteError::Cancelled));
            }
            match context
                .acquire_internal(layout.repo_prefix(), &resource, ttl)
                .await
            {
                Ok(lease) => break lease,
                Err(error @ CoordinationError::PushLockHeld { .. }) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        release_leases(leases).await;
                        return Err(E::from(WriteError::from(error)));
                    }
                    let delay = crate::journal::push_lock_wait_delay(attempt, remaining);
                    attempt = attempt.saturating_add(1);
                    tokio::select! {
                        () = scoped.cancelled() => {
                            release_leases(leases).await;
                            return Err(E::from(WriteError::Cancelled));
                        },
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                Err(error) => {
                    release_leases(leases).await;
                    return Err(E::from(WriteError::from(error)));
                }
            }
        };
        leases.push(crab_coordination::RenewingPushLock::start(lease, &scoped));
    }
    let outcome = operation(scoped).await;
    release_leases(leases).await;
    outcome
}

fn namespace_resource(ref_name: &str) -> String {
    let scope = ref_name.split('/').take(3).collect::<Vec<_>>().join("/");
    let digest = blake3::hash(scope.as_bytes()).to_hex();
    format!("git-ref-namespace-{}", &digest[..32])
}

async fn release_leases(leases: Vec<crab_coordination::RenewingPushLock>) {
    for lease in leases.into_iter().rev() {
        lease.release().await;
    }
}

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
    let lease = loop {
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
    let lease = crab_coordination::RenewingPushLock::start(lease, &scoped);
    let outcome = operation(scoped).await;
    // Drain coordination without replacing the callback's known commit outcome.
    lease.release().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::namespace_resource;

    #[test]
    fn sibling_branch_names_use_independent_namespace_resources() {
        assert_ne!(
            namespace_resource("refs/heads/agent-a"),
            namespace_resource("refs/heads/agent-b")
        );
    }

    #[test]
    fn directory_file_conflicts_share_one_namespace_resource() {
        assert_eq!(
            namespace_resource("refs/heads/agent-a"),
            namespace_resource("refs/heads/agent-a/topic")
        );
    }

    #[test]
    fn branch_and_tag_names_use_independent_namespace_resources() {
        assert_ne!(
            namespace_resource("refs/heads/release"),
            namespace_resource("refs/tags/release")
        );
    }
}
