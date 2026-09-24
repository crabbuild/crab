//! Deterministic qualification workloads and execution accounting.

use super::profile::{
    MAX_QUALIFICATION_CELLS, MAX_QUALIFICATION_CONCURRENCY, MAX_QUALIFICATION_DURATION_SECS,
    MAX_QUALIFICATION_OPERATIONS, MAX_RECEIPT_BYTES, mark_case_coverage, validate_label,
};
use super::*;

mod operation;
mod summary;

use operation::{LCG_SEED_XOR, lcg_state_at, operation_from_state};
pub use operation::{
    QualificationExecution, QualificationOperation, QualificationOperationIter,
    QualificationOutcome, QualificationPrimitiveCounts,
};
pub(super) use operation::{qualification_workload_outcome_digest, valid_primitive_counts};
pub(super) use summary::qualification_run_outcome_digest;
pub use summary::{QualificationLatencyHistogram, QualificationRunSummary};

/// Async application boundary used by [`QualificationWorkload::run`].
pub trait QualificationOperationExecutor {
    /// Future returned for one operation.
    type Future<'a>: Future<Output = Result<QualificationExecution>> + Send + 'a
    where
        Self: 'a;

    /// Runs one workload operation.
    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a>;
}

/// Deterministic logical workload artifact for PR, provider, and scale tiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkload {
    pub(super) schema_version: u32,
    pub(super) profile: String,
    pub(super) profile_digest: [u8; 32],
    pub(super) seed: u64,
    pub(super) cells: u64,
    pub(super) operations: u64,
    pub(super) duration_secs: u64,
    pub(super) primitives: Vec<QualificationPrimitiveCounts>,
    pub(super) outcome_digest: [u8; 32],
}

impl QualificationWorkload {
    /// Generates the canonical operation schedule and logical outcome digest.
    pub fn generate(profile: &QualificationProfile, seed: u64) -> Result<Self> {
        profile.validate()?;
        Self::generate_with_size(
            profile,
            seed,
            profile.minimum_cells(),
            profile
                .minimum_operations()
                .max(QUALIFICATION_CASE_COVERAGE_OPERATIONS),
            profile.minimum_duration_secs(),
        )
    }

    /// Generates a bounded custom-size schedule for fast contract tests.
    pub fn generate_with_size(
        profile: &QualificationProfile,
        seed: u64,
        cells: u64,
        operations: u64,
        duration_secs: u64,
    ) -> Result<Self> {
        profile.validate()?;
        let minimum_operations = profile
            .minimum_operations()
            .max(QUALIFICATION_PRIMITIVES.len() as u64);
        if cells == 0
            || cells > MAX_QUALIFICATION_CELLS
            || operations == 0
            || operations > MAX_QUALIFICATION_OPERATIONS
            || duration_secs == 0
            || duration_secs > MAX_QUALIFICATION_DURATION_SECS
            || cells < profile.minimum_cells()
            || operations < minimum_operations
            || duration_secs < profile.minimum_duration_secs()
        {
            return Err(Error::Control(
                "qualification workload is below profile minimum",
            ));
        }
        let mut primitives = QUALIFICATION_PRIMITIVES
            .iter()
            .map(|primitive| QualificationPrimitiveCounts::new(primitive))
            .collect::<Vec<_>>();
        for operation in QualificationOperationIter::new(seed, cells, operations) {
            let primitive_index = operation.primitive_index as usize;
            let counts = &mut primitives[primitive_index];
            counts.attempted += 1;
            if operation.rejection_hint {
                counts.rejected += 1;
            } else if operation.ambiguous_hint {
                counts.ambiguous += 1;
                if operation.retry_hint {
                    counts.retried += 1;
                }
            } else {
                counts.acknowledged += 1;
                if operation.retry_hint {
                    counts.retried += 1;
                }
                counts.verified += 1;
            }
        }
        let outcome_digest = qualification_workload_outcome_digest(seed, cells, operations);
        Ok(Self {
            schema_version: 1,
            profile: profile.name.clone(),
            profile_digest: *profile.digest()?.as_bytes(),
            seed,
            cells,
            operations,
            duration_secs,
            primitives,
            outcome_digest: *outcome_digest.as_bytes(),
        })
    }

