use std::{
    collections::BTreeSet,
    future::Future,
    path::Path,
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};

use crate::{Digest, Error, Result};

const MAX_LABEL_BYTES: usize = 256;
const MAX_METRICS: usize = 64;
const MAX_RECEIPT_BYTES: usize = 1 << 20;
const MAX_QUALIFICATION_CELLS: u64 = 1_000_000;
const MAX_QUALIFICATION_OPERATIONS: u64 = 100_000_000;
const MAX_QUALIFICATION_DURATION_SECS: u64 = 7 * 24 * 60 * 60;
const MAX_QUALIFICATION_CONCURRENCY: usize = 1_024;

/// Maximum age of protected qualification evidence accepted by a release gate.
pub const QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
/// Clock skew tolerated when a protected receipt is checked by a release gate.
pub const QUALIFICATION_PROTECTED_EVIDENCE_MAX_CLOCK_SKEW_MS: u64 = 5 * 60 * 1_000;

/// Current wire schema for qualification evidence.
pub const QUALIFICATION_SCHEMA_VERSION: u32 = 5;
/// Schema for a manifest that binds one receipt to every qualification row.
pub const QUALIFICATION_MATRIX_SCHEMA_VERSION: u32 = 2;
/// Schema for a versioned workload threshold profile.
pub const QUALIFICATION_PROFILE_SCHEMA_VERSION: u32 = 2;
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
    minimum_throughput_ops_per_sec: u64,
    maximum_peak_rss_bytes: u64,
    maximum_local_disk_bytes: u64,
    maximum_file_descriptors: u64,
    maximum_bucket_calls: u64,
    provider: String,
    topology: String,
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
        let profile = Self {
            schema_version: QUALIFICATION_PROFILE_SCHEMA_VERSION,
            name,
            minimum_cells,
            minimum_operations,
            minimum_duration_secs,
            maximum_p99_latency_ms,
            minimum_throughput_ops_per_sec: 0,
            maximum_peak_rss_bytes: 0,
            maximum_local_disk_bytes: 0,
            maximum_file_descriptors: 0,
            maximum_bucket_calls: 0,
            provider: String::new(),
            topology: String::new(),
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Returns the deterministic local correctness profile.
    pub fn pr_contract() -> Self {
        Self::built_in("pr-contract-v1", 1, 1, 1, 5_000, "", "", 0, 0, 0, 0, 0)
    }

    /// Returns the three-process provider iteration profile.
    pub fn local_provider() -> Self {
        Self::built_in(
            "local-provider-v1",
            256,
            1_000_000,
            60,
            1_000,
            "rustfs",
            "three-process",
            1,
            8 * 1024 * 1024 * 1024,
            20 * 1024 * 1024 * 1024,
            10_000,
            10_000_000,
        )
    }

    /// Returns the dedicated scale profile.
    pub fn scale() -> Self {
        Self::built_in(
            "scale-v1",
            10_000,
            10_000_000,
            3_600,
            500,
            "rustfs",
            "dedicated-hosts",
            2_777,
            32 * 1024 * 1024 * 1024,
            200 * 1024 * 1024 * 1024,
            100_000,
            100_000_000,
        )
    }

    /// Returns the protected Kubernetes fault profile.
    pub fn fault() -> Self {
        Self::built_in(
            "fault-v1",
            256,
            1_000_000,
            60,
            1_000,
            "rustfs",
            "kubernetes",
            1,
            8 * 1024 * 1024 * 1024,
            20 * 1024 * 1024 * 1024,
            10_000,
            10_000_000,
        )
    }

    /// Returns the protected Kubernetes fault profile for an S3 deployment.
    pub fn fault_s3() -> Self {
        Self::fault_for("fault-s3-v1", "s3")
    }

    /// Returns the protected Kubernetes fault profile for a GCS deployment.
    pub fn fault_gcs() -> Self {
        Self::fault_for("fault-gcs-v1", "gcs")
    }

    /// Returns the protected Kubernetes fault profile for an Azure deployment.
    pub fn fault_azure() -> Self {
        Self::fault_for("fault-azure-v1", "azure")
    }

    /// Returns the provider-specific correctness profile.
    pub fn provider() -> Self {
        Self::built_in(
            "provider-v1",
            256,
            1_000_000,
            60,
            1_000,
            "provider-matrix",
            "three-process",
            1,
            8 * 1024 * 1024 * 1024,
            20 * 1024 * 1024 * 1024,
            10_000,
            10_000_000,
        )
    }

    /// Returns the protected provider correctness profile for S3.
    pub fn provider_s3() -> Self {
        Self::provider_for("provider-s3-v1", "s3")
    }

    /// Returns the protected provider correctness profile for GCS.
    pub fn provider_gcs() -> Self {
        Self::provider_for("provider-gcs-v1", "gcs")
    }

    /// Returns the protected provider correctness profile for Azure.
    pub fn provider_azure() -> Self {
        Self::provider_for("provider-azure-v1", "azure")
    }

    /// Returns the rolling-release compatibility profile.
    pub fn compatibility() -> Self {
        Self::built_in(
            "compatibility-v1",
            256,
            1_000_000,
            60,
            1_000,
            "rustfs",
            "rolling",
            1,
            8 * 1024 * 1024 * 1024,
            20 * 1024 * 1024 * 1024,
            10_000,
            10_000_000,
        )
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

    /// Returns the minimum measured throughput, or zero when the profile does
    /// not impose a throughput threshold.
    #[must_use]
    pub const fn minimum_throughput_ops_per_sec(&self) -> u64 {
        self.minimum_throughput_ops_per_sec
    }

    /// Returns the maximum permitted measured peak RSS, or zero when omitted.
    #[must_use]
    pub const fn maximum_peak_rss_bytes(&self) -> u64 {
        self.maximum_peak_rss_bytes
    }

    /// Returns the maximum permitted measured local-disk usage, or zero when omitted.
    #[must_use]
    pub const fn maximum_local_disk_bytes(&self) -> u64 {
        self.maximum_local_disk_bytes
    }

    /// Returns the maximum permitted measured file-descriptor count, or zero when omitted.
    #[must_use]
    pub const fn maximum_file_descriptors(&self) -> u64 {
        self.maximum_file_descriptors
    }

    /// Returns the maximum permitted object-store call count, or zero when omitted.
    #[must_use]
    pub const fn maximum_bucket_calls(&self) -> u64 {
        self.maximum_bucket_calls
    }

    /// Returns the required provider label, or an empty value for a generic profile.
    #[must_use]
    pub fn required_provider(&self) -> &str {
        &self.provider
    }

    /// Returns the required topology label, or an empty value for a generic profile.
    #[must_use]
    pub fn required_topology(&self) -> &str {
        &self.topology
    }

    /// Returns whether this profile represents release evidence rather than a
    /// local contract check.
    #[must_use]
    pub fn requires_protected_evidence(&self) -> bool {
        self != &Self::pr_contract()
    }

    /// Returns whether the protected profile names a provider whose
    /// conditional, range, and multipart semantics require raw evidence.
    #[must_use]
    pub fn requires_provider_evidence(&self) -> bool {
        self.requires_protected_evidence() && !self.provider.is_empty()
    }

    /// Returns whether this profile requires an injected fault schedule and
    /// ownership transition evidence.
    #[must_use]
    pub fn requires_fault_injection(&self) -> bool {
        self.name == "fault-v1" || self.name.starts_with("fault-")
    }

    /// Returns whether this named deployment profile requires every primitive
    /// lifecycle case to be observed by the typed executor.
    #[must_use]
    pub fn requires_lifecycle_case_coverage(&self) -> bool {
        self.requires_protected_evidence() && !self.provider.is_empty() && !self.topology.is_empty()
    }

    fn requires_resource_measurements(&self) -> bool {
        self.maximum_peak_rss_bytes != 0
            || self.maximum_local_disk_bytes != 0
            || self.maximum_file_descriptors != 0
            || self.maximum_bucket_calls != 0
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

    #[expect(
        clippy::too_many_arguments,
        reason = "built-in profiles keep every signed threshold explicit"
    )]
    fn built_in(
        name: &'static str,
        minimum_cells: u64,
        minimum_operations: u64,
        minimum_duration_secs: u64,
        maximum_p99_latency_ms: u64,
        provider: &'static str,
        topology: &'static str,
        minimum_throughput_ops_per_sec: u64,
        maximum_peak_rss_bytes: u64,
        maximum_local_disk_bytes: u64,
        maximum_file_descriptors: u64,
        maximum_bucket_calls: u64,
    ) -> Self {
        Self {
            schema_version: QUALIFICATION_PROFILE_SCHEMA_VERSION,
            name: name.to_owned(),
            minimum_cells,
            minimum_operations,
            minimum_duration_secs,
            maximum_p99_latency_ms,
            minimum_throughput_ops_per_sec,
            maximum_peak_rss_bytes,
            maximum_local_disk_bytes,
            maximum_file_descriptors,
            maximum_bucket_calls,
            provider: provider.to_owned(),
            topology: topology.to_owned(),
        }
    }

    fn fault_for(name: &'static str, provider: &'static str) -> Self {
        Self::built_in(
            name,
            256,
            1_000_000,
            60,
            1_000,
            provider,
            "kubernetes",
            1,
            8 * 1024 * 1024 * 1024,
            20 * 1024 * 1024 * 1024,
            10_000,
            10_000_000,
        )
    }

    fn provider_for(name: &'static str, provider: &'static str) -> Self {
        Self::built_in(
            name,
            256,
            1_000_000,
            60,
            1_000,
            provider,
            "three-process",
            1,
            8 * 1024 * 1024 * 1024,
            20 * 1024 * 1024 * 1024,
            10_000,
            10_000_000,
        )
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
            || self.minimum_throughput_ops_per_sec > MAX_QUALIFICATION_OPERATIONS
            || self.maximum_peak_rss_bytes > (u64::from(u32::MAX) << 32)
            || self.maximum_local_disk_bytes > (u64::from(u32::MAX) << 32)
            || self.maximum_file_descriptors > 1_000_000
            || self.maximum_bucket_calls > MAX_QUALIFICATION_OPERATIONS
        {
            return Err(Error::Control("invalid qualification profile"));
        }
        validate_label(&self.name, "qualification profile name")?;
        if !self.provider.is_empty() {
            validate_label(&self.provider, "qualification profile provider")?;
        }
        if !self.topology.is_empty() {
            validate_label(&self.topology, "qualification profile topology")?;
        }
        Ok(())
    }
}

