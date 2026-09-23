//! Qualification receipts, provider evidence, and the matrix manifest.

use super::profile::{
    MAX_METRICS, MAX_RECEIPT_BYTES, has_complete_case_coverage, validate_label, validate_path,
};
use super::workload::{qualification_run_outcome_digest, valid_primitive_counts};
use super::*;

/// Schema for a measured, typed execution artifact bound to one workload.
pub const QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION: u32 = 4;

/// Schema for canonical provider-semantics evidence.
pub const QUALIFICATION_PROVIDER_EVIDENCE_SCHEMA_VERSION: u32 = 1;

/// Bounded measured outcome consumed by protected primitive qualification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationRunArtifact {
    pub(super) schema_version: u32,
    pub(super) workload: QualificationWorkload,
    pub(super) profile: String,
    pub(super) profile_digest: [u8; 32],
    pub(super) seed: u64,
    pub(super) cells: u64,
    pub(super) operations: u64,
    pub(super) elapsed_ms: u64,
    pub(super) primitive_counts: Vec<QualificationPrimitiveCounts>,
    pub(super) case_coverage: Vec<u8>,
    pub(super) outcome_digest: [u8; 32],
    pub(super) metrics: Vec<QualificationMetric>,
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
        if profile.requires_lifecycle_case_coverage()
            && self
                .primitive_counts
                .iter()
                .any(|counts| counts.verified != counts.acknowledged)
        {
            return Err(Error::Control(
                "qualification run acknowledged outcomes are not fully verified",
            ));
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

    pub(super) fn profile_digest(&self) -> Digest {
        Digest::from_bytes(self.profile_digest)
    }

    pub(super) fn threshold_metric(&self, name: &str, unit: &str) -> Result<u64> {
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

    pub(super) fn verify_resource_metrics(&self, profile: &QualificationProfile) -> Result<()> {
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

    pub(super) fn validate(&self) -> Result<()> {
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
    pub(super) schema_version: u32,
    pub(super) provider: String,
    pub(super) profile: String,
    pub(super) profile_digest: [u8; 32],
    pub(super) workload_seed: u64,
    pub(super) conditional: bool,
    pub(super) range: bool,
    pub(super) multipart: bool,
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

    pub(super) fn verify_for(
        &self,
        profile: &QualificationProfile,
        workload_seed: u64,
    ) -> Result<()> {
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

    pub(super) fn validate(&self) -> Result<()> {
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

pub(super) fn verify_provider_evidence(
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

pub(super) fn provider_evidence_candidate(
    bytes: &[u8],
) -> Result<Option<QualificationProviderEvidence>> {
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

/// Reproducible evidence record for one canonical Cell qualification run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationReceipt {
    pub(super) schema_version: u32,
    pub(super) source_revision: String,
    pub(super) image: [u8; 32],
    pub(super) provider: String,
    pub(super) workload: String,
    pub(super) fault: String,
    pub(super) metrics: Vec<QualificationMetric>,
    pub(super) artifact_digest: [u8; 32],
    pub(super) passed: bool,
    pub(super) dirty: bool,
    pub(super) toolchain: String,
    pub(super) execution_profile: String,
    pub(super) profile: String,
    pub(super) profile_digest: [u8; 32],
    pub(super) topology: String,
    pub(super) workload_seed: u64,
    pub(super) bucket_calls: u64,
    pub(super) peak_rss_bytes: u64,
    pub(super) started_at_ms: u64,
    pub(super) finished_at_ms: u64,
    pub(super) fault_schedule_digest: [u8; 32],
    pub(super) raw_artifact_digests: Vec<[u8; 32]>,
    pub(super) ownership: Vec<QualificationOwnership>,
    pub(super) signer: [u8; 32],
    pub(super) signature: Vec<u8>,
}

/// One bounded ownership proof observed during a qualification run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationOwnership {
    pub(super) epoch: u64,
    pub(super) published_sequence: u64,
    pub(super) root: [u8; 32],
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

    pub(super) fn validate(&self) -> Result<()> {
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
    pub(super) name: String,
    pub(super) value: u64,
    pub(super) unit: String,
}

/// One receipt/artifact pair in a qualification matrix manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationMatrixEntry {
    pub(super) workload: String,
    pub(super) receipt: String,
    pub(super) artifacts: Vec<String>,
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
    pub(super) schema_version: u32,
    pub(super) entries: Vec<QualificationMatrixEntry>,
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

    pub(super) fn validate_contract(&self) -> Result<()> {
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

pub(super) fn validate_metrics(metrics: &[QualificationMetric]) -> Result<()> {
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

pub(super) fn validate_resource_metric_list(metrics: &[QualificationMetric]) -> Result<()> {
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

pub(super) fn validate_resource_metric_units(metrics: &[QualificationMetric]) -> Result<()> {
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

pub(super) fn validate_run_latency_metrics(metrics: &[QualificationMetric]) -> Result<()> {
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

    pub(super) fn validate_contract(&self) -> Result<()> {
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

    pub(super) fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature = vec![0; 64];
        serde_json::to_vec(&unsigned).map_err(Error::from)
    }

    pub(super) fn verify_signature(&self, key: &VerifyingKey) -> Result<()> {
        let signature: [u8; 64] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| Error::Control("qualification receipt signature length"))?;
        key.verify(&self.signing_bytes()?, &Signature::from_bytes(&signature))
            .map_err(Error::PeerSignature)
    }

    pub(super) fn threshold_metric(&self, name: &str, unit: &str) -> Result<u64> {
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

    pub(super) fn verify_primitive_workload(
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

    pub(super) fn verify_primitive_run_artifact(
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

    pub(super) fn verify_execution_environment(
        &self,
        profile: &QualificationProfile,
    ) -> Result<()> {
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
    pub(super) signing_key: SigningKey,
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
