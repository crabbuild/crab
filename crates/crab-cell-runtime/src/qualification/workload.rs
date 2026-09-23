//! Deterministic qualification workloads and execution accounting.

use super::profile::{
    MAX_QUALIFICATION_CELLS, MAX_QUALIFICATION_CONCURRENCY, MAX_QUALIFICATION_DURATION_SECS,
    MAX_QUALIFICATION_OPERATIONS, MAX_RECEIPT_BYTES, mark_case_coverage, validate_label,
};
use super::receipt::{validate_metrics, validate_resource_metric_list};
use super::*;

/// Bounded per-primitive counters emitted by the deterministic workload driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationPrimitiveCounts {
    pub(super) primitive: String,
    pub(super) attempted: u64,
    pub(super) acknowledged: u64,
    pub(super) rejected: u64,
    pub(super) ambiguous: u64,
    pub(super) retried: u64,
    pub(super) verified: u64,
}

impl QualificationPrimitiveCounts {
    pub(super) fn new(primitive: &str) -> Self {
        Self {
            primitive: primitive.to_owned(),
            attempted: 0,
            acknowledged: 0,
            rejected: 0,
            ambiguous: 0,
            retried: 0,
            verified: 0,
        }
    }

    /// Returns the primitive row name.
    #[must_use]
    pub fn primitive(&self) -> &str {
        &self.primitive
    }

    #[must_use]
    pub const fn attempted(&self) -> u64 {
        self.attempted
    }

    #[must_use]
    pub const fn acknowledged(&self) -> u64 {
        self.acknowledged
    }

    #[must_use]
    pub const fn rejected(&self) -> u64 {
        self.rejected
    }

    #[must_use]
    pub const fn ambiguous(&self) -> u64 {
        self.ambiguous
    }

    #[must_use]
    pub const fn retried(&self) -> u64 {
        self.retried
    }

    #[must_use]
    pub const fn verified(&self) -> u64 {
        self.verified
    }
}

/// One deterministic operation in a qualification workload.
///
/// The iterator exposes only bounded scalar identity. Payloads and primitive
/// requests remain owned by the application-specific executor, so a scale run
/// does not retain millions of operation bodies in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualificationOperation {
    pub(super) index: u64,
    pub(super) primitive_index: u8,
    pub(super) cell_index: u64,
    pub(super) nonce: u64,
    pub(super) case: QualificationCase,
    pub(super) retry_hint: bool,
    pub(super) rejection_hint: bool,
    pub(super) ambiguous_hint: bool,
}

impl QualificationOperation {
    /// Returns the monotonically increasing logical operation number.
    #[must_use]
    pub const fn index(self) -> u64 {
        self.index
    }

    /// Returns the primitive selected by the canonical schedule.
    #[must_use]
    pub fn primitive(self) -> &'static str {
        QUALIFICATION_PRIMITIVES[self.primitive_index as usize]
    }

    /// Returns the deterministic Cell ordinal selected by the schedule.
    #[must_use]
    pub const fn cell_index(self) -> u64 {
        self.cell_index
    }

    /// Returns the deterministic nonce for request identity/payload seeding.
    #[must_use]
    pub const fn nonce(self) -> u64 {
        self.nonce
    }

    /// Returns the bounded lifecycle case assigned to this operation.
    #[must_use]
    pub const fn case(self) -> QualificationCase {
        self.case
    }

    /// Returns whether this operation exercises producer/delivery duplicate handling.
    #[must_use]
    pub const fn duplicate_hint(self) -> bool {
        matches!(self.case, QualificationCase::Duplicate)
    }

    /// Returns whether this operation exercises expiry handling.
    #[must_use]
    pub const fn expiry_hint(self) -> bool {
        matches!(self.case, QualificationCase::Expiry)
    }

    /// Returns whether this operation exercises cancellation handling.
    #[must_use]
    pub const fn cancellation_hint(self) -> bool {
        matches!(self.case, QualificationCase::Cancellation)
    }

    /// Returns whether this operation exercises owner-loss handling.
    #[must_use]
    pub const fn owner_loss_hint(self) -> bool {
        matches!(self.case, QualificationCase::OwnerLoss)
    }

    /// Returns whether this operation exercises post-takeover recovery.
    #[must_use]
    pub const fn recovery_hint(self) -> bool {
        matches!(self.case, QualificationCase::Recovery)
    }

    /// Returns whether this operation is scheduled to exercise a retry path.
    #[must_use]
    pub const fn retry_hint(self) -> bool {
        self.retry_hint
    }

    /// Returns whether this operation is scheduled to exercise rejection.
    #[must_use]
    pub const fn rejection_hint(self) -> bool {
        self.rejection_hint
    }

    /// Returns whether this operation is scheduled to exercise ambiguity.
    #[must_use]
    pub const fn ambiguous_hint(self) -> bool {
        self.ambiguous_hint
    }
}