/// Primitive rows exercised by the canonical mixed-load qualification driver.
pub const QUALIFICATION_PRIMITIVES: &[&str] = &[
    "sql", "kv", "blob", "queue", "cron", "workflow", "activity", "effects",
];

/// Resource measurements that protected run artifacts must carry.
pub const QUALIFICATION_RESOURCE_METRICS: &[(&str, &str)] = &[
    ("peak_rss_bytes", "bytes"),
    ("peak_local_disk_bytes", "bytes"),
    ("peak_file_descriptors", "count"),
    ("bucket_calls", "count"),
];

/// Lifecycle cases that a canonical qualification schedule exposes to its
/// application-specific executor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum QualificationCase {
    Happy = 0,
    Retry = 1,
    Duplicate = 2,
    Expiry = 3,
    Cancellation = 4,
    OwnerLoss = 5,
    Recovery = 6,
}

impl QualificationCase {
    /// Returns the stable bounded label used by workload adapters.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Happy => "happy",
            Self::Retry => "retry",
            Self::Duplicate => "duplicate",
            Self::Expiry => "expiry",
            Self::Cancellation => "cancellation",
            Self::OwnerLoss => "owner-loss",
            Self::Recovery => "recovery",
        }
    }
}

/// Lifecycle cases in the deterministic order used by the qualification driver.
pub const QUALIFICATION_CASES: &[QualificationCase] = &[
    QualificationCase::Happy,
    QualificationCase::Retry,
    QualificationCase::Duplicate,
    QualificationCase::Expiry,
    QualificationCase::Cancellation,
    QualificationCase::OwnerLoss,
    QualificationCase::Recovery,
];

/// Minimum generated schedule length that gives every primitive every case.
pub const QUALIFICATION_CASE_COVERAGE_OPERATIONS: u64 =
    (QUALIFICATION_PRIMITIVES.len() * QUALIFICATION_CASES.len()) as u64;
/// Number of bytes needed to retain one bit for every primitive/lifecycle pair.
pub const QUALIFICATION_CASE_COVERAGE_BYTES: usize =
    (QUALIFICATION_PRIMITIVES.len() * QUALIFICATION_CASES.len()).div_ceil(8);

fn case_coverage_index(operation: QualificationOperation) -> usize {
    operation.primitive_index as usize * QUALIFICATION_CASES.len() + operation.case as usize
}

fn mark_case_coverage(coverage: &mut [u8], operation: QualificationOperation) {
    let index = case_coverage_index(operation);
    coverage[index / 8] |= 1 << (index % 8);
}

fn has_complete_case_coverage(coverage: &[u8]) -> bool {
    (0..QUALIFICATION_PRIMITIVES.len()).all(|primitive| {
        (0..QUALIFICATION_CASES.len()).all(|case| {
            let index = primitive * QUALIFICATION_CASES.len() + case;
            coverage[index / 8] & (1 << (index % 8)) != 0
        })
    })
}

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
    case: QualificationCase,
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
    outcome: QualificationOutcome,
    verified: bool,
    retries: u64,
    case: Option<QualificationCase>,
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
struct QualificationLatencyHistogram {
    buckets: Vec<u64>,
    samples: u64,
    maximum_ms: u64,
}

// Built-in profile limits are at most five seconds. Keep millisecond precision
// beyond every gate and fail conservatively for longer samples.
const MAX_EXACT_LATENCY_MS: usize = 10_000;

impl Default for QualificationLatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: vec![0; MAX_EXACT_LATENCY_MS + 2],
            samples: 0,
            maximum_ms: 0,
        }
    }
}

impl QualificationLatencyHistogram {
    fn record(&mut self, latency: Duration) {
        let milliseconds = latency
            .as_millis()
            .saturating_add(u128::from(
                !latency.subsec_nanos().is_multiple_of(1_000_000),
            ))
            .max(1)
            .min(u128::from(u64::MAX)) as u64;
        let bucket = usize::try_from(milliseconds)
            .unwrap_or(MAX_EXACT_LATENCY_MS + 1)
            .min(MAX_EXACT_LATENCY_MS + 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
        self.maximum_ms = self.maximum_ms.max(milliseconds);
    }

    fn percentile_ms(&self, percentile: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let rank = (self.samples.saturating_mul(percentile).saturating_add(99) / 100).max(1);
        let mut seen = 0_u64;
        for (bucket, count) in self.buckets.iter().copied().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= rank {
                // The overflow bucket reports its observed maximum so a slow
                // sample can never pass a profile threshold by rounding down.
                return if bucket > MAX_EXACT_LATENCY_MS {
                    self.maximum_ms
                } else {
                    bucket as u64
                };
            }
        }
        self.maximum_ms
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
    case_coverage: Vec<u8>,
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

/// Schema for a measured, typed execution artifact bound to one workload.
pub const QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION: u32 = 4;

/// Schema for canonical provider-semantics evidence.
pub const QUALIFICATION_PROVIDER_EVIDENCE_SCHEMA_VERSION: u32 = 1;

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
    case_coverage: Vec<u8>,
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
        if profile.requires_lifecycle_case_coverage()
            && !has_complete_case_coverage(&self.case_coverage)
        {
            return Err(Error::Control("qualification run lifecycle case coverage"));
        }
        let duration_secs = self.elapsed_ms.saturating_add(999) / 1_000;
        for (name, unit, expected) in [
            ("cells", "cells", self.cells),
            ("operations", "operations", self.operations),
            ("duration_secs", "seconds", duration_secs),
            (
                "throughput_ops_per_sec",
                "ops/s",
                self.operations / duration_secs,
            ),
        ] {
            if self.threshold_metric(name, unit)? != expected {
                return Err(Error::Control("qualification run measured metrics"));
            }
        }
        if self.elapsed_ms < profile.minimum_duration_secs().saturating_mul(1_000)
            || self.threshold_metric("p99_latency_ms", "ms")? > profile.maximum_p99_latency_ms()
        {
            return Err(Error::Control("qualification run profile threshold failed"));
        }
        if profile.minimum_throughput_ops_per_sec() != 0
            && self.operations / duration_secs < profile.minimum_throughput_ops_per_sec()
        {
            return Err(Error::Control(
                "qualification run throughput threshold failed",
            ));
        }
        self.verify_resource_metrics(profile)?;
        Ok(())
    }

    #[must_use]
    pub fn workload(&self) -> &QualificationWorkload {
        &self.workload
    }

    /// Returns the bounded metrics captured with this measured run.
    #[must_use]
    pub fn metrics(&self) -> &[QualificationMetric] {
        &self.metrics
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

    fn verify_resource_metrics(&self, profile: &QualificationProfile) -> Result<()> {
        if !profile.requires_resource_measurements() {
            return Ok(());
        }
        for (name, unit) in QUALIFICATION_RESOURCE_METRICS {
            let value = self.threshold_metric(name, unit)?;
            if value == 0 {
                return Err(Error::Control("qualification resource metric is zero"));
            }
            let maximum = match *name {
                "peak_rss_bytes" => profile.maximum_peak_rss_bytes(),
                "peak_local_disk_bytes" => profile.maximum_local_disk_bytes(),
                "peak_file_descriptors" => profile.maximum_file_descriptors(),
                "bucket_calls" => profile.maximum_bucket_calls(),
                _ => return Err(Error::Control("unknown qualification resource metric")),
            };
            if maximum != 0 && value > maximum {
                return Err(Error::Control("qualification resource threshold failed"));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION
            || self.profile_digest.iter().all(|byte| *byte == 0)
            || self.outcome_digest.iter().all(|byte| *byte == 0)
            || self.elapsed_ms == 0
            || self.primitive_counts.len() != QUALIFICATION_PRIMITIVES.len()
            || self.case_coverage.len() != QUALIFICATION_CASE_COVERAGE_BYTES
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
        if actual_primitives != expected_primitives {
            return Err(Error::Control("qualification run counters"));
        }
        for counts in &self.primitive_counts {
            let expected = self
                .workload
                .primitives
                .iter()
                .find(|expected| expected.primitive == counts.primitive)
                .ok_or(Error::Control("qualification run primitive identity"))?;
            if counts.attempted != expected.attempted
                || !valid_primitive_counts(counts)
                || counts.verified == 0
            {
                return Err(Error::Control("qualification run counters"));
            }
        }
        if self
            .primitive_counts
            .iter()
            .try_fold(0_u64, |total, counts| total.checked_add(counts.attempted))
            != Some(self.operations)
        {
            return Err(Error::Control("qualification run counters"));
        }
        self.workload.validate()?;
        if qualification_run_outcome_digest(
            &self.workload,
            &self.primitive_counts,
            &self.case_coverage,
        )? != self.outcome_digest()
        {
            return Err(Error::Control("qualification run outcome"));
        }
        validate_metrics(&self.metrics)?;
        validate_resource_metric_units(&self.metrics)?;
        validate_run_latency_metrics(&self.metrics)?;
        Ok(())
    }
}

/// Canonical provider-semantics evidence bound to a protected primitives run.
///
/// Provider profiles require one such artifact proving conditional mutation,
/// bounded range reads, and multipart behavior. The artifact is raw signed
/// evidence; the receipt only stores its digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationProviderEvidence {
    schema_version: u32,
    provider: String,
    profile: String,
    profile_digest: [u8; 32],
    workload_seed: u64,
    conditional: bool,
    range: bool,
    multipart: bool,
}

impl QualificationProviderEvidence {
    /// Creates one provider-semantics artifact for a measured workload.
    pub fn new(
        profile: &QualificationProfile,
        workload_seed: u64,
        conditional: bool,
        range: bool,
        multipart: bool,
    ) -> Result<Self> {
        profile.validate()?;
        if profile.required_provider().is_empty() {
            return Err(Error::Control(
                "provider evidence requires a named qualification provider",
            ));
        }
        let evidence = Self {
            schema_version: QUALIFICATION_PROVIDER_EVIDENCE_SCHEMA_VERSION,
            provider: profile.required_provider().to_owned(),
            profile: profile.name.clone(),
            profile_digest: *profile.digest()?.as_bytes(),
            workload_seed,
            conditional,
            range,
            multipart,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    /// Encodes canonical provider-semantics evidence.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control(
                "qualification provider evidence exceeds limit",
            ));
        }
        Ok(bytes)
    }

