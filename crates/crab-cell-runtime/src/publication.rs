use crate::{
    CellAuthority, CellExecutor, Error, Result, StoredOutcome, Transition, VersionedControl,
};

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

    /// Publishes the executor's retained commit or reconciles an ambiguous CAS.
    pub async fn publish_pending(
        &mut self,
        executor: &mut CellExecutor,
        next_due_ms: Option<i64>,
    ) -> Result<StoredOutcome> {
        let pending = executor.pending().ok_or(Error::PendingPublication)?;
        let base = self.observed.value().ltx_root();
        let prepared = self
            .replica
            .prepare(
                base.as_ref(),
                pending.cuts(),
                pending.outcome().commit_sequence(),
                self.observed.value().schema,
            )
            .await?;
        executor.bind_prepared(&prepared)?;
        let successor = self
            .observed
            .value()
            .publish_prepared(&prepared, next_due_ms)?;
        match self
            .authority
            .transition(&self.observed, successor, Transition::Publish)
            .await
        {
            Ok(published) => {
                self.observed = published;
                executor.confirm_published(&prepared.root())
            }
            Err(error) => {
                let Some(current) = self.authority.load(self.observed.value().cell).await? else {
                    executor.fence();
                    return Err(Error::Fenced);
                };
                if current.value().ltx_root() == Some(prepared.root()) {
                    self.observed = current;
                    return executor.confirm_published(&prepared.root());
                }
                if retryable_after_renewal(self.observed.value(), current.value()) {
                    self.observed = current;
                    return Err(error);
                }
                executor.fence();
                Err(Error::Fenced)
            }
        }
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
