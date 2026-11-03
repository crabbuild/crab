//! Weights & Biases experiment provider.
//!
//! Talks to the W&B GraphQL API. Credentials (`base_url` + API key)
//! come from workspace settings stored in the OS keychain.

use std::collections::HashMap;

use reqwest::Client;

use super::provider::{ExperimentProvider, ProviderConfig, Run, RunDetail, RunStatus};
use crate::{AgentError, Result};

/// W&B experiment provider backed by the GraphQL API.
pub struct WandbProvider {
    base_url: String,
    entity: String,
    project: String,
    client: Client,
}

impl WandbProvider {
    /// Create a new W&B provider from workspace credentials.
    ///
    /// The `base_url` defaults to `https://api.wandb.ai` when empty.
    /// Entity and project are extracted from the base URL path segments
    /// (format: `https://api.wandb.ai/entity/project`) or from the URL
    /// as-is when only two path segments are present.
    ///
    /// The token is used as a Bearer authorization header (W&B API key).
    pub fn new(config: &ProviderConfig) -> Result<Self> {
        let raw_url = if config.base_url.is_empty() {
            "https://api.wandb.ai".to_owned()
        } else {
            config.base_url.trim_end_matches('/').to_owned()
        };

        if !raw_url.starts_with("http://") && !raw_url.starts_with("https://") {
            return Err(AgentError::Other(format!(
                "W&B base_url must start with http:// or https://, got: {raw_url}"
            )));
        }

        // Parse entity and project from the URL path.
        // Expected format: https://api.wandb.ai/entity/project
        // If no path segments, fall back to empty strings (will error on queries).
        let url = reqwest::Url::parse(&raw_url)
            .map_err(|e| AgentError::Other(format!("invalid W&B base_url: {e}")))?;

        let path_segments: Vec<&str> = url
            .path_segments()
            .map(|segs| segs.filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();

        let (base_url, entity, project) = if path_segments.len() >= 2 {
            // URL contains entity/project in path — strip them for the API base.
            let entity = path_segments[0].to_owned();
            let project = path_segments[1].to_owned();
            let mut base = url.clone();
            base.set_path("");
            (
                base.as_str().trim_end_matches('/').to_owned(),
                entity,
                project,
            )
        } else if path_segments.len() == 1 {
            // Only entity provided — project must come from elsewhere.
            let entity = path_segments[0].to_owned();
            let mut base = url.clone();
            base.set_path("");
            (
                base.as_str().trim_end_matches('/').to_owned(),
                entity,
                String::new(),
            )
        } else {
            // No path segments — use the URL as the API base.
            (raw_url, String::new(), String::new())
        };

        if config.token.is_empty() {
            return Err(AgentError::Other(
                "W&B API key (token) is required".to_owned(),
            ));
        }

        let mut headers = reqwest::header::HeaderMap::new();
        let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.token))
            .map_err(|e| AgentError::Other(format!("invalid W&B token header value: {e}")))?;
        headers.insert(reqwest::header::AUTHORIZATION, value);

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| AgentError::Other(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            base_url,
            entity,
            project,
            client,
        })
    }

    /// Execute a GraphQL query against the W&B API.
    async fn graphql_query(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/graphql", self.base_url);

        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AgentError::Other(format!("W&B GraphQL request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(AgentError::Other(format!(
                "W&B GraphQL returned {status}: {text}"
            )));
        }

        let result: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AgentError::Other(format!("W&B GraphQL response parse error: {e}")))?;

        // Check for GraphQL-level errors.
        if let Some(errors) = result.get("errors") {
            if let Some(arr) = errors.as_array() {
                if !arr.is_empty() {
                    let msg = arr
                        .iter()
                        .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(AgentError::Other(format!("W&B GraphQL errors: {msg}")));
                }
            }
        }

        result.get("data").cloned().ok_or_else(|| {
            AgentError::Other("W&B GraphQL response missing 'data' field".to_owned())
        })
    }
}

