//! MLflow experiment provider.
//!
//! Talks to the MLflow Tracking Server HTTP API (self-hosted or managed).
//! Credentials (`base_url` + bearer token) come from workspace settings
//! stored in the OS keychain.

use std::collections::HashMap;

use reqwest::Client;
use serde::Deserialize;

use super::provider::{ExperimentProvider, ProviderConfig, Run, RunDetail, RunStatus};
use crate::{AgentError, Result};

/// MLflow experiment provider backed by the HTTP Tracking API.
pub struct MlflowProvider {
    base_url: String,
    client: Client,
}

impl MlflowProvider {
    /// Create a new MLflow provider from workspace credentials.
    ///
    /// Validates the base URL format and builds a `reqwest::Client` with
    /// the token set as a Bearer authorization header.
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let base_url = config.base_url.trim_end_matches('/').to_owned();

        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(AgentError::Other(format!(
                "MLflow base_url must start with http:// or https://, got: {base_url}"
            )));
        }

        let mut headers = reqwest::header::HeaderMap::new();
        if !config.token.is_empty() {
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.token))
                .map_err(|e| {
                    AgentError::Other(format!("invalid MLflow token header value: {e}"))
                })?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| AgentError::Other(format!("failed to build HTTP client: {e}")))?;

        Ok(Self { base_url, client })
    }
}

#[async_trait::async_trait]
impl ExperimentProvider for MlflowProvider {
    fn id(&self) -> &'static str {
        "mlflow"
    }

    async fn runs_for_commit(&self, sha: &str) -> Result<Vec<Run>> {
        let url = format!("{}/api/2.0/mlflow/runs/search", self.base_url);
        let filter = format!("tags.mlflow.source.git.commit = '{sha}'");

        let body = SearchRunsRequest {
            filter_string: Some(filter),
            max_results: Some(100),
            order_by: Some(vec!["start_time DESC".to_owned()]),
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("MLflow search request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "MLflow runs/search returned {status}: {text}"
            )));
        }

        let search_resp: SearchRunsResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("MLflow response parse error: {e}")))?;

        let runs = search_resp
            .runs
            .unwrap_or_default()
            .into_iter()
            .map(|r| mlflow_run_to_run(r, &self.base_url))
            .collect();

        Ok(runs)
    }

    async fn runs_recent(&self, limit: usize) -> Result<Vec<Run>> {
        let url = format!("{}/api/2.0/mlflow/runs/search", self.base_url);

        let body = SearchRunsRequest {
            filter_string: None,
            max_results: Some(limit as u32),
            order_by: Some(vec!["start_time DESC".to_owned()]),
        };

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("MLflow search request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "MLflow runs/search returned {status}: {text}"
            )));
        }

        let search_resp: SearchRunsResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("MLflow response parse error: {e}")))?;

        let runs = search_resp
            .runs
            .unwrap_or_default()
            .into_iter()
            .map(|r| mlflow_run_to_run(r, &self.base_url))
            .collect();

        Ok(runs)
    }

    async fn run_detail(&self, run_id: &str) -> Result<RunDetail> {
        // Fetch the run itself.
        let run_url = format!("{}/api/2.0/mlflow/runs/get?run_id={run_id}", self.base_url);
        let run_resp = self
            .client
            .get(&run_url)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("MLflow get-run request failed: {e}")))?;

        if !run_resp.status().is_success() {
            let status = run_resp.status();
            let text = run_resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "MLflow runs/get returned {status}: {text}"
            )));
        }

        let get_run_resp: GetRunResponse = run_resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("MLflow get-run parse error: {e}")))?;

        // Fetch artifacts list.
        let artifacts_url = format!(
            "{}/api/2.0/mlflow/artifacts/list?run_id={run_id}",
            self.base_url
        );
        let artifacts_resp =
            self.client.get(&artifacts_url).send().await.map_err(|e| {
                AgentError::Other(format!("MLflow artifacts/list request failed: {e}"))
            })?;

        let artifacts: Vec<String> = if artifacts_resp.status().is_success() {
            let list: ListArtifactsResponse = artifacts_resp.json().await.unwrap_or_default();
            list.files
                .unwrap_or_default()
                .into_iter()
                .map(|f| f.path)
                .collect()
        } else {
            tracing::warn!(
                run_id,
                status = %artifacts_resp.status(),
                "failed to list artifacts for run, continuing without them"
            );
            Vec::new()
        };

        let mlflow_run = get_run_resp.run;
        let run = mlflow_run_to_run(mlflow_run.clone(), &self.base_url);

        // Extract hyperparams from run params.
        let hyperparams: HashMap<String, String> = mlflow_run
            .data
            .as_ref()
            .and_then(|d| d.params.as_ref())
            .map(|params| {
                params
                    .iter()
                    .map(|p| (p.key.clone(), p.value.clone()))
                    .collect()
            })
            .unwrap_or_default();

        // Compute duration if end_time is available.
        let duration_secs = compute_duration(&mlflow_run);

        Ok(RunDetail {
            run,
            hyperparams,
            artifacts,
            duration_secs,
        })
    }
}

