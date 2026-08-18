//! `crab stage add` — DVC-style workflow stage authoring.

use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Serialize;
use serde_yaml::{Mapping, Value};

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::stage::StageName;
use crab_workflow::{Graph, yaml};

pub const STAGE_ADD_SCHEMA: &str = "workflow.stage.add";

const SCHEMA_VERSION: &str = "1.0";

/// Args for `crab stage add`.
#[derive(Debug, Clone, Parser)]
pub struct StageAddArgs {
    /// Stage name to create or update.
    #[arg(long, short = 'n', value_name = "STAGE")]
    pub name: String,

    /// Overwrite an existing stage definition.
    #[arg(long, short = 'f', default_value_t = false)]
    pub force: bool,

    /// File, directory, or URL dependencies.
    #[arg(long = "deps", short = 'd', value_name = "PATH")]
    pub deps: Vec<String>,

    /// Parameter dependencies as `key,key` or `file:key,key`.
    #[arg(long = "params", short = 'p', value_name = "[FILE:]KEYS")]
    pub params: Vec<String>,

    /// Cached output paths.
    #[arg(long = "outs", short = 'o', value_name = "PATH")]
    pub outs: Vec<PathBuf>,

    /// Output paths that are not cached.
    #[arg(long = "outs-no-cache", short = 'O', value_name = "PATH")]
    pub outs_no_cache: Vec<PathBuf>,

    /// Cached output paths preserved before execution.
    #[arg(long = "outs-persist", value_name = "PATH")]
    pub outs_persist: Vec<PathBuf>,

    /// Non-cached output paths preserved before execution.
    #[arg(long = "outs-persist-no-cache", value_name = "PATH")]
    pub outs_persist_no_cache: Vec<PathBuf>,

    /// Experiment checkpoint output paths.
    #[arg(long = "checkpoints", short = 'c', value_name = "PATH")]
    pub checkpoints: Vec<PathBuf>,

    /// Cached metrics output paths.
    #[arg(long = "metrics", short = 'm', value_name = "PATH")]
    pub metrics: Vec<PathBuf>,

    /// Metrics paths that are not cached.
    #[arg(long = "metrics-no-cache", short = 'M', value_name = "PATH")]
    pub metrics_no_cache: Vec<PathBuf>,

    /// Cached plot output paths.
    #[arg(long = "plots", value_name = "PATH")]
    pub plots: Vec<PathBuf>,

    /// Plot paths that are not cached.
    #[arg(long = "plots-no-cache", value_name = "PATH")]
    pub plots_no_cache: Vec<PathBuf>,

    /// Working directory for the command.
    #[arg(long = "wdir", short = 'w', value_name = "PATH")]
    pub wdir: Option<PathBuf>,

    /// Always consider this stage changed.
    #[arg(long = "always-changed", default_value_t = false)]
    pub always_changed: bool,

    /// Human-readable stage description.
    #[arg(long, value_name = "TEXT")]
    pub desc: Option<String>,

    /// Run the stage after writing the workflow file.
    #[arg(long, default_value_t = false)]
    pub run: bool,

