//! Owned publication leases and GC fences shared by remote writers.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    future::Future,
    time::{Duration, Instant},
};

use crab_coordination::{
    BATCH_RESOURCE, CoordinationError, GcFenceHeartbeat, GcFenceLease, PushLock,
    PushLockAcquireContext, RenewingPushLock,
};
use crab_storage::{Store, StoreLayout};
use rand::Rng;
use tokio_util::sync::CancellationToken;

const WAIT_BACKOFF_BASE: Duration = Duration::from_millis(250);
const WAIT_BACKOFF_CAP: Duration = Duration::from_secs(2);
const SUCCESSOR_POLL_CAP: Duration = Duration::from_millis(250);

/// Failure before publication lease admission completes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("publication cancelled")]
    Cancelled,
    #[error("publication coordination failed")]
    Coordination(#[from] CoordinationError),
}

/// Ref-lock admission and renewal policy for one publication attempt.
#[derive(Debug, Clone, Copy)]
pub struct LeaseOptions {
    pub ttl: Duration,
    pub wait: Duration,
    /// `None` leaves ref leases unrenewed; callers must finish before `ttl`.
    pub renewal_interval: Option<Duration>,
}

impl LeaseOptions {
    /// Use renewing leases at one third of their lifetime.
    #[must_use]
    pub fn renewing(ttl: Duration) -> Self {
        Self {
            ttl,
            wait: Duration::ZERO,
            renewal_interval: Some((ttl / 3).max(Duration::from_secs(1))),
        }
    }
}

enum RefLease {
    Fixed(PushLock),
    Renewing(RenewingPushLock),
}

impl RefLease {
    async fn release(self) {
        match self {
            Self::Fixed(lock) => {
                if let Err(error) = lock.release().await {
                    tracing::warn!(%error, "publication ref lease release failed");
                }
            }
            Self::Renewing(lock) => lock.release().await,
        }
    }
}

struct RefLeaseEntry {
    path: String,
    holder: String,
    lease: RefLease,
}

/// Owned, sorted publication ref leases.
///
/// Retain this owner through the marker attempt and explicitly await [`Self::release`].
#[must_use = "publication leases must be retained through commitment and explicitly released"]
pub struct PublicationLeases {
    entries: Vec<RefLeaseEntry>,
    holders: BTreeMap<String, String>,
    fences: Vec<(GcFenceLease, GcFenceHeartbeat)>,
}

impl PublicationLeases {
    /// Return the number of owned ref or batch leases.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether no lease was acquired.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return lock holders keyed by canonical ref name.
    #[must_use]
    pub fn holders(&self) -> &BTreeMap<String, String> {
        &self.holders
    }

    /// Return acquired lock paths and holders for journal attribution.
    pub fn lock_identities(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.holder.as_str()))
    }

    /// Return the number of ref leases with active renewal workers.
    #[must_use]
    pub fn renewing_ref_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.lease, RefLease::Renewing(_)))
            .count()
    }

    /// Stop renewal workers and release every fence and ref lease in reverse order.
    pub async fn release(mut self) {
        while let Some((lease, heartbeat)) = self.fences.pop() {
            heartbeat.stop().await;
            if let Err(error) = lease.release().await {
                tracing::warn!(%error, "publication GC fence release failed");
            }
        }
        while let Some(entry) = self.entries.pop() {
            entry.lease.release().await;
        }
    }
}

async fn release_entries(mut entries: Vec<RefLeaseEntry>) {
    while let Some(entry) = entries.pop() {
        entry.lease.release().await;
    }
}

async fn release_lock_committed_by_visible_transaction(
    store: &Store,
    layout: &StoreLayout<Store>,
    ref_name: &str,
    holder: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let head = crab_metadata::ref_journal::read_ref_head(store, layout, ref_name).await?;
    let Some(transaction_id) = head.visible_transaction else {
        return Ok(false);
    };
    let transaction =
        crab_metadata::ref_journal::read_transaction(store, layout, &transaction_id).await?;
    if !transaction
        .edits
        .iter()
        .any(|edit| edit.ref_name == ref_name && edit.lock_holder.as_deref() == Some(holder))
    {
        return Ok(false);
    }
    PushLock::release_ref_if_holder(store.inner(), layout.repo_prefix(), ref_name, holder)
        .await
        .map_err(Into::into)
}

fn wait_delay(attempt: u32, remaining: Duration, cap: Duration) -> Duration {
    let shift = 1_u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let bound = WAIT_BACKOFF_BASE
        .saturating_mul(shift)
        .min(cap)
        .min(remaining);
    let nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(1..=nanos))
}

