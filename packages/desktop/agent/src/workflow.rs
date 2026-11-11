//! Workflow engine handlers for the desktop agent.
//!
//! Provides JSON-RPC verbs for listing, reading, saving, running,
//! and cancelling crab workflows from the desktop UI.

use std::{io::Write, path::Path};

use tokio::io::AsyncWriteExt;

use serde::Deserialize;
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

mod document;

use document::{WorkflowPayload, payload_to_yaml, yaml_to_payload};

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

    let yaml: Value = serde_yaml::from_str(&content)
        .map_err(|e| AgentError::Other(format!("YAML parse error in {}: {}", p.path, e)))?;

    let mut workflow = yaml_to_payload(&p.path, &yaml)?;
    workflow.resolve(&file_path, &content)?;
    Ok(serde_json::to_value(&workflow)?)
}

/// Save a workflow definition back to YAML on disk.
pub async fn workflow_save(work_dir: &Path, p: WorkflowSaveParams) -> Result<Value> {
    let file_path = work_dir.join(&p.workflow.path);
    let yaml_str = payload_to_yaml(&p.workflow)?;

    // Validate through the runtime parser before replacing the original. A
    // rejected visual edit must leave the last runnable definition intact.
    crab_workflow::parse_at(&file_path, &yaml_str)?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let parent = file_path
            .parent()
            .ok_or_else(|| AgentError::Other("workflow has no parent directory".into()))?;
        std::fs::create_dir_all(parent)?;
        let permissions = match std::fs::metadata(&file_path) {
            Ok(metadata) => {
                if metadata.permissions().readonly() {
                    return Err(AgentError::Other("workflow is read-only".into()));
                }
                Some(metadata.permissions())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        if let Some(permissions) = permissions {
            temporary.as_file().set_permissions(permissions)?;
        }
        temporary.write_all(yaml_str.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&file_path)
            .map_err(|error| AgentError::Io(error.error))?;
        Ok(())
    })
    .await
    .map_err(std::io::Error::other)??;

    Ok(json!({ "saved": true }))
}

