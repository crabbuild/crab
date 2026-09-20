use std::{
    collections::BTreeSet,
    future::Future,
    path::Path,
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{Digest, Error, Result};

const MAX_LABEL_BYTES: usize = 256;
const MAX_METRICS: usize = 64;
const MAX_RECEIPT_BYTES: usize = 1 << 20;
const MAX_QUALIFICATION_CELLS: u64 = 1_000_000;
const MAX_QUALIFICATION_OPERATIONS: u64 = 100_000_000;
const MAX_QUALIFICATION_DURATION_SECS: u64 = 7 * 24 * 60 * 60;

/// Current wire schema for qualification evidence.
pub const QUALIFICATION_SCHEMA_VERSION: u32 = 4;
/// Schema for a manifest that binds one receipt to every qualification row.
pub const QUALIFICATION_MATRIX_SCHEMA_VERSION: u32 = 1;
/// Schema for a versioned workload threshold profile.
pub const QUALIFICATION_PROFILE_SCHEMA_VERSION: u32 = 1;
/// Required workload rows for a complete release qualification matrix.
pub const QUALIFICATION_MATRIX_ROWS: &[&str] = &[
    "protocol",
    "storage",
    "publication",
    "warm-path",
    "churn",
    "fleet",
    "failover",
    "primitives",
    "accounting",
    "compatibility",
];

/// Versioned thresholds used to decide whether a qualification run is admissible.
///
/// The profile is intentionally separate from measured receipts: thresholds must
/// be selected before a candidate run and its digest is the identity bound by
/// the harness and release gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationProfile {
    schema_version: u32,
    name: String,
    minimum_cells: u64,
    minimum_operations: u64,
    minimum_duration_secs: u64,
    maximum_p99_latency_ms: u64,
}

impl QualificationProfile {
    /// Creates one immutable threshold profile.
    pub fn new(
        name: String,
        minimum_cells: u64,
        minimum_operations: u64,
        minimum_duration_secs: u64,
        maximum_p99_latency_ms: u64,
    ) -> Result<Self> {
        validate_label(&name, "qualification profile name")?;
        if minimum_cells == 0
            || minimum_operations == 0
            || minimum_duration_secs == 0
            || maximum_p99_latency_ms == 0
        {
            return Err(Error::Control("qualification profile threshold is zero"));
        }
        Ok(Self {
            schema_version: QUALIFICATION_PROFILE_SCHEMA_VERSION,
            name,
            minimum_cells,
            minimum_operations,
            minimum_duration_secs,
            maximum_p99_latency_ms,
        })
    }

    /// Returns the deterministic local correctness profile.
    pub fn pr_contract() -> Self {
        Self::built_in("pr-contract-v1", 1, 1, 1, 5_000)
    }

    /// Returns the three-process provider iteration profile.
    pub fn local_provider() -> Self {
        Self::built_in("local-provider-v1", 256, 1_000_000, 60, 1_000)
    }

    /// Returns the dedicated scale profile.
    pub fn scale() -> Self {
        Self::built_in("scale-v1", 10_000, 10_000_000, 3_600, 500)
    }

    /// Returns the protected Kubernetes fault profile.
    pub fn fault() -> Self {
        Self::built_in("fault-v1", 256, 1_000_000, 60, 1_000)
    }

    /// Returns the provider-specific correctness profile.
    pub fn provider() -> Self {
        Self::built_in("provider-v1", 256, 1_000_000, 60, 1_000)
    }

    /// Returns the rolling-release compatibility profile.
    pub fn compatibility() -> Self {
        Self::built_in("compatibility-v1", 256, 1_000_000, 60, 1_000)
    }

    /// Returns the profile name used in signed receipts.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the minimum Cell cardinality.
    #[must_use]
    pub const fn minimum_cells(&self) -> u64 {
        self.minimum_cells
    }

    /// Returns the minimum operation count.
    #[must_use]
    pub const fn minimum_operations(&self) -> u64 {
        self.minimum_operations
    }

    /// Returns the minimum steady-state duration.
    #[must_use]
    pub const fn minimum_duration_secs(&self) -> u64 {
        self.minimum_duration_secs
    }

    /// Returns the maximum permitted p99 latency.
    #[must_use]
    pub const fn maximum_p99_latency_ms(&self) -> u64 {
        self.maximum_p99_latency_ms
    }

    /// Encodes the canonical threshold profile.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).map_err(Error::from)
    }

    /// Decodes and revalidates one canonical threshold profile.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let profile: Self = serde_json::from_slice(bytes)?;
        profile.validate()?;
        let mut canonical_end = bytes.len();
        while canonical_end > 0 && matches!(bytes[canonical_end - 1], b' ' | b'\n' | b'\r' | b'\t')
        {
            canonical_end -= 1;
        }
        if profile.encode()?.as_slice() != &bytes[..canonical_end] {
            return Err(Error::Control("qualification profile is not canonical"));
        }
        Ok(profile)
    }

    /// Returns the digest that must be bound to a candidate receipt/artifact.
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::from_bytes(
            *blake3::hash(&self.encode()?).as_bytes(),
        ))
    }

    fn built_in(
        name: &'static str,
        minimum_cells: u64,
        minimum_operations: u64,
        minimum_duration_secs: u64,
        maximum_p99_latency_ms: u64,
    ) -> Self {
        Self {
            schema_version: QUALIFICATION_PROFILE_SCHEMA_VERSION,
            name: name.to_owned(),
            minimum_cells,
            minimum_operations,
            minimum_duration_secs,
            maximum_p99_latency_ms,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_PROFILE_SCHEMA_VERSION
            || self.minimum_cells == 0
            || self.minimum_operations == 0
            || self.minimum_duration_secs == 0
            || self.maximum_p99_latency_ms == 0
            || self.minimum_cells > MAX_QUALIFICATION_CELLS
            || self.minimum_operations > MAX_QUALIFICATION_OPERATIONS
            || self.minimum_duration_secs > MAX_QUALIFICATION_DURATION_SECS
        {
            return Err(Error::Control("invalid qualification profile"));
        }
        validate_label(&self.name, "qualification profile name")
    }
}

/// Primitive rows exercised by the canonical mixed-load qualification driver.
pub const QUALIFICATION_PRIMITIVES: &[&str] = &[
    "sql", "kv", "blob", "queue", "cron", "workflow", "activity", "effects",
];

/// Bounded per-primitive counters emitted by the deterministic workload driver.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationPrimitiveCounts {
    primitive: String,
    attempted: u64,
    acknowledged: u64,
    rejected: u64,
    ambiguous: u64,
    retried: u64,
    verified: u64,
}

impl QualificationPrimitiveCounts {
    fn new(primitive: &str) -> Self {
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
    index: u64,
    primitive_index: u8,
    cell_index: u64,
    nonce: u64,
    retry_hint: bool,
    rejection_hint: bool,
    ambiguous_hint: bool,
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
    outcome: QualificationOutcome,
    verified: bool,
    retries: u64,
}

impl QualificationExecution {
    /// Records an acknowledged operation and whether its state was verified.
    #[must_use]
    pub const fn acknowledged(verified: bool) -> Self {
        Self {
            outcome: QualificationOutcome::Acknowledged,
            verified,
            retries: 0,
        }
    }

    /// Records a rejected operation.
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            outcome: QualificationOutcome::Rejected,
            verified: false,
            retries: 0,
        }
    }

    /// Records an ambiguous operation with the number of resolution attempts.
    #[must_use]
    pub const fn ambiguous(retries: u64) -> Self {
        Self {
            outcome: QualificationOutcome::Ambiguous,
            verified: false,
            retries,
        }
    }

    /// Adds a retry count to an execution result.
    #[must_use]
    pub const fn with_retries(mut self, retries: u64) -> Self {
        self.retries = retries;
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
struct QualificationLatencyHistogram {
    buckets: [u64; 64],
    samples: u64,
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
    fn record(&mut self, latency: Duration) {
        let micros = latency.as_micros().max(1).min(u128::from(u64::MAX)) as u64;
        let bucket = (u64::BITS - micros.leading_zeros() - 1) as usize;
        self.buckets[bucket.min(self.buckets.len() - 1)] =
            self.buckets[bucket.min(self.buckets.len() - 1)].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
    }

    fn percentile_ms(&self, percentile: u64) -> u64 {
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
    profile: String,
    profile_digest: Digest,
    seed: u64,
    cells: u64,
    operations: u64,
    elapsed: Duration,
    primitive_counts: Vec<QualificationPrimitiveCounts>,
    outcome_digest: Digest,
    latency: QualificationLatencyHistogram,
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
        let duration_secs = self.elapsed.as_secs();
        Ok(vec![
            QualificationMetric::new("cells".into(), self.cells, "cells".into())?,
            QualificationMetric::new("operations".into(), self.operations, "operations".into())?,
            QualificationMetric::new("duration_secs".into(), duration_secs, "seconds".into())?,
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
        if self.profile != workload.profile
            || self.profile_digest != workload.profile_digest()
            || self.seed != workload.seed
            || self.cells != workload.cells
            || self.operations != workload.operations
        {
            return Err(Error::Control("qualification run workload identity"));
        }
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
            outcome_digest: *self.outcome_digest.as_bytes(),
            metrics: self.metrics()?,
        })
    }
}

/// Schema for a measured, typed execution artifact bound to one workload.
pub const QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION: u32 = 1;

/// Bounded measured outcome consumed by protected primitive qualification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationRunArtifact {
    schema_version: u32,
    workload: QualificationWorkload,
    profile: String,
    profile_digest: [u8; 32],
    seed: u64,
    cells: u64,
    operations: u64,
    elapsed_ms: u64,
    primitive_counts: Vec<QualificationPrimitiveCounts>,
    outcome_digest: [u8; 32],
    metrics: Vec<QualificationMetric>,
}