// --- Internal helper types for MLflow API deserialization ---

#[derive(serde::Serialize)]
struct SearchRunsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    filter_string: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_results: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order_by: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct SearchRunsResponse {
    runs: Option<Vec<MlflowRun>>,
}

#[derive(Debug, Deserialize)]
struct GetRunResponse {
    run: MlflowRun,
}

#[derive(Debug, Clone, Deserialize)]
struct MlflowRun {
    info: Option<MlflowRunInfo>,
    data: Option<MlflowRunData>,
}

#[derive(Debug, Clone, Deserialize)]
struct MlflowRunInfo {
    run_id: Option<String>,
    run_name: Option<String>,
    status: Option<String>,
    start_time: Option<u64>,
    end_time: Option<u64>,
    experiment_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MlflowRunData {
    metrics: Option<Vec<MlflowMetric>>,
    params: Option<Vec<MlflowParam>>,
    tags: Option<Vec<MlflowTag>>,
}

#[derive(Debug, Clone, Deserialize)]
struct MlflowMetric {
    key: String,
    value: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct MlflowParam {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MlflowTag {
    key: String,
    value: String,
}

#[derive(Debug, Deserialize, Default)]
struct ListArtifactsResponse {
    files: Option<Vec<ArtifactFile>>,
}

#[derive(Debug, Deserialize)]
struct ArtifactFile {
    path: String,
}

// --- Conversion helpers ---

/// Convert an MLflow API run into our provider-agnostic `Run` type.
fn mlflow_run_to_run(mlflow_run: MlflowRun, base_url: &str) -> Run {
    let info = mlflow_run.info.as_ref();
    let data = mlflow_run.data.as_ref();

    let id = info.and_then(|i| i.run_id.clone()).unwrap_or_default();

    let name = info
        .and_then(|i| i.run_name.clone())
        .unwrap_or_else(|| id.clone());

    let status = info
        .and_then(|i| i.status.as_deref())
        .map(parse_mlflow_status)
        .unwrap_or(RunStatus::Unknown);

    let start_time = info
        .and_then(|i| i.start_time)
        .map(millis_to_iso8601)
        .unwrap_or_default();

    let commit_sha = data.and_then(|d| d.tags.as_ref()).and_then(|tags| {
        tags.iter()
            .find(|t| t.key == "mlflow.source.git.commit")
            .map(|t| t.value.clone())
    });

    if commit_sha.is_none() {
        tracing::warn!(run_id = %id, "MLflow run missing git commit tag");
    }

    let metrics: HashMap<String, f64> = data
        .and_then(|d| d.metrics.as_ref())
        .map(|ms| ms.iter().map(|m| (m.key.clone(), m.value)).collect())
        .unwrap_or_default();

    let experiment_id = info
        .and_then(|i| i.experiment_id.clone())
        .unwrap_or_default();

    let url = format!("{base_url}/#/experiments/{experiment_id}/runs/{id}");

    Run {
        id,
        name,
        commit_sha,
        metrics,
        status,
        start_time,
        url,
    }
}

fn parse_mlflow_status(s: &str) -> RunStatus {
    match s {
        "RUNNING" => RunStatus::Running,
        "FINISHED" => RunStatus::Completed,
        "FAILED" => RunStatus::Failed,
        "KILLED" => RunStatus::Killed,
        _ => RunStatus::Unknown,
    }
}

/// Convert milliseconds-since-epoch to an ISO-8601 string.
fn millis_to_iso8601(ms: u64) -> String {
    use chrono::DateTime;
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    match DateTime::from_timestamp(secs, nanos) {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        None => String::new(),
    }
}

/// Compute wall-clock duration in seconds from start/end timestamps.
fn compute_duration(mlflow_run: &MlflowRun) -> Option<f64> {
    let info = mlflow_run.info.as_ref()?;
    let start = info.start_time?;
    let end = info.end_time?;
    if end > start {
        Some((end - start) as f64 / 1000.0)
    } else {
        None
    }
}
