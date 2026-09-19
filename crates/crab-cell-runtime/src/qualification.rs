use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{Digest, Error, Result};

const MAX_LABEL_BYTES: usize = 256;
const MAX_METRICS: usize = 64;
const MAX_RECEIPT_BYTES: usize = 1 << 20;

/// Current wire schema for qualification evidence.
pub const QUALIFICATION_SCHEMA_VERSION: u32 = 3;

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
        if metrics.len() > MAX_METRICS {
            return Err(Error::Control("qualification metric count exceeds limit"));
        }
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
        let signature: [u8; 64] = receipt
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| Error::Control("qualification receipt signature length"))?;
        key.verify(
            &receipt.signing_bytes()?,
            &Signature::from_bytes(&signature),
        )
        .map_err(Error::PeerSignature)?;
        if receipt.encode()? != bytes {
            return Err(Error::Control("qualification receipt is not canonical"));
        }
        Ok(receipt)
    }

    /// Verifies a decoded receipt against the expected release and artifact.
    pub fn verify_for(&self, source_revision: &str, image: Digest, artifact: &[u8]) -> Result<()> {
        if !self.passed {
            return Err(Error::Control("qualification receipt is not passed"));
        }
        validate_label(source_revision, "qualification source revision")?;
        if self.source_revision != source_revision || self.image() != image {
            return Err(Error::Control("qualification release identity"));
        }
        if self.artifact_digest() != Digest::from_bytes(*blake3::hash(artifact).as_bytes()) {
            return Err(Error::Control("qualification artifact digest"));
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
        if self.metrics.len() > MAX_METRICS {
            return Err(Error::Control("qualification metric count exceeds limit"));
        }
        if self.started_at_ms == 0
            || self.finished_at_ms < self.started_at_ms
            || self.fault_schedule_digest.iter().all(|byte| *byte == 0)
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
        for metric in &self.metrics {
            validate_label(&metric.name, "qualification metric name")?;
            validate_label(&metric.unit, "qualification metric unit")?;
        }
        Ok(())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature = vec![0; 64];
        serde_json::to_vec(&unsigned).map_err(Error::from)
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
        let artifact_digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
        let fault_schedule = fault.clone();
        let (toolchain, profile, topology, seed, bucket_calls, peak_rss_bytes, dirty) = execution;
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
            profile,
            topology,
            seed,
            bucket_calls,
            peak_rss_bytes,
            dirty,
        )?
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
        let (toolchain, profile, topology, seed, bucket_calls, peak_rss_bytes, dirty) = execution;
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
            profile,
            topology,
            seed,
            bucket_calls,
            peak_rss_bytes,
            dirty,
        )?
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

#[cfg(test)]
mod tests {
    use super::*;

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
        .attest(&SigningKey::from_bytes(&[9; 32]))
        .unwrap();
        assert_eq!(receipt.schema_version(), QUALIFICATION_SCHEMA_VERSION);
        let decoded = QualificationReceipt::decode(&receipt.encode().unwrap()).unwrap();
        assert_eq!(decoded, receipt);
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
