//! `crab freeze` / `crab unfreeze` — toggle workflow stage freezing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use clap::Parser;
use serde::Serialize;
use serde_yaml::{Mapping, Value};

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::discover::{self, DiscoverMode};
use crate::workflow::stage::StageName;

pub const FREEZE_SCHEMA: &str = "workflow.freeze";
pub const UNFREEZE_SCHEMA: &str = "workflow.unfreeze";

const SCHEMA_VERSION: &str = "1.0";

/// Args for `crab freeze` and `crab unfreeze`.
#[derive(Debug, Clone, Parser)]
pub struct FreezeArgs {
    /// Stage targets to freeze or unfreeze.
    #[arg(required = true, value_name = "TARGET")]
    pub targets: Vec<String>,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl FreezeArgs {
    pub(crate) fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FreezePayload {
    pub frozen: bool,
    pub stages: Vec<FreezeStageUpdate>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FreezeStageUpdate {
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_stage: Option<String>,
    pub workflow_file: PathBuf,
    pub changed: bool,
}

pub fn exec_freeze(args: FreezeArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_freeze(&args, &cwd).map(|_| ())
}

pub fn exec_unfreeze(args: FreezeArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_unfreeze(&args, &cwd).map(|_| ())
}

pub fn run_freeze(args: &FreezeArgs, repo_root: &Path) -> Result<FreezePayload> {
    run_toggle(args, repo_root, true)
}

pub fn run_unfreeze(args: &FreezeArgs, repo_root: &Path) -> Result<FreezePayload> {
    run_toggle(args, repo_root, false)
}

fn run_toggle(args: &FreezeArgs, repo_root: &Path, frozen: bool) -> Result<FreezePayload> {
    let yaml_paths = discover::discover(repo_root, DiscoverMode::Recursive)?;
    if yaml_paths.is_empty() {
        return Err(CrabError::Configuration {
            key: "workflow freeze".to_owned(),
            origin: "no crab.yaml found".to_owned(),
        });
    }

    let (workflow, origins) = discover::parse_all_with_provenance(repo_root, &yaml_paths)?;
    let selected = resolve_targets(args, &workflow)?;
    let mut docs = BTreeMap::<PathBuf, Value>::new();
    let mut changed_files = BTreeSet::<PathBuf>::new();
    let mut updates = Vec::with_capacity(selected.len());

    for stage_name in selected {
        let origin = origins
            .get(&stage_name)
            .ok_or_else(|| CrabError::Configuration {
                key: format!("stage target '{stage_name}'"),
                origin: "workflow provenance missing for selected stage".to_owned(),
            })?
            .clone();
        if !docs.contains_key(&origin) {
            docs.insert(origin.clone(), read_yaml_document(&origin)?);
        }
        let doc = docs.get_mut(&origin).ok_or_else(|| {
            CrabError::Internal(format!(
                "workflow document cache missing {}",
                origin.display()
            ))
        })?;
        let local_stage = local_stage_name(repo_root, &origin, &stage_name)?;
        let (declared_stage, changed) = set_declared_stage_frozen(doc, &local_stage, frozen)?;
        if changed {
            changed_files.insert(origin.clone());
        }

        updates.push(FreezeStageUpdate {
            stage: stage_name.to_string(),
            declared_stage: (declared_stage != stage_name.as_str()).then_some(declared_stage),
            workflow_file: repo_relative_path(repo_root, &origin),
            changed,
        });
    }

    for path in changed_files {
        let doc = docs.get(&path).ok_or_else(|| {
            CrabError::Internal(format!(
                "workflow document cache missing {}",
                path.display()
            ))
        })?;
        write_yaml_document(&path, doc)?;
    }

    let payload = FreezePayload {
        frozen,
        stages: updates,
    };
    emit_toggle(&payload, args.output_mode());
    Ok(payload)
}

fn resolve_targets(
    args: &FreezeArgs,
    workflow: &crab_workflow::Workflow,
) -> Result<BTreeSet<StageName>> {
    let mut selected = BTreeSet::new();
    for raw in &args.targets {
        let stage_name = crate::cmd::run::stage_target_name(raw)?;
        if !workflow.stages.contains_key(&stage_name) {
            return Err(CrabError::Configuration {
                key: format!("stage target '{raw}' not found in crab.yaml"),
                origin: "cli".to_owned(),
            });
        }
        selected.insert(stage_name);
    }
    Ok(selected)
}

fn read_yaml_document(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(CrabError::Io)?;
    serde_yaml::from_str(&text).map_err(|source| CrabError::WorkflowParse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_yaml_document(path: &Path, doc: &Value) -> Result<()> {
    let mut text = serde_yaml::to_string(doc).map_err(|e| {
        CrabError::Internal(format!(
            "failed to serialize workflow yaml {}: {e}",
            path.display()
        ))
    })?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    atomic_write(path, text.as_bytes())
}

fn set_declared_stage_frozen(
    doc: &mut Value,
    local_stage: &str,
    frozen: bool,
) -> Result<(String, bool)> {
    let candidates = stage_key_candidates(local_stage);
    if let Some(update) = set_stage_in_top_level_stages(doc, &candidates, frozen)? {
        return Ok(update);
    }
    if let Some(update) = set_stage_in_workflow_groups(doc, &candidates, frozen)? {
        return Ok(update);
    }
    Err(CrabError::Configuration {
        key: format!("stage target '{local_stage}'"),
        origin: "stage is expanded from a workflow template that could not be located".to_owned(),
    })
}

fn set_stage_in_top_level_stages(
    doc: &mut Value,
    candidates: &[String],
    frozen: bool,
) -> Result<Option<(String, bool)>> {
    let Some(root) = doc.as_mapping_mut() else {
        return Err(CrabError::Configuration {
            key: "workflow yaml".to_owned(),
            origin: "document root must be a mapping".to_owned(),
        });
    };
    let Some(stages) = mapping_get_mut(root, "stages") else {
        return Ok(None);
    };
    set_stage_in_stage_map(stages, candidates, frozen)
}

fn set_stage_in_workflow_groups(
    doc: &mut Value,
    candidates: &[String],
    frozen: bool,
) -> Result<Option<(String, bool)>> {
    let Some(root) = doc.as_mapping_mut() else {
        return Err(CrabError::Configuration {
            key: "workflow yaml".to_owned(),
            origin: "document root must be a mapping".to_owned(),
        });
    };
    let Some(workflows) = mapping_get_mut(root, "workflows") else {
        return Ok(None);
    };
    let workflows = workflows
        .as_mapping_mut()
        .ok_or_else(|| CrabError::Configuration {
            key: "workflows".to_owned(),
            origin: "workflows must be a mapping".to_owned(),
        })?;

    for group_value in workflows.values_mut() {
        let group = group_value
            .as_mapping_mut()
            .ok_or_else(|| CrabError::Configuration {
                key: "workflows".to_owned(),
                origin: "workflow group must be a mapping".to_owned(),
            })?;
        let Some(stages) = mapping_get_mut(group, "stages") else {
            continue;
        };
        if let Some(update) = set_stage_in_stage_map(stages, candidates, frozen)? {
            return Ok(Some(update));
        }
    }

    Ok(None)
}

fn set_stage_in_stage_map(
    stages: &mut Value,
    candidates: &[String],
    frozen: bool,
) -> Result<Option<(String, bool)>> {
    let stages = stages
        .as_mapping_mut()
        .ok_or_else(|| CrabError::Configuration {
            key: "stages".to_owned(),
            origin: "stages must be a mapping".to_owned(),
        })?;

    for candidate in candidates {
        let key = Value::String(candidate.clone());
        if let Some(stage) = stages.get_mut(&key) {
            let changed = set_stage_value_frozen(candidate, stage, frozen)?;
            return Ok(Some((candidate.clone(), changed)));
        }
    }

    Ok(None)
}

fn set_stage_value_frozen(stage_name: &str, stage: &mut Value, frozen: bool) -> Result<bool> {
    let stage = stage
        .as_mapping_mut()
        .ok_or_else(|| CrabError::Configuration {
            key: format!("stage '{stage_name}'"),
            origin: "stage must be a mapping".to_owned(),
        })?;
    let key = Value::String("frozen".to_owned());
    if frozen {
        let changed = stage.get(&key) != Some(&Value::Bool(true));
        stage.insert(key, Value::Bool(true));
        return Ok(changed);
    }
    Ok(stage.remove(&key).is_some())
}

fn stage_key_candidates(local_stage: &str) -> Vec<String> {
    let mut candidates = vec![local_stage.to_owned()];
    if let Some((base, _)) = local_stage.split_once('@') {
        candidates.push(base.to_owned());
    }
    candidates
}

fn mapping_get_mut<'a>(mapping: &'a mut Mapping, key: &str) -> Option<&'a mut Value> {
    mapping.get_mut(Value::String(key.to_owned()))
}

fn local_stage_name(repo_root: &Path, path: &Path, stage_name: &StageName) -> Result<String> {
    let prefix = workflow_file_prefix(repo_root, path)?;
    if prefix.is_empty() {
        return Ok(stage_name.as_str().to_owned());
    }
    let expected_prefix = format!("{prefix}.");
    stage_name
        .as_str()
        .strip_prefix(&expected_prefix)
        .map(std::borrow::ToOwned::to_owned)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("stage target '{stage_name}'"),
            origin: format!(
                "stage provenance {} does not match nested workflow prefix {prefix}",
                path.display()
            ),
        })
}

fn workflow_file_prefix(repo_root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(repo_root)
        .map_err(|_| CrabError::Configuration {
            key: "workflow path".to_owned(),
            origin: format!("workflow file must be under repo root: {}", path.display()),
        })?;
    let file_name = rel
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CrabError::Configuration {
            key: "workflow path".to_owned(),
            origin: format!("workflow file path is not valid UTF-8: {}", rel.display()),
        })?;
    let mut components = repo_relative_components(rel.parent().unwrap_or_else(|| Path::new("")))?;
    match file_name {
        "crab.yaml" => {}
        name => {
            let stem =
                name.strip_suffix(".workflow.yaml")
                    .ok_or_else(|| CrabError::Configuration {
                        key: "workflow path".to_owned(),
                        origin: format!("unsupported workflow file name: {}", rel.display()),
                    })?;
            if stem.is_empty() {
                return Err(CrabError::Configuration {
                    key: "workflow path".to_owned(),
                    origin: format!("workflow file stem is empty: {}", rel.display()),
                });
            }
            components.push(stem.to_owned());
        }
    }
    Ok(components.join("."))
}