impl QualificationRunArtifact {
    /// Decodes and verifies one canonical measured run artifact.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification run artifact exceeds limit"));
        }
        let artifact: Self = serde_json::from_slice(bytes)?;
        artifact.validate()?;
        if serde_json::to_vec(&artifact).map_err(Error::from)? != bytes {
            return Err(Error::Control(
                "qualification run artifact is not canonical",
            ));
        }
        Ok(artifact)
    }

    /// Encodes one measured result with stable field ordering.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification run artifact exceeds limit"));
        }
        Ok(bytes)
    }

    /// Verifies workload identity, measured counters, and profile thresholds.
    pub fn verify_for_profile(&self, profile: &QualificationProfile) -> Result<()> {
        self.validate()?;
        self.workload.verify_for_profile(profile)?;
        if self.profile != profile.name
            || self.profile_digest() != profile.digest()?
            || self.workload.profile() != profile.name
            || self.seed != self.workload.seed()
            || self.cells != self.workload.cells()
            || self.operations != self.workload.operations()
        {
            return Err(Error::Control("qualification run profile identity"));
        }
        if self.elapsed_ms < profile.minimum_duration_secs().saturating_mul(1_000)
            || self.threshold_metric("p99_latency_ms", "ms")? > profile.maximum_p99_latency_ms()
        {
            return Err(Error::Control("qualification run profile threshold failed"));
        }
        Ok(())
    }

    #[must_use]
    pub fn workload(&self) -> &QualificationWorkload {
        &self.workload
    }

    #[must_use]
    pub const fn outcome_digest(&self) -> Digest {
        Digest::from_bytes(self.outcome_digest)
    }

    fn profile_digest(&self) -> Digest {
        Digest::from_bytes(self.profile_digest)
    }

    fn threshold_metric(&self, name: &str, unit: &str) -> Result<u64> {
        let mut value = None;
        for metric in &self.metrics {
            if metric.name() != name {
                continue;
            }
            if metric.unit() != unit || value.replace(metric.value()).is_some() {
                return Err(Error::Control("qualification run threshold metric"));
            }
        }
        value.ok_or(Error::Control("qualification run threshold metric missing"))
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION
            || self.profile_digest.iter().all(|byte| *byte == 0)
            || self.outcome_digest.iter().all(|byte| *byte == 0)
            || self.elapsed_ms == 0
            || self.primitive_counts.len() != QUALIFICATION_PRIMITIVES.len()
        {
            return Err(Error::Control("invalid qualification run artifact"));
        }
        validate_label(&self.profile, "qualification run profile")?;
        let expected_primitives = QUALIFICATION_PRIMITIVES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual_primitives = self
            .primitive_counts
            .iter()
            .map(QualificationPrimitiveCounts::primitive)
            .collect::<BTreeSet<_>>();
        if actual_primitives != expected_primitives
            || self
                .primitive_counts
                .iter()
                .any(|counts| !valid_primitive_counts(counts))
            || self
                .primitive_counts
                .iter()
                .try_fold(0_u64, |total, counts| total.checked_add(counts.attempted()))
                != Some(self.operations)
        {
            return Err(Error::Control("qualification run counters"));
        }
        self.workload.validate()?;
        validate_metrics(&self.metrics)?;
        Ok(())
    }
}

/// Deterministic logical workload artifact for PR, provider, and scale tiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkload {
    schema_version: u32,
    profile: String,
    profile_digest: [u8; 32],
    seed: u64,
    cells: u64,
    operations: u64,
    duration_secs: u64,
    primitives: Vec<QualificationPrimitiveCounts>,
    outcome_digest: [u8; 32],
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
                .max(QUALIFICATION_PRIMITIVES.len() as u64),
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
        let mut hasher = blake3::Hasher::new();
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
            hasher.update(&operation.index.to_be_bytes());
            hasher.update(&(primitive_index as u64).to_be_bytes());
            hasher.update(&operation.cell_index.to_be_bytes());
            hasher.update(&operation.nonce.to_be_bytes());
        }
        Ok(Self {
            schema_version: 1,
            profile: profile.name.clone(),
            profile_digest: *profile.digest()?.as_bytes(),
            seed,
            cells,
            operations,
            duration_secs,
            primitives,
            outcome_digest: *hasher.finalize().as_bytes(),
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
        let started = Instant::now();
        let mut counts = QUALIFICATION_PRIMITIVES
            .iter()
            .map(|primitive| QualificationPrimitiveCounts::new(primitive))
            .collect::<Vec<_>>();
        let mut latency = QualificationLatencyHistogram::default();
        let mut hasher = blake3::Hasher::new();
        for operation in self.iter_operations() {
            let operation_started = Instant::now();
            let execution = executor.execute(operation).await?;
            latency.record(operation_started.elapsed());
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
            hasher.update(&operation.index.to_be_bytes());
            hasher.update(&[operation.primitive_index]);
            hasher.update(&operation.cell_index.to_be_bytes());
            hasher.update(&operation.nonce.to_be_bytes());
            hasher.update(&[match execution.outcome {
                QualificationOutcome::Acknowledged => 0,
                QualificationOutcome::Rejected => 1,
                QualificationOutcome::Ambiguous => 2,
            }]);
            hasher.update(&[u8::from(execution.verified)]);
            hasher.update(&execution.retries.to_be_bytes());
        }
        Ok(QualificationRunSummary {
            profile: self.profile.clone(),
            profile_digest: self.profile_digest(),
            seed: self.seed,
            cells: self.cells,
            operations: self.operations,
            elapsed: started.elapsed(),
            primitive_counts: counts,
            outcome_digest: Digest::from_bytes(*hasher.finalize().as_bytes()),
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

    fn validate(&self) -> Result<()> {
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
        Ok(())
    }
}

fn valid_primitive_counts(counts: &QualificationPrimitiveCounts) -> bool {
    counts
        .acknowledged
        .checked_add(counts.rejected)
        .and_then(|total| total.checked_add(counts.ambiguous))
        == Some(counts.attempted)
        && counts.retried <= counts.attempted
        && counts.verified <= counts.acknowledged
}

const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;
const LCG_SEED_XOR: u64 = 0x9e37_79b9_7f4a_7c15;

/// Streaming operation iterator; it retains only the LCG state and counters.
pub struct QualificationOperationIter {
    index: u64,
    cells: u64,
    operations: u64,
    state: u64,
}

impl QualificationOperationIter {
    fn new(seed: u64, cells: u64, operations: u64) -> Self {
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

fn operation_from_state(index: u64, state: u64, cells: u64) -> QualificationOperation {
    let primitive_count = QUALIFICATION_PRIMITIVES.len() as u64;
    let primitive_index = if index < primitive_count {
        index as usize
    } else {
        (state as usize) % QUALIFICATION_PRIMITIVES.len()
    };
    QualificationOperation {
        index,
        primitive_index: primitive_index as u8,
        cell_index: state % cells,
        nonce: state,
        retry_hint: state & 0x1f == 0,
        rejection_hint: state & 0x3ff == 0,
        ambiguous_hint: state & 0x3ff != 0 && state & 0x7ff == 0,
    }
}

fn lcg_state_at(seed: u64, steps: u64) -> u64 {
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

fn compose_affine(after: (u64, u64), before: (u64, u64)) -> (u64, u64) {
    (
        after.0.wrapping_mul(before.0),
        after.0.wrapping_mul(before.1).wrapping_add(after.1),
    )
}

/// Reproducible evidence record for one canonical Cell qualification run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationReceipt {
    schema_version: u32,
    source_revision: String,
    image: [u8; 32],
    provider: String,
    workload: String,
    fault: String,
    metrics: Vec<QualificationMetric>,
    artifact_digest: [u8; 32],
    passed: bool,
    dirty: bool,
    toolchain: String,
    profile: String,
    profile_digest: [u8; 32],
    topology: String,
    workload_seed: u64,
    bucket_calls: u64,
    peak_rss_bytes: u64,
    started_at_ms: u64,
    finished_at_ms: u64,
    fault_schedule_digest: [u8; 32],
    raw_artifact_digests: Vec<[u8; 32]>,
    ownership: Vec<QualificationOwnership>,
    signer: [u8; 32],
    signature: Vec<u8>,
}

/// One bounded ownership proof observed during a qualification run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationOwnership {
    epoch: u64,
    published_sequence: u64,
    root: [u8; 32],
}

impl QualificationOwnership {
    /// Creates one ownership/commit watermark proof.
    pub fn new(epoch: u64, published_sequence: u64, root: Digest) -> Self {
        Self {
            epoch,
            published_sequence,
            root: *root.as_bytes(),
        }
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub const fn published_sequence(&self) -> u64 {
        self.published_sequence
    }

    #[must_use]
    pub const fn root(&self) -> Digest {
        Digest::from_bytes(self.root)
    }
}

/// One bounded named measurement attached to a qualification receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMetric {
    name: String,
    value: u64,
    unit: String,
}

/// One receipt/artifact pair in a qualification matrix manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMatrixEntry {
    workload: String,
    receipt: String,
    artifacts: Vec<String>,
}

impl QualificationMatrixEntry {
    /// Creates one manifest entry using paths relative to the manifest file.
    pub fn new(workload: String, receipt: String, artifacts: Vec<String>) -> Result<Self> {
        validate_label(&workload, "qualification matrix workload")?;
        validate_path(&receipt, "qualification matrix receipt path")?;
        if artifacts.is_empty() || artifacts.len() > MAX_METRICS {
            return Err(Error::Control("qualification matrix artifact count"));
        }
        for artifact in &artifacts {
            validate_path(artifact, "qualification matrix artifact path")?;
        }
        Ok(Self {
            workload,
            receipt,
            artifacts,
        })
    }

    /// Returns the required workload row name.
    #[must_use]
    pub fn workload(&self) -> &str {
        &self.workload
    }

    /// Returns the receipt path relative to the manifest.
    #[must_use]
    pub fn receipt(&self) -> &str {
        &self.receipt
    }

    /// Returns raw-artifact paths relative to the manifest.
    #[must_use]
    pub fn artifacts(&self) -> &[String] {
        &self.artifacts
    }
}

/// Complete, bounded manifest for release qualification evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMatrixManifest {
    schema_version: u32,
    entries: Vec<QualificationMatrixEntry>,
}

impl QualificationMatrixManifest {
    /// Builds and validates a complete matrix manifest.
    pub fn new(entries: Vec<QualificationMatrixEntry>) -> Result<Self> {
        let manifest = Self {
            schema_version: QUALIFICATION_MATRIX_SCHEMA_VERSION,
            entries,
        };
        manifest.validate_contract()?;
        Ok(manifest)
    }

    /// Decodes canonical JSON and rejects incomplete or duplicate rows.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification matrix exceeds limit"));
        }
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate_contract()?;
        if serde_json::to_vec(&manifest).map_err(Error::from)? != bytes {
            return Err(Error::Control("qualification matrix is not canonical"));
        }
        Ok(manifest)
    }

    /// Encodes canonical JSON for a release artifact.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate_contract()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification matrix exceeds limit"));
        }
        Ok(bytes)
    }

    /// Returns the entries in their declared manifest order.
    #[must_use]
    pub fn entries(&self) -> &[QualificationMatrixEntry] {
        &self.entries
    }

    fn validate_contract(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_MATRIX_SCHEMA_VERSION
            || self.entries.len() != QUALIFICATION_MATRIX_ROWS.len()
        {
            return Err(Error::Control("qualification matrix schema or row count"));
        }
        let expected = QUALIFICATION_MATRIX_ROWS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = self
            .entries
            .iter()
            .map(QualificationMatrixEntry::workload)
            .collect::<BTreeSet<_>>();
        if actual.len() != self.entries.len() || actual != expected {
            return Err(Error::Control("qualification matrix rows"));
        }
        for entry in &self.entries {
            QualificationMatrixEntry::new(
                entry.workload.clone(),
                entry.receipt.clone(),
                entry.artifacts.clone(),
            )?;
        }
        Ok(())
    }
}

