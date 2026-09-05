//! Commit ref edits and fold committed transactions into generations.
use std::sync::Arc;
use std::time::{Duration, Instant};

use crab_coordination::{PushLock, PushLockAcquireContext};
use crab_metadata::{
    manifest_store::RepositorySnapshot,
    manifests::PackManifestEntry,
    ref_journal::{self, RefJournalCommitResult, RefJournalEdit, RefJournalTransaction},
};
use crab_storage::{Store, StoreLayout};
use rand::Rng;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{Result, WriteError};

const REF_JOURNAL_COMPACTION_LOCK_WAIT_TTL_MULTIPLIER: u32 = 2;
// Yield the manifest lease after a bounded wave count, even under continuous writes.
const MAX_REF_JOURNAL_COMPACTION_PASSES: usize = 5;

/// Commit a validated batch against a snapshot captured while holding every edited ref lease.
///
/// The caller owns authorization, ref/graph/dependency validation, GC fencing,
/// immutable artifact and visibility evidence uploads, and lease renewal through
/// completion. This function checks expected old values, reads causal parents,
/// and publishes through the journal's atomic active marker. It does not publish
/// a readable catalog or release the caller's leases. Ref-name changes acquire
/// a separate namespace lease and recheck a fresh snapshot before publication.
/// Await completion without dropping the
/// future; a storage error at the marker may require commit-outcome recovery.
pub async fn commit_edits(
    store: &Store,
    router: &StoreLayout<Store>,
    snapshot: &RepositorySnapshot,
    edits: Vec<RefJournalEdit>,
    head: Option<String>,
    packs: Vec<PackManifestEntry>,
    shards: Vec<String>,
    cancel: &CancellationToken,
) -> Result<RefJournalCommitResult> {
    commit_edits_inner(
        store,
        router,
        snapshot,
        edits,
        head,
        packs,
        shards,
        CommitContext {
            plan_id: None,
            cancel,
        },
    )
    .await
}

/// Mirror-plan attribution and cancellation for one journal commit.
pub struct MirrorPlanContext<'a> {
    plan_id: &'a str,
    cancel: &'a CancellationToken,
}

impl<'a> MirrorPlanContext<'a> {
    #[must_use]
    pub fn new(plan_id: &'a str, cancel: &'a CancellationToken) -> Self {
        Self { plan_id, cancel }
    }
}

/// Commit a validated batch and bind its terminal outcome to a mirror plan.
pub async fn commit_edits_for_plan(
    store: &Store,
    router: &StoreLayout<Store>,
    snapshot: &RepositorySnapshot,
    edits: Vec<RefJournalEdit>,
    head: Option<String>,
    packs: Vec<PackManifestEntry>,
    shards: Vec<String>,
    context: MirrorPlanContext<'_>,
) -> Result<RefJournalCommitResult> {
    commit_edits_inner(
        store,
        router,
        snapshot,
        edits,
        head,
        packs,
        shards,
        CommitContext {
            plan_id: Some(context.plan_id),
            cancel: context.cancel,
        },
    )
    .await
}

struct CommitContext<'a> {
    plan_id: Option<&'a str>,
    cancel: &'a CancellationToken,
}

async fn commit_edits_inner(
    store: &Store,
    router: &StoreLayout<Store>,
    snapshot: &RepositorySnapshot,
    edits: Vec<RefJournalEdit>,
    head: Option<String>,
    packs: Vec<PackManifestEntry>,
    shards: Vec<String>,
    context: CommitContext<'_>,
) -> Result<RefJournalCommitResult> {
    let CommitContext { plan_id, cancel } = context;
    check_cancelled(cancel)?;
    let parents = edits
        .iter()
        .map(|edit| (edit.ref_name.clone(), None))
        .collect();
    // Validate the whole batch before any I/O; one invalid or stale edit must
    // not leave immutable transaction bodies or prepared heads for its siblings.
    let transaction = RefJournalTransaction::new(parents, edits, head, packs, shards)?;
    check_old_values(router, snapshot, &transaction)?;
    let changes_namespace = transaction
        .edits
        .iter()
        .any(|edit| edit.old_oid.is_none() != edit.new_oid.is_none());
    if !changes_namespace {
        return commit_transaction(store, router, transaction, plan_id, cancel).await;
    }
    check_namespace(snapshot, &transaction)?;
    crate::with_ref_namespace(
        store,
        router,
        crab_coordination::DEFAULT_PUSH_LOCK_TTL,
        cancel,
        |scoped| async move {
            check_cancelled(&scoped)?;
            let fresh =
                crab_metadata::manifest_store::read_repository_snapshot(store, router).await?;
            check_old_values(router, &fresh, &transaction)?;
            check_namespace(&fresh, &transaction)?;
            commit_transaction(store, router, transaction, plan_id, &scoped).await
        },
    )
    .await
}

