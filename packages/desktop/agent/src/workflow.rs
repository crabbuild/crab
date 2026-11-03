//! Workflow engine handlers for the desktop agent.
//!
//! Provides JSON-RPC verbs for listing, reading, saving, running,
//! and cancelling crab workflows from the desktop UI.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{AgentError, Result, SharedStdout, shellout};

// ── Params ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WorkflowListParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowGetParams {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowSaveParams {
    pub repo_id: String,
    pub workflow: WorkflowPayload,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowCreateParams {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowDeleteParams {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRunParams {
    pub repo_id: String,
    pub run_id: String,
    pub path: String,
    #[serde(default)]
    pub stages: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowCancelParams {
    pub repo_id: String,
    pub run_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowValidateParams {
    pub repo_id: String,
    pub path: String,
}

// ── Payload types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPayload {
    pub path: String,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
    #[serde(default)]
    pub plots: Vec<String>,
    #[serde(default)]
    pub defaults: WorkflowDefaultsPayload,
    pub stages: Vec<StagePayload>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowDefaultsPayload {
    #[serde(default)]
    pub env: Option<Value>,
    #[serde(default)]
    pub retry: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagePayload {
    pub name: String,
    pub cmd: CmdPayload,
    #[serde(default)]
    pub deps: Vec<DepPayload>,
    #[serde(default)]
    pub outs: Vec<OutPayload>,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub env: Value,
    #[serde(default)]
    pub retry: Option<Value>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub nondeterministic: Option<bool>,
    #[serde(default)]
    pub hermetic: Option<bool>,
    #[serde(default)]
    pub side_effects: Option<bool>,
    #[serde(default)]
    pub resources: Option<Value>,
    #[serde(default)]
    pub frozen: Option<bool>,
    #[serde(default)]
    pub wdir: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    #[serde(default)]
    pub meta: Option<Value>,
    #[serde(default)]
    pub condition: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdPayload {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepPayload {
    pub kind: String,
    pub value: String,
    #[serde(default)]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutPayload {
    pub path: String,
    pub kind: String,
    #[serde(default)]
    pub cache: Option<bool>,
    #[serde(default)]
    pub persist: Option<bool>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

// ── Handlers ────────────────────────────────────────────────────────

/// List workflow YAML files in the repository root.
pub async fn workflow_list(work_dir: &Path) -> Result<Value> {
    let files = crab_workflow::discover::discover(
        work_dir,
        crab_workflow::discover::DiscoverMode::Recursive,
    )?;
    let files: Vec<_> = files
        .iter()
        .map(|path| {
            path.strip_prefix(work_dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .map_err(|error| AgentError::Other(error.to_string()))
        })
        .collect::<Result<_>>()?;
    Ok(json!({ "files": files }))
}

/// Read and parse a workflow YAML file, returning the structured
/// definition for the visual editor.
pub async fn workflow_get(work_dir: &Path, p: WorkflowGetParams) -> Result<Value> {
    let file_path = work_dir.join(&p.path);
    let content = tokio::fs::read_to_string(&file_path)
        .await
        .map_err(|e| AgentError::Other(format!("failed to read {}: {}", p.path, e)))?;

    // Parse YAML into a generic Value first, then transform to our
    // payload format. This avoids depending on the crab crate
    // directly (the agent is a separate binary).
    let yaml: Value = serde_yaml::from_str(&content)
        .map_err(|e| AgentError::Other(format!("YAML parse error in {}: {}", p.path, e)))?;

    let workflow = yaml_to_payload(&p.path, &yaml)?;
    Ok(serde_json::to_value(&workflow).unwrap_or(json!(null)))
}

/// Save a workflow definition back to YAML on disk.
pub async fn workflow_save(work_dir: &Path, p: WorkflowSaveParams) -> Result<Value> {
    let file_path = work_dir.join(&p.workflow.path);
    let yaml_str = payload_to_yaml(&p.workflow)?;

    // Ensure parent directory exists.
    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::Other(format!("mkdir failed: {}", e)))?;
    }

    tokio::fs::write(&file_path, yaml_str.as_bytes())
        .await
        .map_err(|e| AgentError::Other(format!("write failed: {}", e)))?;

    Ok(json!({ "saved": true }))
}

/// Create a new empty workflow file.
pub async fn workflow_create(work_dir: &Path, p: WorkflowCreateParams) -> Result<Value> {
    let file_path = work_dir.join(&p.path);
    if file_path.exists() {
        return Err(AgentError::Other(format!(
            "file already exists: {}",
            p.path
        )));
    }

    let template = "stages:\n  example:\n    cmd: \"echo hello\"\n";

    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::Other(format!("mkdir failed: {}", e)))?;
    }

    tokio::fs::write(&file_path, template.as_bytes())
        .await
        .map_err(|e| AgentError::Other(format!("write failed: {}", e)))?;

    Ok(json!({ "created": true }))
}

/// Delete a workflow file from disk.
pub async fn workflow_delete(work_dir: &Path, p: WorkflowDeleteParams) -> Result<Value> {
    let file_path = work_dir.join(&p.path);
    if !file_path.exists() {
        return Err(AgentError::Other(format!("file not found: {}", p.path)));
    }

    tokio::fs::remove_file(&file_path)
        .await
        .map_err(|e| AgentError::Other(format!("delete failed: {}", e)))?;

    Ok(json!({ "deleted": true }))
}

/// Run the selected workflow stages using the CLI's native v2 event stream.
pub async fn workflow_run(
    work_dir: &Path,
    p: WorkflowRunParams,
    out: SharedStdout,
) -> Result<Value> {
    let stage_names = match p.stages {
        Some(stages) if !stages.is_empty() => stages,
        _ => read_stage_names(work_dir, &p.path).await?,
    };
    if stage_names.is_empty() {
        return Err(AgentError::Other("workflow has no stages".into()));
    }
    // Recursive discovery and path-qualified targets preserve the selected file's
    // ownership; dropping the path can execute unrelated root workflow stages.
    let mut args = vec!["run".into(), "--jsonl".into(), "--recursive".into()];
    args.extend(
        stage_names
            .iter()
            .map(|stage| format!("{}:{stage}", p.path)),
    );
    let summary = shellout::run_structured(
        "workflow_run",
        &p.run_id,
        work_dir,
        &args,
        &[],
        "workflow/event",
        out,
    )
    .await?;
    Ok(json!({ "run_id": p.run_id, "stage_names": stage_names, "summary": summary }))
}

/// Validate a workflow file without running it.
pub async fn workflow_validate(work_dir: &Path, p: WorkflowValidateParams) -> Result<Value> {
    let file_path = work_dir.join(&p.path);
    let content = tokio::fs::read_to_string(&file_path)
        .await
        .map_err(|e| AgentError::Other(format!("failed to read {}: {}", p.path, e)))?;

    // Try parsing — collect errors.
    let mut errors: Vec<String> = Vec::new();

    let yaml_result: std::result::Result<Value, _> = serde_yaml::from_str(&content);
    match yaml_result {
        Err(e) => {
            errors.push(format!("YAML syntax error: {}", e));
        }
        Ok(ref yaml) => {
            // Validate structure.
            if !yaml.is_object() {
                errors.push("workflow must be a YAML mapping".to_string());
            } else if yaml.get("stages").is_none() {
                errors.push("workflow must have a 'stages' key".to_string());
            } else if let Some(stages) = yaml.get("stages") {
                if !stages.is_object() {
                    errors.push("'stages' must be a mapping".to_string());
                } else if let Some(map) = stages.as_object() {
                    for (name, stage) in map {
                        if !stage.is_object() {
                            errors.push(format!("stage '{}' must be a mapping", name));
                            continue;
                        }
                        if stage.get("cmd").is_none() {
                            errors.push(format!("stage '{}' is missing 'cmd'", name));
                        }
                    }
                }
            }
        }
    }

    Ok(json!({
        "valid": errors.is_empty(),
        "errors": errors,
    }))
}

// ── YAML ↔ Payload conversion ───────────────────────────────────────

/// Convert a parsed YAML Value into our WorkflowPayload structure.
fn yaml_to_payload(file_path: &str, yaml: &Value) -> Result<WorkflowPayload> {
    let stages_map = yaml
        .get("stages")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AgentError::Other("missing or invalid 'stages' key".to_string()))?;

    let mut stages = Vec::new();
    for (name, stage_val) in stages_map {
        let stage = parse_stage_yaml(name, stage_val)?;
        stages.push(stage);
    }

    let params = yaml
        .get("params")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let metrics = yaml
        .get("metrics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let plots = yaml
        .get("plots")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let defaults = yaml
        .get("defaults")
        .map(|d| WorkflowDefaultsPayload {
            env: d.get("env").cloned(),
            retry: d.get("retry").cloned(),
        })
        .unwrap_or_default();

    Ok(WorkflowPayload {
        path: file_path.to_string(),
        params,
        metrics,
        plots,
        defaults,
        stages,
    })
}

fn parse_stage_yaml(name: &str, val: &Value) -> Result<StagePayload> {
    let cmd = match val.get("cmd") {
        Some(Value::String(s)) => CmdPayload {
            kind: "shell".to_string(),
            value: s.clone(),
        },
        Some(Value::Object(obj)) if obj.contains_key("argv") => CmdPayload {
            kind: "argv".to_string(),
            value: serde_json::to_string(&obj["argv"])?,
        },
        Some(Value::Array(commands)) => CmdPayload {
            kind: "shell_list".to_string(),
            value: serde_json::to_string(commands)?,
        },
        _ => {
            return Err(AgentError::Other(format!(
                "stage '{}': missing or invalid 'cmd'",
                name
            )));
        }
    };

    let deps = val
        .get("deps")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(parse_dep_yaml).collect())
        .unwrap_or_default();

    let outs = val
        .get("outs")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(parse_out_yaml).collect())
        .unwrap_or_default();

    let params = val
        .get("params")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let env = val
        .get("env")
        .cloned()
        .unwrap_or(Value::String("inherit".to_string()));

    Ok(StagePayload {
        name: name.to_string(),
        cmd,
        deps,
        outs,
        params,
        env,
        retry: val.get("retry").cloned(),
        timeout: val
            .get("timeout")
            .and_then(|v| v.as_str())
            .map(String::from),
        nondeterministic: val.get("nondeterministic").and_then(|v| v.as_bool()),
        hermetic: val.get("hermetic").and_then(|v| v.as_bool()),
        side_effects: val.get("side_effects").and_then(|v| v.as_bool()),
        resources: val.get("resources").cloned(),
        frozen: val.get("frozen").and_then(|v| v.as_bool()),
        wdir: val.get("wdir").and_then(|v| v.as_str()).map(String::from),
        desc: val.get("desc").and_then(|v| v.as_str()).map(String::from),
        meta: val.get("meta").cloned(),
        condition: val.get("condition").cloned(),
    })
}

fn parse_dep_yaml(val: &Value) -> Option<DepPayload> {
    match val {
        Value::String(s) => Some(DepPayload {
            kind: "path".to_string(),
            value: s.clone(),
            meta: None,
        }),
        Value::Object(obj) => {
            if let Some(stage_out) = obj.get("stage_out") {
                let stage = stage_out
                    .get("stage")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let out = stage_out.get("out").and_then(|v| v.as_str()).unwrap_or("");
                Some(DepPayload {
                    kind: "stage_out".to_string(),
                    value: format!("{}:{}", stage, out),
                    meta: None,
                })
            } else if let Some(path) = obj.get("path") {
                Some(DepPayload {
                    kind: "path".to_string(),
                    value: path.as_str().unwrap_or("").to_string(),
                    meta: None,
                })
            } else {
                // Other dep kinds (crab, git, url, oci).
                let kind = if obj.contains_key("crab") {
                    "crab"
                } else if obj.contains_key("git") {
                    "git"
                } else if obj.contains_key("url") {
                    "url"
                } else if obj.contains_key("oci") {
                    "oci"
                } else {
                    "path"
                };
                Some(DepPayload {
                    kind: kind.to_string(),
                    value: serde_json::to_string(obj).unwrap_or_default(),
                    meta: Some(Value::Object(obj.clone())),
                })
            }
        }
        _ => None,
    }
}

fn parse_out_yaml(val: &Value) -> Option<OutPayload> {
    match val {
        Value::String(s) => Some(OutPayload {
            path: s.clone(),
            kind: "file".to_string(),
            cache: None,
            persist: None,
            max_bytes: None,
        }),
        Value::Object(obj) => {
            let path = obj
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let kind = obj
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("file")
                .to_string();
            Some(OutPayload {
                path,
                kind,
                cache: obj.get("cache").and_then(|v| v.as_bool()),
                persist: obj.get("persist").and_then(|v| v.as_bool()),
                max_bytes: obj.get("max_bytes").and_then(|v| v.as_u64()),
            })
        }
        _ => None,
    }
}

/// Convert a WorkflowPayload back to YAML string for saving.
fn payload_to_yaml(workflow: &WorkflowPayload) -> Result<String> {
    use std::fmt::Write;

    let mut out = String::new();

    // Top-level fields.
    if !workflow.params.is_empty() {
        writeln!(out, "params:").ok();
        for p in &workflow.params {
            writeln!(out, "  - {}", p).ok();
        }
    }
    if !workflow.metrics.is_empty() {
        writeln!(out, "metrics:").ok();
        for m in &workflow.metrics {
            writeln!(out, "  - {}", m).ok();
        }
    }
    if !workflow.plots.is_empty() {
        writeln!(out, "plots:").ok();
        for p in &workflow.plots {
            writeln!(out, "  - {}", p).ok();
        }
    }

    // Stages.
    writeln!(out, "stages:").ok();
    for stage in &workflow.stages {
        writeln!(out, "  {}:", stage.name).ok();

        // Command.
        match stage.cmd.kind.as_str() {
            "argv" | "shell_list" => {
                // The editor uses JSON arrays so quoted arguments and separate
                // shell commands survive load/save without changing execution.
                let parts: Vec<String> = serde_json::from_str(&stage.cmd.value)?;
                let command = if stage.cmd.kind == "argv" {
                    json!({ "argv": parts })
                } else {
                    json!(parts)
                };
                writeln!(out, "    cmd: {}", serde_json::to_string(&command)?).ok();
            }
            "shell" => {
                writeln!(out, "    cmd: {}", serde_json::to_string(&stage.cmd.value)?).ok();
            }
            kind => {
                return Err(AgentError::Other(format!(
                    "unsupported command kind: {kind}"
                )));
            }
        }

        // Deps.
        if !stage.deps.is_empty() {
            writeln!(out, "    deps:").ok();
            for dep in &stage.deps {
                match dep.kind.as_str() {
                    "path" => {
                        writeln!(out, "      - {}", dep.value).ok();
                    }
                    "stage_out" => {
                        let parts: Vec<&str> = dep.value.splitn(2, ':').collect();
                        if parts.len() == 2 {
                            writeln!(out, "      - stage_out:").ok();
                            writeln!(out, "          stage: \"{}\"", parts[0]).ok();
                            writeln!(out, "          out: \"{}\"", parts[1]).ok();
                        } else {
                            writeln!(out, "      - {}", dep.value).ok();
                        }
                    }
                    _ => {
                        writeln!(out, "      - {}", dep.value).ok();
                    }
                }
            }
        }

        // Outs.
        if !stage.outs.is_empty() {
            writeln!(out, "    outs:").ok();
            for o in &stage.outs {
                let has_extras = o.cache.is_some()
                    || o.persist.is_some()
                    || o.max_bytes.is_some()
                    || o.kind != "file";
                if has_extras {
                    writeln!(out, "      - path: {}", o.path).ok();
                    if o.kind != "file" {
                        writeln!(out, "        kind: {}", o.kind).ok();
                    }
                    if let Some(cache) = o.cache {
                        writeln!(out, "        cache: {}", cache).ok();
                    }
                    if let Some(persist) = o.persist {
                        writeln!(out, "        persist: {}", persist).ok();
                    }
                    if let Some(max_bytes) = o.max_bytes {
                        writeln!(out, "        max_bytes: {}", max_bytes).ok();
                    }
                } else {
                    writeln!(out, "      - {}", o.path).ok();
                }
            }
        }

        // Params.
        if !stage.params.is_empty() {
            writeln!(out, "    params:").ok();
            for p in &stage.params {
                writeln!(out, "      - {}", p).ok();
            }
        }

        // Options.
        if let Some(true) = stage.nondeterministic {
            writeln!(out, "    nondeterministic: true").ok();
        }
        if let Some(true) = stage.hermetic {
            writeln!(out, "    hermetic: true").ok();
        }
        if let Some(true) = stage.side_effects {
            writeln!(out, "    side_effects: true").ok();
        }
        if let Some(ref timeout) = stage.timeout {
            writeln!(out, "    timeout: \"{}\"", timeout).ok();
        }

        // Frozen.
        if let Some(true) = stage.frozen {
            writeln!(out, "    frozen: true").ok();
        }

        // Working directory.
        if let Some(ref wdir) = stage.wdir {
            if !wdir.is_empty() {
                writeln!(out, "    wdir: \"{}\"", escape_yaml_string(wdir)).ok();
            }
        }

        // Description.
        if let Some(ref desc) = stage.desc {
            if !desc.is_empty() {
                writeln!(out, "    desc: \"{}\"", escape_yaml_string(desc)).ok();
            }
        }

        // Metadata.
        if let Some(ref meta) = stage.meta {
            if let Some(obj) = meta.as_object() {
                if !obj.is_empty() {
                    writeln!(out, "    meta:").ok();
                    for (k, v) in obj {
                        let val_str = match v {
                            Value::String(s) => format!("\"{}\"", escape_yaml_string(s)),
                            _ => v.to_string(),
                        };
                        writeln!(out, "      {}: {}", k, val_str).ok();
                    }
                }
            }
        }

        // Condition.
        if let Some(ref condition) = stage.condition {
            if let Some(obj) = condition.as_object() {
                if let (Some(kind), Some(value)) = (
                    obj.get("kind").and_then(|v| v.as_str()),
                    obj.get("value").and_then(|v| v.as_str()),
                ) {
                    writeln!(out, "    condition:").ok();
                    writeln!(out, "      {}: \"{}\"", kind, escape_yaml_string(value)).ok();
                }
            }
        }
    }

    Ok(out)
}

/// Escape special characters in a YAML string value.
fn escape_yaml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Read stage names from a workflow YAML file without full parsing.
async fn read_stage_names(work_dir: &Path, file_path: &str) -> Result<Vec<String>> {
    let full_path = work_dir.join(file_path);
    let content = tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| AgentError::Other(format!("read failed: {}", e)))?;

    let yaml: Value = serde_yaml::from_str(&content)
        .map_err(|e| AgentError::Other(format!("YAML parse error: {}", e)))?;

    let names = yaml
        .get("stages")
        .and_then(|v| v.as_object())
        .map(|map| map.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    Ok(names)
}

// ── SDK-backed workflow builder handlers ─────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WorkflowStatusParams {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowDagParams {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowResolveTemplateParams {
    pub repo_id: String,
    pub expr: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowTemplateCompletionsParams {
    pub repo_id: String,
    pub prefix: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowExpListParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowExpEnqueueParams {
    pub repo_id: String,
    pub overrides: Vec<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowExpStartParams {
    pub repo_id: String,
    pub jobs: u32,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowExpStopParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowMigratePreviewParams {
    pub repo_id: String,
    pub dvc_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowMigrateApplyParams {
    pub repo_id: String,
    pub output_path: Option<String>,
}

/// Compute workflow state through the same command used by terminal users.
pub async fn workflow_status(work_dir: &Path, p: WorkflowStatusParams) -> Result<Value> {
    let data = shellout::run_json(
        Some(work_dir),
        &["workflow", "status", "--json", &p.path],
        &[],
    )
    .await?;
    let stages = data["stages"]
        .as_array()
        .ok_or_else(|| AgentError::Other("workflow status has no stage list".into()))?;
    let stages: Vec<Value> = stages.iter().map(|stage| {
        let status = match stage["state"].as_str() {
            Some("up_to_date") => "up_to_date",
            Some("never_run") => "not_run",
            _ => "outdated",
        };
        json!({ "name": stage["stage"], "status": status,
            "changed_deps": stage.get("changed_key").and_then(Value::as_str).into_iter().collect::<Vec<_>>(),
            "changed_params": [], "last_run": null, "last_duration_ms": null })
    }).collect();
    let overall = if stages.iter().all(|stage| stage["status"] == "up_to_date") {
        "all_up_to_date"
    } else if stages.iter().any(|stage| stage["status"] == "outdated") {
        "has_outdated"
    } else {
        "has_not_run"
    };
    Ok(json!({ "stages": stages, "overall": overall }))
}

pub async fn workflow_dag(work_dir: &Path, p: WorkflowDagParams) -> Result<Value> {
    let content = tokio::fs::read_to_string(work_dir.join(&p.path)).await?;
    let parsed = crab_workflow::parse_at(&work_dir.join(&p.path), &content)?;
    let graph = crab_workflow::Graph::build(&parsed.stages)?;
    let order = graph.toposort();
    let mut depths: std::collections::BTreeMap<crab_workflow::StageName, usize> =
        std::collections::BTreeMap::new();
    let mut paths: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut edges = Vec::new();
    for name in &order {
        let producers = graph.producers_of(name);
        let depth = producers
            .iter()
            .filter_map(|producer| depths.get(producer))
            .max()
            .map_or(0, |depth| depth + 1);
        depths.insert(name.clone(), depth);
        if groups.len() <= depth {
            groups.resize_with(depth + 1, Vec::new);
        }
        groups[depth].push(name.to_string());
        let mut longest = producers
            .iter()
            .filter_map(|producer| paths.get(&producer.to_string()))
            .max_by_key(|path| path.len())
            .cloned()
            .unwrap_or_default();
        longest.push(name.to_string());
        paths.insert(name.to_string(), longest);
        for producer in producers {
            edges.push(json!({ "from": producer.to_string(), "to": name.to_string(), "kind": "stage_output" }));
        }
    }
    let nodes: Vec<_> = parsed.stages.iter().map(|(name, stage)| json!({
        "name": name.to_string(), "frozen": stage.frozen, "has_condition": stage.condition.is_some(),
        "wdir": stage.wdir, "desc": stage.desc,
    })).collect();
    let critical = paths.into_values().max_by_key(Vec::len).unwrap_or_default();
    Ok(
        json!({ "nodes": nodes, "edges": edges, "topological_order": order.iter().map(ToString::to_string).collect::<Vec<_>>(), "parallelism_groups": groups, "critical_path": critical }),
    )
}

fn template_context(content: &str, work_dir: &Path) -> Result<crab_workflow::TemplateContext> {
    let raw: serde_yaml::Value = serde_yaml::from_str(content)?;
    let sources = match &raw["vars"] {
        serde_yaml::Value::Null => Vec::new(),
        serde_yaml::Value::Sequence(items) => items.clone(),
        value => vec![value.clone()],
    };
    let vars = crab_workflow::TemplateContext::load_vars(&sources, work_dir)?;
    let mut paths = vec![std::path::PathBuf::from("params.yaml")];
    if let Some(params) = raw["params"].as_sequence() {
        for value in params.iter().filter_map(serde_yaml::Value::as_str) {
            let path = std::path::PathBuf::from(value);
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    let params = crab_workflow::TemplateContext::load_params(&paths, work_dir)?;
    Ok(crab_workflow::TemplateContext::new(vars, params, false))
}

pub async fn workflow_resolve_template(
    work_dir: &Path,
    p: WorkflowResolveTemplateParams,
) -> Result<Value> {
    let content = tokio::fs::read_to_string(work_dir.join(&p.path)).await?;
    let context = template_context(&content, work_dir)?;
    let expression = p.expr.trim_start_matches("${").trim_end_matches('}');
    Ok(json!({ "resolved": context.resolve(expression)? }))
}

pub async fn workflow_template_completions(
    work_dir: &Path,
    p: WorkflowTemplateCompletionsParams,
) -> Result<Value> {
    fn collect(value: &serde_yaml::Value, key: &str, source: &str, output: &mut Vec<Value>) {
        if let Some(map) = value.as_mapping() {
            for (name, value) in map {
                if let Some(name) = name.as_str() {
                    let key = if key.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{key}.{name}")
                    };
                    collect(value, &key, source, output);
                }
            }
        } else {
            output.push(json!({ "key": key, "value": value.as_str().map(str::to_owned).unwrap_or_else(|| format!("{value:?}")), "source": source }));
        }
    }
    let content = tokio::fs::read_to_string(work_dir.join(&p.path)).await?;
    let context = template_context(&content, work_dir)?;
    let mut completions = Vec::new();
    collect(context.vars_value(), "", "vars", &mut completions);
    collect(context.params_value(), "", "params", &mut completions);
    completions.retain(|item| {
        item["key"]
            .as_str()
            .is_some_and(|key| key.starts_with(&p.prefix))
    });
    Ok(json!({ "completions": completions }))
}

pub async fn workflow_exp_list(work_dir: &Path) -> Result<Value> {
    let queue = crab_workflow::ExpQueue::new(work_dir.join(".crab/exp-queue"));
    Ok(json!({ "experiments": queue.list_all()? }))
}

pub async fn workflow_exp_enqueue(work_dir: &Path, p: WorkflowExpEnqueueParams) -> Result<Value> {
    let mut ids = Vec::new();
    for overrides in p.overrides {
        let mut args = vec!["exp".to_owned(), "queue".to_owned(), "--json".to_owned()];
        for (key, value) in overrides {
            args.extend(["--set-param".into(), format!("{key}={value}")]);
        }
        let arguments: Vec<_> = args.iter().map(String::as_str).collect();
        let result = shellout::run_json(Some(work_dir), &arguments, &[]).await?;
        let queued = result["experiment_ids"]
            .as_array()
            .ok_or_else(|| AgentError::Other("experiment queue returned no IDs".into()))?;
        ids.extend(queued.iter().cloned());
    }
    Ok(json!({ "queued": ids.len(), "ids": ids }))
}

/// Start experiment workers (shells out to CLI).
pub async fn workflow_exp_start(
    work_dir: &Path,
    p: WorkflowExpStartParams,
    out: SharedStdout,
) -> Result<Value> {
    let job_id = format!(
        "exp-start-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );

    let jobs_str = p.jobs.to_string();
    let args = vec!["exp", "start", "--jobs", &jobs_str];

    let exit_code =
        shellout::run_streaming("exp_start", &job_id, Some(work_dir), &args, out).await?;

    Ok(json!({ "started": exit_code == 0 }))
}

/// Stop experiment workers (shells out to CLI).
pub async fn workflow_exp_stop(work_dir: &Path) -> Result<Value> {
    let crab_bin = shellout::resolve_crab_bin();
    let output = tokio::process::Command::new(&crab_bin)
        .args(["exp", "stop"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    Ok(json!({ "stopped": output.status.success() }))
}

/// Preview DVC migration without writing files.
pub async fn workflow_migrate_preview(
    work_dir: &Path,
    p: WorkflowMigratePreviewParams,
) -> Result<Value> {
    let dvc_path = p
        .dvc_path
        .map(|dp| work_dir.join(dp))
        .unwrap_or_else(|| work_dir.join("dvc.yaml"));

    let content = tokio::fs::read_to_string(&dvc_path)
        .await
        .map_err(|e| AgentError::Other(format!("failed to read dvc.yaml: {e}")))?;

    let (crab_yaml, report) = crab_workflow::convert_dvc_to_crab(&content)?;

    Ok(
        json!({ "crab_yaml": crab_yaml, "stage_count": report.stages_converted, "warnings": report.warnings.iter().map(|warning| format!("{}: {}", warning.stage, warning.message)).collect::<Vec<_>>() }),
    )
}

/// Apply DVC migration — write the converted crab.yaml.
pub async fn workflow_migrate_apply(
    work_dir: &Path,
    p: WorkflowMigrateApplyParams,
) -> Result<Value> {
    let dvc_path = work_dir.join("dvc.yaml");
    let content = tokio::fs::read_to_string(&dvc_path)
        .await
        .map_err(|e| AgentError::Other(format!("failed to read dvc.yaml: {e}")))?;

    let (crab_yaml, _) = crab_workflow::convert_dvc_to_crab(&content)?;

    let output_path = p
        .output_path
        .map(|op| work_dir.join(op))
        .unwrap_or_else(|| work_dir.join("crab.yaml"));

    tokio::fs::write(&output_path, crab_yaml.as_bytes())
        .await
        .map_err(|e| AgentError::Other(format!("failed to write: {e}")))?;

    Ok(json!({
        "written": true,
        "path": output_path.to_string_lossy(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_forms_preserve_argument_boundaries() {
        for command in [
            json!("echo first\necho second"),
            json!({ "argv": ["node", "-e", "console.log('hello world')", "quoted \"value\""] }),
            json!(["echo first", "echo second"]),
        ] {
            let yaml = json!({ "stages": { "report": { "cmd": command } } });
            let payload = yaml_to_payload("crab.yaml", &yaml).unwrap();
            let saved = payload_to_yaml(&payload).unwrap();
            let restored: Value = serde_yaml::from_str(&saved).unwrap();
            assert_eq!(restored["stages"]["report"]["cmd"], command);
        }
    }
}
