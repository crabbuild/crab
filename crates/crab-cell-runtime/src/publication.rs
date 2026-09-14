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
        self.prepare_cuts(
            base.as_ref(),
            pending.cuts(),
            pending.outcome().commit_sequence(),
        )
        .await
    }

    pub(crate) async fn prepare_initial(
        &self,
        cuts: &crab_ltx::CaptureBatch,
    ) -> Result<crab_ltx::PreparedRoot> {
        if self.observed.value().root.is_some() {
            return Err(Error::Control("bootstrap control already has a root"));
        }
        self.prepare_cuts(None, cuts, 0).await
    }

    async fn prepare_cuts(
        &self,
        base: Option<&crab_ltx::RootRef>,
        cuts: &crab_ltx::CaptureBatch,
        commit_sequence: u64,
    ) -> Result<crab_ltx::PreparedRoot> {
        let mut backoff = PublicationBackoff::default();
        loop {
            match self
                .replica
                .prepare(base, cuts, commit_sequence, self.observed.value().schema)
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
                .transition(&self.observed, successor.clone(), Transition::Publish)
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
                        return if self.observed.value().is_same_or_pure_renewal_of(&successor) {
                            Ok(prepared.root())
                        } else {
                            Err(Error::Fenced)
                        };
                    }
                    if current
                        .value()
                        .is_same_or_pure_renewal_of(self.observed.value())
                        && retryable_publication_error(&error)
                    {
                        self.observed = current;
                        backoff.wait(runtime_retry_hint(&error)).await;
                        continue;
                    }
                    return Err(
                        if current
                            .value()
                            .is_same_or_pure_renewal_of(self.observed.value())
                        {
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

    /// Releases ownership after the SQL worker has closed the drained Cell.
    pub(crate) async fn release(&mut self) -> Result<()> {
        let mut backoff = PublicationBackoff::default();
        loop {
            let successor = self.observed.value().release()?;
            match self
                .authority
                .transition(&self.observed, successor.clone(), Transition::Release)
                .await
            {
                Ok(released) => {
                    self.observed = released;
                    return Ok(());
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
                    if current.value() == &successor {
                        self.observed = current;
                        return Ok(());
                    }
                    let still_owned = current
                        .value()
                        .is_same_or_pure_renewal_of(self.observed.value());
                    if still_owned && retryable_publication_error(&error) {
                        self.observed = current;
                        backoff.wait(runtime_retry_hint(&error)).await;
                        continue;
                    }
                    return Err(if still_owned { error } else { Error::Fenced });
                }
            }
        }
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