impl QualificationMetric {
    pub fn new(name: String, value: u64, unit: String) -> Result<Self> {
        validate_label(&name, "qualification metric name")?;
        validate_label(&unit, "qualification metric unit")?;
        Ok(Self { name, value, unit })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn value(&self) -> u64 {
        self.value
    }

    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }
}

fn validate_metrics(metrics: &[QualificationMetric]) -> Result<()> {
    if metrics.len() > MAX_METRICS {
        return Err(Error::Control("qualification metric count exceeds limit"));
    }
    let mut identities = BTreeSet::new();
    for metric in metrics {
        validate_label(&metric.name, "qualification metric name")?;
        validate_label(&metric.unit, "qualification metric unit")?;
        if !identities.insert((metric.name.as_str(), metric.unit.as_str())) {
            return Err(Error::Control("duplicate qualification metric"));
        }
    }
    Ok(())
}

impl QualificationReceipt {
    pub fn new(
        source_revision: String,
        image: Digest,
        provider: String,
        workload: String,
        fault: String,
        metrics: Vec<QualificationMetric>,
        artifact_digest: Digest,
        passed: bool,
    ) -> Result<Self> {
        validate_label(&source_revision, "qualification source revision")?;
        validate_label(&provider, "qualification provider")?;
        validate_label(&workload, "qualification workload")?;
        validate_label(&fault, "qualification fault")?;
        validate_metrics(&metrics)?;
        let fault_schedule_digest = *blake3::hash(fault.as_bytes()).as_bytes();
        let artifact_digest = *artifact_digest.as_bytes();
        Ok(Self {
            schema_version: QUALIFICATION_SCHEMA_VERSION,
            source_revision,
            image: *image.as_bytes(),
            provider,
            workload,
            fault,
            metrics,
            artifact_digest,
            passed,
            dirty: false,
            toolchain: "unknown".into(),
            profile: "unqualified".into(),
            profile_digest: [0; 32],
            topology: "local".into(),
            workload_seed: 0,
            bucket_calls: 0,
            peak_rss_bytes: 0,
            started_at_ms: 1,
            finished_at_ms: 1,
            fault_schedule_digest,
            raw_artifact_digests: vec![artifact_digest],
            ownership: Vec::new(),
            signer: [0; 32],
            signature: vec![0; 64],
        })
    }

    /// Adds bounded, non-secret execution identity to a receipt.
    pub fn with_execution(
        mut self,
        toolchain: String,
        profile: String,
        topology: String,
        workload_seed: u64,
        bucket_calls: u64,
        peak_rss_bytes: u64,
        dirty: bool,
    ) -> Result<Self> {
        validate_label(&toolchain, "qualification toolchain")?;
        validate_label(&profile, "qualification profile")?;
        validate_label(&topology, "qualification topology")?;
        self.toolchain = toolchain;
        self.profile = profile;
        self.topology = topology;
        self.workload_seed = workload_seed;
        self.bucket_calls = bucket_calls;
        self.peak_rss_bytes = peak_rss_bytes;
        self.dirty = dirty;
        Ok(self)
    }

    /// Binds the canonical threshold profile whose limits govern this run.
    pub fn with_profile(mut self, profile: &QualificationProfile) -> Result<Self> {
        profile.validate()?;
        self.profile = profile.name.clone();
        self.profile_digest = *profile.digest()?.as_bytes();
        Ok(self)
    }

    /// Binds timestamps, fault schedule bytes, and raw artifact/ownership evidence.
    pub fn with_evidence(
        mut self,
        started_at_ms: u64,
        finished_at_ms: u64,
        fault_schedule: &[u8],
        raw_artifact_digests: Vec<Digest>,
        ownership: Vec<QualificationOwnership>,
    ) -> Result<Self> {
        if started_at_ms == 0 || finished_at_ms < started_at_ms {
            return Err(Error::Control("qualification evidence timestamps"));
        }
        if raw_artifact_digests.is_empty() || raw_artifact_digests.len() > MAX_METRICS {
            return Err(Error::Control("qualification artifact digest count"));
        }
        if ownership.len() > MAX_METRICS {
            return Err(Error::Control("qualification ownership proof count"));
        }
        self.started_at_ms = started_at_ms;
        self.finished_at_ms = finished_at_ms;
        self.fault_schedule_digest = *blake3::hash(fault_schedule).as_bytes();
        self.raw_artifact_digests = raw_artifact_digests
            .into_iter()
            .map(|digest| *digest.as_bytes())
            .collect();
        self.ownership = ownership;
        Ok(self)
    }

