use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{check_digest, digest, encode, invalid};
use crate::{ExperimentId, PlotConfig, Result};

pub const RESULTS_SCHEMA_VERSION: u16 = 1;
pub const RESULTS_RENDERER: &str = "vega@5.33.1/vega-lite@5.23.0/vega-interpreter@1.2.1";
pub const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_REPRODUCTION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_REPRODUCTION_FILES: usize = 1024;
pub const MAX_FIELDS: usize = 4096;
pub const MAX_ATTACHMENTS: usize = 4096;
pub const MAX_CATALOG_RUNS: usize = 10_000;
pub const MAX_PAGE_ROWS: usize = 200;
pub const MAX_COMPARISON_RUNS: usize = 20;
pub const MAX_COLUMNS: usize = 32;
pub const MAX_SERIES_POINTS: usize = 2000;
pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Present,
    Missing,
    Invalid,
    NotCaptured,
    Corrupt,
    Denied,
    Unsupported,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Datum {
    pub file: String,
    pub pointer: String,
    pub state: Availability,
    pub value: Value,
    pub reason: Option<String>,
}

impl Datum {
    #[must_use]
    pub fn key(&self) -> String {
        // JSON string framing makes paths containing '#' or ':' unambiguous.
        Value::Array(vec![self.file.clone().into(), self.pointer.clone().into()]).to_string()
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Metric,
    Plot,
    Image,
    Log,
    Workflow,
    Input,
    Output,
    Checkpoint,
    Template,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub path: String,
    pub kind: AttachmentKind,
    pub stage: Option<String>,
    pub hash: Option<String>,
    pub size: u64,
    pub state: Availability,
    pub reason: Option<String>,
    pub push: bool,
    pub remote: Option<String>,
}

impl Attachment {
    /// Returns whether a report retains this attachment's body.
    #[must_use]
    pub fn report_content(&self, include_logs: bool) -> bool {
        match self.kind {
            AttachmentKind::Metric
            | AttachmentKind::Plot
            | AttachmentKind::Image
            | AttachmentKind::Workflow
            | AttachmentKind::Template => true,
            AttachmentKind::Log => include_logs,
            AttachmentKind::Input | AttachmentKind::Output | AttachmentKind::Checkpoint => false,
        }
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputWitness {
    /// Repository dependency key, absent for external or absolute locations.
    pub path: Option<String>,
    pub hash: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputWitness {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageResult {
    pub name: String,
    pub hash: Option<String>,
    pub state: String,
    /// Observed execution duration, absent when this attempt did not record one.
    pub duration_ms: Option<u64>,
    pub attempts: u32,
    pub source: String,
    pub inputs: Vec<InputWitness>,
    pub outputs: Vec<OutputWitness>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeWitness {
    pub program: String,
    pub hash: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReproductionSpec {
    pub source_commit: String,
    /// Exact source result when this run was reproduced.
    pub source_result: Option<super::RunSelection>,
    pub files: Vec<Attachment>,
    /// Closed input trees, including empty directories, restored before files.
    pub directories: Vec<String>,
    pub runtimes: Vec<RuntimeWitness>,
    pub targets: Vec<String>,
    pub recursive: bool,
    pub single_item: bool,
    pub downstream: bool,
    pub pipeline: bool,
    pub all_pipelines: bool,
    pub glob: bool,
    pub unavailable_reasons: Vec<String>,
}

impl ReproductionSpec {
    pub(crate) fn validate_inputs(&self) -> Result<()> {
        validate_attachments(&self.files)?;
        if self.directories.len() > MAX_REPRODUCTION_FILES
            || self
                .directories
                .windows(2)
                .any(|paths| paths[0] >= paths[1])
        {
            return Err(invalid(
                "input directories exceed bounds or are not sorted and unique",
            ));
        }
        for directory in &self.directories {
            validate_path(directory)?;
            if self.files.iter().any(|file| {
                file.state == Availability::Present && Path::new(directory).starts_with(&file.path)
            }) {
                return Err(invalid("an input file cannot contain an input directory"));
            }
        }
        Ok(())
    }
}

/// Distinguishes retained observations from an explicit metadata-only migration.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultOrigin {
    Captured,
    ExperimentMetadata,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultSnapshot {
    pub schema_version: u16,
    pub origin: ResultOrigin,
    pub experiment_id: String,
    pub source_metadata_hash: String,
    pub base_commit: String,
    pub name: Option<String>,
    pub status: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub parameters: Vec<Datum>,
    pub metrics: Vec<Datum>,
    pub stages: Vec<StageResult>,
    pub attachments: Vec<Attachment>,
    pub plots: Vec<PlotConfig>,
    pub reproduction: Option<ReproductionSpec>,
    pub capture_issues: Vec<String>,
}

impl ResultSnapshot {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != RESULTS_SCHEMA_VERSION {
            return Err(invalid("unsupported result schema"));
        }
        self.experiment_id.parse::<ExperimentId>()?;
        check_digest(&self.source_metadata_hash)?;
        if self.base_commit.len() != 40
            || !self
                .base_commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("result base must be a canonical Git commit ID"));
        }
        if !matches!(self.status.as_str(), "success" | "failed" | "saved")
            || self.started_at.len() > 64
            || self.ended_at.as_ref().is_some_and(|time| time.len() > 64)
        {
            return Err(invalid("invalid result status or timestamp"));
        }
        if self.status == "saved"
            && (!self.stages.is_empty()
                || self.reproduction.is_some()
                || self.attachments.iter().any(|attachment| {
                    matches!(
                        attachment.kind,
                        AttachmentKind::Log | AttachmentKind::Checkpoint
                    )
                }))
        {
            return Err(invalid(
                "saved observations cannot claim execution evidence",
            ));
        }
        if self.origin == ResultOrigin::ExperimentMetadata
            && (!self.parameters.is_empty()
                || !self.stages.is_empty()
                || !self.attachments.is_empty()
                || !self.plots.is_empty()
                || self.reproduction.is_some())
        {
            return Err(invalid(
                "imported metadata cannot claim captured execution or workspace evidence",
            ));
        }
        if self.parameters.len() + self.metrics.len() > MAX_FIELDS {
            return Err(invalid("too many scalar fields"));
        }
        for fields in [&self.parameters, &self.metrics] {
            let mut keys = BTreeSet::new();
            for field in fields {
                validate_path(&field.file)?;
                if !field.pointer.is_empty() && !field.pointer.starts_with('/') {
                    return Err(invalid("scalar pointer must be a JSON pointer"));
                }
                if !keys.insert(field.key()) {
                    return Err(invalid("duplicate scalar field"));
                }
                if field.value.is_array() || field.value.is_object() {
                    return Err(invalid("result field must be scalar"));
                }
                if unsafe_integer(&field.value) {
                    return Err(invalid(
                        "result integer exceeds exact JSON client precision",
                    ));
                }
                // Availability carries absence; JSON null remains an observed
                // scalar and must survive Serde round trips without becoming missing.
                if field.state != Availability::Present && !field.value.is_null() {
                    return Err(invalid("unavailable scalar contains a value"));
                }
            }
        }
        validate_attachments(&self.attachments)?;
        if let Some(spec) = &self.reproduction {
            if let Some(source) = &spec.source_result {
                let id = source.id.parse::<ExperimentId>()?;
                if id.to_string() != source.id || source.id == self.experiment_id {
                    return Err(invalid(
                        "reproduction source must name a distinct canonical run",
                    ));
                }
                check_digest(&source.digest)?;
            }
            spec.validate_inputs()?;
            for runtime in &spec.runtimes {
                check_digest(&runtime.hash)?;
            }
            if spec.source_commit != self.base_commit {
                return Err(invalid("reproduction source differs from result base"));
            }
        }
        let witnesses: usize = self
            .stages
            .iter()
            .map(|stage| stage.inputs.len() + stage.outputs.len())
            .sum();
        if self.stages.len() > MAX_ATTACHMENTS || witnesses > MAX_FIELDS {
            return Err(invalid("too many stage input/output witnesses"));
        }
        for stage in &self.stages {
            if stage
                .duration_ms
                .is_some_and(|duration| duration > MAX_SAFE_INTEGER)
            {
                return Err(invalid(
                    "stage duration exceeds exact JSON client precision",
                ));
            }
            if let Some(hash) = &stage.hash {
                check_digest(hash)?;
            }
            for input in &stage.inputs {
                check_digest(&input.hash)?;
                if input
                    .path
                    .as_ref()
                    .is_some_and(|path| path.is_empty() || path.len() > 4096)
                {
                    return Err(invalid("invalid input/output witness location"));
                }
            }
            for output in &stage.outputs {
                check_digest(&output.hash)?;
                if output.path.is_empty()
                    || output.path.len() > 4096
                    || output.size > MAX_SAFE_INTEGER
                {
                    return Err(invalid("invalid output witness location or size"));
                }
            }
        }
        validate_evidence(self.evidence())?;
        Ok(())
    }

    pub fn seal(self) -> Result<SealedResult> {
        self.validate()?;
        let bytes = encode(&self)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid("result snapshot exceeds the size limit"));
        }
        Ok(SealedResult {
            digest: digest(&bytes),
            snapshot: self,
        })
    }

    pub fn evidence(&self) -> impl Iterator<Item = &Attachment> {
        self.attachments
            .iter()
            .chain(self.reproduction.iter().flat_map(|spec| &spec.files))
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedResult {
    pub digest: String,
    pub snapshot: ResultSnapshot,
}

impl SealedResult {
    pub fn verify(&self) -> Result<()> {
        check_digest(&self.digest)?;
        self.snapshot.validate()?;
        if digest(&encode(&self.snapshot)?) != self.digest {
            return Err(invalid("result snapshot digest mismatch"));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Minimize,
    Maximize,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetIdentity {
    pub hash: String,
    pub split: String,
    pub evaluator: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisRange {
    pub lower: f64,
    pub upper: f64,
}

impl AxisRange {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.lower.is_finite()
            || !self.upper.is_finite()
            || self.lower >= self.upper
            || !(self.upper - self.lower).is_finite()
        {
            return Err(invalid("axis range must have finite, increasing bounds"));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComparisonSpec {
    pub baseline: String,
    pub columns: Option<Vec<String>>,
    pub objectives: BTreeMap<String, Direction>,
    pub datasets: BTreeMap<String, DatasetIdentity>,
    pub include_logs: bool,
    pub axes: BTreeMap<String, AxisRange>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonCell {
    pub key: String,
    pub candidate: Option<Datum>,
    pub baseline: Option<Datum>,
    pub absolute_delta: Option<f64>,
    pub relative_delta: Option<f64>,
    pub improvement: Option<bool>,
    pub reason: Option<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComparisonRow {
    pub run: SealedResult,
    pub comparable: bool,
    pub cells: Vec<ComparisonCell>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    pub spec: ComparisonSpec,
    pub rows: Vec<ComparisonRow>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub schema_version: u16,
    pub title: String,
    pub comparison: Comparison,
    pub created_at: String,
    pub renderer: String,
    pub charts: Vec<super::Chart>,
}

impl Report {
    pub fn new(
        title: String,
        comparison: Comparison,
        mut charts: Vec<super::Chart>,
    ) -> Result<Self> {
        super::interactions::apply_axes(&mut charts, &comparison.spec.axes)?;
        let report = Self {
            schema_version: RESULTS_SCHEMA_VERSION,
            title,
            comparison,
            created_at: crab_types::time::now_rfc3339_millis()?,
            renderer: RESULTS_RENDERER.into(),
            charts,
        };
        report.verify()?;
        Ok(report)
    }
    pub fn verify(&self) -> Result<()> {
        if self.title.trim().is_empty() || self.title.len() > 256 || self.created_at.len() > 64 {
            return Err(invalid("invalid report title or creation time"));
        }
        if self.schema_version != RESULTS_SCHEMA_VERSION || self.title.len() > 4096 {
            return Err(invalid("unsupported or oversized report"));
        }
        let runs = self
            .comparison
            .rows
            .iter()
            .map(|r| r.run.clone())
            .collect::<Vec<_>>();
        let expected = super::compare(&runs, &self.comparison.spec)?;
        if expected != self.comparison {
            return Err(invalid("report comparison does not match its evidence"));
        }
        if self.renderer != RESULTS_RENDERER || self.charts.len() > MAX_ATTACHMENTS {
            return Err(invalid("unsupported report renderer or chart count"));
        }
        super::interactions::verify_axes(&self.charts, &self.comparison.spec.axes)?;
        let mut keys = BTreeSet::new();
        for chart in &self.charts {
            if !keys.insert(&chart.key)
                || chart.key.len() > 4096
                || chart.title.len() > 4096
                || chart.displayed_points > chart.points
                || chart.points as u64 > MAX_SAFE_INTEGER
                || (chart.reduction.is_some() && chart.spec.is_none())
                || (chart.spec.is_some()
                    && match chart.reduction {
                        None => chart.displayed_points != chart.points,
                        Some(super::ChartReduction::Sampled) => {
                            chart.displayed_points >= chart.points
                        }
                        Some(super::ChartReduction::Counted) => false,
                    })
            {
                return Err(invalid("invalid frozen report chart"));
            }
            if chart.images.len() > MAX_COMPARISON_RUNS {
                return Err(invalid("too many plot images"));
            }
            for image in &chart.images {
                let retained = runs
                    .iter()
                    .find(|run| {
                        run.digest == image.digest
                            && run.snapshot.experiment_id == image.experiment_id
                    })
                    .and_then(|run| {
                        run.snapshot.attachments.iter().find(|a| {
                            a.path == image.path
                                && a.hash.as_ref() == Some(&image.hash)
                                && a.state == Availability::Present
                        })
                    });
                if retained.is_none() || super::charts::image_mime(Path::new(&image.path)).is_none()
                {
                    return Err(invalid("image does not belong to frozen report evidence"));
                }
            }
            if let Some(spec) = &chart.spec {
                super::validate_inline_spec(spec)?;
            }
        }
        validate_evidence(super::local::report_evidence(self))?;
        Ok(())
    }

    pub fn id(&self) -> Result<String> {
        self.verify()?;
        let bytes = encode(self)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid("report exceeds size limit"));
        }
        Ok(digest(&bytes))
    }
}

pub(crate) fn unsafe_integer(value: &Value) -> bool {
    value
        .as_i64()
        .is_some_and(|value| value.unsigned_abs() > MAX_SAFE_INTEGER)
        || value.as_u64().is_some_and(|value| value > MAX_SAFE_INTEGER)
}

pub(crate) fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains(['\\', '\0', ':'])
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".." | ".git" | ".crab"))
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid(
            "evidence path must be repository-relative without traversal",
        ));
    }
    Ok(())
}

fn validate_attachments(attachments: &[Attachment]) -> Result<()> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(invalid("too many attachments"));
    }
    let mut paths = BTreeSet::new();
    for attachment in attachments {
        validate_path(&attachment.path)?;
        if !paths.insert((&attachment.path, &attachment.stage)) {
            return Err(invalid("duplicate attachment path"));
        }
        if attachment.size > MAX_ATTACHMENT_BYTES as u64 {
            return Err(invalid("attachment exceeds capture limit"));
        }
        if let Some(hash) = &attachment.hash {
            check_digest(hash)?;
        }
        if (attachment.state == Availability::Present) != attachment.hash.is_some() {
            return Err(invalid(
                "attachment availability disagrees with its identity",
            ));
        }
    }
    Ok(())
}

fn validate_evidence<'a>(attachments: impl Iterator<Item = &'a Attachment>) -> Result<()> {
    let mut sizes = BTreeMap::new();
    for attachment in attachments {
        if let Some(hash) = attachment.hash.as_deref()
            && sizes
                .insert(hash, attachment.size)
                .is_some_and(|size| size != attachment.size)
        {
            return Err(invalid(
                "one content digest has conflicting attachment sizes",
            ));
        }
    }
    Ok(())
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportDraft {
    pub title: String,
    pub runs: Vec<super::RunSelection>,
    pub spec: ComparisonSpec,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPromotionRequest {
    pub name: String,
    pub version: String,
    pub stage: String,
    pub expected: Option<String>,
}

/// Local execution facts shown before applying or resuming a run.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionPreview {
    pub base_commit: String,
    pub checkpoint: Option<String>,
    pub applied: Vec<String>,
    pub deleted: Vec<String>,
}
