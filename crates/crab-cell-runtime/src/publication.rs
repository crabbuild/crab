use crate::{
    CellAuthority, CellExecutor, Error, Result, StoredOutcome, Transition, VersionedControl,
};

const MAX_RETRY_DELAY_MS: u64 = 1_000;

/// Coordinates immutable preparation, authority CAS and result release.
///
/// A failed CAS response is reconciled against origin before returning. Exact
/// prepared-root equality proves success; a pure renewal can be retried without
/// rerunning SQL. Any ownership or root divergence fences local admission.
pub struct CellPublisher {
    replica: crab_ltx::CellReplica,
    authority: CellAuthority,
    observed: VersionedControl,
}

impl CellPublisher {
    #[must_use]
    pub fn new(
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> Self {
        Self {
            replica,
            authority,
            observed,
        }
    }

    #[must_use]
    pub fn control(&self) -> &VersionedControl {
        &self.observed
    }

    pub(crate) async fn prepare(
        &self,
        pending: &crate::PendingCommit,
    ) -> Result<crab_ltx::PreparedRoot> {
        let base = self.observed.value().ltx_root();
        let mut backoff = PublicationBackoff::default();
        loop {
            match self
                .replica
                .prepare(
                    base.as_ref(),
                    pending.cuts(),
                    pending.outcome().commit_sequence(),
                    self.observed.value().schema,
                )
                .await
            {
                Ok(prepared) => return Ok(prepared),
                Err(error) if retryable_ltx_error(&error) => {
                    backoff.wait(ltx_retry_hint(&error)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) async fn publish_prepared(
        &mut self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
    ) -> Result<crab_ltx::RootRef> {
        let mut backoff = PublicationBackoff::default();
        loop {
            let successor = self
                .observed
                .value()
                .publish_prepared(prepared, next_due_ms)?;
            match self
                .authority
                .transition(&self.observed, successor, Transition::Publish)
                .await
            {
                Ok(published) => {
                    self.observed = published;
                    return Ok(prepared.root());
                }
                Err(error) => {
                    let current = loop {
                        match self.authority.load(self.observed.value().cell).await {
                            Ok(Some(current)) => break current,
                            Ok(None) => return Err(Error::Fenced),
                            Err(load_error) if retryable_publication_error(&load_error) => {
                                backoff.wait(runtime_retry_hint(&load_error)).await;
                            }
                            Err(load_error) => return Err(load_error),
                        }
                    };
                    if current.value().ltx_root() == Some(prepared.root()) {
                        self.observed = current;
                        return Ok(prepared.root());
                    }
                    if retryable_after_renewal(self.observed.value(), current.value())
                        && retryable_publication_error(&error)
                    {
                        self.observed = current;
                        backoff.wait(runtime_retry_hint(&error)).await;
                        continue;
                    }
                    return Err(
                        if retryable_after_renewal(self.observed.value(), current.value()) {
                            error
                        } else {
                            Error::Fenced
                        },
                    );
                }
            }
        }
    }

    /// Publishes the executor's retained commit or reconciles an ambiguous CAS.
    pub async fn publish_pending(
        &mut self,
        executor: &mut CellExecutor,
        next_due_ms: Option<i64>,
    ) -> Result<StoredOutcome> {
        let pending = executor.pending().ok_or(Error::PendingPublication)?;
        let prepared = self.prepare(pending).await?;
        executor.bind_prepared(&prepared)?;
        let root = match self.publish_prepared(&prepared, next_due_ms).await {
            Ok(root) => root,
            Err(error @ Error::Fenced) => {
                executor.fence();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        executor.confirm_published(&root)
    }
}

fn retryable_publication_error(error: &Error) -> bool {
    let Error::Storage(error) = error else {
        return false;
    };
    retryable_storage_error(error)
}

fn retryable_ltx_error(error: &crab_ltx::CrabError) -> bool {
    matches!(
        error,
        crab_ltx::CrabError::Storage(error)
            if retryable_storage_error(error)
    )
}

fn runtime_retry_hint(error: &Error) -> Option<std::time::Duration> {
    let Error::Storage(error) = error else {
        return None;
    };
    storage_retry_hint(error)
}

fn ltx_retry_hint(error: &crab_ltx::CrabError) -> Option<std::time::Duration> {
    let crab_ltx::CrabError::Storage(error) = error else {
        return None;
    };
    storage_retry_hint(error)
}

fn storage_retry_hint(error: &crab_storage::StorageError) -> Option<std::time::Duration> {
    match crab_storage::retry_class(error) {
        crab_storage::RetryClass::Throttled { retry_after } => retry_after,
        _ => None,
    }
}

fn retryable_storage_error(error: &crab_storage::StorageError) -> bool {
    matches!(
        crab_storage::retry_class(error),
        crab_storage::RetryClass::Transient
            | crab_storage::RetryClass::Throttled { .. }
            | crab_storage::RetryClass::StateDependent
            | crab_storage::RetryClass::InspectErrno
    )
}

struct PublicationBackoff {
    delay_ms: u64,
}

impl Default for PublicationBackoff {
    fn default() -> Self {
        Self { delay_ms: 100 }
    }
}

impl PublicationBackoff {
    async fn wait(&mut self, minimum: Option<std::time::Duration>) {
        let delay = std::time::Duration::from_millis(self.delay_ms);
        tokio::time::sleep(minimum.map_or(delay, |minimum| minimum.max(delay))).await;
        self.delay_ms = self.delay_ms.saturating_mul(2).min(MAX_RETRY_DELAY_MS);
    }
}

fn retryable_after_renewal(previous: &crate::Control, current: &crate::Control) -> bool {
    previous == current
        || (previous.cell == current.cell
            && previous.incarnation == current.incarnation
            && previous.epoch == current.epoch
            && previous.state == current.state
            && previous.owner == current.owner
            && previous.root == current.root
            && previous.code == current.code
            && previous.schema == current.schema
            && previous.next_due_ms == current.next_due_ms
            && current.revision > previous.revision
            && current.progress > previous.progress)
}
