//! Qualification profiles, cases, and primitive coverage.

use super::*;

pub(super) const MAX_LABEL_BYTES: usize = 256;
pub(super) const MAX_METRICS: usize = 64;
pub(super) const MAX_RECEIPT_BYTES: usize = 1 << 20;
pub(super) const MAX_QUALIFICATION_CELLS: u64 = 1_000_000;
pub(super) const MAX_QUALIFICATION_OPERATIONS: u64 = 100_000_000;
pub(super) const MAX_QUALIFICATION_DURATION_SECS: u64 = 7 * 24 * 60 * 60;
pub(super) const MAX_QUALIFICATION_CONCURRENCY: usize = 1_024;

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
    pub(super) schema_version: u32,
    pub(super) name: String,
    pub(super) minimum_cells: u64,
    pub(super) minimum_operations: u64,
    pub(super) minimum_duration_secs: u64,
    pub(super) maximum_p99_latency_ms: u64,
    pub(super) minimum_throughput_ops_per_sec: u64,
    pub(super) maximum_peak_rss_bytes: u64,
    pub(super) maximum_local_disk_bytes: u64,
    pub(super) maximum_file_descriptors: u64,
    pub(super) maximum_bucket_calls: u64,
    pub(super) provider: String,
    pub(super) topology: String,
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

    pub(super) fn requires_resource_measurements(&self) -> bool {
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
    pub(super) fn built_in(
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

    pub(super) fn fault_for(name: &'static str, provider: &'static str) -> Self {
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

    pub(super) fn provider_for(name: &'static str, provider: &'static str) -> Self {
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

    pub(super) fn validate(&self) -> Result<()> {
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

pub(super) fn case_coverage_index(operation: QualificationOperation) -> usize {
    operation.primitive_index as usize * QUALIFICATION_CASES.len() + operation.case as usize
}

pub(super) fn mark_case_coverage(coverage: &mut [u8], operation: QualificationOperation) {
    let index = case_coverage_index(operation);
    coverage[index / 8] |= 1 << (index % 8);
}

pub(super) fn has_complete_case_coverage(coverage: &[u8]) -> bool {
    (0..QUALIFICATION_PRIMITIVES.len()).all(|primitive| {
        (0..QUALIFICATION_CASES.len()).all(|case| {
            let index = primitive * QUALIFICATION_CASES.len() + case;
            coverage[index / 8] & (1 << (index % 8)) != 0
        })
    })
}

pub(super) fn validate_label(value: &str, field: &'static str) -> Result<()> {
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

pub(super) fn validate_path(value: &str, field: &'static str) -> Result<()> {
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