    /// Decodes and validates a canonical workload artifact.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification workload exceeds limit"));
        }
        let workload: Self = serde_json::from_slice(bytes)?;
        workload.validate()?;
        if serde_json::to_vec(&workload).map_err(Error::from)? != bytes {
            return Err(Error::Control("qualification workload is not canonical"));
        }
        Ok(workload)
    }

    /// Encodes the workload artifact with stable field ordering.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification workload exceeds limit"));
        }
        Ok(bytes)
    }

    /// Returns the threshold profile the workload was built for.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Returns the digest of that threshold profile.
    #[must_use]
    pub const fn profile_digest(&self) -> Digest {
        Digest::from_bytes(self.profile_digest)
    }

    /// Returns the seed the operation schedule is derived from.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Returns how many Cells the workload addresses.
    #[must_use]
    pub const fn cells(&self) -> u64 {
        self.cells
    }

    /// Returns how many operations the schedule contains.
    #[must_use]
    pub const fn operations(&self) -> u64 {
        self.operations
    }

    /// Returns the target duration in seconds.
    #[must_use]
    pub const fn duration_secs(&self) -> u64 {
        self.duration_secs
    }

    /// Returns the per-primitive operation counts.
    #[must_use]
    pub fn primitives(&self) -> &[QualificationPrimitiveCounts] {
        &self.primitives
    }

    /// Returns a streaming iterator over the canonical operation schedule.
    #[must_use]
    pub fn iter_operations(&self) -> QualificationOperationIter {
        QualificationOperationIter::new(self.seed, self.cells, self.operations)
    }

    /// Returns one operation by index without materializing the schedule.
    pub fn operation_at(&self, index: u64) -> Result<QualificationOperation> {
        if index >= self.operations {
            return Err(Error::Control("qualification operation index"));
        }
        let state = lcg_state_at(self.seed ^ LCG_SEED_XOR, index + 1);
        Ok(operation_from_state(index, state, self.cells))
    }

    /// Returns the digest of the measured outcome.
    #[must_use]
    pub const fn outcome_digest(&self) -> Digest {
        Digest::from_bytes(self.outcome_digest)
    }

    /// Executes every scheduled operation through a typed application adapter.
    ///
    /// The executor owns request construction, retries, and post-commit
    /// verification. This method only retains bounded counters and a fixed
    /// latency histogram, making it suitable for million-operation runs.
    pub async fn run<E>(&self, executor: &mut E) -> Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        self.run_internal(executor, false).await
    }

    /// Executes the schedule and fails unless every executor result identifies
    /// the lifecycle case assigned to that operation.
    pub async fn run_with_case_coverage<E>(
        &self,
        executor: &mut E,
    ) -> Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        self.run_internal(executor, true).await
    }

    pub(super) async fn run_internal<E>(
        &self,
        executor: &mut E,
        require_case: bool,
    ) -> Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor,
    {
        let started = Instant::now();
        let mut counts = QUALIFICATION_PRIMITIVES
            .iter()
            .map(|primitive| QualificationPrimitiveCounts::new(primitive))
            .collect::<Vec<_>>();
        let mut case_coverage = vec![0; QUALIFICATION_CASE_COVERAGE_BYTES];
        let mut latency = QualificationLatencyHistogram::default();
        for operation in self.iter_operations() {
            let operation_started = Instant::now();
            let execution = executor.execute(operation).await?;
            if require_case && execution.case() != Some(operation.case()) {
                return Err(Error::Control(
                    "qualification executor did not report lifecycle case",
                ));
            }
            Self::record_execution(
                &mut counts,
                &mut case_coverage,
                &mut latency,
                operation,
                execution,
                operation_started.elapsed(),
            );
        }
        self.summary(counts, case_coverage, latency, started.elapsed())
    }

    /// Executes the schedule with bounded in-flight operations through cloned
    /// typed executors. The executor must make operations independent or
    /// idempotent when it opts into concurrency; aggregate counters and the
    /// logical outcome digest remain schedule-order independent.
    pub async fn run_concurrent<E>(
        &self,
        executor: E,
        concurrency: usize,
    ) -> Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor + Clone + Send + 'static,
    {
        self.run_concurrent_internal(executor, concurrency, false)
            .await
    }

    /// Executes a concurrent schedule and requires case identity from every
    /// completed operation before emitting a measured result.
    pub async fn run_concurrent_with_case_coverage<E>(
        &self,
        executor: E,
        concurrency: usize,
    ) -> Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor + Clone + Send + 'static,
    {
        self.run_concurrent_internal(executor, concurrency, true)
            .await
    }

    pub(super) async fn run_concurrent_internal<E>(
        &self,
        executor: E,
        concurrency: usize,
        require_case: bool,
    ) -> Result<QualificationRunSummary>
    where
        E: QualificationOperationExecutor + Clone + Send + 'static,
    {
        if concurrency == 0 || concurrency > MAX_QUALIFICATION_CONCURRENCY {
            return Err(Error::Control("qualification concurrency is out of bounds"));
        }
        let started = Instant::now();
        let mut counts = QUALIFICATION_PRIMITIVES
            .iter()
            .map(|primitive| QualificationPrimitiveCounts::new(primitive))
            .collect::<Vec<_>>();
        let mut case_coverage = vec![0; QUALIFICATION_CASE_COVERAGE_BYTES];
        let mut latency = QualificationLatencyHistogram::default();
        let mut operations = self.iter_operations();
        let mut pending = FuturesUnordered::new();
        let mut first_error = None;

        loop {
            while first_error.is_none() && pending.len() < concurrency {
                let Some(operation) = operations.next() else {
                    break;
                };
                let mut worker = executor.clone();
                pending.push(async move {
                    let operation_started = Instant::now();
                    let execution = worker.execute(operation).await?;
                    Ok::<_, Error>((operation, execution, operation_started.elapsed()))
                });
            }
            let Some(result) = pending.next().await else {
                break;
            };
            match result {
                Ok((operation, execution, elapsed)) => {
                    if require_case && execution.case() != Some(operation.case()) {
                        if first_error.is_none() {
                            first_error = Some(Error::Control(
                                "qualification executor did not report lifecycle case",
                            ));
                        }
                        continue;
                    }
                    Self::record_execution(
                        &mut counts,
                        &mut case_coverage,
                        &mut latency,
                        operation,
                        execution,
                        elapsed,
                    );
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.summary(counts, case_coverage, latency, started.elapsed())
    }

    pub(super) fn record_execution(
        counts: &mut [QualificationPrimitiveCounts],
        case_coverage: &mut [u8],
        latency: &mut QualificationLatencyHistogram,
        operation: QualificationOperation,
        execution: QualificationExecution,
        elapsed: Duration,
    ) {
        if execution.case() == Some(operation.case()) {
            mark_case_coverage(case_coverage, operation);
        }
        latency.record(elapsed);
        let primitive_index = usize::from(operation.primitive_index);
        let primitive = &mut counts[primitive_index];
        primitive.attempted = primitive.attempted.saturating_add(1);
        match execution.outcome {
            QualificationOutcome::Acknowledged => {
                primitive.acknowledged = primitive.acknowledged.saturating_add(1);
                if execution.verified {
                    primitive.verified = primitive.verified.saturating_add(1);
                }
            }
            QualificationOutcome::Rejected => {
                primitive.rejected = primitive.rejected.saturating_add(1);
            }
            QualificationOutcome::Ambiguous => {
                primitive.ambiguous = primitive.ambiguous.saturating_add(1);
            }
        }
        if execution.retries != 0 {
            primitive.retried = primitive.retried.saturating_add(1);
        }
    }

    pub(super) fn summary(
        &self,
        primitive_counts: Vec<QualificationPrimitiveCounts>,
        case_coverage: Vec<u8>,
        latency: QualificationLatencyHistogram,
        elapsed: Duration,
    ) -> Result<QualificationRunSummary> {
        let outcome_digest =
            qualification_run_outcome_digest(self, &primitive_counts, &case_coverage)?;
        Ok(QualificationRunSummary {
            profile: self.profile.clone(),
            profile_digest: self.profile_digest(),
            seed: self.seed,
            cells: self.cells,
            operations: self.operations,
            elapsed,
            primitive_counts,
            case_coverage,
            outcome_digest,
            latency,
        })
    }

    /// Recomputes the schedule and requires an exact profile/seed/size match.
    pub fn verify_for_profile(&self, profile: &QualificationProfile) -> Result<()> {
        if self.profile != profile.name || self.profile_digest() != profile.digest()? {
            return Err(Error::Control("qualification workload profile identity"));
        }
        let expected = Self::generate_with_size(
            profile,
            self.seed,
            self.cells,
            self.operations,
            self.duration_secs,
        )?;
        if expected != *self {
            return Err(Error::Control("qualification workload outcome"));
        }
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<()> {
        let expected_primitives = QUALIFICATION_PRIMITIVES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if self.schema_version != 1
            || self.cells == 0
            || self.cells > MAX_QUALIFICATION_CELLS
            || self.operations == 0
            || self.operations > MAX_QUALIFICATION_OPERATIONS
            || self.duration_secs == 0
            || self.duration_secs > MAX_QUALIFICATION_DURATION_SECS
            || self.profile_digest.iter().all(|byte| *byte == 0)
            || self.outcome_digest.iter().all(|byte| *byte == 0)
            || self.primitives.len() != QUALIFICATION_PRIMITIVES.len()
            || self
                .primitives
                .iter()
                .map(QualificationPrimitiveCounts::primitive)
                .collect::<BTreeSet<_>>()
                != expected_primitives
        {
            return Err(Error::Control("invalid qualification workload"));
        }
        validate_label(&self.profile, "qualification workload profile")?;
        if self
            .primitives
            .iter()
            .any(|counts| counts.attempted == 0 || !valid_primitive_counts(counts))
        {
            return Err(Error::Control("qualification workload counters"));
        }
        if self
            .primitives
            .iter()
            .try_fold(0_u64, |total, counts| total.checked_add(counts.attempted))
            != Some(self.operations)
            || qualification_workload_outcome_digest(self.seed, self.cells, self.operations)
                != self.outcome_digest()
        {
            return Err(Error::Control("qualification workload outcome"));
        }
        Ok(())
    }
}