fn repo_relative_components(path: &Path) -> Result<Vec<String>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let Some(part) = part.to_str() else {
                    return Err(CrabError::Configuration {
                        key: "workflow path".to_owned(),
                        origin: format!("path is not valid UTF-8: {}", path.display()),
                    });
                };
                components.push(part.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CrabError::Configuration {
                    key: "workflow path".to_owned(),
                    origin: format!("workflow path must be repo-relative: {}", path.display()),
                });
            }
        }
    }
    Ok(components)
}

fn repo_relative_path(repo_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(repo_root).unwrap_or(path).to_path_buf()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        CrabError::Internal(format!("workflow path has no parent: {}", path.display()))
    })?;
    let tmp = tempfile::NamedTempFile::new_in(parent).map_err(CrabError::Io)?;
    std::fs::write(tmp.path(), bytes).map_err(CrabError::Io)?;
    tmp.persist(path).map_err(|e| {
        CrabError::Internal(format!(
            "failed to persist workflow yaml to {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

fn emit_toggle(payload: &FreezePayload, mode: OutputMode) {
    let schema = if payload.frozen {
        FREEZE_SCHEMA
    } else {
        UNFREEZE_SCHEMA
    };
    match mode {
        OutputMode::Json | OutputMode::Jsonl => emit_json(schema, SCHEMA_VERSION, payload),
        OutputMode::Text => {
            let verb = if payload.frozen { "Froze" } else { "Unfroze" };
            println!("{verb} {} stage(s).", payload.stages.len());
            for stage in &payload.stages {
                let marker = if stage.changed {
                    "updated"
                } else {
                    "unchanged"
                };
                println!(
                    "  {} ({}, {})",
                    stage.stage,
                    stage.workflow_file.display(),
                    marker
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn freeze_and_unfreeze_toggle_stage_frozen_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  train:\n    cmd: python train.py\n    deps: [train.py]\n    outs: [model.pkl]\n",
        )
        .unwrap();

        let payload = run_freeze(
            &FreezeArgs {
                targets: vec!["train".to_owned()],
                json: true,
            },
            root,
        )
        .unwrap();
        assert!(payload.frozen);
        assert_eq!(payload.stages.len(), 1);
        assert!(payload.stages[0].changed);

        let doc = read_yaml_document(&root.join("crab.yaml")).unwrap();
        assert_eq!(
            doc["stages"]["train"]["frozen"],
            serde_yaml::Value::Bool(true)
        );

        let payload = run_unfreeze(
            &FreezeArgs {
                targets: vec!["train".to_owned()],
                json: true,
            },
            root,
        )
        .unwrap();
        assert!(!payload.frozen);
        assert!(payload.stages[0].changed);

        let doc = read_yaml_document(&root.join("crab.yaml")).unwrap();
        assert!(doc["stages"]["train"]["frozen"].is_null());
    }

    #[test]
    fn freeze_accepts_dvc_path_qualified_nested_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(
            root.join("models/crab.yaml"),
            "stages:\n  train:\n    cmd: python train.py\n    deps: [train.py]\n    outs: [model.pkl]\n",
        )
        .unwrap();

        let payload = run_freeze(
            &FreezeArgs {
                targets: vec!["models/dvc.yaml:train".to_owned()],
                json: true,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.stages[0].stage, "models.train");
        assert_eq!(
            payload.stages[0].workflow_file,
            PathBuf::from("models/crab.yaml")
        );
        let doc = read_yaml_document(&root.join("models/crab.yaml")).unwrap();
        assert_eq!(
            doc["stages"]["train"]["frozen"],
            serde_yaml::Value::Bool(true)
        );
    }
}
