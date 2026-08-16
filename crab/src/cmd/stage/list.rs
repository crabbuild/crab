//! `crab stage list` — DVC-style workflow stage listing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Serialize;
use tracing::warn;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::discover;
use crate::workflow::stage::{Dep, Stage, StageName};
use crab_workflow::{Workflow, yaml};

pub const STAGE_LIST_SCHEMA: &str = "workflow.stage.list";

const SCHEMA_VERSION: &str = "1.0";
const DEFAULT_IGNORE_DIRS: &[&str] = &[".git", ".crab", "node_modules", "target"];

/// Args for `crab stage list`.
#[derive(Debug, Clone, Parser)]
pub struct StageListArgs {
    /// Workflow files, directories, or stages to list.
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,

    /// Discover workflow files recursively under directory targets.
    #[arg(long, short = 'R', default_value_t = false)]
    pub recursive: bool,

    /// List stages from every workflow file under the repo root.
    #[arg(long, default_value_t = false)]
    pub all: bool,

    /// Fail when an invalid workflow file is found during recursive scans.
    #[arg(long, default_value_t = false)]
    pub fail: bool,

    /// Print only stage names.
    #[arg(long, default_value_t = false)]
    pub name_only: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl StageListArgs {
    fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

/// Payload emitted by `stage list`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StageListPayload {
    pub stages: Vec<StageListEntry>,
    pub skipped: Vec<PathBuf>,
}

/// One stage listed by `stage list`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct StageListEntry {
    pub name: String,
    pub workflow_file: PathBuf,
    pub description: String,
    pub frozen: bool,
}

struct WorkflowCandidate {
    path: PathBuf,
    allow_skip: bool,
}

pub fn exec_list(args: StageListArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_stage_list(&args, &cwd).map(|_| ())
}

pub fn run_stage_list(args: &StageListArgs, repo_root: &Path) -> Result<StageListPayload> {
    let mode = args.output_mode();
    let (candidates, selected) = resolve_stage_list_inputs(args, repo_root)?;
    if candidates.is_empty() {
        return Err(CrabError::Configuration {
            key: "stage list".to_owned(),
            origin: "no workflow files matched".to_owned(),
        });
    }

    let (workflow, provenance, skipped) = parse_candidates(repo_root, &candidates, args.fail)?;
    let stages = collect_stage_list_entries(repo_root, &workflow, &provenance, &selected)?;
    let payload = StageListPayload { stages, skipped };
    emit_stage_list(&payload, args.name_only, mode);
    Ok(payload)
}

fn resolve_stage_list_inputs(
    args: &StageListArgs,
    repo_root: &Path,
) -> Result<(Vec<WorkflowCandidate>, BTreeSet<StageName>)> {
    let mut candidates = BTreeMap::<PathBuf, bool>::new();
    let mut selected = BTreeSet::new();

    if args.all {
        for path in find_workflow_yamls(repo_root)? {
            candidates.insert(path, !args.fail);
        }
        return Ok((candidate_vec(candidates), selected));
    }

    if args.targets.is_empty() {
        candidates.insert(repo_root.join("crab.yaml"), false);
        return Ok((candidate_vec(candidates), selected));
    }

    for raw in &args.targets {
        if let Some((path, _stage)) = raw.rsplit_once(':')
            && !path.is_empty()
        {
            let file = normalize_workflow_file_path(repo_root.join(path));
            candidates.insert(file, false);
            selected.insert(crate::cmd::run::stage_target_name(raw)?);
            continue;
        }

        let target = repo_root.join(raw);
        let normalized = normalize_workflow_file_path(target.clone());
        if looks_like_workflow_file(&target) || looks_like_workflow_file(&normalized) {
            candidates.insert(normalized, false);
            continue;
        }
        if target.is_dir() {
            if args.recursive {
                for path in find_workflow_yamls(&target)? {
                    candidates.insert(path, !args.fail);
                }
            } else {
                candidates.insert(target.join("crab.yaml"), false);
            }
            continue;
        }

        candidates.insert(repo_root.join("crab.yaml"), false);
        selected.insert(crate::cmd::run::stage_target_name(raw)?);
    }

    Ok((candidate_vec(candidates), selected))
}

fn candidate_vec(candidates: BTreeMap<PathBuf, bool>) -> Vec<WorkflowCandidate> {
    candidates
        .into_iter()
        .map(|(path, allow_skip)| WorkflowCandidate { path, allow_skip })
        .collect()
}