    /// Signs the exact canonical receipt with a qualification attestation key.
    pub fn attest(mut self, signing_key: &SigningKey) -> Result<Self> {
        self.signer = signing_key.verifying_key().to_bytes();
        self.signature = vec![0; 64];
        self.signature = signing_key.sign(&self.signing_bytes()?).to_bytes().to_vec();
        Ok(self)
    }

    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    #[must_use]
    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    #[must_use]
    pub const fn image(&self) -> Digest {
        Digest::from_bytes(self.image)
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn workload(&self) -> &str {
        &self.workload
    }

    #[must_use]
    pub fn fault(&self) -> &str {
        &self.fault
    }

    #[must_use]
    pub fn metrics(&self) -> &[QualificationMetric] {
        &self.metrics
    }

    #[must_use]
    pub const fn artifact_digest(&self) -> Digest {
        Digest::from_bytes(self.artifact_digest)
    }

    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }

    #[must_use]
    pub const fn dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub fn toolchain(&self) -> &str {
        &self.toolchain
    }

    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Returns the digest of the exact threshold profile signed into this receipt.
    #[must_use]
    pub const fn profile_digest(&self) -> Digest {
        Digest::from_bytes(self.profile_digest)
    }

    #[must_use]
    pub fn topology(&self) -> &str {
        &self.topology
    }

    #[must_use]
    pub const fn workload_seed(&self) -> u64 {
        self.workload_seed
    }

    #[must_use]
    pub const fn bucket_calls(&self) -> u64 {
        self.bucket_calls
    }

    #[must_use]
    pub const fn peak_rss_bytes(&self) -> u64 {
        self.peak_rss_bytes
    }

    #[must_use]
    pub const fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    #[must_use]
    pub const fn finished_at_ms(&self) -> u64 {
        self.finished_at_ms
    }

    #[must_use]
    pub const fn fault_schedule_digest(&self) -> Digest {
        Digest::from_bytes(self.fault_schedule_digest)
    }

    pub fn raw_artifact_digests(&self) -> impl Iterator<Item = Digest> + '_ {
        self.raw_artifact_digests
            .iter()
            .copied()
            .map(Digest::from_bytes)
    }

    #[must_use]
    pub fn ownership(&self) -> &[QualificationOwnership] {
        &self.ownership
    }

    #[must_use]
    pub const fn signer(&self) -> [u8; 32] {
        self.signer
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate_contract()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification receipt exceeds limit"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification receipt exceeds limit"));
        }
        let receipt: Self = serde_json::from_slice(bytes)?;
        if receipt.schema_version != QUALIFICATION_SCHEMA_VERSION {
            return Err(Error::Control("qualification receipt schema version"));
        }
        receipt.validate_contract()?;
        if receipt.dirty {
            return Err(Error::Control("qualification receipt has dirty source"));
        }
        let key = VerifyingKey::from_bytes(&receipt.signer).map_err(Error::PeerSignature)?;
        receipt.verify_signature(&key)?;
        if receipt.encode()? != bytes {
            return Err(Error::Control("qualification receipt is not canonical"));
        }
        Ok(receipt)
    }

    /// Verifies a decoded, passing receipt against the expected release and artifact.
    ///
    /// Failed receipts remain retainable evidence but cannot satisfy this release gate.
    pub fn verify_for(&self, source_revision: &str, image: Digest, artifact: &[u8]) -> Result<()> {
        self.verify_for_artifacts(source_revision, image, &[artifact])
    }

    /// Verifies a receipt against the exact threshold profile used for the run.
    pub fn verify_for_profile(
        &self,
        source_revision: &str,
        image: Digest,
        profile: &QualificationProfile,
        artifacts: &[&[u8]],
    ) -> Result<()> {
        if self.profile != profile.name || self.profile_digest() != profile.digest()? {
            return Err(Error::Control("qualification profile identity"));
        }
        self.verify_for_artifacts(source_revision, image, artifacts)
    }

    /// Verifies a receipt against a pinned qualification attestation key.
    pub fn verify_for_trusted_signer(
        &self,
        source_revision: &str,
        image: Digest,
        artifact: &[u8],
        trusted_signer: [u8; 32],
    ) -> Result<()> {
        self.verify_for_artifacts(source_revision, image, &[artifact])?;
        self.verify_trusted_signer(trusted_signer)
    }

    /// Verifies a receipt against a profile and a pinned attestation key.
    pub fn verify_for_profile_with_signer(
        &self,
        source_revision: &str,
        image: Digest,
        profile: &QualificationProfile,
        artifacts: &[&[u8]],
        trusted_signer: [u8; 32],
    ) -> Result<()> {
        self.verify_for_profile(source_revision, image, profile, artifacts)?;
        self.verify_profile_thresholds(profile)?;
        self.verify_trusted_signer(trusted_signer)
    }

    /// Verifies the exact profile thresholds from measured receipt metrics.
    pub fn verify_profile_thresholds(&self, profile: &QualificationProfile) -> Result<()> {
        let cells = self.threshold_metric("cells", "cells")?;
        let operations = self.threshold_metric("operations", "operations")?;
        let duration_secs = self.threshold_metric("duration_secs", "seconds")?;
        let p99_latency_ms = self.threshold_metric("p99_latency_ms", "ms")?;
        if cells < profile.minimum_cells()
            || operations < profile.minimum_operations()
            || duration_secs < profile.minimum_duration_secs()
            || p99_latency_ms > profile.maximum_p99_latency_ms()
        {
            return Err(Error::Control("qualification profile threshold failed"));
        }
        Ok(())
    }

    /// Verifies the receipt's Ed25519 signature against an expected public key.
    pub fn verify_trusted_signer(&self, trusted_signer: [u8; 32]) -> Result<()> {
        if self.signer != trusted_signer {
            return Err(Error::Control("qualification receipt signer"));
        }
        let key = VerifyingKey::from_bytes(&trusted_signer).map_err(Error::PeerSignature)?;
        self.verify_signature(&key)
    }

    /// Verifies every raw artifact digest listed by a passing receipt.
    pub fn verify_for_artifacts(
        &self,
        source_revision: &str,
        image: Digest,
        artifacts: &[&[u8]],
    ) -> Result<()> {
        if !self.passed {
            return Err(Error::Control("qualification receipt is not passed"));
        }
        validate_label(source_revision, "qualification source revision")?;
        if self.source_revision != source_revision || self.image() != image {
            return Err(Error::Control("qualification release identity"));
        }
        if artifacts.len() != self.raw_artifact_digests.len() || artifacts.is_empty() {
            return Err(Error::Control("qualification artifact evidence count"));
        }
        for (expected, artifact) in self.raw_artifact_digests().zip(artifacts.iter().copied()) {
            if expected != Digest::from_bytes(*blake3::hash(artifact).as_bytes()) {
                return Err(Error::Control("qualification artifact digest"));
            }
        }
        if self.artifact_digest() != Digest::from_bytes(*blake3::hash(artifacts[0]).as_bytes()) {
            return Err(Error::Control("qualification primary artifact digest"));
        }
        let encoded = self.encode()?;
        let decoded = Self::decode(&encoded)?;
        if decoded != *self {
            return Err(Error::Control(
                "qualification receipt changed during verification",
            ));
        }
        Ok(())
    }

    /// Verifies one complete matrix against an exact source/image identity.
    pub fn verify_matrix(
        source_revision: &str,
        image: Digest,
        evidence: &[(&str, &QualificationReceipt, &[&[u8]])],
    ) -> Result<()> {
        if evidence.len() != QUALIFICATION_MATRIX_ROWS.len() {
            return Err(Error::Control("qualification matrix evidence count"));
        }
        let expected = QUALIFICATION_MATRIX_ROWS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = evidence
            .iter()
            .map(|(workload, _, _)| *workload)
            .collect::<BTreeSet<_>>();
        if actual != expected || actual.len() != evidence.len() {
            return Err(Error::Control("qualification matrix evidence rows"));
        }
        for (workload, receipt, artifacts) in evidence {
            if *workload != receipt.workload() {
                return Err(Error::Control("qualification matrix receipt workload"));
            }
            receipt.verify_for_artifacts(source_revision, image, artifacts)?;
        }
        Ok(())
    }

    /// Verifies a complete matrix and requires one exact threshold profile.
    pub fn verify_matrix_for_profile(
        source_revision: &str,
        image: Digest,
        profile: &QualificationProfile,
        evidence: &[(&str, &QualificationReceipt, &[&[u8]])],
    ) -> Result<()> {
        if evidence.len() != QUALIFICATION_MATRIX_ROWS.len() {
            return Err(Error::Control("qualification matrix evidence count"));
        }
        let expected = QUALIFICATION_MATRIX_ROWS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = evidence
            .iter()
            .map(|(workload, _, _)| *workload)
            .collect::<BTreeSet<_>>();
        if actual != expected || actual.len() != evidence.len() {
            return Err(Error::Control("qualification matrix evidence rows"));
        }
        for (workload, receipt, artifacts) in evidence {
            if *workload != receipt.workload() {
                return Err(Error::Control("qualification matrix receipt workload"));
            }
            receipt.verify_for_profile(source_revision, image, profile, artifacts)?;
            if *workload == "primitives" {
                receipt.verify_primitive_workload(profile, artifacts)?;
            }
        }
        Ok(())
    }

    /// Verifies a complete profile matrix with pinned attestation and thresholds.
    pub fn verify_matrix_for_profile_with_signer(
        source_revision: &str,
        image: Digest,
        profile: &QualificationProfile,
        evidence: &[(&str, &QualificationReceipt, &[&[u8]])],
        trusted_signer: [u8; 32],
    ) -> Result<()> {
        if evidence.len() != QUALIFICATION_MATRIX_ROWS.len() {
            return Err(Error::Control("qualification matrix evidence count"));
        }
        let expected = QUALIFICATION_MATRIX_ROWS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = evidence
            .iter()
            .map(|(workload, _, _)| *workload)
            .collect::<BTreeSet<_>>();
        if actual != expected || actual.len() != evidence.len() {
            return Err(Error::Control("qualification matrix evidence rows"));
        }
        for (workload, receipt, artifacts) in evidence {
            if *workload != receipt.workload() {
                return Err(Error::Control("qualification matrix receipt workload"));
            }
            receipt.verify_for_profile_with_signer(
                source_revision,
                image,
                profile,
                artifacts,
                trusted_signer,
            )?;
            if *workload == "primitives" {
                receipt.verify_primitive_workload(profile, artifacts)?;
                receipt.verify_primitive_run_artifact(profile, artifacts)?;
            }
        }
        Ok(())
    }

    fn validate_contract(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_SCHEMA_VERSION
            || self.dirty
            || self.signer.iter().all(|byte| *byte == 0)
            || self.signature.len() != 64
            || self.signature.iter().all(|byte| *byte == 0)
        {
            return Err(Error::Control("qualification receipt is not attested"));
        }
        validate_label(&self.source_revision, "qualification source revision")?;
        validate_label(&self.provider, "qualification provider")?;
        validate_label(&self.workload, "qualification workload")?;
        validate_label(&self.fault, "qualification fault")?;
        validate_label(&self.toolchain, "qualification toolchain")?;
        validate_label(&self.profile, "qualification profile")?;
        validate_label(&self.topology, "qualification topology")?;
        validate_metrics(&self.metrics)?;
        if self.started_at_ms == 0
            || self.finished_at_ms < self.started_at_ms
            || self.fault_schedule_digest.iter().all(|byte| *byte == 0)
            || self.profile_digest.iter().all(|byte| *byte == 0)
            || self.raw_artifact_digests.is_empty()
            || self.raw_artifact_digests.len() > MAX_METRICS
            || self.ownership.len() > MAX_METRICS
            || !self.raw_artifact_digests.contains(&self.artifact_digest)
        {
            return Err(Error::Control("qualification evidence is incomplete"));
        }
        if self
            .ownership
            .iter()
            .any(|proof| proof.root.iter().all(|byte| *byte == 0))
        {
            return Err(Error::Control("qualification ownership proof"));
        }
        Ok(())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature = vec![0; 64];
        serde_json::to_vec(&unsigned).map_err(Error::from)
    }

    fn verify_signature(&self, key: &VerifyingKey) -> Result<()> {
        let signature: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| Error::Control("qualification receipt signature length"))?;
        key.verify(&self.signing_bytes()?, &Signature::from_bytes(&signature))
            .map_err(Error::PeerSignature)
    }

    fn threshold_metric(&self, name: &str, unit: &str) -> Result<u64> {
        let mut value = None;
        for metric in &self.metrics {
            if metric.name() != name {
                continue;
            }
            if metric.unit() != unit || value.replace(metric.value()).is_some() {
                return Err(Error::Control("qualification threshold metric"));
            }
        }
        value.ok_or(Error::Control("qualification threshold metric missing"))
    }

    fn verify_primitive_workload(
        &self,
        profile: &QualificationProfile,
        artifacts: &[&[u8]],
    ) -> Result<()> {
        let mut matching = artifacts.iter().filter_map(|artifact| {
            QualificationWorkload::decode(artifact)
                .ok()
                .filter(|workload| workload.verify_for_profile(profile).is_ok())
        });
        let workload = matching.next().ok_or(Error::Control(
            "qualification matrix is missing its canonical workload",
        ))?;
        if matching.next().is_some() || self.workload_seed != workload.seed() {
            return Err(Error::Control(
                "qualification receipt does not bind the canonical workload seed",
            ));
        }
        Ok(())
    }

    fn verify_primitive_run_artifact(
        &self,
        profile: &QualificationProfile,
        artifacts: &[&[u8]],
    ) -> Result<()> {
        let mut matching = artifacts.iter().filter_map(|artifact| {
            QualificationRunArtifact::decode(artifact)
                .ok()
                .filter(|run| run.verify_for_profile(profile).is_ok())
        });
        let run = matching.next().ok_or(Error::Control(
            "protected primitive evidence is missing its measured run artifact",
        ))?;
        if matching.next().is_some() || self.workload_seed != run.workload().seed() {
            return Err(Error::Control(
                "qualification receipt does not bind the measured workload seed",
            ));
        }
        Ok(())
    }
}