    /// Structured JSON output.
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Command to store in the stage.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

impl StageAddArgs {
    pub(crate) fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json, false)
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct StageAddPayload {
    pub stage: String,
    pub workflow_file: PathBuf,
    pub created_workflow_file: bool,
    pub overwritten: bool,
    pub run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutputSpec {
    path: PathBuf,
    cache: bool,
    persist: bool,
    checkpoint: bool,
}

pub async fn exec_add(args: StageAddArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_stage_add(&args, &cwd)?;
    if args.run {
        let run_args = run_args_for_added_stage(&args);
        crate::cmd::run::run_in(&run_args, &cwd, OutputMode::from_flags(args.json, false)).await?;
    }
    Ok(())
}

pub fn run_stage_add(args: &StageAddArgs, repo_root: &Path) -> Result<StageAddPayload> {
    StageName::parse(&args.name)?;
    if args.command.is_empty() {
        return Err(CrabError::Configuration {
            key: "stage add command".to_owned(),
            origin: "command must not be empty".to_owned(),
        });
    }

    let workflow_path = repo_root.join("crab.yaml");
    let created_workflow_file = !workflow_path.exists();
    let mut doc = read_or_empty_workflow_document(&workflow_path)?;
    let stage = build_stage_yaml(args)?;
    let overwritten = insert_stage_yaml(&mut doc, &args.name, stage, args.force)?;
    validate_workflow_document(&workflow_path, &doc)?;
    write_yaml_document(&workflow_path, &doc)?;

    let payload = StageAddPayload {
        stage: args.name.clone(),
        workflow_file: PathBuf::from("crab.yaml"),
        created_workflow_file,
        overwritten,
        run: args.run,
    };
    emit_stage_add(&payload, args.output_mode());
    Ok(payload)
}

fn build_stage_yaml(args: &StageAddArgs) -> Result<Value> {
    let mut stage = Mapping::new();
    stage.insert(
        Value::String("cmd".to_owned()),
        Value::String(args.command.join(" ")),
    );

    insert_sequence(
        &mut stage,
        "deps",
        args.deps.iter().map(|dep| dep_yaml_value(dep)).collect(),
    );
    insert_sequence(&mut stage, "params", params_yaml_values(&args.params)?);

    let mut outs = Vec::new();
    push_outputs(&mut outs, &args.outs, true, false)?;
    push_outputs(&mut outs, &args.outs_no_cache, false, false)?;
    push_outputs(&mut outs, &args.outs_persist, true, true)?;
    push_outputs(&mut outs, &args.outs_persist_no_cache, false, true)?;
    for path in &args.checkpoints {
        let spec = OutputSpec {
            path: path.clone(),
            cache: true,
            persist: false,
            checkpoint: true,
        };
        if let Some(existing) = outs.iter().find(|existing| existing.path == spec.path) {
            if existing == &spec {
                continue;
            }
            return Err(CrabError::Configuration {
                key: format!("stage add output '{}'", path.display()),
                origin: "checkpoint path conflicts with another output".to_owned(),
            });
        }
        outs.push(spec);
    }
    push_outputs(&mut outs, &args.plots, true, false)?;
    push_outputs(&mut outs, &args.plots_no_cache, false, false)?;
    insert_sequence(
        &mut stage,
        "outs",
        outs.iter().map(output_yaml_value).collect(),
    );

    let mut metrics = Vec::new();
    metrics.extend(
        args.metrics
            .iter()
            .map(|path| metric_yaml_value(path, true)),
    );
    metrics.extend(
        args.metrics_no_cache
            .iter()
            .map(|path| metric_yaml_value(path, false)),
    );
    insert_sequence(&mut stage, "metrics", metrics);

    let mut plots = Vec::new();
    plots.extend(args.plots.iter().map(|path| path_yaml_value(path)));
    plots.extend(args.plots_no_cache.iter().map(|path| path_yaml_value(path)));
    insert_sequence(&mut stage, "plots", plots);

    if let Some(wdir) = &args.wdir {
        stage.insert(Value::String("wdir".to_owned()), path_yaml_value(wdir));
    }
    if args.always_changed {
        stage.insert(
            Value::String("always_changed".to_owned()),
            Value::Bool(true),
        );
    }
    if let Some(desc) = &args.desc {
        stage.insert(
            Value::String("desc".to_owned()),
            Value::String(desc.clone()),
        );
    }

    Ok(Value::Mapping(stage))
}

fn insert_sequence(stage: &mut Mapping, key: &str, values: Vec<Value>) {
    if !values.is_empty() {
        stage.insert(Value::String(key.to_owned()), Value::Sequence(values));
    }
}

fn dep_yaml_value(dep: &str) -> Value {
    if dep.contains("://") {
        let mut url = Mapping::new();
        url.insert(
            Value::String("url".to_owned()),
            Value::String(dep.to_owned()),
        );
        let mut structured = Mapping::new();
        structured.insert(Value::String("url".to_owned()), Value::Mapping(url));
        return Value::Mapping(structured);
    }
    Value::String(dep.to_owned())
}

fn params_yaml_values(params: &[String]) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for spec in params {
        if let Some((file, keys)) = spec.split_once(':') {
            if file.trim().is_empty() {
                return Err(CrabError::Configuration {
                    key: "stage add params".to_owned(),
                    origin: "params file prefix must not be empty".to_owned(),
                });
            }
            let mut scoped = Mapping::new();
            let refs = if keys.is_empty() {
                Value::Null
            } else {
                Value::Sequence(
                    split_param_keys(keys)?
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                )
            };
            scoped.insert(Value::String(file.to_owned()), refs);
            values.push(Value::Mapping(scoped));
            continue;
        }
        values.extend(split_param_keys(spec)?.into_iter().map(Value::String));
    }
    Ok(values)
}

fn split_param_keys(raw: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for key in raw.split(',').map(str::trim) {
        if key.is_empty() {
            return Err(CrabError::Configuration {
                key: "stage add params".to_owned(),
                origin: "params list must not contain empty keys".to_owned(),
            });
        }
        keys.push(key.to_owned());
    }
    Ok(keys)
}

fn push_outputs(
    outs: &mut Vec<OutputSpec>,
    paths: &[PathBuf],
    cache: bool,
    persist: bool,
) -> Result<()> {
    for path in paths {
        let spec = OutputSpec {
            path: path.clone(),
            cache,
            persist,
            checkpoint: false,
        };
        if let Some(existing) = outs.iter().find(|existing| existing.path == spec.path) {
            if existing == &spec {
                continue;
            }
            return Err(CrabError::Configuration {
                key: format!("stage add output '{}'", path.display()),
                origin: "duplicate output path has conflicting cache or persist settings"
                    .to_owned(),
            });
        }
        outs.push(spec);
    }
    Ok(())
}

fn output_yaml_value(out: &OutputSpec) -> Value {
    if out.cache && !out.persist && !out.checkpoint {
        return path_yaml_value(&out.path);
    }
    let mut map = Mapping::new();
    map.insert(Value::String("path".to_owned()), path_yaml_value(&out.path));
    if !out.cache {
        map.insert(Value::String("cache".to_owned()), Value::Bool(false));
    }
    if out.persist {
        map.insert(Value::String("persist".to_owned()), Value::Bool(true));
    }
    if out.checkpoint {
        map.insert(Value::String("checkpoint".to_owned()), Value::Bool(true));
    }
    Value::Mapping(map)
}

fn metric_yaml_value(path: &Path, cache: bool) -> Value {
    let mut map = Mapping::new();
    map.insert(Value::String("path".to_owned()), path_yaml_value(path));
    if !cache {
        map.insert(Value::String("cache".to_owned()), Value::Bool(false));
    }
    Value::Mapping(map)
}

fn path_yaml_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().replace('\\', "/"))
}