fn parse_candidates(
    repo_root: &Path,
    candidates: &[WorkflowCandidate],
    fail: bool,
) -> Result<(Workflow, BTreeMap<StageName, PathBuf>, Vec<PathBuf>)> {
    let mut parsed = Vec::new();
    let mut skipped = Vec::new();

    for candidate in candidates {
        let text = match std::fs::read_to_string(&candidate.path) {
            Ok(text) => text,
            Err(e) if candidate.allow_skip && e.kind() == std::io::ErrorKind::NotFound => {
                skipped.push(repo_relative_path(repo_root, &candidate.path));
                continue;
            }
            Err(e) => return Err(CrabError::Io(e)),
        };
        match yaml::parse_at(&candidate.path, &text) {
            Ok(workflow) => parsed.push((candidate.path.clone(), workflow)),
            Err(e) if candidate.allow_skip && !fail => {
                warn!(
                    path = %candidate.path.display(),
                    error = %e,
                    "stage list: skipping invalid workflow file"
                );
                skipped.push(repo_relative_path(repo_root, &candidate.path));
            }
            Err(e) => return Err(e.into()),
        }
    }

    if parsed.is_empty() {
        return Err(CrabError::Configuration {
            key: "stage list".to_owned(),
            origin: "no valid workflow files matched".to_owned(),
        });
    }

    let (workflow, provenance) = discover::merge_with_provenance(repo_root, &parsed)?;
    Ok((workflow, provenance, skipped))
}

fn collect_stage_list_entries(
    repo_root: &Path,
    workflow: &Workflow,
    provenance: &BTreeMap<StageName, PathBuf>,
    selected: &BTreeSet<StageName>,
) -> Result<Vec<StageListEntry>> {
    let names = if selected.is_empty() {
        workflow.stages.keys().cloned().collect::<Vec<_>>()
    } else {
        selected.iter().cloned().collect::<Vec<_>>()
    };
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        let stage = workflow
            .stages
            .get(&name)
            .ok_or_else(|| CrabError::Configuration {
                key: format!("stage target '{name}' not found in crab.yaml"),
                origin: "cli".to_owned(),
            })?;
        let workflow_file =
            provenance
                .get(&name)
                .cloned()
                .ok_or_else(|| CrabError::Configuration {
                    key: format!("stage '{name}'"),
                    origin: "workflow provenance missing".to_owned(),
                })?;
        entries.push(StageListEntry {
            name: name.to_string(),
            workflow_file: repo_relative_path(repo_root, &workflow_file),
            description: stage_description(stage),
            frozen: stage.frozen,
        });
    }
    Ok(entries)
}

fn stage_description(stage: &Stage) -> String {
    if let Some(desc) = stage.desc.as_deref().filter(|desc| !desc.trim().is_empty()) {
        return truncate_description(desc.trim());
    }

    let generated = if !stage.outs.is_empty() {
        format!(
            "Outputs {}",
            join_paths(stage.outs.iter().map(|out| out.path.as_path()))
        )
    } else if !stage.metrics.is_empty() {
        format!(
            "Reports {}",
            join_paths(stage.metrics.iter().map(PathBuf::as_path))
        )
    } else if !stage.plots.is_empty() {
        format!(
            "Plots {}",
            join_paths(stage.plots.iter().map(PathBuf::as_path))
        )
    } else if !stage.deps.is_empty() {
        format!("Depends on {}", join_deps(&stage.deps))
    } else {
        "Runs command".to_owned()
    };
    truncate_description(&generated)
}