/// Actual outcome reported by one application-specific operation executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualificationOutcome {
    Acknowledged,
    Rejected,
    Ambiguous,
}

/// Bounded result returned by a typed workload executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualificationExecution {
    pub(super) outcome: QualificationOutcome,
    pub(super) verified: bool,
    pub(super) retries: u64,
    pub(super) case: Option<QualificationCase>,
}

impl QualificationExecution {
    /// Records an acknowledged operation and whether its state was verified.
    #[must_use]
    pub const fn acknowledged(verified: bool) -> Self {
        Self {
            outcome: QualificationOutcome::Acknowledged,
            verified,
            retries: 0,
            case: None,
        }
    }

    /// Records a rejected operation.
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            outcome: QualificationOutcome::Rejected,
            verified: false,
            retries: 0,
            case: None,
        }
    }

    /// Records an ambiguous operation with the number of resolution attempts.
    #[must_use]
    pub const fn ambiguous(retries: u64) -> Self {
        Self {
            outcome: QualificationOutcome::Ambiguous,
            verified: false,
            retries,
            case: None,
        }
    }

    /// Adds a retry count to an execution result.
    #[must_use]
    pub const fn with_retries(mut self, retries: u64) -> Self {
        self.retries = retries;
        self
    }

    /// Binds the executor result to the lifecycle case it actually exercised.
    #[must_use]
    pub const fn with_case(mut self, case: QualificationCase) -> Self {
        self.case = Some(case);
        self
    }

    #[must_use]
    pub const fn outcome(self) -> QualificationOutcome {
        self.outcome
    }

    #[must_use]
    pub const fn verified(self) -> bool {
        self.verified
    }

    #[must_use]
    pub const fn retries(self) -> u64 {
        self.retries
    }

    #[must_use]
    pub const fn case(self) -> Option<QualificationCase> {
        self.case
    }
}

/// Async application boundary used by [`QualificationWorkload::run`].
pub trait QualificationOperationExecutor {
    type Future<'a>: Future<Output = Result<QualificationExecution>> + Send + 'a
    where
        Self: 'a;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a>;
}

/// Bounded latency histogram used by a qualification run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationLatencyHistogram {
    pub(super) buckets: [u64; 64],
    pub(super) samples: u64,
}

impl Default for QualificationLatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; 64],
            samples: 0,
        }
    }
}

impl QualificationLatencyHistogram {
    pub(super) fn record(&mut self, latency: Duration) {
        let micros = latency.as_micros().max(1).min(u128::from(u64::MAX)) as u64;
        let bucket = (u64::BITS - micros.leading_zeros() - 1) as usize;
        self.buckets[bucket.min(self.buckets.len() - 1)] =
            self.buckets[bucket.min(self.buckets.len() - 1)].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
    }

    pub(super) fn percentile_ms(&self, percentile: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let rank = self.samples.saturating_mul(percentile).saturating_add(99) / 100;
        let mut seen = 0_u64;
        for (bucket, count) in self.buckets.iter().copied().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= rank {
                let micros = 1_u64 << bucket.min(63);
                return micros.saturating_add(999) / 1_000;
            }
        }
        u64::MAX / 1_000
    }
}

/// Measured result of executing a canonical workload through typed APIs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationRunSummary {
    pub(super) profile: String,
    pub(super) profile_digest: Digest,
    pub(super) seed: u64,
    pub(super) cells: u64,
    pub(super) operations: u64,
    pub(super) elapsed: Duration,
    pub(super) primitive_counts: Vec<QualificationPrimitiveCounts>,
    pub(super) case_coverage: Vec<u8>,
    pub(super) outcome_digest: Digest,
    pub(super) latency: QualificationLatencyHistogram,
}