/// Deterministic receipt emitter used by local and protected qualification
/// harnesses. The harness owns workload/fault execution; this type only binds
/// its measured outputs to exact source, image and artifact bytes.
pub struct QualificationRunner {
    signing_key: SigningKey,
}

impl QualificationRunner {
    #[must_use]
    pub fn new(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "receipt identity is intentionally explicit and fully bound"
    )]
    pub fn emit(
        &self,
        source_revision: String,
        image: Digest,
        provider: String,
        workload: String,
        fault: String,
        metrics: Vec<QualificationMetric>,
        artifact: &[u8],
        passed: bool,
        execution: (String, String, String, u64, u64, u64, bool),
    ) -> Result<QualificationReceipt> {
        self.emit_with_profile(
            &QualificationProfile::pr_contract(),
            source_revision,
            image,
            provider,
            workload,
            fault,
            metrics,
            artifact,
            passed,
            execution,
        )
    }

    /// Emits evidence while binding the exact threshold profile used by the harness.
    #[expect(
        clippy::too_many_arguments,
        reason = "receipt identity is intentionally explicit and fully bound"
    )]
    pub fn emit_with_profile(
        &self,
        profile: &QualificationProfile,
        source_revision: String,
        image: Digest,
        provider: String,
        workload: String,
        fault: String,
        metrics: Vec<QualificationMetric>,
        artifact: &[u8],
        passed: bool,
        execution: (String, String, String, u64, u64, u64, bool),
    ) -> Result<QualificationReceipt> {
        let artifact_digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
        let fault_schedule = fault.clone();
        let (toolchain, execution_profile, topology, seed, bucket_calls, peak_rss_bytes, dirty) =
            execution;
        QualificationReceipt::new(
            source_revision,
            image,
            provider,
            workload,
            fault,
            metrics,
            artifact_digest,
            passed,
        )?
        .with_execution(
            toolchain,
            execution_profile,
            topology,
            seed,
            bucket_calls,
            peak_rss_bytes,
            dirty,
        )?
        .with_profile(profile)?
        .with_evidence(
            1,
            1,
            fault_schedule.as_bytes(),
            vec![artifact_digest],
            Vec::new(),
        )?
        .attest(&self.signing_key)
    }

    /// Emits a receipt with externally captured timing, fault, artifact, and ownership evidence.
    #[expect(
        clippy::too_many_arguments,
        reason = "qualification evidence is intentionally bound in one receipt"
    )]
    pub fn emit_with_evidence(
        &self,
        source_revision: String,
        image: Digest,
        provider: String,
        workload: String,
        fault: String,
        metrics: Vec<QualificationMetric>,
        artifact: &[u8],
        passed: bool,
        execution: (String, String, String, u64, u64, u64, bool),
        started_at_ms: u64,
        finished_at_ms: u64,
        fault_schedule: &[u8],
        raw_artifact_digests: Vec<Digest>,
        ownership: Vec<QualificationOwnership>,
    ) -> Result<QualificationReceipt> {
        self.emit_with_profile_and_evidence(
            &QualificationProfile::pr_contract(),
            source_revision,
            image,
            provider,
            workload,
            fault,
            metrics,
            artifact,
            passed,
            execution,
            started_at_ms,
            finished_at_ms,
            fault_schedule,
            raw_artifact_digests,
            ownership,
        )
    }

    /// Emits evidence with an explicit profile and externally captured proof.
    #[expect(
        clippy::too_many_arguments,
        reason = "qualification evidence is intentionally bound in one receipt"
    )]
    pub fn emit_with_profile_and_evidence(
        &self,
        profile: &QualificationProfile,
        source_revision: String,
        image: Digest,
        provider: String,
        workload: String,
        fault: String,
        metrics: Vec<QualificationMetric>,
        artifact: &[u8],
        passed: bool,
        execution: (String, String, String, u64, u64, u64, bool),
        started_at_ms: u64,
        finished_at_ms: u64,
        fault_schedule: &[u8],
        raw_artifact_digests: Vec<Digest>,
        ownership: Vec<QualificationOwnership>,
    ) -> Result<QualificationReceipt> {
        let (toolchain, execution_profile, topology, seed, bucket_calls, peak_rss_bytes, dirty) =
            execution;
        QualificationReceipt::new(
            source_revision,
            image,
            provider,
            workload,
            fault,
            metrics,
            Digest::from_bytes(*blake3::hash(artifact).as_bytes()),
            passed,
        )?
        .with_execution(
            toolchain,
            execution_profile,
            topology,
            seed,
            bucket_calls,
            peak_rss_bytes,
            dirty,
        )?
        .with_profile(profile)?
        .with_evidence(
            started_at_ms,
            finished_at_ms,
            fault_schedule,
            raw_artifact_digests,
            ownership,
        )?
        .attest(&self.signing_key)
    }
}