fn join_paths<'a>(paths: impl Iterator<Item = &'a Path>) -> String {
    paths
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn join_deps(deps: &[Dep]) -> String {
    deps.iter()
        .map(dep_description)
        .collect::<Vec<_>>()
        .join(", ")
}

fn dep_description(dep: &Dep) -> String {
    match dep {
        Dep::Path(path) => path.display().to_string(),
        Dep::CrabRef { repo, path, .. } => format!("{repo}:{}", path.display()),
        Dep::GitRef { url, path, .. } => format!("{url}:{}", path.display()),
        Dep::Url { url, .. } => url.clone(),
        Dep::OciImage { reference, .. } => reference.clone(),
        Dep::StageOut { stage, out } => format!("{stage}:{}", out.display()),
    }
}

fn truncate_description(desc: &str) -> String {
    const MAX: usize = 80;
    if desc.chars().count() <= MAX {
        return desc.to_owned();
    }
    let mut out = desc.chars().take(MAX.saturating_sub(3)).collect::<String>();
    out.push_str("...");
    out
}

fn find_workflow_yamls(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    walk_workflow_yamls(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_workflow_yamls(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CrabError::Io(e)),
    };

    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;
        if file_type.is_dir() {
            let skip = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| DEFAULT_IGNORE_DIRS.contains(&name));
            if !skip {
                walk_workflow_yamls(&path, out)?;
            }
            continue;
        }
        if file_type.is_file() && looks_like_crab_workflow_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn normalize_workflow_file_path(path: PathBuf) -> PathBuf {
    if path.file_name().and_then(|name| name.to_str()) == Some("dvc.yaml") {
        return path.with_file_name("crab.yaml");
    }
    path
}

fn looks_like_workflow_file(path: &Path) -> bool {
    let normalized = normalize_workflow_file_path(path.to_path_buf());
    looks_like_crab_workflow_file(&normalized)
}

fn looks_like_crab_workflow_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "crab.yaml" || name.ends_with(".workflow.yaml"))
}

fn repo_relative_path(repo_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(repo_root).unwrap_or(path).to_path_buf()
}

fn emit_stage_list(payload: &StageListPayload, name_only: bool, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(STAGE_LIST_SCHEMA, SCHEMA_VERSION, payload);
        }
        OutputMode::Text if name_only => {
            for stage in &payload.stages {
                println!("{}", stage.name);
            }
        }
        OutputMode::Text => {
            let width = payload
                .stages
                .iter()
                .map(|stage| stage.name.len())
                .max()
                .unwrap_or(0);
            for stage in &payload.stages {
                println!(
                    "{:<width$}  {}",
                    stage.name,
                    stage.description,
                    width = width
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
    fn stage_list_args_accept_dvc_flags() {
        let args = StageListArgs::try_parse_from([
            "list",
            "-R",
            "--all",
            "--fail",
            "--name-only",
            "--json",
            "pipelines",
        ])
        .unwrap();

        assert!(args.recursive);
        assert!(args.all);
        assert!(args.fail);
        assert!(args.name_only);
        assert!(args.json);
        assert_eq!(args.targets, vec!["pipelines".to_owned()]);
    }

    #[test]
    fn stage_list_lists_root_stages_with_descriptions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  prepare:\n    cmd: python prepare.py\n    outs: [data/prepared]\n  train:\n    cmd: python train.py\n    desc: Train the model\n    deps: [data/prepared]\n    outs: [model.pkl]\n",
        )
        .unwrap();

        let payload = run_stage_list(
            &StageListArgs {
                targets: Vec::new(),
                recursive: false,
                all: false,
                fail: false,
                name_only: false,
                json: true,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.stages.len(), 2);
        assert_eq!(payload.stages[0].name, "prepare");
        assert_eq!(payload.stages[0].description, "Outputs data/prepared");
        assert_eq!(payload.stages[1].name, "train");
        assert_eq!(payload.stages[1].description, "Train the model");
    }

    #[test]
    fn stage_list_accepts_path_qualified_dvc_stage_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::write(
            root.join("models/crab.yaml"),
            "stages:\n  train:\n    cmd: python train.py\n    outs: [model.pkl]\n  eval:\n    cmd: python eval.py\n    metrics: [scores.json]\n",
        )
        .unwrap();

        let payload = run_stage_list(
            &StageListArgs {
                targets: vec!["models/dvc.yaml:eval".to_owned()],
                recursive: false,
                all: false,
                fail: false,
                name_only: false,
                json: true,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.stages.len(), 1);
        assert_eq!(payload.stages[0].name, "models.eval");
        assert_eq!(
            payload.stages[0].workflow_file,
            PathBuf::from("models/crab.yaml")
        );
        assert_eq!(payload.stages[0].description, "Reports models/scores.json");
    }

    #[test]
    fn stage_list_all_skips_invalid_workflows_without_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("bad")).unwrap();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  ok:\n    cmd: echo ok\n    outs: [ok.txt]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("bad/crab.yaml"),
            "stages:\n  bad:\n    unknown: true\n",
        )
        .unwrap();

        let payload = run_stage_list(
            &StageListArgs {
                targets: Vec::new(),
                recursive: false,
                all: true,
                fail: false,
                name_only: false,
                json: true,
            },
            root,
        )
        .unwrap();

        assert_eq!(payload.stages.len(), 1);
        assert_eq!(payload.skipped, vec![PathBuf::from("bad/crab.yaml")]);
    }
}