fn check_old_values(
    router: &StoreLayout<Store>,
    snapshot: &RepositorySnapshot,
    transaction: &RefJournalTransaction,
) -> Result<()> {
    for edit in &transaction.edits {
        if snapshot.journal.refs.get(&edit.ref_name) != edit.old_oid.as_ref() {
            return Err(WriteError::RefChanged {
                ref_name: edit.ref_name.clone(),
                path: router
                    .ref_journal_head_path(&ref_journal::ref_name_hash(&edit.ref_name))
                    .to_string(),
            });
        }
    }
    Ok(())
}

fn check_namespace(
    snapshot: &RepositorySnapshot,
    transaction: &RefJournalTransaction,
) -> Result<()> {
    let removed: std::collections::BTreeSet<_> = transaction
        .edits
        .iter()
        .filter(|edit| edit.new_oid.is_none())
        .map(|edit| edit.ref_name.as_str())
        .collect();
    let retained = snapshot
        .journal
        .refs
        .keys()
        .map(String::as_str)
        .filter(|name| !removed.contains(name));
    let added = transaction
        .edits
        .iter()
        .filter(|edit| edit.new_oid.is_some())
        .map(|edit| edit.ref_name.as_str());
    crab_git::refname::validate_ref_namespace(retained.chain(added))?;
    Ok(())
}

async fn commit_transaction(
    store: &Store,
    router: &StoreLayout<Store>,
    mut transaction: RefJournalTransaction,
    plan_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<RefJournalCommitResult> {
    let mut expected_heads = Vec::with_capacity(transaction.edits.len());
    for edit in &transaction.edits {
        check_cancelled(cancel)?;
        let observed = ref_journal::read_ref_head(store, router, &edit.ref_name).await?;
        transaction
            .parents
            .insert(edit.ref_name.clone(), observed.visible_transaction.clone());
        expected_heads.push(observed);
    }
    Ok(match plan_id {
        Some(plan_id) => {
            ref_journal::commit_ref_transaction_for_plan(
                store,
                router,
                &transaction,
                &expected_heads,
                plan_id,
                || cancel.is_cancelled(),
            )
            .await?
        }
        None => {
            ref_journal::commit_ref_transaction(
                store,
                router,
                &transaction,
                &expected_heads,
                || cancel.is_cancelled(),
            )
            .await?
        }
    })
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(WriteError::Cancelled);
    }
    Ok(())
}

pub(crate) fn push_lock_wait_delay(attempt: u32, remaining: Duration) -> Duration {
    let shift = 1_u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let bound = Duration::from_millis(250)
        .saturating_mul(shift)
        .min(Duration::from_secs(2))
        .min(remaining);
    let nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rand::rng().random_range(1..=nanos))
}