#[async_trait::async_trait]
impl ExperimentProvider for WandbProvider {
    fn id(&self) -> &'static str {
        "wandb"
    }

    async fn runs_for_commit(&self, sha: &str) -> Result<Vec<Run>> {
        let query = r#"
            query RunsForCommit($project: String!, $entity: String!, $filters: JSONString, $first: Int) {
                project(name: $project, entityName: $entity) {
                    runs(filters: $filters, first: $first, order: "-createdAt") {
                        edges {
                            node {
                                id
                                name
                                displayName
                                state
                                createdAt
                                summaryMetrics
                                config
                            }
                        }
                    }
                }
            }
        "#;

        // W&B stores git commit in the run config under various keys.
        // Filter using the config JSON path for git commit.
        let filters = serde_json::json!({
            "config.git.commit": sha
        });

        let variables = serde_json::json!({
            "project": self.project,
            "entity": self.entity,
            "filters": filters.to_string(),
            "first": 100,
        });

        let data = self.graphql_query(query, variables).await?;

        let edges = data
            .pointer("/project/runs/edges")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let runs = edges
            .into_iter()
            .filter_map(|edge| {
                let node = edge.get("node")?;
                Some(wandb_node_to_run(
                    node,
                    &self.entity,
                    &self.project,
                    &self.base_url,
                ))
            })
            .collect();

        Ok(runs)
    }

    async fn runs_recent(&self, limit: usize) -> Result<Vec<Run>> {
        let query = r#"
            query RecentRuns($project: String!, $entity: String!, $first: Int) {
                project(name: $project, entityName: $entity) {
                    runs(first: $first, order: "-createdAt") {
                        edges {
                            node {
                                id
                                name
                                displayName
                                state
                                createdAt
                                summaryMetrics
                                config
                            }
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "project": self.project,
            "entity": self.entity,
            "first": limit,
        });

        let data = self.graphql_query(query, variables).await?;

        let edges = data
            .pointer("/project/runs/edges")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let runs = edges
            .into_iter()
            .filter_map(|edge| {
                let node = edge.get("node")?;
                Some(wandb_node_to_run(
                    node,
                    &self.entity,
                    &self.project,
                    &self.base_url,
                ))
            })
            .collect();

        Ok(runs)
    }

    async fn run_detail(&self, run_id: &str) -> Result<RunDetail> {
        let query = r#"
            query RunDetail($project: String!, $entity: String!, $runName: String!) {
                project(name: $project, entityName: $entity) {
                    run(name: $runName) {
                        id
                        name
                        displayName
                        state
                        createdAt
                        heartbeatAt
                        summaryMetrics
                        config
                        systemMetrics
                        files {
                            edges {
                                node {
                                    name
                                    url
                                }
                            }
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "project": self.project,
            "entity": self.entity,
            "runName": run_id,
        });

        let data = self.graphql_query(query, variables).await?;

        let node = data
            .pointer("/project/run")
            .ok_or_else(|| AgentError::Other(format!("W&B run '{run_id}' not found")))?;

        let run = wandb_node_to_run(node, &self.entity, &self.project, &self.base_url);

        // Extract hyperparams from the config JSON.
        let hyperparams = extract_hyperparams(node);

        // Extract artifact file paths.
        let artifacts = node
            .pointer("/files/edges")
            .and_then(|v| v.as_array())
            .map(|edges| {
                edges
                    .iter()
                    .filter_map(|edge| {
                        edge.pointer("/node/name")
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Compute duration from createdAt and heartbeatAt.
        let duration_secs = compute_duration(node);

        Ok(RunDetail {
            run,
            hyperparams,
            artifacts,
            duration_secs,
        })
    }
}

// --- Conversion helpers ---

/// Convert a W&B GraphQL run node into our provider-agnostic `Run` type.
fn wandb_node_to_run(node: &serde_json::Value, entity: &str, project: &str, base_url: &str) -> Run {
    let id = node
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    let name = node
        .get("displayName")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            node.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
        })
        .to_owned();

    let status = node
        .get("state")
        .and_then(|v| v.as_str())
        .map(parse_wandb_status)
        .unwrap_or(RunStatus::Unknown);

    let start_time = node
        .get("createdAt")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    let commit_sha = extract_commit_sha(node);

    if commit_sha.is_none() {
        tracing::warn!(run_id = %id, "W&B run missing git commit in config");
    }

    let metrics = extract_summary_metrics(node);

    // W&B run URL format: https://wandb.ai/{entity}/{project}/runs/{run_id}
    let wandb_ui_base = base_url.replace("api.wandb.ai", "wandb.ai");
    let url = format!("{wandb_ui_base}/{entity}/{project}/runs/{id}");

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

fn parse_wandb_status(s: &str) -> RunStatus {
    match s {
        "running" => RunStatus::Running,
        "finished" => RunStatus::Completed,
        "failed" | "crashed" => RunStatus::Failed,
        "killed" => RunStatus::Killed,
        _ => RunStatus::Unknown,
    }
}

/// Extract the git commit SHA from the run's config JSON.
///
/// W&B stores git metadata in the config under keys like:
/// - `git.commit`
/// - `_wandb.git.commit`
fn extract_commit_sha(node: &serde_json::Value) -> Option<String> {
    let config_str = node.get("config").and_then(|v| v.as_str())?;
    let config: serde_json::Value = serde_json::from_str(config_str).ok()?;

    // Try `git.commit` first.
    if let Some(sha) = config.pointer("/git/commit").and_then(|v| v.as_str()) {
        if !sha.is_empty() {
            return Some(sha.to_owned());
        }
    }

    // Try nested value format: `git.commit.value`.
    if let Some(sha) = config.pointer("/git/commit/value").and_then(|v| v.as_str()) {
        if !sha.is_empty() {
            return Some(sha.to_owned());
        }
    }

    // Try `_wandb` metadata section.
    if let Some(sha) = config
        .pointer("/_wandb/git/commit")
        .and_then(|v| v.as_str())
    {
        if !sha.is_empty() {
            return Some(sha.to_owned());
        }
    }

    None
}

/// Extract summary metrics from the run node.
fn extract_summary_metrics(node: &serde_json::Value) -> HashMap<String, f64> {
    let metrics_str = match node.get("summaryMetrics").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return HashMap::new(),
    };

    let metrics_val: serde_json::Value = match serde_json::from_str(metrics_str) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let obj = match metrics_val.as_object() {
        Some(o) => o,
        None => return HashMap::new(),
    };

    obj.iter()
        .filter_map(|(k, v)| {
            // Skip internal W&B metrics (prefixed with _).
            if k.starts_with('_') {
                return None;
            }
            v.as_f64().map(|f| (k.clone(), f))
        })
        .collect()
}

/// Extract hyperparameters from the run config JSON.
fn extract_hyperparams(node: &serde_json::Value) -> HashMap<String, String> {
    let config_str = match node.get("config").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return HashMap::new(),
    };

    let config: serde_json::Value = match serde_json::from_str(config_str) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let obj = match config.as_object() {
        Some(o) => o,
        None => return HashMap::new(),
    };

    obj.iter()
        .filter_map(|(k, v)| {
            // Skip internal W&B keys and git metadata.
            if k.starts_with('_') || k == "git" {
                return None;
            }
            // W&B config values can be nested objects with a `value` field.
            let val_str = if let Some(inner) = v.get("value") {
                match inner {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            } else {
                match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            };
            Some((k.clone(), val_str))
        })
        .collect()
}

/// Compute wall-clock duration from createdAt and heartbeatAt timestamps.
fn compute_duration(node: &serde_json::Value) -> Option<f64> {
    let created = node.get("createdAt").and_then(|v| v.as_str())?;
    let heartbeat = node.get("heartbeatAt").and_then(|v| v.as_str())?;

    let created_dt = chrono::DateTime::parse_from_rfc3339(created).ok()?;
    let heartbeat_dt = chrono::DateTime::parse_from_rfc3339(heartbeat).ok()?;

    let duration = heartbeat_dt.signed_duration_since(created_dt);
    let secs = duration.num_milliseconds() as f64 / 1000.0;

    if secs > 0.0 { Some(secs) } else { None }
}