fn read_or_empty_workflow_document(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_yaml::from_str(&text).map_err(|source| CrabError::WorkflowParse {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Mapping(Mapping::new())),
        Err(e) => Err(CrabError::Io(e)),
    }
}

fn insert_stage_yaml(doc: &mut Value, stage_name: &str, stage: Value, force: bool) -> Result<bool> {
    let root = doc
        .as_mapping_mut()
        .ok_or_else(|| CrabError::Configuration {
            key: "workflow yaml".to_owned(),
            origin: "document root must be a mapping".to_owned(),
        })?;
    let stages_key = Value::String("stages".to_owned());
    if !root.contains_key(&stages_key) {
        root.insert(stages_key.clone(), Value::Mapping(Mapping::new()));
    }
    let stages = root
        .get_mut(&stages_key)
        .and_then(Value::as_mapping_mut)
        .ok_or_else(|| CrabError::Configuration {
            key: "stages".to_owned(),
            origin: "stages must be a mapping".to_owned(),
        })?;

    let key = Value::String(stage_name.to_owned());
    let overwritten = stages.contains_key(&key);
    if overwritten && !force {
        return Err(CrabError::Configuration {
            key: format!("stage '{stage_name}'"),
            origin: "stage already exists in crab.yaml; pass --force to overwrite".to_owned(),
        });
    }
    stages.insert(key, stage);
    Ok(overwritten)
}