async fn acquire_ref_journal_compaction_lock(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction_id: &str,
    ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<PushLock>> {
    let deadline =
        Instant::now() + ttl.saturating_mul(REF_JOURNAL_COMPACTION_LOCK_WAIT_TTL_MULTIPLIER);
    let mut attempt = 0;
    let mut acquire_context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    loop {
        if !crab_metadata::ref_journal::transaction_is_active(store, router, transaction_id).await?
        {
            return Ok(None);
        }
        check_cancelled(cancel)?;
        match acquire_context
            .acquire_internal(
                router.repo_prefix(),
                crab_coordination::GIT_MANIFEST_RESOURCE,
                ttl,
            )
            .await
            .map_err(WriteError::from)
        {
            Ok(lock) => return Ok(Some(lock)),
            Err(
                error @ WriteError::Coordination(
                    crab_coordination::CoordinationError::PushLockHeld { .. },
                ),
            ) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(error);
                }
                let delay = push_lock_wait_delay(attempt, deadline.saturating_duration_since(now));
                attempt = attempt.saturating_add(1);
                debug!(
                    attempt,
                    delay_ms = delay.as_millis(),
                    %transaction_id,
                    "committed ref transaction is waiting for compaction handoff"
                );
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = cancel.cancelled() => return Err(WriteError::Cancelled),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn try_acquire_ref_journal_compaction_lock(
    store: &Store,
    router: &StoreLayout<Store>,
    transaction_id: &str,
    ttl: Duration,
) -> Result<Option<PushLock>> {
    if !crab_metadata::ref_journal::transaction_is_active(store, router, transaction_id).await? {
        return Ok(None);
    }

    let mut acquire_context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    match acquire_context
        .try_acquire_internal(
            router.repo_prefix(),
            crab_coordination::GIT_MANIFEST_RESOURCE,
            ttl,
        )
        .await
        .map_err(WriteError::from)
    {
        Ok(lock) => Ok(Some(lock)),
        Err(WriteError::Coordination(crab_coordination::CoordinationError::PushLockHeld {
            ..
        })) => Ok(None),
        Err(error) => Err(error),
    }
}

async fn compact_ref_journal_until_idle(
    store: &Store,
    router: &StoreLayout<Store>,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<Option<crab_metadata::manifest_store::RefJournalCompaction>> {
    let mut latest = None;
    let mut passes = 0;
    while passes < MAX_REF_JOURNAL_COMPACTION_PASSES {
        // Cancellation stops the next wave, never the in-flight manifest CAS
        // and holder cleanup for a transaction that is already committed.
        check_cancelled(cancel)?;
        let compacted = crab_metadata::manifest_store::compact_ref_journal(
            store,
            router,
            crab_types::time::now_rfc3339_millis(),
            pusher.clone(),
            uuid::Uuid::now_v7().to_string(),
        )
        .await?;
        let Some(compaction) = compacted else {
            return Ok(latest);
        };
        passes += 1;
        debug!(
            pass = passes,
            generation = compaction.manifest.generation,
            "ref journal compactor drained one visible transaction wave"
        );
        for (ref_name, holder) in &compaction.edited_ref_lock_holders {
            match PushLock::release_ref_if_holder(
                store.inner(),
                router.repo_prefix(),
                ref_name,
                holder,
            )
            .await
            .map_err(WriteError::from)
            {
                Ok(true) => debug!(
                    %ref_name,
                    %holder,
                    "released ref lock after journal compaction"
                ),
                Ok(false) => debug!(
                    %ref_name,
                    %holder,
                    "ref lock was already handed off after journal compaction"
                ),
                Err(error) => warn!(
                    %ref_name,
                    %holder,
                    %error,
                    "ref lock cleanup after journal compaction failed"
                ),
            }
        }
        latest = Some(compaction);
    }
    // Bound ownership so a continuous push stream cannot monopolize the
    // derived lock. Any transaction left active waits and becomes the next
    // owner through the handoff protocol.
    debug!(
        passes,
        "ref journal compactor reached its bounded drain limit"
    );
    Ok(latest)
}

async fn compact_ref_journal_with_lock(
    store: &Store,
    router: &StoreLayout<Store>,
    mut lock: PushLock,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let operation = crab_coordination::while_renewing(
        &mut lock,
        Some(cancel),
        compact_ref_journal_until_idle(store, router, pusher, cancel),
    )
    .await;
    let release = lock.release().await.map_err(WriteError::from);
    match (operation, release) {
        (Ok(compacted), Ok(())) => Ok(compacted.is_some()),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => {
            warn!(
                error = %release_error,
                "ref journal compaction lock release also failed after owner error"
            );
            Err(error)
        }
    }
}

/// Compact committed ref-journal transactions under the generation owner.
///
/// A push only publishes the immutable transaction and its visibility
/// evidence. The owner folds that bounded journal into the manifest after the
/// ref lock is released, so repository-sized metadata work cannot delay the
/// push acknowledgement.
///
/// Await to completion even after cancellation so the manifest lease is released.
/// Cancellation stops between complete waves; committed refs are not rolled back.
/// Returns whether any wave compacted, or the primary operation/release error.
pub async fn compact_for_owner(
    store: &Store,
    router: &StoreLayout<Store>,
    lock_ttl: Duration,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_cancelled(cancel)?;
    let active = crab_metadata::ref_journal::list_active_transactions(store, router).await?;
    let Some(transaction_id) = active.first() else {
        return Ok(false);
    };
    let Some(lock) =
        acquire_ref_journal_compaction_lock(store, router, transaction_id, lock_ttl, cancel)
            .await?
    else {
        return Ok(false);
    };
    compact_ref_journal_with_lock(store, router, lock, pusher, cancel).await
}

/// Make one bounded, non-blocking reader repair attempt for an active ref journal.
///
/// Upload-pack readers may arrive as a large fanout while a push is handing
/// off its ref journal. Only one reader may compact; other readers retry their
/// normal admission path without waiting on or probing the manifest lease.
///
/// Await to completion even after cancellation so the manifest lease is released.
/// The scheduling budget is checked between batches, not during storage writes.
/// Returns false if no work remains or another actor owns the lease.
pub async fn compact_for_reader(
    store: &Store,
    router: &StoreLayout<Store>,
    lock_ttl: Duration,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_cancelled(cancel)?;
    let active = crab_metadata::ref_journal::list_active_transactions(store, router).await?;
    let Some(transaction_id) = active.first() else {
        return Ok(false);
    };
    let Some(lock) =
        try_acquire_ref_journal_compaction_lock(store, router, transaction_id, lock_ttl).await?
    else {
        debug!(
            %transaction_id,
            "reader skipped ref journal compaction because another actor owns the manifest lease"
        );
        return Ok(false);
    };
    let deadline = Instant::now() + (lock_ttl / 2).max(Duration::from_secs(1));
    let mut lock = lock;
    let operation = crab_coordination::while_renewing(&mut lock, Some(cancel), async {
        let mut compacted = false;
        while Instant::now() < deadline {
            check_cancelled(cancel)?;
            let pass =
                compact_ref_journal_until_idle(store, router, pusher.clone(), cancel).await?;
            if pass.is_none() {
                break;
            }
            compacted = true;
        }
        Ok::<_, WriteError>(compacted)
    })
    .await;
    let release = lock.release().await.map_err(WriteError::from);
    match (operation, release) {
        (Ok(compacted), Ok(())) => Ok(compacted),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => {
            warn!(
                error = %release_error,
                "reader ref journal lock release also failed after compaction error"
            );
            Err(error)
        }
    }
}