impl QualificationRunSummary {
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub const fn profile_digest(&self) -> Digest {
        self.profile_digest
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub const fn cells(&self) -> u64 {
        self.cells
    }

    #[must_use]
    pub const fn operations(&self) -> u64 {
        self.operations
    }

    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    #[must_use]
    pub fn primitive_counts(&self) -> &[QualificationPrimitiveCounts] {
        &self.primitive_counts
    }

    /// Returns one bounded bitset of observed primitive/lifecycle pairs.
    #[must_use]
    pub fn case_coverage(&self) -> &[u8] {
        &self.case_coverage
    }

    #[must_use]
    pub const fn outcome_digest(&self) -> Digest {
        self.outcome_digest
    }

    /// Returns the measured latency percentile rounded up to milliseconds.
    #[must_use]
    pub fn latency_percentile_ms(&self, percentile: u64) -> u64 {
        self.latency.percentile_ms(percentile)
    }

    /// Returns receipt-compatible bounded metrics from this run.
    pub fn metrics(&self) -> Result<Vec<QualificationMetric>> {
        let duration_secs = self
            .elapsed
            .as_secs()
            .saturating_add(u64::from(self.elapsed.subsec_nanos() != 0))
            .max(1);
        let throughput = self.operations / duration_secs;
        Ok(vec![
            QualificationMetric::new("cells".into(), self.cells, "cells".into())?,
            QualificationMetric::new("operations".into(), self.operations, "operations".into())?,
            QualificationMetric::new("duration_secs".into(), duration_secs, "seconds".into())?,
            QualificationMetric::new("throughput_ops_per_sec".into(), throughput, "ops/s".into())?,
            QualificationMetric::new(
                "p50_latency_ms".into(),
                self.latency_percentile_ms(50),
                "ms".into(),
            )?,
            QualificationMetric::new(
                "p95_latency_ms".into(),
                self.latency_percentile_ms(95),
                "ms".into(),
            )?,
            QualificationMetric::new(
                "p99_latency_ms".into(),
                self.latency_percentile_ms(99),
                "ms".into(),
            )?,
            QualificationMetric::new(
                "max_latency_ms".into(),
                self.latency_percentile_ms(100),
                "ms".into(),
            )?,
        ])
    }

    /// Encodes the measured result together with the exact canonical workload.
    pub fn artifact(&self, workload: &QualificationWorkload) -> Result<QualificationRunArtifact> {
        self.artifact_with_resource_metrics(workload, &[])
    }

    /// Encodes a measured result and the resource observations captured for the same run.
    ///
    /// Protected profiles require every [`QUALIFICATION_RESOURCE_METRICS`]
    /// observation. The receipt verifier compares these values with the signed
    /// execution measurements, so a harness cannot substitute a different
    /// machine's resource envelope after the workload has completed.
    pub fn artifact_with_resource_metrics(
        &self,
        workload: &QualificationWorkload,
        resource_metrics: &[QualificationMetric],
    ) -> Result<QualificationRunArtifact> {
        if self.profile != workload.profile
            || self.profile_digest != workload.profile_digest()
            || self.seed != workload.seed
            || self.cells != workload.cells
            || self.operations != workload.operations
        {
            return Err(Error::Control("qualification run workload identity"));
        }
        validate_resource_metric_list(resource_metrics)?;
        let mut metrics = self.metrics()?;
        metrics.extend(resource_metrics.iter().cloned());
        validate_metrics(&metrics)?;
        Ok(QualificationRunArtifact {
            schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
            workload: workload.clone(),
            profile: self.profile.clone(),
            profile_digest: *self.profile_digest.as_bytes(),
            seed: self.seed,
            cells: self.cells,
            operations: self.operations,
            elapsed_ms: self.elapsed.as_millis().max(1).min(u128::from(u64::MAX)) as u64,
            primitive_counts: self.primitive_counts.clone(),
            case_coverage: self.case_coverage.clone(),
            outcome_digest: *qualification_run_outcome_digest(
                workload,
                &self.primitive_counts,
                &self.case_coverage,
            )?
            .as_bytes(),
            metrics,
        })
    }
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

    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub const fn profile_digest(&self) -> Digest {
        Digest::from_bytes(self.profile_digest)
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub const fn cells(&self) -> u64 {
        self.cells
    }

    #[must_use]
    pub const fn operations(&self) -> u64 {
        self.operations
    }

    #[must_use]
    pub const fn duration_secs(&self) -> u64 {
        self.duration_secs
    }

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

pub(super) fn qualification_workload_outcome_digest(
    seed: u64,
    cells: u64,
    operations: u64,
) -> Digest {
    let mut hasher = blake3::Hasher::new();
    for operation in QualificationOperationIter::new(seed, cells, operations) {
        hasher.update(&operation.index.to_be_bytes());
        hasher.update(&u64::from(operation.primitive_index).to_be_bytes());
        hasher.update(&operation.cell_index.to_be_bytes());
        hasher.update(&operation.nonce.to_be_bytes());
        hasher.update(&[operation.case as u8]);
    }
    Digest::from_bytes(*hasher.finalize().as_bytes())
}

pub(super) fn qualification_run_outcome_digest(
    workload: &QualificationWorkload,
    counts: &[QualificationPrimitiveCounts],
    case_coverage: &[u8],
) -> Result<Digest> {
    if case_coverage.len() != QUALIFICATION_CASE_COVERAGE_BYTES {
        return Err(Error::Control("qualification run case coverage"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab-cell-runtime/qualification/run-v3");
    hasher.update(workload.outcome_digest().as_bytes());
    hasher.update(workload.profile_digest().as_bytes());
    hasher.update(&workload.seed.to_be_bytes());
    hasher.update(&workload.cells.to_be_bytes());
    hasher.update(&workload.operations.to_be_bytes());
    hasher.update(case_coverage);
    for primitive in QUALIFICATION_PRIMITIVES {
        let primitive_counts = counts
            .iter()
            .find(|counts| counts.primitive == *primitive)
            .ok_or(Error::Control("qualification run primitive identity"))?;
        hasher.update(&(primitive.len() as u64).to_be_bytes());
        hasher.update(primitive.as_bytes());
        for value in [
            primitive_counts.attempted,
            primitive_counts.acknowledged,
            primitive_counts.rejected,
            primitive_counts.ambiguous,
            primitive_counts.retried,
            primitive_counts.verified,
        ] {
            hasher.update(&value.to_be_bytes());
        }
    }
    Ok(Digest::from_bytes(*hasher.finalize().as_bytes()))
}

pub(super) fn valid_primitive_counts(counts: &QualificationPrimitiveCounts) -> bool {
    counts
        .acknowledged
        .checked_add(counts.rejected)
        .and_then(|total| total.checked_add(counts.ambiguous))
        == Some(counts.attempted)
        && counts.retried <= counts.attempted
        && counts.verified <= counts.acknowledged
}

pub(super) const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
pub(super) const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
pub(super) const LCG_SEED_XOR: u64 = 0x9e37_79b9_7f4a_7c15;

/// Streaming operation iterator; it retains only the LCG state and counters.
pub struct QualificationOperationIter {
    pub(super) index: u64,
    pub(super) cells: u64,
    pub(super) operations: u64,
    pub(super) state: u64,
}

impl QualificationOperationIter {
    pub(super) fn new(seed: u64, cells: u64, operations: u64) -> Self {
        Self {
            index: 0,
            cells,
            operations,
            state: seed ^ LCG_SEED_XOR,
        }
    }
}

impl Iterator for QualificationOperationIter {
    type Item = QualificationOperation;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.operations {
            return None;
        }
        self.state = self
            .state
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        let operation = operation_from_state(self.index, self.state, self.cells);
        self.index = self.index.saturating_add(1);
        Some(operation)
    }
}

pub(super) fn operation_from_state(index: u64, state: u64, cells: u64) -> QualificationOperation {
    let primitive_count = QUALIFICATION_PRIMITIVES.len() as u64;
    let coverage_operations = QUALIFICATION_CASE_COVERAGE_OPERATIONS;
    let (primitive_index, case_index) = if index < coverage_operations {
        (
            (index % primitive_count) as usize,
            (index / primitive_count) as usize,
        )
    } else {
        (
            (state as usize) % QUALIFICATION_PRIMITIVES.len(),
            (state.rotate_left(19) as usize) % QUALIFICATION_CASES.len(),
        )
    };
    QualificationOperation {
        index,
        primitive_index: primitive_index as u8,
        cell_index: state % cells,
        nonce: state,
        case: QUALIFICATION_CASES[case_index],
        retry_hint: state & 0x1f == 0,
        rejection_hint: state & 0x3ff == 0,
        ambiguous_hint: state & 0x7ff == 0x200,
    }
}

pub(super) fn lcg_state_at(seed: u64, steps: u64) -> u64 {
    let mut result = (1_u64, 0_u64);
    let mut base = (LCG_MULTIPLIER, LCG_INCREMENT);
    let mut steps = steps;
    while steps != 0 {
        if steps & 1 != 0 {
            result = compose_affine(base, result);
        }
        base = compose_affine(base, base);
        steps >>= 1;
    }
    result.0.wrapping_mul(seed).wrapping_add(result.1)
}

pub(super) fn compose_affine(after: (u64, u64), before: (u64, u64)) -> (u64, u64) {
    (
        after.0.wrapping_mul(before.0),
        after.0.wrapping_mul(before.1).wrapping_add(after.1),
    )
}
