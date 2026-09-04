//! Fold committed ref transactions into generations under the manifest lease.
use std::sync::Arc;
use std::time::{Duration, Instant};

use crab_coordination::{PushLock, PushLockAcquireContext};
use crab_storage::{Store, StoreLayout};
use rand::Rng;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{Result, WriteError};

const REF_JOURNAL_COMPACTION_LOCK_WAIT_TTL_MULTIPLIER: u32 = 2;
// Yield the manifest lease after a bounded wave count, even under continuous writes.
const MAX_REF_JOURNAL_COMPACTION_PASSES: usize = 5;

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(WriteError::Cancelled);
    }
    Ok(())
}

fn push_lock_wait_delay(attempt: u32, remaining: Duration) -> Duration {
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