fn validate_label(value: &str, field: &'static str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_LABEL_BYTES
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
        || value.contains("@")
        || value.contains("://")
        || value.to_ascii_lowercase().contains("secret")
        || value.to_ascii_lowercase().contains("password")
    {
        return Err(Error::Control(field));
    }
    Ok(())
}

fn validate_path(value: &str, field: &'static str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_LABEL_BYTES
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
        || value.contains('\\')
        || value.contains("://")
        || Path::new(value).is_absolute()
        || Path::new(value)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(Error::Control(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_profiles_are_canonical_and_distinct() {
        let contract = QualificationProfile::pr_contract();
        let local = QualificationProfile::local_provider();
        let scale = QualificationProfile::scale();
        let fault = QualificationProfile::fault();
        let provider = QualificationProfile::provider();
        let compatibility = QualificationProfile::compatibility();
        assert_eq!(
            contract.schema_version,
            QUALIFICATION_PROFILE_SCHEMA_VERSION
        );
        assert!(contract.minimum_operations() < local.minimum_operations());
        assert!(local.minimum_cells() < scale.minimum_cells());
        assert_eq!(fault.name(), "fault-v1");
        assert_eq!(provider.name(), "provider-v1");
        assert_eq!(compatibility.name(), "compatibility-v1");
        assert_ne!(contract.digest().unwrap(), local.digest().unwrap());
        assert_eq!(
            QualificationProfile::decode(&contract.encode().unwrap()).unwrap(),
            contract
        );
    }

    #[test]
    fn tracked_profile_newline_does_not_change_its_canonical_digest() {
        let profile = QualificationProfile::scale();
        let mut encoded = profile.encode().unwrap();
        encoded.extend_from_slice(b"\n");
        assert_eq!(QualificationProfile::decode(&encoded).unwrap(), profile);
        encoded.extend_from_slice(b" ");
        assert_eq!(QualificationProfile::decode(&encoded).unwrap(), profile);
    }

    #[test]
    fn mixed_workload_is_seed_deterministic_and_covers_every_primitive() {
        let profile = QualificationProfile::pr_contract();
        let first = QualificationWorkload::generate(&profile, 7).unwrap();
        let second = QualificationWorkload::generate(&profile, 7).unwrap();
        let changed = QualificationWorkload::generate(&profile, 8).unwrap();
        assert_eq!(first, second);
        assert_ne!(first.outcome_digest(), changed.outcome_digest());
        assert_eq!(first.primitives().len(), QUALIFICATION_PRIMITIVES.len());
        assert!(
            first
                .primitives()
                .iter()
                .all(|counts| counts.attempted() > 0 && counts.verified() > 0)
        );
        for seed in 0..32 {
            let workload = QualificationWorkload::generate(&profile, seed).unwrap();
            assert!(
                workload
                    .primitives()
                    .iter()
                    .all(|counts| counts.attempted() > 0)
            );
        }
        first.verify_for_profile(&profile).unwrap();
        let encoded = first.encode().unwrap();
        assert_eq!(QualificationWorkload::decode(&encoded).unwrap(), first);
    }

    #[test]
    fn workload_bounds_and_counter_overflow_fail_closed() {
        let profile = QualificationProfile::pr_contract();
        let mut workload = QualificationWorkload::generate(&profile, 7).unwrap();
        workload.operations = MAX_QUALIFICATION_OPERATIONS + 1;
        assert!(workload.encode().is_err());

        workload.operations = 8;
        workload.primitives[0].attempted = u64::MAX;
        workload.primitives[0].acknowledged = u64::MAX;
        workload.primitives[0].rejected = u64::MAX;
        assert!(workload.encode().is_err());
    }

    struct ContractExecutor {
        calls: u64,
    }

    impl QualificationOperationExecutor for ContractExecutor {
        type Future<'a> = std::future::Ready<Result<QualificationExecution>>;

        fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
            self.calls = self.calls.saturating_add(1);
            let execution = if operation.rejection_hint() {
                QualificationExecution::rejected()
            } else if operation.ambiguous_hint() {
                QualificationExecution::ambiguous(u64::from(operation.retry_hint()))
            } else {
                QualificationExecution::acknowledged(true)
                    .with_retries(u64::from(operation.retry_hint()))
            };
            std::future::ready(Ok(execution))
        }
    }

    #[tokio::test]
    async fn workload_iterator_and_executor_are_streaming_and_reproducible() {
        let profile = QualificationProfile::new("contract-run".into(), 1, 32, 1, 1_000).unwrap();
        let workload = QualificationWorkload::generate_with_size(&profile, 41, 3, 32, 1).unwrap();
        let operations = workload.iter_operations().collect::<Vec<_>>();
        assert_eq!(operations.len(), 32);
        for (index, operation) in operations.iter().copied().enumerate() {
            assert_eq!(workload.operation_at(index as u64).unwrap(), operation);
        }
        let mut executor = ContractExecutor { calls: 0 };
        let summary = workload.run(&mut executor).await.unwrap();
        assert_eq!(executor.calls, workload.operations());
        assert_eq!(summary.operations(), workload.operations());
        assert_eq!(summary.cells(), workload.cells());
        assert!(
            summary
                .primitive_counts()
                .iter()
                .all(|counts| counts.attempted() > 0 && counts.verified() > 0)
        );
        assert!(
            summary
                .metrics()
                .unwrap()
                .iter()
                .any(|metric| { metric.name() == "p99_latency_ms" && metric.unit() == "ms" })
        );
        assert_ne!(summary.outcome_digest(), workload.outcome_digest());
    }

    #[test]
    fn measured_run_artifact_binds_the_canonical_workload_and_thresholds() {
        let profile = QualificationProfile::pr_contract();
        let workload = QualificationWorkload::generate(&profile, 19).unwrap();
        let artifact = QualificationRunArtifact {
            schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
            workload: workload.clone(),
            profile: profile.name.clone(),
            profile_digest: *profile.digest().unwrap().as_bytes(),
            seed: workload.seed(),
            cells: workload.cells(),
            operations: workload.operations(),
            elapsed_ms: 1_000,
            primitive_counts: workload.primitives.clone(),
            outcome_digest: [7; 32],
            metrics: vec![
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            ],
        };
        let encoded = artifact.encode().unwrap();
        assert_eq!(
            QualificationRunArtifact::decode(&encoded).unwrap(),
            artifact
        );
        artifact.verify_for_profile(&profile).unwrap();

        let mut forged = artifact.clone();
        forged.seed = forged.seed.saturating_add(1);
        assert!(forged.verify_for_profile(&profile).is_err());
    }

    #[test]
    fn receipt_round_trips_and_binds_measurements() {
        let receipt = QualificationReceipt::new(
            "abc".into(),
            Digest::from_bytes([1; 32]),
            "memory".into(),
            "warm-route".into(),
            "none".into(),
            vec![QualificationMetric::new("p99".into(), 7, "ms".into()).unwrap()],
            Digest::from_bytes([2; 32]),
            true,
        )
        .unwrap()
        .with_execution(
            "rustc".into(),
            "unit".into(),
            "local".into(),
            7,
            0,
            1024,
            false,
        )
        .unwrap()
        .with_profile(&QualificationProfile::pr_contract())
        .unwrap()
        .attest(&SigningKey::from_bytes(&[9; 32]))
        .unwrap();
        assert_eq!(receipt.schema_version(), QUALIFICATION_SCHEMA_VERSION);
        let decoded = QualificationReceipt::decode(&receipt.encode().unwrap()).unwrap();
        assert_eq!(decoded, receipt);
    }

    #[test]
    fn qualification_metrics_reject_duplicate_identities() {
        let duplicate = vec![
            QualificationMetric::new("p99".into(), 7, "ms".into()).unwrap(),
            QualificationMetric::new("p99".into(), 8, "ms".into()).unwrap(),
        ];
        assert!(
            QualificationReceipt::new(
                "abc".into(),
                Digest::from_bytes([1; 32]),
                "memory".into(),
                "warm-route".into(),
                "none".into(),
                duplicate,
                Digest::from_bytes([2; 32]),
                true,
            )
            .is_err()
        );

        let profile = QualificationProfile::pr_contract();
        let workload = QualificationWorkload::generate(&profile, 19).unwrap();
        let artifact = QualificationRunArtifact {
            schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
            workload: workload.clone(),
            profile: profile.name.clone(),
            profile_digest: *profile.digest().unwrap().as_bytes(),
            seed: workload.seed(),
            cells: workload.cells(),
            operations: workload.operations(),
            elapsed_ms: 1_000,
            primitive_counts: workload.primitives.clone(),
            outcome_digest: [7; 32],
            metrics: vec![
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 2, "ms".into()).unwrap(),
            ],
        };
        assert!(artifact.encode().is_err());
    }

    #[test]
    fn runner_binds_artifact_and_rejects_forged_or_dirty_receipts() {
        let runner = QualificationRunner::new(SigningKey::from_bytes(&[4; 32]));
        let receipt = runner
            .emit(
                "abc".into(),
                Digest::from_bytes([1; 32]),
                "rustfs".into(),
                "warm".into(),
                "none".into(),
                Vec::new(),
                b"artifact",
                true,
                (
                    "rustc".into(),
                    "ci".into(),
                    "three-node".into(),
                    1,
                    2,
                    3,
                    false,
                ),
            )
            .unwrap();
        assert_eq!(
            receipt.artifact_digest(),
            Digest::from_bytes(*blake3::hash(b"artifact").as_bytes())
        );
        let encoded = receipt.encode().unwrap();
        assert_eq!(QualificationReceipt::decode(&encoded).unwrap(), receipt);
        assert_eq!(
            receipt.profile_digest(),
            QualificationProfile::pr_contract().digest().unwrap()
        );
        receipt
            .verify_for("abc", Digest::from_bytes([1; 32]), b"artifact")
            .unwrap();
        let failed = QualificationReceipt::new(
            "abc".into(),
            Digest::from_bytes([1; 32]),
            "rustfs".into(),
            "warm".into(),
            "none".into(),
            Vec::new(),
            Digest::from_bytes(*blake3::hash(b"artifact").as_bytes()),
            false,
        )
        .unwrap()
        .with_execution(
            "rustc".into(),
            "ci".into(),
            "three-node".into(),
            1,
            2,
            3,
            false,
        )
        .unwrap()
        .with_profile(&QualificationProfile::pr_contract())
        .unwrap()
        .attest(&SigningKey::from_bytes(&[4; 32]))
        .unwrap();
        assert!(
            failed
                .verify_for("abc", Digest::from_bytes([1; 32]), b"artifact")
                .is_err()
        );
        assert!(
            receipt
                .verify_for("other", Digest::from_bytes([1; 32]), b"artifact")
                .is_err()
        );
        let mut forged = receipt.clone();
        forged.bucket_calls = forged.bucket_calls.saturating_add(1);
        assert!(QualificationReceipt::decode(&forged.encode().unwrap()).is_err());
        assert!(
            receipt
                .verify_for("abc", Digest::from_bytes([1; 32]), b"other")
                .is_err()
        );
        let dirty = runner
            .emit(
                "abc".into(),
                Digest::from_bytes([1; 32]),
                "rustfs".into(),
                "warm".into(),
                "none".into(),
                Vec::new(),
                b"artifact",
                true,
                (
                    "rustc".into(),
                    "ci".into(),
                    "three-node".into(),
                    1,
                    2,
                    3,
                    true,
                ),
            )
            .unwrap();
        assert!(dirty.encode().is_err());
    }

    #[test]
    fn protected_verification_pins_signer_and_threshold_metrics() {
        let profile = QualificationProfile::pr_contract();
        let key = SigningKey::from_bytes(&[12; 32]);
        let metrics = vec![
            QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 1, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 5_000, "ms".into()).unwrap(),
        ];
        let receipt = QualificationRunner::new(key.clone())
            .emit_with_profile_and_evidence(
                &profile,
                "source".into(),
                Digest::from_bytes([13; 32]),
                "provider".into(),
                "primitives".into(),
                "none".into(),
                metrics,
                b"artifact",
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "local".into(),
                    7,
                    0,
                    0,
                    false,
                ),
                1,
                1,
                b"none",
                vec![Digest::from_bytes(*blake3::hash(b"artifact").as_bytes())],
                Vec::new(),
            )
            .unwrap();
        receipt
            .verify_for_profile_with_signer(
                "source",
                Digest::from_bytes([13; 32]),
                &profile,
                &[b"artifact"],
                key.verifying_key().to_bytes(),
            )
            .unwrap();
        assert!(
            receipt
                .verify_for_profile_with_signer(
                    "source",
                    Digest::from_bytes([13; 32]),
                    &profile,
                    &[b"artifact"],
                    SigningKey::from_bytes(&[14; 32]).verifying_key().to_bytes(),
                )
                .is_err()
        );
        let mut below_threshold = receipt.clone();
        below_threshold.metrics[1].value = 0;
        assert!(below_threshold.verify_profile_thresholds(&profile).is_err());
    }

    #[test]
    fn evidence_binds_fault_artifacts_and_ownership_watermarks() {
        let artifact = b"raw qualification output";
        let runner = QualificationRunner::new(SigningKey::from_bytes(&[6; 32]));
        let receipt = runner
            .emit_with_evidence(
                "abc".into(),
                Digest::from_bytes([1; 32]),
                "rustfs".into(),
                "failover".into(),
                "lost-release-reply".into(),
                Vec::new(),
                artifact,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "three-node".into(),
                    7,
                    8,
                    9,
                    false,
                ),
                10,
                20,
                b"seed=7;fault=lost-release-reply",
                vec![Digest::from_bytes(*blake3::hash(artifact).as_bytes())],
                vec![QualificationOwnership::new(
                    4,
                    12,
                    Digest::from_bytes([3; 32]),
                )],
            )
            .unwrap();
        let encoded = receipt.encode().unwrap();
        let decoded = QualificationReceipt::decode(&encoded).unwrap();
        assert_eq!(decoded.started_at_ms(), 10);
        assert_eq!(decoded.finished_at_ms(), 20);
        assert_eq!(decoded.ownership()[0].epoch(), 4);
        assert_eq!(decoded.ownership()[0].published_sequence(), 12);
        assert!(
            decoded
                .raw_artifact_digests()
                .any(|digest| { digest == Digest::from_bytes(*blake3::hash(artifact).as_bytes()) })
        );
    }

    #[test]
    fn matrix_manifest_requires_each_bounded_workload_once() {
        let entries = QUALIFICATION_MATRIX_ROWS
            .iter()
            .map(|workload| {
                QualificationMatrixEntry::new(
                    (*workload).to_owned(),
                    format!("receipts/{workload}.json"),
                    vec![format!("artifacts/{workload}.json")],
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let manifest = QualificationMatrixManifest::new(entries).unwrap();
        assert_eq!(
            QualificationMatrixManifest::decode(&manifest.encode().unwrap()).unwrap(),
            manifest
        );

        let mut incomplete = manifest.entries().to_vec();
        incomplete.pop();
        assert!(QualificationMatrixManifest::new(incomplete).is_err());

        let mut duplicate = manifest.entries().to_vec();
        duplicate[0] = QualificationMatrixEntry::new(
            duplicate[1].workload().to_owned(),
            duplicate[0].receipt().to_owned(),
            duplicate[0].artifacts().to_vec(),
        )
        .unwrap();
        assert!(QualificationMatrixManifest::new(duplicate).is_err());

        assert!(
            QualificationMatrixEntry::new(
                "protocol".into(),
                "../receipt.json".into(),
                vec!["artifact.bin".into()],
            )
            .is_err()
        );
        assert!(
            QualificationMatrixEntry::new(
                "protocol".into(),
                "/tmp/receipt.json".into(),
                vec!["artifact.bin".into()],
            )
            .is_err()
        );
    }

    #[test]
    fn matrix_verifier_recomputes_every_row_artifact() {
        let image = Digest::from_bytes([7; 32]);
        let runner = QualificationRunner::new(SigningKey::from_bytes(&[8; 32]));
        let mut receipts = Vec::new();
        let mut artifacts = Vec::new();
        for workload in QUALIFICATION_MATRIX_ROWS {
            let artifact = format!("artifact-{workload}").into_bytes();
            receipts.push(
                runner
                    .emit(
                        "source".into(),
                        image,
                        "local".into(),
                        (*workload).into(),
                        "none".into(),
                        Vec::new(),
                        &artifact,
                        true,
                        (
                            "rustc".into(),
                            "test".into(),
                            "local".into(),
                            1,
                            0,
                            0,
                            false,
                        ),
                    )
                    .unwrap(),
            );
            artifacts.push(artifact);
        }
        let artifact_views = artifacts
            .iter()
            .map(|artifact| vec![artifact.as_slice()])
            .collect::<Vec<_>>();
        let evidence = QUALIFICATION_MATRIX_ROWS
            .iter()
            .enumerate()
            .map(|(index, workload)| {
                (
                    *workload,
                    &receipts[index],
                    artifact_views[index].as_slice(),
                )
            })
            .collect::<Vec<_>>();
        QualificationReceipt::verify_matrix("source", image, &evidence).unwrap();

        let mut forged_artifact = artifacts[0].clone();
        forged_artifact.push(b'!');
        let forged_views = [vec![forged_artifact.as_slice()]];
        let forged_evidence = evidence
            .iter()
            .enumerate()
            .map(|(index, (workload, receipt, row_artifacts))| {
                if index == 0 {
                    (*workload, *receipt, forged_views[0].as_slice())
                } else {
                    (*workload, *receipt, *row_artifacts)
                }
            })
            .collect::<Vec<_>>();
        assert!(QualificationReceipt::verify_matrix("source", image, &forged_evidence).is_err());
    }

    #[test]
    fn primitive_matrix_binds_the_receipt_to_the_workload_seed() {
        let profile = QualificationProfile::pr_contract();
        let image = Digest::from_bytes([17; 32]);
        let workload = QualificationWorkload::generate(&profile, 7).unwrap();
        let workload_artifact = workload.encode().unwrap();
        let runner = QualificationRunner::new(SigningKey::from_bytes(&[18; 32]));
        let mut receipts = Vec::new();
        let mut artifacts = Vec::new();
        for workload_name in QUALIFICATION_MATRIX_ROWS {
            let artifact = if *workload_name == "primitives" {
                workload_artifact.clone()
            } else {
                format!("artifact-{workload_name}").into_bytes()
            };
            let seed = if *workload_name == "primitives" {
                workload.seed()
            } else {
                0
            };
            receipts.push(
                runner
                    .emit_with_profile(
                        &profile,
                        "source".into(),
                        image,
                        "local".into(),
                        (*workload_name).into(),
                        "none".into(),
                        Vec::new(),
                        &artifact,
                        true,
                        (
                            "rustc".into(),
                            "test".into(),
                            "local".into(),
                            seed,
                            0,
                            0,
                            false,
                        ),
                    )
                    .unwrap(),
            );
            artifacts.push(artifact);
        }
        let artifact_views = artifacts
            .iter()
            .map(|artifact| vec![artifact.as_slice()])
            .collect::<Vec<_>>();
        let evidence = QUALIFICATION_MATRIX_ROWS
            .iter()
            .enumerate()
            .map(|(index, workload_name)| {
                (
                    *workload_name,
                    &receipts[index],
                    artifact_views[index].as_slice(),
                )
            })
            .collect::<Vec<_>>();
        QualificationReceipt::verify_matrix_for_profile("source", image, &profile, &evidence)
            .unwrap();

        let bad = runner
            .emit_with_profile(
                &profile,
                "source".into(),
                image,
                "local".into(),
                "primitives".into(),
                "none".into(),
                Vec::new(),
                &workload_artifact,
                true,
                (
                    "rustc".into(),
                    "test".into(),
                    "local".into(),
                    workload.seed() + 1,
                    0,
                    0,
                    false,
                ),
            )
            .unwrap();
        let mut bad_evidence = evidence;
        bad_evidence[7] = ("primitives", &bad, artifact_views[7].as_slice());
        assert!(
            QualificationReceipt::verify_matrix_for_profile(
                "source",
                image,
                &profile,
                &bad_evidence,
            )
            .is_err()
        );
    }

    #[test]
    fn protected_primitive_matrix_requires_measured_run_artifact() {
        let profile = QualificationProfile::pr_contract();
        let image = Digest::from_bytes([27; 32]);
        let key = SigningKey::from_bytes(&[28; 32]);
        let runner = QualificationRunner::new(key.clone());
        let workload = QualificationWorkload::generate(&profile, 7).unwrap();
        let workload_artifact = workload.encode().unwrap();
        let run_artifact = QualificationRunArtifact {
            schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
            workload: workload.clone(),
            profile: profile.name.clone(),
            profile_digest: *profile.digest().unwrap().as_bytes(),
            seed: workload.seed(),
            cells: workload.cells(),
            operations: workload.operations(),
            elapsed_ms: 1_000,
            primitive_counts: workload.primitives.clone(),
            outcome_digest: [29; 32],
            metrics: vec![
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            ],
        }
        .encode()
        .unwrap();
        let threshold_metrics = || {
            vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 1, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            ]
        };
        let mut receipts = Vec::new();
        let mut artifacts = Vec::new();
        for workload_name in QUALIFICATION_MATRIX_ROWS {
            let (primary, row_artifacts, seed) = if *workload_name == "primitives" {
                (
                    workload_artifact.clone(),
                    vec![workload_artifact.clone(), run_artifact.clone()],
                    workload.seed(),
                )
            } else {
                let artifact = format!("artifact-{workload_name}").into_bytes();
                (artifact.clone(), vec![artifact], 0)
            };
            let raw_digests = row_artifacts
                .iter()
                .map(|artifact| Digest::from_bytes(*blake3::hash(artifact).as_bytes()))
                .collect();
            receipts.push(
                runner
                    .emit_with_profile_and_evidence(
                        &profile,
                        "source".into(),
                        image,
                        "local".into(),
                        (*workload_name).into(),
                        "none".into(),
                        threshold_metrics(),
                        &primary,
                        true,
                        (
                            "rustc".into(),
                            "test".into(),
                            "local".into(),
                            seed,
                            0,
                            0,
                            false,
                        ),
                        1,
                        2,
                        b"none",
                        raw_digests,
                        Vec::new(),
                    )
                    .unwrap(),
            );
            artifacts.push(row_artifacts);
        }
        let artifact_views = artifacts
            .iter()
            .map(|row| row.iter().map(Vec::as_slice).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let evidence = QUALIFICATION_MATRIX_ROWS
            .iter()
            .enumerate()
            .map(|(index, workload_name)| {
                (
                    *workload_name,
                    &receipts[index],
                    artifact_views[index].as_slice(),
                )
            })
            .collect::<Vec<_>>();
        QualificationReceipt::verify_matrix_for_profile_with_signer(
            "source",
            image,
            &profile,
            &evidence,
            key.verifying_key().to_bytes(),
        )
        .unwrap();

        let missing_artifacts = artifacts
            .iter()
            .enumerate()
            .map(|(index, row)| {
                if index == 7 {
                    vec![row[0].clone()]
                } else {
                    row.clone()
                }
            })
            .collect::<Vec<_>>();
        let missing_views = missing_artifacts
            .iter()
            .map(|row| row.iter().map(Vec::as_slice).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let missing_run = QUALIFICATION_MATRIX_ROWS
            .iter()
            .enumerate()
            .map(|(index, workload_name)| {
                (
                    *workload_name,
                    &receipts[index],
                    missing_views[index].as_slice(),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            QualificationReceipt::verify_matrix_for_profile_with_signer(
                "source",
                image,
                &profile,
                &missing_run,
                key.verifying_key().to_bytes(),
            )
            .is_err()
        );
    }

    #[test]
    fn multi_artifact_receipts_require_all_raw_bytes() {
        let primary = b"primary";
        let secondary = b"secondary";
        let runner = QualificationRunner::new(SigningKey::from_bytes(&[10; 32]));
        let receipt = runner
            .emit_with_evidence(
                "source".into(),
                Digest::from_bytes([11; 32]),
                "local".into(),
                "storage".into(),
                "none".into(),
                Vec::new(),
                primary,
                true,
                (
                    "rustc".into(),
                    "test".into(),
                    "local".into(),
                    0,
                    0,
                    0,
                    false,
                ),
                1,
                2,
                b"none",
                vec![
                    Digest::from_bytes(*blake3::hash(primary).as_bytes()),
                    Digest::from_bytes(*blake3::hash(secondary).as_bytes()),
                ],
                Vec::new(),
            )
            .unwrap();
        receipt
            .verify_for_artifacts(
                "source",
                Digest::from_bytes([11; 32]),
                &[primary, secondary],
            )
            .unwrap();
        assert!(
            receipt
                .verify_for("source", Digest::from_bytes([11; 32]), primary)
                .is_err()
        );
    }

    #[test]
    fn receipt_rejects_unbounded_or_non_ascii_identity() {
        assert!(
            QualificationReceipt::new(
                "\n".into(),
                Digest::from_bytes([1; 32]),
                "provider".into(),
                "workload".into(),
                "fault".into(),
                Vec::new(),
                Digest::from_bytes([2; 32]),
                false,
            )
            .is_err()
        );
        assert!(QualificationReceipt::decode(&vec![b' '; MAX_RECEIPT_BYTES + 1]).is_err());
    }
}