/// Acquire sorted publication ref leases, including durable-holder recovery.
///
/// An empty ref set acquires the batch resource. On contention this announces
/// a successor and may release the holder only when a visible transaction binds
/// that exact ref and holder. Partial admission is always released before retry.
pub async fn acquire_ref_leases(
    store: &Store,
    layout: &StoreLayout<Store>,
    names: impl IntoIterator<Item = String>,
    options: LeaseOptions,
    cancel: &CancellationToken,
) -> Result<PublicationLeases, Error> {
    let names = names.into_iter().collect::<BTreeSet<_>>();
    let deadline = (!options.wait.is_zero()).then(|| Instant::now() + options.wait);
    let mut attempt = 0;
    let mut checked_holders = HashSet::new();
    let mut announced_successor = false;
    let mut context = PushLockAcquireContext::new(store.inner().clone());

    loop {
        let mut entries = Vec::with_capacity(names.len().max(1));
        let mut retry = None;
        let mut reclaimed = false;

        for name in names
            .iter()
            .map(Some)
            .chain(names.is_empty().then_some(None))
        {
            if cancel.is_cancelled() {
                release_entries(entries).await;
                return Err(Error::Cancelled);
            }
            let acquired = match name {
                Some(name) => {
                    context
                        .acquire_ref(layout.repo_prefix(), name, options.ttl)
                        .await
                }
                None => {
                    context
                        .acquire_internal(layout.repo_prefix(), BATCH_RESOURCE, options.ttl)
                        .await
                }
            };
            if let (Some(name), Err(CoordinationError::PushLockHeld { holder, .. })) =
                (name, &acquired)
            {
                let key = (name.clone(), holder.clone());
                if checked_holders.insert(key) {
                    if !holder.is_empty()
                        && PushLock::announce_ref_successor(
                            store.inner(),
                            layout.repo_prefix(),
                            name,
                            holder,
                        )
                        .await
                        .is_ok()
                    {
                        announced_successor = true;
                    }
                    match release_lock_committed_by_visible_transaction(store, layout, name, holder)
                        .await
                    {
                        Ok(true) => {
                            tracing::info!(%name, %holder, "reclaimed ref lease after visible commit");
                            reclaimed = true;
                            break;
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(%name, %holder, %error, "could not verify committed ref lease holder")
                        }
                    }
                }
            }
            let lock = match acquired {
                Ok(lock) => lock,
                Err(error @ CoordinationError::PushLockHeld { .. }) if deadline.is_some() => {
                    retry = Some(error);
                    break;
                }
                Err(error) => {
                    release_entries(entries).await;
                    return Err(error.into());
                }
            };
            let path = lock.path().to_owned();
            let holder = lock.holder().to_owned();
            let lease = match options.renewal_interval {
                Some(interval) => RefLease::Renewing(RenewingPushLock::start_with_interval(
                    lock, cancel, interval,
                )),
                None => RefLease::Fixed(lock),
            };
            entries.push(RefLeaseEntry {
                path,
                holder: holder.clone(),
                lease,
            });
        }

        if reclaimed {
            release_entries(entries).await;
            continue;
        }
        let Some(error) = retry else {
            let holders = names
                .iter()
                .zip(entries.iter())
                .map(|(name, entry)| (name.clone(), entry.holder.clone()))
                .collect();
            return Ok(PublicationLeases {
                entries,
                holders,
                fences: Vec::new(),
            });
        };
        release_entries(entries).await;
        let Some(deadline) = deadline else {
            return Err(error.into());
        };
        let now = Instant::now();
        if now >= deadline {
            return Err(error.into());
        }
        let cap = if announced_successor {
            SUCCESSOR_POLL_CAP
        } else {
            WAIT_BACKOFF_CAP
        };
        let delay = wait_delay(attempt, deadline.saturating_duration_since(now), cap);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => return Err(Error::Cancelled),
        }
    }
}

/// Acquire sorted ref leases followed by global and repository GC fences.
///
/// Failure at any later admission boundary releases every earlier resource.
/// Renewal loss cancels the supplied operation token; explicit release drains
/// fence and ref workers before deleting their leases.
pub async fn acquire_leases(
    store: &Store,
    layout: &StoreLayout<Store>,
    names: impl IntoIterator<Item = String>,
    options: LeaseOptions,
    cancel: &CancellationToken,
) -> Result<PublicationLeases, Error> {
    let mut leases = acquire_ref_leases(store, layout, names, options, cancel).await?;
    for domain in [layout.global_prefix(), layout.repo_prefix()] {
        if cancel.is_cancelled() {
            leases.release().await;
            return Err(Error::Cancelled);
        }
        let lease = match GcFenceLease::acquire_writer(store.inner(), domain, options.ttl).await {
            Ok(lease) => lease,
            Err(error) => {
                leases.release().await;
                return Err(error.into());
            }
        };
        let interval = options
            .renewal_interval
            .unwrap_or((options.ttl / 3).max(Duration::from_secs(1)));
        let heartbeat = GcFenceHeartbeat::spawn(&lease, cancel.clone(), interval);
        leases.fences.push((lease, heartbeat));
    }
    Ok(leases)
}

/// Proven or unresolved result after attempting a journal visibility marker.
#[derive(Debug)]
#[must_use]
pub enum CommitOutcome {
    Committed(crab_metadata::ref_journal::RefJournalCommitResult),
    Indeterminate {
        transaction_id: String,
        source: Box<crab_write::WriteError>,
    },
}

