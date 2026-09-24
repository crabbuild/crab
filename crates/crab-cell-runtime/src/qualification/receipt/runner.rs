//! Deterministic receipt emitter used by the qualification harnesses.

use super::*;

/// Deterministic receipt emitter used by local and protected qualification
/// harnesses. The harness owns workload/fault execution; this type only binds
/// its measured outputs to exact source, image and artifact bytes.
pub struct QualificationRunner {
    pub(in crate::qualification) signing_key: SigningKey,
}

impl QualificationRunner {
    /// Creates a runner that signs receipts with the given attestation key.
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

    /// Builds, signs, and returns one receipt for a verified run artifact.
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
