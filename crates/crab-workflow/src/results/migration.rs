use super::{
    MAX_FIELDS, RESULTS_SCHEMA_VERSION, ResultOrigin, ResultSnapshot, flatten_scalars, invalid,
};
use crate::{EXPERIMENT_METADATA_SCHEMA_VERSION, ExperimentMetadata, Result, WorkflowError};

impl TryFrom<&ExperimentMetadata> for ResultSnapshot {
    type Error = WorkflowError;

    fn try_from(metadata: &ExperimentMetadata) -> Result<Self> {
        if metadata.schema_version != EXPERIMENT_METADATA_SCHEMA_VERSION {
            return Err(invalid(
                "unsupported experiment metadata schema for Results migration",
            ));
        }
        let mut metrics = Vec::new();
        for (path, value) in &metadata.metrics {
            metrics.extend(flatten_scalars(path, value)?);
            // Imports have no retained source files for omitted values. Refuse
            // over-limit documents rather than truncating their recorded fields.
            if metrics.len() > MAX_FIELDS {
                return Err(invalid("imported metrics exceed the scalar field limit"));
            }
        }
        // v1 retained values, not their original files or effective inputs.
        // Neither today's tree nor saved CLI arguments can fill that evidence gap.
        Ok(Self {
            schema_version: RESULTS_SCHEMA_VERSION,
            origin: ResultOrigin::ExperimentMetadata,
            experiment_id: metadata.exp_id.to_string(),
            source_metadata_hash: metadata.content_hash()?,
            base_commit: metadata.base_commit.clone(),
            name: metadata.name.clone(),
            status: metadata.status.clone(),
            started_at: metadata.started_at.clone(),
            ended_at: metadata.ended_at.clone(),
            parameters: Vec::new(),
            metrics,
            stages: Vec::new(),
            attachments: Vec::new(),
            plots: Vec::new(),
            reproduction: None,
            capture_issues: vec![
                "Imported experiment metadata records metrics and reported status. Resolved inputs, stage attempts, plots, and original artifact bytes were not captured.".into(),
            ],
        })
    }
}
