//! Captured experiment evidence, comparison semantics, and immutable reports.

mod capture;
mod charts;
mod html;
mod interactions;
mod local;
mod migration;
pub(crate) mod model;
mod plot_matrices;
pub mod plot_specs;
pub mod plot_templates;
pub mod plots;
mod query;
mod reader;
mod remote;
mod stages;

pub use capture::{ResultCapture, runtime_witness};
pub use charts::{Chart, ChartImage, ChartReduction, charts, validate_inline_spec};
pub use html::{CHART_LICENSES, CHART_RUNTIME, report_html};
pub use interactions::ChartInteraction;
pub use local::LocalResults;
pub use model::*;
pub use query::{Column, RunPage, RunQuery, RunSummary, compare, flatten_scalars, query};
pub use reader::*;
pub use remote::RemoteResults;

use serde::{Serialize, de::DeserializeOwned};

use crate::{Result, WorkflowError};

// Share the renderer budget across runs and metrics: nearest-point hit regions
// and matrix imputation otherwise multiply browser work with each added series.
const MAX_CHART_POINTS: usize = 2000;

pub(crate) fn stored(path: impl Into<String>, source: WorkflowError) -> WorkflowError {
    WorkflowError::ResultsStored {
        path: path.into(),
        source: Box::new(source),
    }
}

pub(crate) fn invalid(reason: impl Into<String>) -> WorkflowError {
    WorkflowError::Configuration {
        key: "workflow_results_invalid".into(),
        origin: reason.into(),
    }
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    // Cargo consumers can unify serde_json's preserve_order feature. Explicit
    // sorting keeps stored digests identical across CLI, SDK and server builds.
    let mut value = serde_json::to_value(value).map_err(WorkflowError::ResultsJson)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(WorkflowError::ResultsJson)
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(invalid("result document exceeds the size limit"));
    }
    serde_json::from_slice(bytes).map_err(WorkflowError::ResultsJson)
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(crate) fn check_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("expected a canonical BLAKE3 content digest"));
    }
    Ok(())
}

#[cfg(test)]
mod contracts;
#[cfg(test)]
mod tests;