/// Create a new empty workflow file.
pub async fn workflow_create(work_dir: &Path, p: WorkflowCreateParams) -> Result<Value> {
    let file_path = work_dir.join(&p.path);
    let template = "stages:\n  example:\n    cmd: \"echo hello\"\n";

    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::Other(format!("mkdir failed: {}", e)))?;
    }

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file_path)
        .await?;
    file.write_all(template.as_bytes()).await?;
    file.sync_all().await?;

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
    cancel: tokio_util::sync::WaitForCancellationFutureOwned,
) -> Result<Value> {
    let definition = workflow_get(
        work_dir,
        WorkflowGetParams {
            repo_id: p.repo_id,
            path: p.path.clone(),
        },
    )
    .await?;
    let workflow: WorkflowPayload = serde_json::from_value(definition)?;
    let stage_names: Vec<_> = workflow
        .executions
        .iter()
        .filter(|execution| {
            p.stages.as_ref().is_none_or(|selected| {
                selected.is_empty()
                    || selected
                        .iter()
                        .any(|name| name == &execution.source || name == &execution.name)
            })
        })
        .map(|execution| execution.name.clone())
        .collect();
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
        "workflow/event",
        out,
        Some(cancel),
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

    let errors: Vec<String> = crab_workflow::parse_at(&file_path, &content)
        .err()
        .map(|error| vec![error.to_string()])
        .unwrap_or_default();
    Ok(json!({ "valid": errors.is_empty(), "errors": errors }))
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
    let output = shellout::command()?
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
    fn advanced_document_fields_survive_visual_edits() {
        let source = json!({
            "vars": [{"message": "hello"}],
            "artifacts": {"model": {"path": "model.bin"}},
            "defaults": {"env": "empty", "retry": {"max_attempts": 3}},
            "plots": [{"scores.csv": {"x": "epoch", "y": "loss"}}],
            "stages": {"train": {
                "cmd": "echo hello", "env": ["PATH"],
                "retry": {"max_attempts": 2}, "resources": {"cpu": 2},
                "metrics": ["scores.json"], "persist": true,
                "on_cache_hit": "echo cached", "side_effects": true,
                "outs": [{"path": "file: with # syntax", "remote": "archive"}]
            }}
        });
        let mut payload = yaml_to_payload("crab.yaml", &source).unwrap();
        payload.stages[0].cmd.value = "echo updated".into();
        let saved: Value = serde_yaml::from_str(&payload_to_yaml(&payload).unwrap()).unwrap();
        let mut expected = source;
        expected["stages"]["train"]["cmd"] = json!("echo updated");
        assert_eq!(saved, expected);
        assert_eq!(
            crab_workflow::parse_yaml(&serde_yaml::to_string(&saved).unwrap()).unwrap(),
            crab_workflow::parse_yaml(&serde_yaml::to_string(&expected).unwrap()).unwrap()
        );
    }

    #[test]
    fn advanced_foreach_and_named_groups_can_be_loaded_and_saved() {
        let source = json!({"stages": {"prepare": {
            "foreach": ["a", "b"], "do": {"cmd": "echo ${item}", "outs": ["${item}.out"]}
        }}, "workflows": {"release": {"stages": {"publish": {"cmd": "echo publish"}}}}});
        let payload = yaml_to_payload("crab.yaml", &source).unwrap();
        let saved: Value = serde_yaml::from_str(&payload_to_yaml(&payload).unwrap()).unwrap();
        assert_eq!(saved, source);
    }

    #[test]
    fn expanded_templates_and_output_settings_keep_runtime_meaning() {
        let source = json!({"defaults": {"env": "empty", "retry": {"max_attempts": 2}}, "stages": {
            "prepare": {"foreach": ["a", "b"], "do": {"cmd": "echo ${item}", "outs": [{"${item}.out": {"cache": false, "push": false}}]}},
            "build": {"matrix": {"os": ["linux", "mac"], "arch": ["arm", "x64"]}, "cmd": "echo ${item.os} ${item.arch}", "outs": ["${item.os}-${item.arch}.out"]}
        }});
        let text = serde_yaml::to_string(&source).unwrap();
        let mut payload = yaml_to_payload("crab.yaml", &source).unwrap();
        payload.resolve(Path::new("crab.yaml"), &text).unwrap();
        assert_eq!(payload.executions.len(), 6);
        let saved = payload_to_yaml(&payload).unwrap();
        assert_eq!(
            crab_workflow::parse_yaml(&text).unwrap(),
            crab_workflow::parse_yaml(&saved).unwrap()
        );
    }

    #[test]
    fn authored_expanded_names_keep_their_own_source() {
        let source = json!({"stages": {
            "prepare": {"cmd": "true"}, "prepare@manual": {"cmd": "true"},
            "train": {"foreach": ["x"], "do": {"cmd": "echo ${item}"}},
            "train@manual": {"cmd": "true"}
        }});
        let text = serde_yaml::to_string(&source).unwrap();
        let mut payload = yaml_to_payload("crab.yaml", &source).unwrap();
        payload.resolve(Path::new("crab.yaml"), &text).unwrap();
        let sources: std::collections::BTreeMap<_, _> = payload
            .executions
            .iter()
            .map(|stage| (stage.name.as_str(), stage.source.as_str()))
            .collect();
        assert_eq!(
            sources,
            std::collections::BTreeMap::from([
                ("prepare", "prepare"),
                ("prepare@manual", "prepare@manual"),
                ("train@x", "train"),
                ("train@manual", "train@manual")
            ])
        );
    }

    #[tokio::test]
    async fn rejected_visual_save_leaves_original_bytes_intact() {
        let dir = tempfile::tempdir().unwrap();
        let original = "stages:\n  train:\n    cmd: echo original\n";
        let path = dir.path().join("crab.yaml");
        std::fs::write(&path, original).unwrap();
        let yaml: Value = serde_yaml::from_str(original).unwrap();
        let mut payload = yaml_to_payload("crab.yaml", &yaml).unwrap();
        payload.stages[0]
            .extra
            .insert("unsupported_policy".into(), json!(true));
        assert!(
            workflow_save(
                dir.path(),
                WorkflowSaveParams {
                    repo_id: "test".into(),
                    workflow: payload
                }
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[tokio::test]
    async fn save_publishes_validated_document_and_create_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let source = json!({"stages": {"train": {"cmd": "echo original"}}});
        let mut payload = yaml_to_payload("crab.yaml", &source).unwrap();
        payload.stages[0].cmd.value = "echo updated".into();
        workflow_save(
            dir.path(),
            WorkflowSaveParams {
                repo_id: "test".into(),
                workflow: payload,
            },
        )
        .await
        .unwrap();
        assert!(
            workflow_create(
                dir.path(),
                WorkflowCreateParams {
                    repo_id: "test".into(),
                    path: "crab.yaml".into()
                }
            )
            .await
            .is_err()
        );
        let loaded = workflow_get(
            dir.path(),
            WorkflowGetParams {
                repo_id: "test".into(),
                path: "crab.yaml".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(loaded["stages"][0]["cmd"]["value"], "echo updated");
        assert_eq!(loaded["executions"][0]["name"], "train");
    }

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