fn validate_workflow_document(path: &Path, doc: &Value) -> Result<()> {
    let text = serialize_yaml_document(path, doc)?;
    let workflow = yaml::parse_at(path, &text)?;
    Graph::build(&workflow.stages)?;
    Ok(())
}

fn write_yaml_document(path: &Path, doc: &Value) -> Result<()> {
    let text = serialize_yaml_document(path, doc)?;
    atomic_write(path, text.as_bytes())
}

fn serialize_yaml_document(path: &Path, doc: &Value) -> Result<String> {
    let mut text = serde_yaml::to_string(doc).map_err(|e| {
        CrabError::Internal(format!(
            "failed to serialize workflow yaml {}: {e}",
            path.display()
        ))
    })?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
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

fn run_args_for_added_stage(args: &StageAddArgs) -> crate::cmd::run::RunArgs {
    crate::cmd::run::RunArgs {
        name: None,
        deps: Vec::new(),
        outs: Vec::new(),
        env: Vec::new(),
        empty_env: false,
        timeout: None,
        hermetic: false,
        nondeterministic: false,
        force: false,
        dry_run: false,
        interactive: false,
        cache_only: false,
        no_run_cache: false,
        no_commit: false,
        no_overwrite: false,
        resume_trust_outputs: false,
        abandon: None,
        explain_miss: false,
        lock_timeout: None,
        no_wait: false,
        json: args.json,
        jsonl: false,
        recursive: false,
        single_item: true,
        downstream: false,
        force_downstream: false,
        pipeline: false,
        all_pipelines: false,
        keep_going: false,
        ignore_errors: false,
        parallelism: None,
        cache_push: false,
        allow_missing: false,
        pull: false,
        validate: false,
        #[cfg(feature = "watch")]
        watch: false,
        workflow: None,
        stages: None,
        glob: false,
        cmd: vec![args.name.clone()],
    }
}

fn emit_stage_add(payload: &StageAddPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(STAGE_ADD_SCHEMA, SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            if payload.created_workflow_file {
                println!("Created '{}'.", payload.workflow_file.display());
            }
            let verb = if payload.overwritten {
                "Updated"
            } else {
                "Added"
            };
            println!(
                "{verb} stage '{}' in '{}'.",
                payload.stage,
                payload.workflow_file.display()
            );
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
    use crate::workflow::stage::Dep;

    fn minimal_add_args(name: &str, command: &[&str]) -> StageAddArgs {
        StageAddArgs {
            name: name.to_owned(),
            force: false,
            deps: Vec::new(),
            params: Vec::new(),
            outs: Vec::new(),
            outs_no_cache: Vec::new(),
            outs_persist: Vec::new(),
            outs_persist_no_cache: Vec::new(),
            checkpoints: Vec::new(),
            metrics: Vec::new(),
            metrics_no_cache: Vec::new(),
            plots: Vec::new(),
            plots_no_cache: Vec::new(),
            wdir: None,
            always_changed: false,
            desc: None,
            run: false,
            json: true,
            command: command.iter().map(|part| (*part).to_owned()).collect(),
        }
    }

    #[test]
    fn stage_add_args_accept_dvc_flags_and_command_flags() {
        let args = StageAddArgs::try_parse_from([
            "add",
            "-n",
            "train",
            "-f",
            "-d",
            "src/train.py",
            "-p",
            "seed,model.lr",
            "-p",
            "params.json:threshold",
            "-p",
            "params.toml:",
            "-o",
            "model.pkl",
            "-O",
            "debug.log",
            "--outs-persist",
            "cache",
            "--outs-persist-no-cache",
            "scratch",
            "-c",
            "checkpoint.pkl",
            "-m",
            "scores.json",
            "-M",
            "metrics-local.json",
            "--plots",
            "plots/roc.csv",
            "--plots-no-cache",
            "plots/debug.csv",
            "-w",
            "training",
            "--always-changed",
            "--desc",
            "Train the model",
            "--run",
            "python",
            "train.py",
            "--epochs",
            "10",
        ])
        .unwrap();

        assert_eq!(args.name, "train");
        assert!(args.force);
        assert_eq!(args.deps, vec!["src/train.py".to_owned()]);
        assert_eq!(
            args.params,
            vec![
                "seed,model.lr".to_owned(),
                "params.json:threshold".to_owned(),
                "params.toml:".to_owned()
            ]
        );
        assert_eq!(args.outs, vec![PathBuf::from("model.pkl")]);
        assert_eq!(args.outs_no_cache, vec![PathBuf::from("debug.log")]);
        assert_eq!(args.outs_persist, vec![PathBuf::from("cache")]);
        assert_eq!(args.outs_persist_no_cache, vec![PathBuf::from("scratch")]);
        assert_eq!(args.checkpoints, vec![PathBuf::from("checkpoint.pkl")]);
        assert_eq!(args.metrics, vec![PathBuf::from("scores.json")]);
        assert_eq!(
            args.metrics_no_cache,
            vec![PathBuf::from("metrics-local.json")]
        );
        assert_eq!(args.plots, vec![PathBuf::from("plots/roc.csv")]);
        assert_eq!(args.plots_no_cache, vec![PathBuf::from("plots/debug.csv")]);
        assert_eq!(args.wdir, Some(PathBuf::from("training")));
        assert!(args.always_changed);
        assert_eq!(args.desc.as_deref(), Some("Train the model"));
        assert!(args.run);
        assert_eq!(
            args.command,
            vec![
                "python".to_owned(),
                "train.py".to_owned(),
                "--epochs".to_owned(),
                "10".to_owned()
            ]
        );
    }

    #[test]
    fn stage_add_writes_checkpoint_outputs_explicitly() {
        let tmp = tempfile::tempdir().unwrap();
        let mut args = minimal_add_args("train", &["python", "train.py"]);
        args.checkpoints = vec![PathBuf::from("checkpoint.pkl")];

        run_stage_add(&args, tmp.path()).unwrap();
        let text = std::fs::read_to_string(tmp.path().join("crab.yaml")).unwrap();
        assert!(text.contains("checkpoint: true"));
        assert!(!text.contains("persist: true"));
    }

    #[test]
    fn stage_add_writes_valid_workflow_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut args = minimal_add_args("train", &["python", "train.py", "--epochs", "10"]);
        args.deps = vec![
            "src/train.py".to_owned(),
            "https://example.com/data.csv".to_owned(),
        ];
        args.params = vec![
            "seed,model.lr".to_owned(),
            "params.json:threshold".to_owned(),
            "params.toml:".to_owned(),
        ];
        args.outs = vec![PathBuf::from("model.pkl")];
        args.outs_no_cache = vec![PathBuf::from("debug.log")];
        args.outs_persist = vec![PathBuf::from("cache")];
        args.metrics = vec![PathBuf::from("scores.json")];
        args.metrics_no_cache = vec![PathBuf::from("metrics-local.json")];
        args.plots = vec![PathBuf::from("plots/roc.csv")];
        args.wdir = Some(PathBuf::from("training"));
        args.always_changed = true;
        args.desc = Some("Train the model".to_owned());

        let payload = run_stage_add(&args, root).unwrap();

        assert!(payload.created_workflow_file);
        assert!(!payload.overwritten);
        let text = std::fs::read_to_string(root.join("crab.yaml")).unwrap();
        let workflow = yaml::parse_at(&root.join("crab.yaml"), &text).unwrap();
        Graph::build(&workflow.stages).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("train").unwrap())
            .unwrap();
        assert!(matches!(
            &stage.cmd,
            crate::workflow::stage::Cmd::Shell(cmd)
                if cmd == "python train.py --epochs 10"
        ));
        assert!(stage.deps.iter().any(|dep| matches!(
            dep,
            Dep::Url { url, .. } if url == "https://example.com/data.csv"
        )));
        assert_eq!(
            stage
                .params
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>(),
            vec![
                "seed".to_owned(),
                "model.lr".to_owned(),
                "params.json:threshold".to_owned(),
                "params.toml:*".to_owned()
            ]
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| out.path == PathBuf::from("model.pkl") && out.cache)
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| out.path == PathBuf::from("debug.log") && !out.cache)
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| out.path == PathBuf::from("cache") && out.persist)
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| out.path == PathBuf::from("scores.json") && out.cache)
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| out.path == PathBuf::from("metrics-local.json") && !out.cache)
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| out.path == PathBuf::from("plots/roc.csv") && out.cache)
        );
        assert_eq!(
            stage.metrics,
            vec![
                PathBuf::from("scores.json"),
                PathBuf::from("metrics-local.json")
            ]
        );
        assert_eq!(stage.plots, vec![PathBuf::from("plots/roc.csv")]);
        assert_eq!(stage.wdir, Some(PathBuf::from("training")));
        assert!(stage.nondeterministic);
        assert_eq!(stage.desc.as_deref(), Some("Train the model"));
    }

    #[test]
    fn stage_add_writes_uncached_external_local_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let external_tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let absolute_out = external_tmp.path().join("model.pkl");
        let file_url_out = external_tmp.path().join("metrics.json");
        let file_url = url::Url::from_file_path(&file_url_out).unwrap();
        let mut args = minimal_add_args("export", &["python", "export.py"]);
        args.outs_no_cache = vec![absolute_out.clone(), PathBuf::from(file_url.as_str())];

        run_stage_add(&args, root).unwrap();

        let text = std::fs::read_to_string(root.join("crab.yaml")).unwrap();
        let workflow = yaml::parse_at(&root.join("crab.yaml"), &text).unwrap();
        Graph::build(&workflow.stages).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("export").unwrap())
            .unwrap();

        assert!(
            stage
                .outs
                .iter()
                .any(|out| { out.path == absolute_out && !out.cache && !out.push && !out.persist })
        );
        assert!(
            stage
                .outs
                .iter()
                .any(|out| { out.path == file_url_out && !out.cache && !out.push && !out.persist })
        );
    }

    #[test]
    fn stage_add_writes_uncached_external_url_output() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut args = minimal_add_args("upload", &["python", "upload.py"]);
        args.outs_no_cache = vec![PathBuf::from("s3://bucket/models/model.pkl")];

        run_stage_add(&args, root).unwrap();

        let text = std::fs::read_to_string(root.join("crab.yaml")).unwrap();
        let workflow = yaml::parse_at(&root.join("crab.yaml"), &text).unwrap();
        Graph::build(&workflow.stages).unwrap();
        let stage = workflow
            .stages
            .get(&StageName::parse("upload").unwrap())
            .unwrap();

        assert!(stage.outs.iter().any(|out| {
            out.path == PathBuf::from("s3://bucket/models/model.pkl")
                && out.is_external_url()
                && !out.cache
                && !out.push
        }));
    }

    #[test]
    fn stage_add_rejects_existing_stage_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  train:\n    cmd: python old.py\n",
        )
        .unwrap();

        let err =
            run_stage_add(&minimal_add_args("train", &["python", "new.py"]), root).unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        let text = std::fs::read_to_string(root.join("crab.yaml")).unwrap();
        assert!(text.contains("python old.py"));
        assert!(!text.contains("python new.py"));
    }

    #[test]
    fn stage_add_force_overwrites_existing_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  train:\n    cmd: python old.py\n",
        )
        .unwrap();
        let mut args = minimal_add_args("train", &["python", "new.py"]);
        args.force = true;

        let payload = run_stage_add(&args, root).unwrap();

        assert!(payload.overwritten);
        let text = std::fs::read_to_string(root.join("crab.yaml")).unwrap();
        assert!(!text.contains("python old.py"));
        assert!(text.contains("python new.py"));
    }

    #[test]
    fn stage_add_rejects_duplicate_output_graph_before_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("crab.yaml"),
            "stages:\n  extract:\n    cmd: python extract.py\n    outs: [shared.txt]\n",
        )
        .unwrap();
        let mut args = minimal_add_args("train", &["python", "train.py"]);
        args.outs = vec![PathBuf::from("shared.txt")];

        let err = run_stage_add(&args, root).unwrap_err();

        assert!(matches!(err, CrabError::WorkflowDuplicateOutput { .. }));
        let text = std::fs::read_to_string(root.join("crab.yaml")).unwrap();
        assert!(!text.contains("train.py"));
    }
}