/// Classify journal commitment without treating an uncertain marker as rejection.
///
/// Other errors occurred before a marker attempt. Preserve the uncertainty's
/// source and transaction identity for backend-specific recovery; matching
/// current refs are not proof of this attempt's historical outcome.
pub fn journal_outcome(
    result: crab_write::Result<crab_metadata::ref_journal::RefJournalCommitResult>,
) -> Result<CommitOutcome, crab_write::WriteError> {
    match result {
        Ok(committed) => Ok(CommitOutcome::Committed(committed)),
        Err(error) => {
            if let crab_write::WriteError::Metadata(
                crab_metadata::error::MetadataError::RefJournalCommitUncertain {
                    transaction_id,
                    ..
                },
            ) = &error
            {
                return Ok(CommitOutcome::Indeterminate {
                    transaction_id: transaction_id.clone(),
                    source: Box::new(error),
                });
            }
            Err(error)
        }
    }
}

/// Read visibility after an accepted publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Readiness<Generation> {
    Ready { generation: Generation },
    Pending,
}

/// Finish read-generation work without rejecting an already accepted publication.
///
/// Call only after acknowledged commitment or a validated no-op, under the
/// required writer fences. The work must honor the operation's cancellation;
/// await completion so its resources drain. A failure is handled here as pending
/// readiness, with its original error recorded for authorized maintenance.
pub async fn finish_committed<Generation, E>(
    work: impl Future<Output = Result<Generation, E>>,
) -> Readiness<Generation>
where
    E: std::error::Error,
{
    match work.await {
        Ok(generation) => Readiness::Ready { generation },
        Err(error) => {
            tracing::warn!(?error, "committed publication read readiness pending");
            Readiness::Pending
        }
    }
}

/// Run an operation under a renewing internal-resource lease.
///
/// Acquire operation-identity leases before ref leases, and manifest leases
/// after ref/GC admission. The callback must check its token before committing
/// and preserve any known outcome. Await completion even after cancellation;
/// cleanup cannot replace the callback's result.
pub async fn with_internal_lease<T, E, F, Fut>(
    store: &Store,
    layout: &StoreLayout<Store>,
    resource: &str,
    ttl: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> Result<T, E>
where
    E: From<Error>,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let cancel = cancel.child_token();
    if cancel.is_cancelled() {
        return Err(E::from(Error::Cancelled));
    }
    let lock = PushLock::acquire_internal(store.inner(), layout.repo_prefix(), resource, ttl)
        .await
        .map_err(Error::from)?;
    let lease = RenewingPushLock::start(lock, &cancel);
    let result = operation(cancel).await;
    lease.release().await;
    result
}

/// Admit an unattempted direct publication plan under its renewing operation lease.
///
/// Authorize first and acquire ref leases inside the callback. Any durable
/// attempt blocks execution, including an intent whose outcome is unresolved.
/// The callback must persist its intent before attempting commitment and drain
/// after cancellation; managed authority remains outside this entry point.
pub async fn with_plan<T, E, F, Fut>(
    store: &Store,
    layout: &StoreLayout<Store>,
    plan_id: &str,
    ttl: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> Result<T, E>
where
    E: From<Error> + From<crab_metadata::error::MetadataError>,
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let resource = format!("publication-plan-{plan_id}");
    with_internal_lease(store, layout, &resource, ttl, cancel, |cancel| async move {
        // Admission and execution share one lease: receipt absence cannot authorize
        // replay. This read owns no sessions; cancellation stops it while the
        // enclosing owner still releases the operation lease.
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(E::from(Error::Cancelled)),
            result = crab_metadata::plan_receipt::ensure_plan_unattempted(store, layout, plan_id) => result?,
        }
        operation(cancel).await
    })
    .await
}

/// Run publication under sorted ref leases and global then repository GC fences.
///
/// Authorize before entering; hold any operation-identity lease outside this
/// scope. The callback must preserve marker-attempt outcomes and check its token
/// before publication. Await this future to completion even after cancellation;
/// dropping it does not drain the operation or release its leases.
pub async fn with_leases<T, E, F, Fut>(
    store: &Store,
    layout: &StoreLayout<Store>,
    names: impl IntoIterator<Item = String>,
    ttl: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> Result<T, E>
where
    E: From<Error>,
    F: FnOnce(BTreeMap<String, String>, CancellationToken) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let cancel = cancel.child_token();
    let leases = acquire_leases(store, layout, names, LeaseOptions::renewing(ttl), &cancel)
        .await
        .map_err(E::from)?;
    let result = operation(leases.holders().clone(), cancel.clone()).await;
    // The operation may have crossed its visibility boundary. Drain every
    // acquired resource without converting that recorded outcome to rejection.
    leases.release().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announced_successor_poll_stays_inside_handoff_window() {
        let delay = wait_delay(32, Duration::from_secs(30), SUCCESSOR_POLL_CAP);

        assert!(delay <= SUCCESSOR_POLL_CAP);
    }
}
