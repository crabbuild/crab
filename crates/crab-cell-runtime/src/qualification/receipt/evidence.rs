//! Provider-semantics evidence bound to a protected primitives run.

use super::*;

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
