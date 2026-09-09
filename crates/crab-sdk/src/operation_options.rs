use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::remote_error::remote_error;
use crate::{ReadLimits, Result};

/// Shared caller cancellation for one or more SDK operations.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(CancellationToken);

impl Cancellation {
    /// Signal cancellation; operation futures still wait for owned cleanup.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    pub(crate) fn token(&self) -> &CancellationToken {
        &self.0
    }
}

/// Cancellation, deadline and finite limits shared by SDK operation entry points.
#[derive(Clone, Debug, Default)]
pub struct OperationOptions {
    pub(crate) cancellation: Cancellation,
    pub(crate) progress: Option<crate::Progress>,
    pub(crate) deadline: Option<Instant>,
    limits: ReadLimits,
    timeout: Option<Duration>,
}

impl OperationOptions {
    /// Send bounded, coalescing observations without routing terminal results to progress.
    #[must_use]
    pub fn with_progress(mut self, progress: crate::Progress) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Use a caller-owned cancellation scope without cancelling sibling operations on drop.
    #[must_use]
    pub fn with_cancellation(mut self, cancellation: Cancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Set an absolute deadline, including time before the operation starts polling.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Set nonzero finite aggregate work limits.
    pub fn with_limits(mut self, limits: ReadLimits) -> Result<Self> {
        self.limits = limits;
        self.validate()?;
        Ok(self)
    }

    /// Set a nonzero owner duration; an absolute deadline may expire sooner.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.timeout = Some(timeout);
        self.validate()?;
        Ok(self)
    }

    /// Return the selected aggregate work limits.
    #[must_use]
    pub fn read_limits(&self) -> ReadLimits {
        self.limits
    }

    fn validate(&self) -> Result<()> {
        crab_remote_git::RepositoryOptions::new(Default::default(), self.owner_limits())
            .map(|_| ())
            .map_err(remote_error)
    }

    pub(crate) fn owner_limits(&self) -> crab_remote_git::OperationLimits {
        let mut limits = self.limits.into_owner();
        if let Some(timeout) = self.timeout {
            limits.max_duration = timeout;
        }
        limits
    }

    #[cfg(any(feature = "write", feature = "managed"))]
    pub(crate) fn with_owner_deadline(mut self) -> Result<Self> {
        let deadline = Instant::now()
            .checked_add(self.owner_limits().max_duration)
            .ok_or_else(|| {
                crate::Error::new(
                    crate::ErrorKind::InvalidInput,
                    "operation timeout exceeds clock range",
                )
            })?;
        self.deadline = Some(self.deadline.map_or(deadline, |prior| prior.min(deadline)));
        Ok(self)
    }
}
