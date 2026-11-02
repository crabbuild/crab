use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use crab_coordination::{CoordinationError, GIT_REF_NAMESPACE_RESOURCE, PushLockAcquireContext};
use crab_storage::{Store, StoreLayout};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::WriteError;

#[cfg(test)]
mod cleanup_tests;

const NAMESPACE_CONCURRENCY: usize = 32;

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
    with_ref_namespaces_wait(
        store,
        layout,
        ref_names,
        ttl,
        ttl.saturating_mul(2),
        cancel,
        operation,
    )
    .await
}

/// Serialize ref-name changes with an explicit contention wait budget.
pub async fn with_ref_namespaces_wait<T, E, F, Fut>(
    store: &Store,
    layout: &StoreLayout<Store>,
    ref_names: &[String],
    ttl: Duration,
    wait: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> std::result::Result<T, E>
where
    E: From<WriteError>,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let mut resources = ref_names
        .iter()
        .map(|name| namespace_resource(name))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|resource| {
            let context = PushLockAcquireContext::new(Arc::clone(store.inner()));
            (resource, context)
        })
        .collect::<Vec<_>>();
    let deadline = if wait.is_zero() {
        None
    } else {
        Some(Instant::now().checked_add(wait).ok_or_else(|| {
            E::from(WriteError::Internal(
                "namespace lease deadline overflow".into(),
            ))
        })?)
    };
    let scoped = cancel.child_token();
    let namespace_count = resources.len();
    let acquire_started = (namespace_count > NAMESPACE_CONCURRENCY).then(Instant::now);
    let mut attempt = 0;
    let leases = loop {
        let stopped = CancellationToken::new();
        // Own pending contexts across I/O so spawned publishers keep a Send future.
        let pending = std::mem::take(&mut resources);
        resources.reserve(namespace_count);
        let mut acquisitions = futures_util::stream::iter(pending)
            .map(|(resource, mut context)| {
                let scoped = &scoped;
                let stopped = &stopped;
                async move {
                    let result = if scoped.is_cancelled() || stopped.is_cancelled() {
                        Ok(None)
                    } else {
                        context
                            .acquire_internal(layout.repo_prefix(), &resource, ttl)
                            .await
                            .map(|lease| {
                                Some(crab_coordination::RenewingPushLock::start(lease, scoped))
                            })
                    };
                    (resource, context, result)
                }
            })
            .buffer_unordered(NAMESPACE_CONCURRENCY);
        let mut leases = Vec::with_capacity(namespace_count);
        let mut failure = None;
        // A failed parallel claim may race successful backend writes. Drain
        // every attempt, then release the whole partial set before waiting;
        // retaining it across contention would allow cyclic ownership.
        while let Some((resource, context, result)) = acquisitions.next().await {
            resources.push((resource, context));
            match result {
                Ok(Some(lease)) => leases.push(lease),
                Ok(None) => {}
                Err(error) => {
                    stopped.cancel();
                    match failure {
                        None => failure = Some(error),
                        Some(CoordinationError::PushLockHeld { .. })
                            if !matches!(error, CoordinationError::PushLockHeld { .. }) =>
                        {
                            failure = Some(error);
                        }
                        _ => {}
                    }
                }
            }
        }
        let Some(error) = failure else {
            if scoped.is_cancelled() {
                release_leases(leases).await;
                return Err(E::from(WriteError::Cancelled));
            }
            break leases;
        };
        release_leases(leases).await;
        if !matches!(error, CoordinationError::PushLockHeld { .. }) {
            return Err(E::from(WriteError::from(error)));
        }
        let Some(deadline) = deadline else {
            return Err(E::from(WriteError::from(error)));
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(E::from(WriteError::from(error)));
        }
        let delay = crate::journal::push_lock_wait_delay(attempt, remaining);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = scoped.cancelled() => return Err(E::from(WriteError::Cancelled)),
            () = tokio::time::sleep(delay) => {}
        }
    };
    if let Some(started) = acquire_started {
        tracing::info!(
            phase = "namespace_acquire",
            namespaces = namespace_count,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Git namespace lease phase finished"
        );
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
    let count = leases.len();
    let started = (count > NAMESPACE_CONCURRENCY).then(Instant::now);
    futures_util::stream::iter(leases.into_iter().rev())
        .for_each_concurrent(NAMESPACE_CONCURRENCY, |lease| lease.release())
        .await;
    if let Some(started) = started {
        tracing::info!(
            phase = "namespace_release",
            namespaces = count,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "Git namespace lease owners drained"
        );
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
