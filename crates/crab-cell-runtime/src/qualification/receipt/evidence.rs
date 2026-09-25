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

const SCALE_EVIDENCE_SCHEMA_VERSION: u32 = 1;
const SCALE_STATES: [&str; 5] = [
    "empty",
    "sparse",
    "resident",
    "pending-publication",
    "churned",
];
const SCALE_CELL_COUNTS: [u64; 3] = [1_000, 5_000, 10_000];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::qualification) struct ScaleEvidence {
    schema_version: u32,
    profile: String,
    profile_digest: [u8; 32],
    workload_seed: u64,
    cell_samples: Vec<ScaleSample>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScaleSample {
    state: String,
    target_cells: u64,
    before: ScaleSnapshot,
    after: ScaleSnapshot,
    peak: ScaleSnapshot,
}

impl ScaleSample {
    fn charges_open_cells(&self) -> bool {
        self.after
            .admitted_resident_bytes
            .checked_sub(self.before.admitted_resident_bytes)
            .is_some_and(|bytes| {
                bytes >= self.target_cells * crate::fleet::resource::ACTIVE_CELL_NATIVE_BYTES as u64
            })
            && self
                .after
                .admitted_file_descriptors
                .checked_sub(self.before.admitted_file_descriptors)
                .is_some_and(|descriptors| {
                    descriptors
                        >= self.target_cells
                            * crate::fleet::resource::ACTIVE_CELL_FILE_DESCRIPTORS as u64
                })
    }

    fn slope_fits_admission(&self, smaller: &Self) -> bool {
        let Some(extra_cells) = self.target_cells.checked_sub(smaller.target_cells) else {
            return false;
        };
        let increment = |sample: &Self, field: fn(&ScaleSnapshot) -> u64| {
            i128::from(field(&sample.after)) - i128::from(field(&sample.before))
        };
        let slope =
            |field: fn(&ScaleSnapshot) -> u64| increment(self, field) - increment(smaller, field);
        let cache_charge =
            i128::from(extra_cells) * i128::from(crate::cell::worker::ACTIVE_CELL_PAGE_CACHE_BYTES);
        let memory_charge = i128::from(extra_cells)
            * crate::fleet::resource::ACTIVE_CELL_NATIVE_BYTES as i128
            + cache_charge
            + slope(|snapshot| snapshot.retained_bytes);
        // Comparing two sample sizes cancels fixed process overhead, which
        // has its own node reserve and cannot be charged to each Cell.
        slope(|snapshot| snapshot.rss_bytes) <= memory_charge
            && slope(|snapshot| snapshot.allocator_bytes) <= memory_charge
            && slope(|snapshot| snapshot.sqlite_cache_bytes) <= cache_charge
            && slope(|snapshot| snapshot.file_descriptors)
                <= i128::from(extra_cells)
                    * crate::fleet::resource::ACTIVE_CELL_FILE_DESCRIPTORS as i128
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ScaleSnapshot {
    active_cells: u64,
    rss_bytes: u64,
    allocator_bytes: u64,
    threads: u64,
    file_descriptors: u64,
    sqlite_cache_bytes: u64,
    admitted_resident_bytes: u64,
    admitted_file_descriptors: u64,
    retained_bytes: u64,
    local_disk_reserved_bytes: u64,
    local_disk_bytes: u64,
}

impl ScaleSnapshot {
    fn values(&self) -> [u64; 11] {
        [
            self.active_cells,
            self.rss_bytes,
            self.allocator_bytes,
            self.threads,
            self.file_descriptors,
            self.sqlite_cache_bytes,
            self.admitted_resident_bytes,
            self.admitted_file_descriptors,
            self.retained_bytes,
            self.local_disk_reserved_bytes,
            self.local_disk_bytes,
        ]
    }

    fn covers(&self, other: &Self) -> bool {
        self.values()
            .into_iter()
            .zip(other.values())
            .all(|(peak, observed)| peak >= observed)
    }
}

impl ScaleEvidence {
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECEIPT_BYTES {
            return Err(Error::Control("qualification scale evidence exceeds limit"));
        }
        let evidence: Self = serde_json::from_slice(bytes)?;
        if serde_json::to_vec(&evidence).map_err(Error::from)? != bytes {
            return Err(Error::Control(
                "qualification scale evidence is not canonical",
            ));
        }
        Ok(evidence)
    }

    fn verify_for(
        &self,
        profile: &QualificationProfile,
        run: &QualificationRunArtifact,
    ) -> Result<()> {
        if self.schema_version != SCALE_EVIDENCE_SCHEMA_VERSION
            || self.profile != profile.name()
            || self.profile_digest != *profile.digest()?.as_bytes()
            || self.workload_seed != run.workload().seed()
            || run.workload().cells() < 10_000
            || self.cell_samples.len() != SCALE_STATES.len() * SCALE_CELL_COUNTS.len()
        {
            return Err(Error::Control(
                "qualification scale evidence identity or samples",
            ));
        }
        for ((state, count), sample) in SCALE_STATES
            .into_iter()
            .flat_map(|state| {
                SCALE_CELL_COUNTS
                    .into_iter()
                    .map(move |count| (state, count))
            })
            .zip(&self.cell_samples)
        {
            if sample.state != state
                || sample.target_cells != count
                || sample.before.active_cells != 0
                || sample.after.active_cells != count
                || sample.peak.active_cells < count
                || sample.before.rss_bytes == 0
                || sample.before.threads == 0
                || sample.before.file_descriptors == 0
                || sample.after.rss_bytes == 0
                || sample.after.allocator_bytes == 0
                || sample.after.sqlite_cache_bytes == 0
                || sample.after.local_disk_bytes == 0
                || !sample.charges_open_cells()
                || !sample.peak.covers(&sample.before)
                || !sample.peak.covers(&sample.after)
            {
                return Err(Error::Control("qualification scale sample is incomplete"));
            }
        }
        for samples in self.cell_samples.chunks_exact(SCALE_CELL_COUNTS.len()) {
            for pair in samples.windows(2) {
                if !pair[1].slope_fits_admission(&pair[0]) {
                    return Err(Error::Control(
                        "qualification scale slope exceeds admission",
                    ));
                }
            }
        }
        for (name, unit, observed) in [
            (
                "peak_rss_bytes",
                "bytes",
                self.cell_samples
                    .iter()
                    .map(|sample| sample.peak.rss_bytes)
                    .max(),
            ),
            (
                "peak_local_disk_bytes",
                "bytes",
                self.cell_samples
                    .iter()
                    .map(|sample| sample.peak.local_disk_bytes)
                    .max(),
            ),
            (
                "peak_file_descriptors",
                "count",
                self.cell_samples
                    .iter()
                    .map(|sample| sample.peak.file_descriptors)
                    .max(),
            ),
        ] {
            if run.threshold_metric(name, unit)? < observed.unwrap_or(0) {
                return Err(Error::Control(
                    "qualification scale peaks exceed measured run",
                ));
            }
        }
        Ok(())
    }
}

pub(super) fn verify_scale_evidence(
    profile: &QualificationProfile,
    run: &QualificationRunArtifact,
    artifacts: &[&[u8]],
) -> Result<()> {
    if profile.name() != "scale-v1" {
        return Ok(());
    }
    let mut matches = artifacts.iter().filter_map(|artifact| {
        let value = serde_json::from_slice::<serde_json::Value>(artifact).ok()?;
        value
            .as_object()?
            .contains_key("cell_samples")
            .then_some(*artifact)
    });
    let evidence = matches.next().ok_or(Error::Control(
        "protected scale evidence is missing its Cell samples",
    ))?;
    if matches.next().is_some() {
        return Err(Error::Control(
            "protected scale evidence has multiple sample artifacts",
        ));
    }
    ScaleEvidence::decode(evidence)?.verify_for(profile, run)
}