    /// Decodes and validates canonical provider-semantics evidence.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control(
                "qualification provider evidence exceeds limit",
            ));
        }
        let evidence: Self = serde_json::from_slice(bytes)?;
        evidence.validate()?;
        if evidence.encode()? != bytes {
            return Err(Error::Control(
                "qualification provider evidence is not canonical",
            ));
        }
        Ok(evidence)
    }

    fn verify_for(&self, profile: &QualificationProfile, workload_seed: u64) -> Result<()> {
        if !profile.requires_provider_evidence()
            || self.provider != profile.required_provider()
            || self.profile != profile.name
            || self.profile_digest != *profile.digest()?.as_bytes()
            || self.workload_seed != workload_seed
            || !self.conditional
            || !self.range
            || !self.multipart
        {
            return Err(Error::Control(
                "qualification provider semantics are incomplete or mismatched",
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != QUALIFICATION_PROVIDER_EVIDENCE_SCHEMA_VERSION
            || self.profile_digest.iter().all(|byte| *byte == 0)
        {
            return Err(Error::Control("invalid qualification provider evidence"));
        }
        validate_label(&self.provider, "qualification provider evidence provider")?;
        validate_label(&self.profile, "qualification provider evidence profile")?;
        Ok(())
    }
}

fn verify_provider_evidence(
    profile: &QualificationProfile,
    run: &QualificationRunArtifact,
    artifacts: &[&[u8]],
) -> Result<()> {
    if !profile.requires_provider_evidence() {
        return Ok(());
    }
    let mut matches = Vec::new();
    for artifact in artifacts {
        let Some(candidate) = provider_evidence_candidate(artifact)? else {
            continue;
        };
        matches.push(candidate);
    }
    let mut matches = matches.into_iter();
    let evidence = matches.next().ok_or(Error::Control(
        "protected provider evidence is missing its semantics artifact",
    ))?;
    if matches.next().is_some() {
        return Err(Error::Control(
            "protected provider evidence has multiple semantics artifacts",
        ));
    }
    evidence.verify_for(profile, run.workload().seed())
}

fn provider_evidence_candidate(bytes: &[u8]) -> Result<Option<QualificationProviderEvidence>> {
    // Raw artifacts may use arbitrary formats, but a JSON object that starts
    // claiming provider semantics must decode completely or fail the receipt;
    // otherwise a partial duplicate could hide beside a valid artifact.
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    if !["conditional", "range", "multipart"]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return Ok(None);
    }
    QualificationProviderEvidence::decode(bytes).map(Some)
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

    async fn run_internal<E>(
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

    async fn run_concurrent_internal<E>(
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

    fn record_execution(
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

    fn summary(
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

fn qualification_workload_outcome_digest(seed: u64, cells: u64, operations: u64) -> Digest {
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

fn qualification_run_outcome_digest(
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
    execution_profile: String,
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

/// Non-secret execution identity and fault evidence supplied by a protected
/// qualification harness when it binds a measured run to a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationExecutionEvidence {
    /// Provider identity recorded by the harness.
    pub provider: String,
    /// Logical workload row in the ten-row qualification matrix.
    pub workload: String,
    /// Named fault schedule, or `none` for a non-fault run.
    pub fault: String,
    /// Toolchain identity used by the harness.
    pub toolchain: String,
    /// Immutable execution image/profile identity.
    pub execution_profile: String,
    /// Topology identity required by the selected profile.
    pub topology: String,
    /// Wall-clock start timestamp in Unix milliseconds.
    pub started_at_ms: u64,
    /// Wall-clock finish timestamp in Unix milliseconds.
    pub finished_at_ms: u64,
    /// Canonical fault schedule bytes retained in the raw evidence bundle.
    pub fault_schedule: Vec<u8>,
    /// Ownership watermarks captured before/after any injected fault.
    pub ownership: Vec<QualificationOwnership>,
    /// Whether the harness observed a dirty source or workspace.
    pub dirty: bool,
}

impl QualificationExecutionEvidence {
    /// Encodes the non-secret execution evidence in its canonical JSON form.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(Error::from)?;
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control(
                "qualification execution evidence exceeds limit",
            ));
        }
        Ok(bytes)
    }

    /// Decodes and validates one canonical execution evidence file.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control(
                "qualification execution evidence exceeds limit",
            ));
        }
        let evidence: Self = serde_json::from_slice(bytes)?;
        evidence.validate()?;
        if evidence.encode()? != bytes {
            return Err(Error::Control(
                "qualification execution evidence is not canonical",
            ));
        }
        Ok(evidence)
    }

    fn validate(&self) -> Result<()> {
        validate_label(&self.provider, "qualification evidence provider")?;
        validate_label(&self.workload, "qualification evidence workload")?;
        validate_label(&self.fault, "qualification evidence fault")?;
        validate_label(&self.toolchain, "qualification evidence toolchain")?;
        validate_label(
            &self.execution_profile,
            "qualification evidence execution profile",
        )?;
        validate_label(&self.topology, "qualification evidence topology")?;
        if self.started_at_ms == 0
            || self.finished_at_ms < self.started_at_ms
            || self.fault_schedule.is_empty()
            || self.fault_schedule.len() > MAX_RECEIPT_BYTES
            || self.ownership.len() > MAX_METRICS
            || self
                .ownership
                .iter()
                .any(|proof| proof.root.iter().all(|byte| *byte == 0))
        {
            return Err(Error::Control("qualification execution evidence"));
        }
        Ok(())
    }
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
        if self
            .entries
            .iter()
            .zip(QUALIFICATION_MATRIX_ROWS.iter().copied())
            .any(|(entry, expected)| entry.workload != expected)
        {
            return Err(Error::Control("qualification matrix row order"));
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

fn validate_resource_metric_list(metrics: &[QualificationMetric]) -> Result<()> {
    validate_metrics(metrics)?;
    if metrics.iter().any(|metric| {
        !QUALIFICATION_RESOURCE_METRICS
            .iter()
            .any(|(name, unit)| metric.name() == *name && metric.unit() == *unit)
    }) {
        return Err(Error::Control("unknown qualification resource metric"));
    }
    Ok(())
}

fn validate_resource_metric_units(metrics: &[QualificationMetric]) -> Result<()> {
    for metric in metrics {
        if let Some((_, expected_unit)) = QUALIFICATION_RESOURCE_METRICS
            .iter()
            .find(|(name, _)| metric.name() == *name)
            && metric.unit() != *expected_unit
        {
            return Err(Error::Control("qualification resource metric unit"));
        }
    }
    Ok(())
}

fn validate_run_latency_metrics(metrics: &[QualificationMetric]) -> Result<()> {
    let mut latency = [0_u64; 4];
    for (index, (name, unit)) in [
        ("p50_latency_ms", "ms"),
        ("p95_latency_ms", "ms"),
        ("p99_latency_ms", "ms"),
        ("max_latency_ms", "ms"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut matches = metrics.iter().filter(|metric| metric.name() == name);
        let Some(metric) = matches.next() else {
            return Err(Error::Control("qualification run latency metrics"));
        };
        if matches.next().is_some() || metric.unit() != unit {
            return Err(Error::Control("qualification run latency metrics"));
        }
        latency[index] = metric.value();
    }
    if !latency.windows(2).all(|pair| pair[0] <= pair[1]) {
        return Err(Error::Control("qualification run latency percentile order"));
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
            execution_profile: "unknown".into(),
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
        execution_profile: String,
        topology: String,
        workload_seed: u64,
        bucket_calls: u64,
        peak_rss_bytes: u64,
        dirty: bool,
    ) -> Result<Self> {
        validate_label(&toolchain, "qualification toolchain")?;
        validate_label(&execution_profile, "qualification execution profile")?;
        validate_label(&topology, "qualification topology")?;
        self.toolchain = toolchain;
        self.execution_profile = execution_profile;
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
        if fault_schedule.is_empty() || fault_schedule.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification fault schedule"));
        }
        if raw_artifact_digests.is_empty() || raw_artifact_digests.len() > MAX_METRICS {
            return Err(Error::Control("qualification artifact digest count"));
        }
        if ownership.len() > MAX_METRICS {
            return Err(Error::Control("qualification ownership proof count"));
        }
        if ownership
            .iter()
            .any(|proof| proof.root.iter().all(|byte| *byte == 0))
        {
            return Err(Error::Control("qualification ownership proof"));
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
    pub fn execution_profile(&self) -> &str {
        &self.execution_profile
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
        if (!profile.provider.is_empty() && self.provider != profile.provider)
            || (!profile.topology.is_empty() && self.topology != profile.topology)
        {
            return Err(Error::Control("qualification environment identity"));
        }
        self.verify_execution_environment(profile)?;
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

    /// Rejects protected evidence that is stale or materially ahead of the verifier clock.
    pub fn verify_fresh_at(&self, now_ms: u64) -> Result<()> {
        if now_ms == 0 {
            return Err(Error::Control("qualification verifier timestamp"));
        }
        if self.finished_at_ms
            > now_ms.saturating_add(QUALIFICATION_PROTECTED_EVIDENCE_MAX_CLOCK_SKEW_MS)
        {
            return Err(Error::Control(
                "qualification evidence timestamp is in the future",
            ));
        }
        if now_ms.saturating_sub(self.finished_at_ms) > QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS
        {
            return Err(Error::Control("qualification evidence is stale"));
        }
        Ok(())
    }

    /// Verifies the exact profile thresholds from measured receipt metrics.
    pub fn verify_profile_thresholds(&self, profile: &QualificationProfile) -> Result<()> {
        let cells = self.threshold_metric("cells", "cells")?;
        let operations = self.threshold_metric("operations", "operations")?;
        let duration_secs = self.threshold_metric("duration_secs", "seconds")?;
        let p99_latency_ms = self.threshold_metric("p99_latency_ms", "ms")?;
        if duration_secs == 0 {
            return Err(Error::Control("qualification duration threshold is zero"));
        }
        if cells < profile.minimum_cells()
            || operations < profile.minimum_operations()
            || duration_secs < profile.minimum_duration_secs()
            || p99_latency_ms > profile.maximum_p99_latency_ms()
        {
            return Err(Error::Control("qualification profile threshold failed"));
        }
        if profile.minimum_throughput_ops_per_sec() != 0
            && operations / duration_secs < profile.minimum_throughput_ops_per_sec()
        {
            return Err(Error::Control("qualification throughput threshold failed"));
        }
        let resource_envelope = profile.requires_resource_measurements();
        if (profile.maximum_peak_rss_bytes() != 0 || resource_envelope)
            && (self.peak_rss_bytes == 0
                || (profile.maximum_peak_rss_bytes() != 0
                    && self.peak_rss_bytes > profile.maximum_peak_rss_bytes()))
        {
            return Err(Error::Control("qualification RSS threshold failed"));
        }
        if profile.maximum_local_disk_bytes() != 0 || resource_envelope {
            let peak_local_disk_bytes = self.threshold_metric("peak_local_disk_bytes", "bytes")?;
            if peak_local_disk_bytes == 0
                || (profile.maximum_local_disk_bytes() != 0
                    && peak_local_disk_bytes > profile.maximum_local_disk_bytes())
            {
                return Err(Error::Control("qualification disk threshold failed"));
            }
        }
        if profile.maximum_file_descriptors() != 0 || resource_envelope {
            let peak_file_descriptors = self.threshold_metric("peak_file_descriptors", "count")?;
            if peak_file_descriptors == 0
                || (profile.maximum_file_descriptors() != 0
                    && peak_file_descriptors > profile.maximum_file_descriptors())
            {
                return Err(Error::Control(
                    "qualification file-descriptor threshold failed",
                ));
            }
        }
        if (profile.maximum_bucket_calls() != 0 || resource_envelope)
            && (self.bucket_calls == 0
                || (profile.maximum_bucket_calls() != 0
                    && self.bucket_calls > profile.maximum_bucket_calls()))
        {
            return Err(Error::Control(
                "qualification object-store threshold failed",
            ));
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
            receipt.verify_profile_thresholds(profile)?;
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

    /// Verifies a signed protected matrix and requires every row to be recent.
    pub fn verify_matrix_for_profile_with_signer_fresh_at(
        source_revision: &str,
        image: Digest,
        profile: &QualificationProfile,
        evidence: &[(&str, &QualificationReceipt, &[&[u8]])],
        trusted_signer: [u8; 32],
        now_ms: u64,
    ) -> Result<()> {
        Self::verify_matrix_for_profile_with_signer(
            source_revision,
            image,
            profile,
            evidence,
            trusted_signer,
        )?;
        for (_, receipt, _) in evidence {
            receipt.verify_fresh_at(now_ms)?;
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
        validate_label(&self.execution_profile, "qualification execution profile")?;
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
        let mut workloads = artifacts.iter().filter_map(|artifact| {
            QualificationWorkload::decode(artifact)
                .ok()
                .filter(|workload| workload.verify_for_profile(profile).is_ok())
        });
        if let Some(workload) = workloads.next()
            && (workloads.next().is_some() || workload != *run.workload())
        {
            return Err(Error::Control(
                "qualification receipt does not bind the measured workload",
            ));
        }
        verify_provider_evidence(profile, &run, artifacts)?;
        for (name, unit) in [
            ("cells", "cells"),
            ("operations", "operations"),
            ("duration_secs", "seconds"),
            ("throughput_ops_per_sec", "ops/s"),
            ("p50_latency_ms", "ms"),
            ("p95_latency_ms", "ms"),
            ("p99_latency_ms", "ms"),
            ("max_latency_ms", "ms"),
        ] {
            if self.threshold_metric(name, unit)? != run.threshold_metric(name, unit)? {
                return Err(Error::Control(
                    "qualification receipt does not bind measured run metrics",
                ));
            }
        }
        if profile.requires_resource_measurements() {
            for (name, unit) in QUALIFICATION_RESOURCE_METRICS {
                let receipt_value = match *name {
                    "peak_rss_bytes" => self.peak_rss_bytes,
                    "bucket_calls" => self.bucket_calls,
                    "peak_local_disk_bytes" | "peak_file_descriptors" => {
                        self.threshold_metric(name, unit)?
                    }
                    _ => return Err(Error::Control("unknown qualification resource metric")),
                };
                if receipt_value != run.threshold_metric(name, unit)? {
                    return Err(Error::Control(
                        "qualification receipt does not bind resource measurements",
                    ));
                }
            }
        }
        Ok(())
    }

    fn verify_execution_environment(&self, profile: &QualificationProfile) -> Result<()> {
        if !profile.requires_protected_evidence() {
            return Ok(());
        }
        if self.finished_at_ms.saturating_sub(self.started_at_ms)
            < profile.minimum_duration_secs().saturating_mul(1_000)
        {
            return Err(Error::Control(
                "protected qualification evidence duration is below profile minimum",
            ));
        }
        if self.topology == "local"
            || self.toolchain == "unknown"
            || self.execution_profile != "release"
            || self.bucket_calls == 0
            || self.peak_rss_bytes == 0
            || self.ownership.is_empty()
        {
            return Err(Error::Control(
                "protected qualification evidence lacks measured environment proof",
            ));
        }
        if profile.requires_fault_injection()
            && (self.fault.eq_ignore_ascii_case("none")
                || self.fault_schedule_digest == *blake3::hash(b"none").as_bytes()
                || self.ownership.len() < 2
                // Two repeated samples prove no failover; require a real watermark advance.
                || !self.ownership.windows(2).any(|observations| {
                    observations[1].epoch > observations[0].epoch
                        || observations[1].published_sequence > observations[0].published_sequence
                })
                || self.ownership.windows(2).any(|observations| {
                    observations[1].epoch < observations[0].epoch
                        || observations[1].published_sequence < observations[0].published_sequence
                }))
        {
            return Err(Error::Control(
                "fault qualification evidence lacks an injected ownership transition",
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

    /// Binds a verified typed run artifact to one signed receipt.
    ///
    /// The run artifact must be the first raw artifact and a canonical
    /// workload artifact must be present in the remaining list. This keeps the
    /// protected `primitives` row reproducible while allowing the harness to
    /// retain additional raw provider/fault evidence in the same receipt.
    pub fn emit_protected_run(
        &self,
        profile: &QualificationProfile,
        source_revision: String,
        image: Digest,
        evidence: QualificationExecutionEvidence,
        run: &QualificationRunArtifact,
        artifacts: &[&[u8]],
    ) -> Result<QualificationReceipt> {
        evidence.validate()?;
        if evidence.dirty {
            return Err(Error::Control("qualification run source is dirty"));
        }
        run.verify_for_profile(profile)?;
        if artifacts.is_empty() {
            return Err(Error::Control("qualification run artifacts are empty"));
        }
        if evidence.workload != "primitives" {
            return Err(Error::Control("qualification run workload row"));
        }
        if (!profile.required_provider().is_empty()
            && evidence.provider != profile.required_provider())
            || (!profile.required_topology().is_empty()
                && evidence.topology != profile.required_topology())
        {
            return Err(Error::Control("qualification run environment identity"));
        }
        if profile.requires_protected_evidence()
            && (evidence.topology == "local"
                || evidence.toolchain == "unknown"
                || evidence.execution_profile != "release"
                || evidence.ownership.is_empty()
                || evidence
                    .finished_at_ms
                    .saturating_sub(evidence.started_at_ms)
                    < run.elapsed_ms)
        {
            return Err(Error::Control(
                "qualification run lacks protected execution proof",
            ));
        }
        if profile.requires_fault_injection()
            && (evidence.fault.eq_ignore_ascii_case("none")
                || evidence.fault_schedule == b"none"
                || evidence.ownership.len() < 2
                || !evidence.ownership.windows(2).any(|observations| {
                    observations[1].epoch() > observations[0].epoch()
                        || observations[1].published_sequence()
                            > observations[0].published_sequence()
                })
                || evidence.ownership.windows(2).any(|observations| {
                    observations[1].epoch() < observations[0].epoch()
                        || observations[1].published_sequence()
                            < observations[0].published_sequence()
                }))
        {
            return Err(Error::Control(
                "qualification run lacks protected fault transition proof",
            ));
        }
        let encoded_run = run.encode()?;
        if Digest::from_bytes(*blake3::hash(artifacts[0]).as_bytes())
            != Digest::from_bytes(*blake3::hash(&encoded_run).as_bytes())
            || QualificationRunArtifact::decode(artifacts[0])? != *run
        {
            return Err(Error::Control("qualification run primary artifact differs"));
        }
        let mut workload_artifacts = artifacts.iter().filter_map(|artifact| {
            QualificationWorkload::decode(artifact)
                .ok()
                .filter(|workload| workload.verify_for_profile(profile).is_ok())
        });
        let Some(workload) = workload_artifacts.next() else {
            return Err(Error::Control("qualification run workload artifact count"));
        };
        if workload_artifacts.next().is_some() || workload != *run.workload() {
            return Err(Error::Control("qualification run workload identity"));
        }
        verify_provider_evidence(profile, run, artifacts)?;
        let bucket_calls = run.threshold_metric("bucket_calls", "count")?;
        let peak_rss_bytes = run.threshold_metric("peak_rss_bytes", "bytes")?;
        let artifact_digests = artifacts
            .iter()
            .map(|artifact| Digest::from_bytes(*blake3::hash(artifact).as_bytes()))
            .collect::<Vec<_>>();
        let receipt = QualificationReceipt::new(
            source_revision,
            image,
            evidence.provider,
            evidence.workload,
            evidence.fault,
            run.metrics().to_vec(),
            artifact_digests[0],
            true,
        )?
        .with_execution(
            evidence.toolchain,
            evidence.execution_profile,
            evidence.topology,
            run.workload().seed(),
            bucket_calls,
            peak_rss_bytes,
            evidence.dirty,
        )?
        .with_profile(profile)?
        .with_evidence(
            evidence.started_at_ms,
            evidence.finished_at_ms,
            &evidence.fault_schedule,
            artifact_digests,
            evidence.ownership,
        )?
        .attest(&self.signing_key)?;
        Ok(receipt)
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
    fn qualification_p99_never_rounds_a_slow_operation_below_the_gate() {
        let mut latency = QualificationLatencyHistogram::default();
        for _ in 0..63 {
            latency.record(Duration::from_millis(80));
        }
        latency.record(Duration::from_millis(6_082));
        assert_eq!(latency.percentile_ms(99), 6_082);
    }

    #[test]
    fn qualification_p99_uses_the_measured_overflow_maximum() {
        let mut latency = QualificationLatencyHistogram::default();
        for _ in 0..63 {
            latency.record(Duration::from_millis(80));
        }
        latency.record(Duration::from_millis(16_778));
        assert_eq!(latency.percentile_ms(99), 16_778);
    }

    #[test]
    fn execution_evidence_round_trip_is_canonical_and_bounded() {
        let evidence = QualificationExecutionEvidence {
            provider: "s3".into(),
            workload: "primitives".into(),
            fault: "owner-loss".into(),
            toolchain: "rustc-1.90".into(),
            execution_profile: "release".into(),
            topology: "kubernetes".into(),
            started_at_ms: 10,
            finished_at_ms: 20,
            fault_schedule: b"owner-loss-before-commit".to_vec(),
            ownership: vec![QualificationOwnership::new(
                4,
                8,
                Digest::from_bytes([7; 32]),
            )],
            dirty: false,
        };
        let encoded = evidence.encode().expect("evidence encoding");
        assert_eq!(
            QualificationExecutionEvidence::decode(&encoded).expect("evidence decoding"),
            evidence
        );
        let mut noncanonical = encoded;
        noncanonical.push(b'\n');
        assert!(QualificationExecutionEvidence::decode(&noncanonical).is_err());
    }

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
        assert!(!contract.requires_lifecycle_case_coverage());
        assert!(local.requires_lifecycle_case_coverage());
        assert!(scale.requires_lifecycle_case_coverage());
        assert!(fault.requires_lifecycle_case_coverage());
        assert!(provider.requires_lifecycle_case_coverage());
        assert!(compatibility.requires_lifecycle_case_coverage());
        assert_eq!(fault.name(), "fault-v1");
        assert_eq!(provider.name(), "provider-v1");
        assert_eq!(compatibility.name(), "compatibility-v1");
        for (profile, name, provider, topology) in [
            (
                QualificationProfile::provider_s3(),
                "provider-s3-v1",
                "s3",
                "three-process",
            ),
            (
                QualificationProfile::provider_gcs(),
                "provider-gcs-v1",
                "gcs",
                "three-process",
            ),
            (
                QualificationProfile::provider_azure(),
                "provider-azure-v1",
                "azure",
                "three-process",
            ),
            (
                QualificationProfile::fault_s3(),
                "fault-s3-v1",
                "s3",
                "kubernetes",
            ),
            (
                QualificationProfile::fault_gcs(),
                "fault-gcs-v1",
                "gcs",
                "kubernetes",
            ),
            (
                QualificationProfile::fault_azure(),
                "fault-azure-v1",
                "azure",
                "kubernetes",
            ),
        ] {
            assert_eq!(profile.name(), name);
            assert_eq!(profile.required_provider(), provider);
            assert_eq!(profile.required_topology(), topology);
            assert!(profile.requires_protected_evidence());
        }
        assert_ne!(contract.digest().unwrap(), local.digest().unwrap());
        assert_eq!(
            QualificationProfile::decode(&contract.encode().unwrap()).unwrap(),
            contract
        );
    }

    #[test]
    fn threshold_profile_constructor_rejects_unbounded_workloads() {
        assert!(
            QualificationProfile::new(
                "too-many-cells".into(),
                MAX_QUALIFICATION_CELLS + 1,
                1,
                1,
                1,
            )
            .is_err()
        );
        assert!(
            QualificationProfile::new(
                "too-many-operations".into(),
                1,
                MAX_QUALIFICATION_OPERATIONS + 1,
                1,
                1,
            )
            .is_err()
        );
        assert!(
            QualificationProfile::new(
                "too-long".into(),
                1,
                1,
                MAX_QUALIFICATION_DURATION_SECS + 1,
                1,
            )
            .is_err()
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

        let mut forged = first.clone();
        forged.outcome_digest[0] ^= 1;
        assert!(forged.encode().is_err());
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

    #[test]
    fn qualification_schedule_reaches_each_outcome_hint() {
        let profile = QualificationProfile::new("hint-run".into(), 1, 8, 1, 1_000).unwrap();
        let workload =
            QualificationWorkload::generate_with_size(&profile, 41, 1, 2_048, 1).unwrap();
        let operations = workload.iter_operations().collect::<Vec<_>>();
        assert!(operations.iter().any(|operation| operation.retry_hint()));
        assert!(
            operations
                .iter()
                .any(|operation| operation.rejection_hint())
        );
        assert!(
            operations
                .iter()
                .any(|operation| operation.ambiguous_hint())
        );
        assert!(
            operations
                .iter()
                .filter(|operation| operation.ambiguous_hint())
                .all(|operation| !operation.rejection_hint())
        );
    }

    #[test]
    fn qualification_schedule_covers_each_lifecycle_case_per_primitive() {
        let profile = QualificationProfile::new("case-run".into(), 1, 1, 1, 1_000).unwrap();
        let workload = QualificationWorkload::generate(&profile, 41).unwrap();
        assert_eq!(
            workload.operations(),
            QUALIFICATION_CASE_COVERAGE_OPERATIONS
        );
        for primitive in QUALIFICATION_PRIMITIVES {
            for case in QUALIFICATION_CASES {
                assert!(
                    workload
                        .iter_operations()
                        .any(|operation| operation.primitive() == *primitive
                            && operation.case() == *case),
                    "missing {} case for {primitive}",
                    case.name()
                );
            }
        }
    }

    struct ContractExecutor {
        calls: u64,
        case_coverage: bool,
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
            let execution = if self.case_coverage {
                execution.with_case(operation.case())
            } else {
                execution
            };
            std::future::ready(Ok(execution))
        }
    }

    #[derive(Clone)]
    struct ConcurrentExecutor {
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        maximum: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_at: Option<u64>,
    }

    impl QualificationOperationExecutor for ConcurrentExecutor {
        type Future<'a> =
            std::pin::Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

        fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
            let active = std::sync::Arc::clone(&self.active);
            let maximum = std::sync::Arc::clone(&self.maximum);
            let fail_at = self.fail_at;
            Box::pin(async move {
                let current = active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
                maximum.fetch_max(current, std::sync::atomic::Ordering::AcqRel);
                tokio::task::yield_now().await;
                active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                if Some(operation.index()) == fail_at {
                    return Err(Error::Control("qualification executor failed"));
                }
                let execution = if operation.rejection_hint() {
                    QualificationExecution::rejected()
                } else if operation.ambiguous_hint() {
                    QualificationExecution::ambiguous(u64::from(operation.retry_hint()))
                } else {
                    QualificationExecution::acknowledged(true)
                        .with_retries(u64::from(operation.retry_hint()))
                };
                Ok(execution)
            })
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
        let mut executor = ContractExecutor {
            calls: 0,
            case_coverage: false,
        };
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

    #[tokio::test]
    async fn case_coverage_runner_rejects_unverified_lifecycle_hints() {
        let profile = QualificationProfile::pr_contract();
        let workload = QualificationWorkload::generate(&profile, 41).unwrap();
        let mut missing = ContractExecutor {
            calls: 0,
            case_coverage: false,
        };
        assert!(workload.run_with_case_coverage(&mut missing).await.is_err());

        let mut covered = ContractExecutor {
            calls: 0,
            case_coverage: true,
        };
        let summary = workload.run_with_case_coverage(&mut covered).await.unwrap();
        assert!(summary.case_coverage().iter().all(|byte| *byte == u8::MAX));
        summary.artifact(&workload).unwrap();
    }

    #[tokio::test]
    async fn concurrent_workload_runner_bounds_inflight_operations() {
        let profile = QualificationProfile::new("concurrent-run".into(), 1, 32, 1, 1_000).unwrap();
        let workload = QualificationWorkload::generate_with_size(&profile, 41, 2, 32, 1).unwrap();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let maximum = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor = ConcurrentExecutor {
            active: std::sync::Arc::clone(&active),
            maximum: std::sync::Arc::clone(&maximum),
            fail_at: None,
        };
        assert!(workload.run_concurrent(executor.clone(), 0).await.is_err());
        assert!(
            workload
                .run_concurrent(executor.clone(), MAX_QUALIFICATION_CONCURRENCY + 1)
                .await
                .is_err()
        );
        let summary = workload.run_concurrent(executor, 4).await.unwrap();
        assert_eq!(summary.operations(), workload.operations());
        assert!(maximum.load(std::sync::atomic::Ordering::Acquire) >= 2);
        assert!(maximum.load(std::sync::atomic::Ordering::Acquire) <= 4);
        summary.artifact(&workload).unwrap().encode().unwrap();
    }

    #[tokio::test]
    async fn concurrent_workload_runner_drains_inflight_operations_after_failure() {
        let profile =
            QualificationProfile::new("concurrent-failure".into(), 1, 32, 1, 1_000).unwrap();
        let workload = QualificationWorkload::generate_with_size(&profile, 41, 2, 32, 1).unwrap();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let maximum = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor = ConcurrentExecutor {
            active: std::sync::Arc::clone(&active),
            maximum,
            fail_at: Some(0),
        };
        assert!(workload.run_concurrent(executor, 4).await.is_err());
        assert_eq!(active.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn measured_run_artifact_binds_the_canonical_workload_and_thresholds() {
        let profile = QualificationProfile::pr_contract();
        let workload = QualificationWorkload::generate_with_size(&profile, 19, 1, 64, 1).unwrap();
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
            case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
            outcome_digest: *qualification_run_outcome_digest(
                &workload,
                &workload.primitives,
                &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
            )
            .unwrap()
            .as_bytes(),
            metrics: vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 64, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                QualificationMetric::new("throughput_ops_per_sec".into(), 64, "ops/s".into())
                    .unwrap(),
                QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
            ],
        };
        let encoded = artifact.encode().unwrap();
        assert_eq!(
            QualificationRunArtifact::decode(&encoded).unwrap(),
            artifact
        );
        artifact.verify_for_profile(&profile).unwrap();

        let mut missing_latency = artifact.clone();
        missing_latency
            .metrics
            .retain(|metric| metric.name() != "p95_latency_ms");
        assert!(missing_latency.encode().is_err());

        let mut unordered_latency = artifact.clone();
        unordered_latency
            .metrics
            .iter_mut()
            .find(|metric| metric.name() == "p50_latency_ms")
            .unwrap()
            .value = 2;
        assert!(unordered_latency.encode().is_err());

        let mut all_acknowledged = artifact.clone();
        for counts in &mut all_acknowledged.primitive_counts {
            counts.acknowledged = counts.attempted;
            counts.rejected = 0;
            counts.ambiguous = 0;
            counts.retried = 0;
            counts.verified = counts.attempted;
        }
        all_acknowledged.outcome_digest = *qualification_run_outcome_digest(
            &all_acknowledged.workload,
            &all_acknowledged.primitive_counts,
            &all_acknowledged.case_coverage,
        )
        .unwrap()
        .as_bytes();
        all_acknowledged.verify_for_profile(&profile).unwrap();

        let mut throughput_profile =
            QualificationProfile::new("throughput-run".into(), 1, 8, 1, 1_000).unwrap();
        throughput_profile.minimum_throughput_ops_per_sec = 8;
        let throughput_workload =
            QualificationWorkload::generate_with_size(&throughput_profile, 19, 1, 8, 1).unwrap();
        let mut slow = QualificationRunArtifact {
            schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
            workload: throughput_workload.clone(),
            profile: throughput_profile.name.clone(),
            profile_digest: *throughput_profile.digest().unwrap().as_bytes(),
            seed: throughput_workload.seed(),
            cells: throughput_workload.cells(),
            operations: throughput_workload.operations(),
            elapsed_ms: 2_000,
            primitive_counts: throughput_workload.primitives.clone(),
            case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
            outcome_digest: *qualification_run_outcome_digest(
                &throughput_workload,
                &throughput_workload.primitives,
                &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
            )
            .unwrap()
            .as_bytes(),
            metrics: vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 8, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 2, "seconds".into()).unwrap(),
                QualificationMetric::new("throughput_ops_per_sec".into(), 4, "ops/s".into())
                    .unwrap(),
                QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
            ],
        };
        assert!(slow.verify_for_profile(&throughput_profile).is_err());
        slow.elapsed_ms = 1_000;
        slow.metrics[2].value = 1;
        slow.metrics[3].value = 8;
        slow.verify_for_profile(&throughput_profile).unwrap();

        let mut forged = artifact.clone();
        forged.seed = forged.seed.saturating_add(1);
        assert!(forged.verify_for_profile(&profile).is_err());

        let mut forged_digest = artifact.clone();
        forged_digest.outcome_digest[0] ^= 1;
        assert!(forged_digest.encode().is_err());

        let mut unverified = artifact.clone();
        unverified.primitive_counts[0].verified = 0;
        assert!(unverified.encode().is_err());

        let mut redistributed = artifact.clone();
        let donor = redistributed
            .primitive_counts
            .iter()
            .position(|counts| counts.acknowledged >= 2)
            .unwrap();
        let recipient = redistributed
            .primitive_counts
            .iter()
            .position(|counts| counts.primitive != redistributed.primitive_counts[donor].primitive)
            .unwrap();
        redistributed.primitive_counts[donor].attempted -= 1;
        redistributed.primitive_counts[donor].acknowledged -= 1;
        redistributed.primitive_counts[donor].verified -= 1;
        redistributed.primitive_counts[recipient].attempted += 1;
        redistributed.primitive_counts[recipient].acknowledged += 1;
        redistributed.primitive_counts[recipient].verified += 1;
        assert!(redistributed.encode().is_err());
    }

    #[tokio::test]
    async fn protected_run_artifact_binds_receipt_resource_measurements() {
        let mut profile =
            QualificationProfile::new("protected-resource-contract".into(), 1, 8, 1, 5_000)
                .unwrap();
        profile.maximum_peak_rss_bytes = 1_000;
        profile.maximum_local_disk_bytes = 2_000;
        profile.maximum_file_descriptors = 3_000;
        profile.maximum_bucket_calls = 4_000;
        let workload = QualificationWorkload::generate_with_size(&profile, 23, 1, 8, 1).unwrap();
        let mut executor = ContractExecutor {
            calls: 0,
            case_coverage: false,
        };
        let summary = workload.run(&mut executor).await.unwrap();
        assert!(
            summary
                .artifact(&workload)
                .unwrap()
                .verify_for_profile(&profile)
                .is_err()
        );

        let resources = [
            QualificationMetric::new("peak_rss_bytes".into(), 19, "bytes".into()).unwrap(),
            QualificationMetric::new("peak_local_disk_bytes".into(), 29, "bytes".into()).unwrap(),
            QualificationMetric::new("peak_file_descriptors".into(), 39, "count".into()).unwrap(),
            QualificationMetric::new("bucket_calls".into(), 49, "count".into()).unwrap(),
        ];
        let mut run = summary
            .artifact_with_resource_metrics(&workload, &resources)
            .unwrap();
        run.elapsed_ms = 1_000;
        for (name, value) in [("duration_secs", 1), ("throughput_ops_per_sec", 8)] {
            run.metrics
                .iter_mut()
                .find(|metric| metric.name() == name)
                .unwrap()
                .value = value;
        }
        run.verify_for_profile(&profile).unwrap();
        let encoded = run.encode().unwrap();

        let mut receipt_metrics = summary.metrics().unwrap();
        receipt_metrics.extend(resources.iter().cloned());
        let key = SigningKey::from_bytes(&[93; 32]);
        let receipt = QualificationRunner::new(key)
            .emit_with_profile_and_evidence(
                &profile,
                "resource-source".into(),
                Digest::from_bytes([94; 32]),
                "provider".into(),
                "primitives".into(),
                "none".into(),
                receipt_metrics,
                &encoded,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "three-process".into(),
                    workload.seed(),
                    49,
                    19,
                    false,
                ),
                1,
                1_001,
                b"none",
                vec![Digest::from_bytes(*blake3::hash(&encoded).as_bytes())],
                vec![QualificationOwnership::new(
                    1,
                    1,
                    Digest::from_bytes([95; 32]),
                )],
            )
            .unwrap();
        receipt.verify_profile_thresholds(&profile).unwrap();
        receipt
            .verify_primitive_run_artifact(&profile, &[&encoded])
            .unwrap();

        let mut forged = receipt;
        forged.bucket_calls += 1;
        assert!(
            forged
                .verify_primitive_run_artifact(&profile, &[&encoded])
                .is_err()
        );
    }

    #[tokio::test]
    async fn protected_run_binder_requires_one_canonical_run_and_workload() {
        let mut profile =
            QualificationProfile::new("protected-binder-contract".into(), 1, 8, 1, 5_000).unwrap();
        profile.maximum_peak_rss_bytes = 1_000;
        profile.maximum_local_disk_bytes = 2_000;
        profile.maximum_file_descriptors = 3_000;
        profile.maximum_bucket_calls = 4_000;
        profile.provider = "rustfs".into();
        let workload = QualificationWorkload::generate_with_size(&profile, 29, 1, 8, 1).unwrap();
        let mut executor = ContractExecutor {
            calls: 0,
            case_coverage: false,
        };
        let summary = workload.run(&mut executor).await.unwrap();
        let resources = [
            QualificationMetric::new("peak_rss_bytes".into(), 19, "bytes".into()).unwrap(),
            QualificationMetric::new("peak_local_disk_bytes".into(), 29, "bytes".into()).unwrap(),
            QualificationMetric::new("peak_file_descriptors".into(), 39, "count".into()).unwrap(),
            QualificationMetric::new("bucket_calls".into(), 49, "count".into()).unwrap(),
        ];
        let mut run = summary
            .artifact_with_resource_metrics(&workload, &resources)
            .unwrap();
        run.elapsed_ms = 1_000;
        for (name, value) in [("duration_secs", 1), ("throughput_ops_per_sec", 8)] {
            run.metrics
                .iter_mut()
                .find(|metric| metric.name() == name)
                .unwrap()
                .value = value;
        }
        run.verify_for_profile(&profile).unwrap();
        let run_bytes = run.encode().unwrap();
        let workload_bytes = workload.encode().unwrap();
        let provider_evidence =
            QualificationProviderEvidence::new(&profile, workload.seed(), true, true, true)
                .unwrap()
                .encode()
                .unwrap();
        let key = SigningKey::from_bytes(&[96; 32]);
        let runner = QualificationRunner::new(key.clone());
        let evidence = QualificationExecutionEvidence {
            provider: "rustfs".into(),
            workload: "primitives".into(),
            fault: "none".into(),
            toolchain: "rustc".into(),
            execution_profile: "release".into(),
            topology: "three-process".into(),
            started_at_ms: 1,
            finished_at_ms: 1_001,
            fault_schedule: b"none".to_vec(),
            ownership: vec![QualificationOwnership::new(
                1,
                1,
                Digest::from_bytes([98; 32]),
            )],
            dirty: false,
        };
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    evidence.clone(),
                    &run,
                    &[&run_bytes, &workload_bytes],
                )
                .is_err()
        );
        let partial_provider_evidence =
            QualificationProviderEvidence::new(&profile, workload.seed(), true, true, false)
                .unwrap()
                .encode()
                .unwrap();
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    evidence.clone(),
                    &run,
                    &[&run_bytes, &workload_bytes, &partial_provider_evidence],
                )
                .is_err()
        );
        let malformed_provider_evidence = br#"{"schema_version":1,"provider":"rustfs","profile":"protected-binder-contract","conditional":true}"#;
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    evidence.clone(),
                    &run,
                    &[
                        &run_bytes,
                        &workload_bytes,
                        &provider_evidence,
                        malformed_provider_evidence
                    ],
                )
                .is_err()
        );
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    evidence.clone(),
                    &run,
                    &[
                        &run_bytes,
                        &workload_bytes,
                        &provider_evidence,
                        &provider_evidence,
                    ],
                )
                .is_err()
        );
        let receipt = runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                evidence.clone(),
                &run,
                &[&run_bytes, &workload_bytes, &provider_evidence],
            )
            .unwrap();
        receipt
            .verify_for_profile_with_signer(
                "binder-source",
                Digest::from_bytes([97; 32]),
                &profile,
                &[&run_bytes, &workload_bytes, &provider_evidence],
                key.verifying_key().to_bytes(),
            )
            .unwrap();
        receipt
            .verify_primitive_workload(&profile, &[&run_bytes, &workload_bytes, &provider_evidence])
            .unwrap();
        receipt
            .verify_primitive_run_artifact(
                &profile,
                &[&run_bytes, &workload_bytes, &provider_evidence],
            )
            .unwrap();
        let mut short_evidence = evidence.clone();
        short_evidence.finished_at_ms = 2;
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    short_evidence,
                    &run,
                    &[&run_bytes, &workload_bytes, &provider_evidence],
                )
                .is_err()
        );
        let mut dirty_evidence = evidence.clone();
        dirty_evidence.dirty = true;
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    dirty_evidence,
                    &run,
                    &[&run_bytes, &workload_bytes, &provider_evidence],
                )
                .is_err()
        );

        let mismatched_workload =
            QualificationWorkload::generate_with_size(&profile, 29, 2, 8, 1).unwrap();
        let mismatched_workload_bytes = mismatched_workload.encode().unwrap();
        assert!(
            runner
                .emit_protected_run(
                    &profile,
                    "binder-source".into(),
                    Digest::from_bytes([97; 32]),
                    evidence,
                    &run,
                    &[&run_bytes, &mismatched_workload_bytes, &provider_evidence],
                )
                .is_err()
        );
    }

    #[test]
    fn protected_run_artifacts_require_complete_lifecycle_case_coverage() {
        let mut profile =
            QualificationProfile::new("protected-case-coverage".into(), 1, 8, 1, 1_000).unwrap();
        profile.provider = "rustfs".into();
        profile.topology = "three-process".into();
        let workload = QualificationWorkload::generate_with_size(&profile, 19, 1, 56, 1).unwrap();
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
            case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
            outcome_digest: *qualification_run_outcome_digest(
                &workload,
                &workload.primitives,
                &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
            )
            .unwrap()
            .as_bytes(),
            metrics: vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 56, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                QualificationMetric::new("throughput_ops_per_sec".into(), 56, "ops/s".into())
                    .unwrap(),
                QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
            ],
        };
        artifact.encode().unwrap();
        assert!(artifact.verify_for_profile(&profile).is_err());
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
            case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
            outcome_digest: *qualification_run_outcome_digest(
                &workload,
                &workload.primitives,
                &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
            )
            .unwrap()
            .as_bytes(),
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
        below_threshold.metrics[1].value = 1;
        below_threshold.metrics[2].value = 0;
        assert!(below_threshold.verify_profile_thresholds(&profile).is_err());
    }

    #[test]
    fn protected_profiles_require_measured_nonlocal_environment() {
        let profile = QualificationProfile::scale();
        assert!(profile.requires_protected_evidence());
        let renamed_profile =
            QualificationProfile::new("pr-contract-v1".into(), 2, 1, 1, 5_000).unwrap();
        assert!(renamed_profile.requires_protected_evidence());
        let image = Digest::from_bytes([31; 32]);
        let key = SigningKey::from_bytes(&[32; 32]);
        let artifact = b"scale-evidence";
        let artifact_digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
        let metrics = vec![
            QualificationMetric::new("cells".into(), 10_000, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 10_000_000, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 3_600, "seconds".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 500, "ms".into()).unwrap(),
            QualificationMetric::new("peak_local_disk_bytes".into(), 1, "bytes".into()).unwrap(),
            QualificationMetric::new("peak_file_descriptors".into(), 1, "count".into()).unwrap(),
        ];
        let local = QualificationRunner::new(key.clone())
            .emit_with_profile_and_evidence(
                &profile,
                "source".into(),
                image,
                "rustfs".into(),
                "storage".into(),
                "none".into(),
                metrics.clone(),
                artifact,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "local".into(),
                    7,
                    1,
                    1,
                    false,
                ),
                1,
                3_600_001,
                b"none",
                vec![artifact_digest],
                Vec::new(),
            )
            .unwrap();
        assert!(
            local
                .verify_for_profile_with_signer(
                    "source",
                    image,
                    &profile,
                    &[artifact],
                    key.verifying_key().to_bytes(),
                )
                .is_err()
        );

        let protected = QualificationRunner::new(key.clone())
            .emit_with_profile_and_evidence(
                &profile,
                "source".into(),
                image,
                "rustfs".into(),
                "storage".into(),
                "none".into(),
                metrics,
                artifact,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "dedicated-hosts".into(),
                    7,
                    1,
                    1,
                    false,
                ),
                1,
                3_600_001,
                b"none",
                vec![artifact_digest],
                vec![QualificationOwnership::new(
                    2,
                    3,
                    Digest::from_bytes([33; 32]),
                )],
            )
            .unwrap();
        protected
            .verify_for_profile_with_signer(
                "source",
                image,
                &profile,
                &[artifact],
                key.verifying_key().to_bytes(),
            )
            .unwrap();

        let debug_execution = protected
            .clone()
            .with_execution(
                "rustc".into(),
                "debug".into(),
                "dedicated-hosts".into(),
                7,
                1,
                1,
                false,
            )
            .unwrap()
            .attest(&key)
            .unwrap();
        assert!(
            debug_execution
                .verify_for_profile_with_signer(
                    "source",
                    image,
                    &profile,
                    &[artifact],
                    key.verifying_key().to_bytes(),
                )
                .is_err(),
            "protected evidence must come from a release execution profile"
        );

        for metric_name in ["peak_local_disk_bytes", "peak_file_descriptors"] {
            let mut missing_measurement = protected.clone();
            missing_measurement
                .metrics
                .iter_mut()
                .find(|metric| metric.name() == metric_name)
                .expect("resource metric")
                .value = 0;
            assert!(
                missing_measurement
                    .verify_profile_thresholds(&profile)
                    .is_err(),
                "zero {metric_name} must not stand in for a protected measurement"
            );
        }
    }

    #[test]
    fn fault_profiles_require_injected_schedule_and_monotonic_ownership() {
        let profile = QualificationProfile::fault_s3();
        assert!(profile.requires_fault_injection());
        let image = Digest::from_bytes([34; 32]);
        let artifact = b"fault-evidence";
        let artifact_digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
        let metrics = vec![
            QualificationMetric::new("cells".into(), 256, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 1_000_000, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 60, "seconds".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("peak_local_disk_bytes".into(), 1, "bytes".into()).unwrap(),
            QualificationMetric::new("peak_file_descriptors".into(), 1, "count".into()).unwrap(),
        ];
        let key = SigningKey::from_bytes(&[35; 32]);
        let runner = QualificationRunner::new(key.clone());
        let incomplete = runner
            .emit_with_profile_and_evidence(
                &profile,
                "fault-source".into(),
                image,
                "s3".into(),
                "failover".into(),
                "none".into(),
                metrics.clone(),
                artifact,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "kubernetes".into(),
                    7,
                    1,
                    1,
                    false,
                ),
                1,
                60_001,
                b"none",
                vec![artifact_digest],
                vec![QualificationOwnership::new(
                    1,
                    1,
                    Digest::from_bytes([36; 32]),
                )],
            )
            .unwrap();
        assert!(
            incomplete
                .verify_for_profile_with_signer(
                    "fault-source",
                    image,
                    &profile,
                    &[artifact],
                    key.verifying_key().to_bytes(),
                )
                .is_err()
        );

        let complete = runner
            .emit_with_profile_and_evidence(
                &profile,
                "fault-source".into(),
                image,
                "s3".into(),
                "failover".into(),
                "owner-kill".into(),
                metrics,
                artifact,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "kubernetes".into(),
                    7,
                    1,
                    1,
                    false,
                ),
                1,
                60_001,
                b"fault=owner-kill;phase=after-publication",
                vec![artifact_digest],
                vec![
                    QualificationOwnership::new(1, 1, Digest::from_bytes([36; 32])),
                    QualificationOwnership::new(2, 2, Digest::from_bytes([37; 32])),
                ],
            )
            .unwrap();
        complete
            .verify_for_profile_with_signer(
                "fault-source",
                image,
                &profile,
                &[artifact],
                key.verifying_key().to_bytes(),
            )
            .unwrap();

        let no_transition = runner
            .emit_with_profile_and_evidence(
                &profile,
                "fault-source".into(),
                image,
                "s3".into(),
                "failover".into(),
                "owner-kill".into(),
                vec![
                    QualificationMetric::new("cells".into(), 256, "cells".into()).unwrap(),
                    QualificationMetric::new("operations".into(), 1_000_000, "operations".into())
                        .unwrap(),
                    QualificationMetric::new("duration_secs".into(), 60, "seconds".into()).unwrap(),
                    QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                    QualificationMetric::new("peak_local_disk_bytes".into(), 1, "bytes".into())
                        .unwrap(),
                    QualificationMetric::new("peak_file_descriptors".into(), 1, "count".into())
                        .unwrap(),
                ],
                artifact,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "kubernetes".into(),
                    7,
                    1,
                    1,
                    false,
                ),
                1,
                60_001,
                b"fault=owner-kill;phase=after-publication",
                vec![artifact_digest],
                vec![
                    QualificationOwnership::new(1, 1, Digest::from_bytes([36; 32])),
                    QualificationOwnership::new(1, 1, Digest::from_bytes([36; 32])),
                ],
            )
            .unwrap();
        assert!(
            no_transition
                .verify_for_profile_with_signer(
                    "fault-source",
                    image,
                    &profile,
                    &[artifact],
                    key.verifying_key().to_bytes(),
                )
                .is_err(),
            "fault evidence must contain an actual ownership transition"
        );
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
    fn evidence_requires_bounded_fault_schedule_and_nonzero_ownership_roots() {
        let valid = QualificationExecutionEvidence {
            provider: "rustfs".into(),
            workload: "failover".into(),
            fault: "owner-kill".into(),
            toolchain: "rustc".into(),
            execution_profile: "release".into(),
            topology: "three-process".into(),
            started_at_ms: 1,
            finished_at_ms: 2,
            fault_schedule: b"owner-kill".to_vec(),
            ownership: Vec::new(),
            dirty: false,
        };
        assert!(valid.encode().is_ok());

        let mut empty = valid.clone();
        empty.fault_schedule.clear();
        assert!(empty.encode().is_err());

        let mut zero_root = valid.clone();
        zero_root.ownership = vec![QualificationOwnership::new(
            1,
            1,
            Digest::from_bytes([0; 32]),
        )];
        assert!(zero_root.encode().is_err());

        let mut oversized = valid;
        oversized.fault_schedule = vec![0; MAX_RECEIPT_BYTES + 1];
        assert!(oversized.encode().is_err());

        let artifact = b"raw qualification output";
        let digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
        let receipt = QualificationReceipt::new(
            "source".into(),
            Digest::from_bytes([1; 32]),
            "rustfs".into(),
            "failover".into(),
            "owner-kill".into(),
            Vec::new(),
            digest,
            true,
        )
        .expect("receipt identity");
        assert!(
            receipt
                .clone()
                .with_evidence(1, 2, b"", vec![digest], Vec::new())
                .is_err()
        );
        assert!(
            receipt
                .clone()
                .with_evidence(
                    1,
                    2,
                    b"owner-kill",
                    vec![digest],
                    vec![QualificationOwnership::new(
                        1,
                        1,
                        Digest::from_bytes([0; 32]),
                    )]
                )
                .is_err()
        );
        assert!(
            receipt
                .with_evidence(
                    1,
                    2,
                    &vec![0; MAX_RECEIPT_BYTES + 1],
                    vec![digest],
                    Vec::new()
                )
                .is_err()
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

        let mut reordered = manifest.entries().to_vec();
        reordered.swap(0, 1);
        assert!(QualificationMatrixManifest::new(reordered).is_err());

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
                        threshold_metrics(),
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

        let below_threshold = runner
            .emit_with_profile(
                &profile,
                "source".into(),
                image,
                "local".into(),
                "protocol".into(),
                "none".into(),
                vec![
                    QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                    QualificationMetric::new("operations".into(), 1, "operations".into()).unwrap(),
                    QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                    QualificationMetric::new("p99_latency_ms".into(), 5_001, "ms".into()).unwrap(),
                ],
                b"artifact-protocol",
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
            )
            .unwrap();
        let mut below_threshold_evidence = evidence.clone();
        let below_threshold_artifacts = [b"artifact-protocol" as &[u8]];
        below_threshold_evidence[0] = ("protocol", &below_threshold, &below_threshold_artifacts);
        assert!(
            QualificationReceipt::verify_matrix_for_profile(
                "source",
                image,
                &profile,
                &below_threshold_evidence,
            )
            .is_err()
        );

        let bad = runner
            .emit_with_profile(
                &profile,
                "source".into(),
                image,
                "local".into(),
                "primitives".into(),
                "none".into(),
                threshold_metrics(),
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
        let workload = QualificationWorkload::generate_with_size(&profile, 7, 1, 8, 1).unwrap();
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
            case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
            outcome_digest: *qualification_run_outcome_digest(
                &workload,
                &workload.primitives,
                &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
            )
            .unwrap()
            .as_bytes(),
            metrics: vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 8, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                QualificationMetric::new("throughput_ops_per_sec".into(), 8, "ops/s".into())
                    .unwrap(),
                QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
            ],
        }
        .encode()
        .unwrap();
        let threshold_metrics = || {
            vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 8, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                QualificationMetric::new("throughput_ops_per_sec".into(), 8, "ops/s".into())
                    .unwrap(),
                QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
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

        let mut mismatched_receipt = receipts[7].clone();
        mismatched_receipt
            .metrics
            .iter_mut()
            .find(|metric| metric.name() == "p95_latency_ms")
            .unwrap()
            .value = 2;
        let mismatched_receipt = mismatched_receipt.attest(&key).unwrap();
        let mut mismatched_evidence = evidence.clone();
        mismatched_evidence[7] = (
            "primitives",
            &mismatched_receipt,
            artifact_views[7].as_slice(),
        );
        assert!(
            QualificationReceipt::verify_matrix_for_profile_with_signer(
                "source",
                image,
                &profile,
                &mismatched_evidence,
                key.verifying_key().to_bytes(),
            )
            .is_err()
        );

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
