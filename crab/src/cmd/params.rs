//! `crab params show` / `crab params diff` — structured read
//! and diff of parameter files across git refs.
//!
//! Entry points [`exec_show`] and [`exec_diff`] are thin wrappers
//! around the pure logic in [`workflow::params`]. Output is routed
//! through the existing `core::output::Envelope<T>` so `--json`
//! matches the rest of the CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Serialize;

use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use crate::workflow::params::{
    self, RenderOptions, Scalar, ScalarDiff, ScalarMap, render_markdown, render_pr_comment,
    render_table,
};
use crab_workflow::yaml;

const SCHEMA_SHOW: &str = "params.show";
const SCHEMA_DIFF: &str = "params.diff";
const SCHEMA_VERSION: &str = "1.0";
const WORKSPACE_REF: &str = "workspace";
const DEFAULT_PARAMS_FILE: &str = "params.yaml";

/// `crab params show [--ref HEAD] [--paths params.yaml ...]`.
#[derive(Debug, Clone, Parser)]
pub struct ShowArgs {
    /// Git ref to read from. Defaults to `HEAD`.
    #[arg(long = "ref", value_name = "REF", default_value = "HEAD")]
    pub git_ref: String,

    /// Params file paths. If omitted, defaults to `params.yaml` at
    /// the repo root when it exists.
    #[arg(long = "paths", value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Output format: table, json, md, pr-comment.
    #[arg(long, value_name = "FMT", default_value = "table")]
    pub format: Format,

    /// Structured JSON output (single envelope). Equivalent to
    /// `--format=json` but routes through the `core::output`
    /// envelope shape.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `crab params diff [--targets PATH ...] [revisions ...]`.
#[derive(Debug, Clone, Parser)]
pub struct DiffArgs {
    /// Revisions to compare. None means HEAD vs workspace; one means ref vs workspace.
    #[arg(value_name = "REV")]
    pub revisions: Vec<String>,

    /// Params file targets. Defaults to params.yaml plus workflow-declared params.
    #[arg(long = "targets", alias = "paths", value_name = "PATH", num_args = 1..)]
    pub paths: Vec<PathBuf>,

    /// Output format: table, json, md, pr-comment.
    #[arg(long, value_name = "FMT", default_value = "table")]
    pub format: Format,

    /// DVC-compatible alias for `--format md`.
    #[arg(long = "md", default_value_t = false)]
    pub md: bool,

    /// Include unchanged params in rendered output and JSON.
    #[arg(long = "all", default_value_t = false)]
    pub all: bool,

    /// Include only params used as stage dependencies.
    #[arg(long = "deps", default_value_t = false)]
    pub deps: bool,

    /// Hide the path column in human-readable output.
    #[arg(long = "no-path", default_value_t = false)]
    pub no_path: bool,

    /// Structured JSON output (single envelope).
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

impl DiffArgs {
    fn comparison_refs(&self) -> Result<(String, String)> {
        match self.revisions.as_slice() {
            [] => Ok(("HEAD".to_owned(), WORKSPACE_REF.to_owned())),
            [baseline] => Ok((baseline.clone(), WORKSPACE_REF.to_owned())),
            [baseline, target] => Ok((baseline.clone(), target.clone())),
            _ => Err(CrabError::Configuration {
                key: "params diff revisions".to_owned(),
                origin: "params diff accepts at most two revisions".to_owned(),
            }),
        }
    }

    fn effective_format(&self) -> Result<Format> {
        if self.md {
            if self.format != Format::Table {
                return Err(CrabError::Configuration {
                    key: "params diff --md".to_owned(),
                    origin: "--md cannot be combined with --format".to_owned(),
                });
            }
            return Ok(Format::Md);
        }
        Ok(self.format)
    }
}

/// Rendering format for params/metrics output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Table,
    Json,
    Md,
    PrComment,
}

/// Single-envelope payload for `crab params show`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ParamsShow {
    pub git_ref: String,
    pub paths: Vec<PathBuf>,
    pub entries: std::collections::BTreeMap<String, ScalarJson>,
}

/// Single-envelope payload for `crab params diff`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ParamsDiff {
    pub ref_a: String,
    pub ref_b: String,
    pub added: BTreeMap<String, BTreeMap<String, ScalarJson>>,
    pub removed: BTreeMap<String, BTreeMap<String, ScalarJson>>,
    pub changed: BTreeMap<String, BTreeMap<String, ChangedEntry>>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub unchanged: BTreeMap<String, BTreeMap<String, ScalarJson>>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ChangedEntry {
    pub old: ScalarJson,
    pub new: ScalarJson,
}

/// JSON-safe mirror of [`Scalar`]. Untagged so the wire shape is
/// the bare value (boolean, number, string, null) rather than a
/// discriminated union.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum ScalarJson {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    /// JSON `null`. Serialized via [`Option::<()>::None`] so the
    /// untagged enum picks `null` for this variant.
    Null(Option<()>),
}

impl From<&Scalar> for ScalarJson {
    fn from(s: &Scalar) -> Self {
        match s {
            Scalar::Bool(b) => Self::Bool(*b),
            Scalar::Int(i) => Self::Int(*i),
            Scalar::Float(f) => Self::Float(*f),
            Scalar::String(s) => Self::String(s.clone()),
            Scalar::Null => Self::Null(None),
        }
    }
}

fn map_to_json(m: &ScalarMap) -> std::collections::BTreeMap<String, ScalarJson> {
    m.iter().map(|(k, v)| (k.clone(), v.into())).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ParamKey {
    path: PathBuf,
    param: String,
}

type ParamMap = BTreeMap<ParamKey, Scalar>;

#[derive(Debug, Clone, PartialEq)]
struct ParamDiff {
    added: ParamMap,
    removed: ParamMap,
    changed: BTreeMap<ParamKey, (Scalar, Scalar)>,
    unchanged: ParamMap,
}

#[derive(Debug, Clone)]
struct ParamTarget {
    path: PathBuf,
    keys: Option<BTreeSet<String>>,
}

#[derive(Debug, Clone, Copy)]
struct ParamRenderOptions {
    no_path: bool,
    include_unchanged: bool,
}

fn default_targets(
    repo_root: &Path,
    paths: &[PathBuf],
    deps_only: bool,
) -> Result<Vec<ParamTarget>> {
    if !paths.is_empty() {
        return Ok(paths
            .iter()
            .map(|path| ParamTarget {
                path: path.clone(),
                keys: None,
            })
            .collect());
    }

    let mut targets = BTreeMap::<PathBuf, Option<BTreeSet<String>>>::new();
    let yaml_path = repo_root.join("crab.yaml");
    if yaml_path.is_file() {
        let text = std::fs::read_to_string(&yaml_path).map_err(CrabError::Io)?;
        let workflow = yaml::parse_at(&yaml_path, &text)?;
        if !deps_only {
            add_full_param_target(&mut targets, PathBuf::from(DEFAULT_PARAMS_FILE));
            for path in &workflow.params {
                add_full_param_target(&mut targets, path.clone());
            }
        }

        let effective_files = if workflow.params.is_empty() {
            vec![PathBuf::from(DEFAULT_PARAMS_FILE)]
        } else {
            workflow.params.clone()
        };
        for stage in workflow.stages.values() {
            for param_ref in &stage.params {
                match (param_ref.file(), param_ref.key()) {
                    (Some(file), Some(key)) => {
                        add_key_param_target(
                            &mut targets,
                            stage_scoped_param_path(stage.wdir.as_deref(), file),
                            key,
                        );
                    }
                    (Some(file), None) => {
                        add_full_param_target(
                            &mut targets,
                            stage_scoped_param_path(stage.wdir.as_deref(), file),
                        );
                    }
                    (None, Some(key)) => {
                        if workflow.params.is_empty() {
                            add_key_param_target(
                                &mut targets,
                                stage_scoped_param_path(
                                    stage.wdir.as_deref(),
                                    Path::new(DEFAULT_PARAMS_FILE),
                                ),
                                key,
                            );
                        } else {
                            for file in &effective_files {
                                add_key_param_target(&mut targets, file.clone(), key);
                            }
                        }
                    }
                    (None, None) => {}
                }
            }
        }
    } else if !deps_only {
        add_full_param_target(&mut targets, PathBuf::from(DEFAULT_PARAMS_FILE));
    }

    Ok(targets
        .into_iter()
        .map(|(path, keys)| ParamTarget { path, keys })
        .collect())
}

fn add_full_param_target(targets: &mut BTreeMap<PathBuf, Option<BTreeSet<String>>>, path: PathBuf) {
    targets.insert(path, None);
}

fn add_key_param_target(
    targets: &mut BTreeMap<PathBuf, Option<BTreeSet<String>>>,
    path: PathBuf,
    key: &str,
) {
    match targets.entry(path).or_insert_with(|| Some(BTreeSet::new())) {
        Some(keys) => {
            keys.insert(key.to_owned());
        }
        None => {}
    }
}

fn stage_scoped_param_path(wdir: Option<&Path>, file: &Path) -> PathBuf {
    if file.is_absolute() {
        return file.to_path_buf();
    }
    match wdir {
        Some(wdir) => wdir.join(file),
        None => file.to_path_buf(),
    }
}

fn read_param_map_at_ref(
    repo_root: &Path,
    ref_name: &str,
    targets: &[ParamTarget],
) -> Result<ParamMap> {
    let mut merged = ParamMap::new();
    for target in targets {
        let Some(values) = read_param_file_at_ref(repo_root, ref_name, &target.path)? else {
            continue;
        };
        for (param, value) in values {
            if target.keys.as_ref().is_none_or(|keys| {
                keys.iter()
                    .any(|key| params::param_key_matches(key, &param))
            }) {
                merged.insert(
                    ParamKey {
                        path: target.path.clone(),
                        param,
                    },
                    value,
                );
            }
        }
    }
    Ok(merged)
}

fn read_param_file_at_ref(
    repo_root: &Path,
    ref_name: &str,
    path: &Path,
) -> Result<Option<ScalarMap>> {
    if ref_name == WORKSPACE_REF {
        let working = repo_root.join(path);
        if working.is_file() {
            let bytes = std::fs::read(working).map_err(CrabError::Io)?;
            return params::parse(&bytes, path).map(Some).map_err(Into::into);
        }
        return Ok(None);
    }

    let git_dir = params::find_git_dir(repo_root)?;
    let bytes = match params::read_blob_at_ref(&git_dir, ref_name, path)? {
        Some(bytes) => Some(bytes),
        None if ref_name == "HEAD" && repo_root.join(path).is_file() => {
            Some(std::fs::read(repo_root.join(path)).map_err(CrabError::Io)?)
        }
        None => None,
    };
    bytes
        .map(|bytes| params::parse(&bytes, path))
        .transpose()
        .map_err(Into::into)
}

fn diff_param_maps(a: &ParamMap, b: &ParamMap) -> ParamDiff {
    let mut added = ParamMap::new();
    let mut removed = ParamMap::new();
    let mut changed = BTreeMap::new();
    let mut unchanged = ParamMap::new();

    for (key, old) in a {
        match b.get(key) {
            Some(new) if old == new => {
                unchanged.insert(key.clone(), old.clone());
            }
            Some(new) => {
                changed.insert(key.clone(), (old.clone(), new.clone()));
            }
            None => {
                removed.insert(key.clone(), old.clone());
            }
        }
    }
    for (key, new) in b {
        if !a.contains_key(key) {
            added.insert(key.clone(), new.clone());
        }
    }

    ParamDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

fn param_values_to_json(map: &ParamMap) -> BTreeMap<String, BTreeMap<String, ScalarJson>> {
    let mut out = BTreeMap::<String, BTreeMap<String, ScalarJson>>::new();
    for (key, value) in map {
        out.entry(key.path.display().to_string())
            .or_default()
            .insert(key.param.clone(), value.into());
    }
    out
}

fn param_changes_to_json(
    map: &BTreeMap<ParamKey, (Scalar, Scalar)>,
) -> BTreeMap<String, BTreeMap<String, ChangedEntry>> {
    let mut out = BTreeMap::<String, BTreeMap<String, ChangedEntry>>::new();
    for (key, (old, new)) in map {
        out.entry(key.path.display().to_string())
            .or_default()
            .insert(
                key.param.clone(),
                ChangedEntry {
                    old: old.into(),
                    new: new.into(),
                },
            );
    }
    out
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Dispatch from `main.rs` for `crab params show`.
pub fn exec_show(args: ShowArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_show_in(&args, &cwd)
}

/// Dispatch from `main.rs` for `crab params diff`.
pub fn exec_diff(args: DiffArgs) -> Result<()> {
    let cwd = std::env::current_dir().map_err(CrabError::Io)?;
    run_diff_in(&args, &cwd)
}

/// Testable entry point that accepts a working directory explicitly.
pub fn run_show_in(args: &ShowArgs, repo_root: &Path) -> Result<()> {
    let map = params::read_at_ref(repo_root, &args.git_ref, &args.paths)?;
    let mode = OutputMode::from_flags(args.json, false);
    render_show(args, &map, mode);
    Ok(())
}

/// Testable entry point for `diff`.
pub fn run_diff_in(args: &DiffArgs, repo_root: &Path) -> Result<()> {
    let (ref_a, ref_b) = args.comparison_refs()?;
    let targets = default_targets(repo_root, &args.paths, args.deps)?;
    let a = read_param_map_at_ref(repo_root, &ref_a, &targets)?;
    let b = read_param_map_at_ref(repo_root, &ref_b, &targets)?;
    let d = diff_param_maps(&a, &b);
    let mode = OutputMode::from_flags(args.json, false);
    render_diff(args, &ref_a, &ref_b, &d, mode)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_show(args: &ShowArgs, map: &ScalarMap, mode: OutputMode) {
    // Envelope path: `--json` takes precedence over `--format`.
    if mode == OutputMode::Json {
        let payload = ParamsShow {
            git_ref: args.git_ref.clone(),
            paths: args.paths.clone(),
            entries: map_to_json(map),
        };
        emit_json(SCHEMA_SHOW, SCHEMA_VERSION, payload);
        return;
    }

    match args.format {
        Format::Table => {
            // Reuse the diff-table rendering by faking an empty
            // "a" side so every entry lands in `added`. Keeps the
            // output visually consistent between `show` and
            // `diff`.
            let diff = ScalarDiff {
                added: map.clone(),
                removed: ScalarMap::new(),
                changed: std::collections::BTreeMap::new(),
                unchanged: ScalarMap::new(),
            };
            print!("{}", render_table(&diff, RenderOptions::default()));
        }
        Format::Json => {
            let payload = ParamsShow {
                git_ref: args.git_ref.clone(),
                paths: args.paths.clone(),
                entries: map_to_json(map),
            };
            emit_json(SCHEMA_SHOW, SCHEMA_VERSION, payload);
        }
        Format::Md => {
            let diff = ScalarDiff {
                added: map.clone(),
                removed: ScalarMap::new(),
                changed: std::collections::BTreeMap::new(),
                unchanged: ScalarMap::new(),
            };
            print!("{}", render_markdown(&diff, RenderOptions::default()));
        }
        Format::PrComment => {
            let diff = ScalarDiff {
                added: map.clone(),
                removed: ScalarMap::new(),
                changed: std::collections::BTreeMap::new(),
                unchanged: ScalarMap::new(),
            };
            print!(
                "{}",
                render_pr_comment(&diff, RenderOptions::default(), false)
            );
        }
    }
}

fn render_diff(
    args: &DiffArgs,
    ref_a: &str,
    ref_b: &str,
    diff: &ParamDiff,
    mode: OutputMode,
) -> Result<()> {
    let format = args.effective_format()?;
    if mode == OutputMode::Json {
        emit_diff_envelope(args, ref_a, ref_b, diff);
        return Ok(());
    }

    let opts = ParamRenderOptions {
        no_path: args.no_path,
        include_unchanged: args.all,
    };
    match format {
        Format::Table => print!("{}", render_param_table(diff, ref_a, ref_b, opts)),
        Format::Json => emit_diff_envelope(args, ref_a, ref_b, diff),
        Format::Md => print!("{}", render_param_markdown(diff, ref_a, ref_b, opts)),
        Format::PrComment => print!("{}", render_param_pr_comment(diff, ref_a, ref_b, opts)),
    }
    Ok(())
}

fn emit_diff_envelope(args: &DiffArgs, ref_a: &str, ref_b: &str, diff: &ParamDiff) {
    let payload = ParamsDiff {
        ref_a: ref_a.to_owned(),
        ref_b: ref_b.to_owned(),
        added: param_values_to_json(&diff.added),
        removed: param_values_to_json(&diff.removed),
        changed: param_changes_to_json(&diff.changed),
        unchanged: if args.all {
            param_values_to_json(&diff.unchanged)
        } else {
            BTreeMap::new()
        },
    };
    emit_json(SCHEMA_DIFF, SCHEMA_VERSION, payload);
}

fn render_param_table(
    diff: &ParamDiff,
    ref_a: &str,
    ref_b: &str,
    opts: ParamRenderOptions,
) -> String {
    let mut rows = Vec::new();
    for (key, value) in &diff.added {
        rows.push(param_row(key, "-", &value.display(), opts.no_path));
    }
    for (key, value) in &diff.removed {
        rows.push(param_row(key, &value.display(), "-", opts.no_path));
    }
    for (key, (old, new)) in &diff.changed {
        rows.push(param_row(key, &old.display(), &new.display(), opts.no_path));
    }
    if opts.include_unchanged {
        for (key, value) in &diff.unchanged {
            rows.push(param_row(
                key,
                &value.display(),
                &value.display(),
                opts.no_path,
            ));
        }
    }
    if rows.is_empty() {
        return "_no changes_\n".to_owned();
    }

    let mut out = String::new();
    if opts.no_path {
        render_param_ascii_rows(&["param", ref_a, ref_b], &rows, &mut out);
    } else {
        render_param_ascii_rows(&["path", "param", ref_a, ref_b], &rows, &mut out);
    }
    out
}

fn param_row(key: &ParamKey, old: &str, new: &str, no_path: bool) -> Vec<String> {
    if no_path {
        vec![key.param.clone(), old.to_owned(), new.to_owned()]
    } else {
        vec![
            key.path.display().to_string(),
            key.param.clone(),
            old.to_owned(),
            new.to_owned(),
        ]
    }
}

fn render_param_markdown(
    diff: &ParamDiff,
    ref_a: &str,
    ref_b: &str,
    opts: ParamRenderOptions,
) -> String {
    let mut rows = Vec::new();
    for (key, value) in &diff.added {
        rows.push(param_row(key, "-", &value.display(), opts.no_path));
    }
    for (key, value) in &diff.removed {
        rows.push(param_row(key, &value.display(), "-", opts.no_path));
    }
    for (key, (old, new)) in &diff.changed {
        rows.push(param_row(key, &old.display(), &new.display(), opts.no_path));
    }
    if opts.include_unchanged {
        for (key, value) in &diff.unchanged {
            rows.push(param_row(
                key,
                &value.display(),
                &value.display(),
                opts.no_path,
            ));
        }
    }
    if rows.is_empty() {
        return "_no changes_\n".to_owned();
    }

    let mut out = String::new();
    if opts.no_path {
        let _ = writeln!(out, "| param | {ref_a} | {ref_b} |\n| --- | --- | --- |");
    } else {
        let _ = writeln!(
            out,
            "| path | param | {ref_a} | {ref_b} |\n| --- | --- | --- | --- |"
        );
    }
    for row in rows {
        let _ = writeln!(
            out,
            "| {} |",
            row.into_iter()
                .map(|cell| format!("`{cell}`"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    out
}

fn render_param_pr_comment(
    diff: &ParamDiff,
    ref_a: &str,
    ref_b: &str,
    opts: ParamRenderOptions,
) -> String {
    let mut out = String::new();
    if diff.added.is_empty()
        && diff.removed.is_empty()
        && diff.changed.is_empty()
        && !(opts.include_unchanged && !diff.unchanged.is_empty())
    {
        out.push_str("_no changes_\n");
        return out;
    }

    if !diff.added.is_empty() {
        out.push_str("**Added**\n\n");
        for (key, value) in &diff.added {
            let _ = writeln!(
                out,
                "- + `{}` = `{}`",
                param_label(key, opts.no_path),
                value.display()
            );
        }
        out.push('\n');
    }
    if !diff.removed.is_empty() {
        out.push_str("**Removed**\n\n");
        for (key, value) in &diff.removed {
            let _ = writeln!(
                out,
                "- - `{}` (was `{}`)",
                param_label(key, opts.no_path),
                value.display()
            );
        }
        out.push('\n');
    }
    if !diff.changed.is_empty() {
        out.push_str("**Changed**\n\n");
        for (key, (old, new)) in &diff.changed {
            let _ = writeln!(
                out,
                "- `{}`: `{}` ({ref_a}) -> `{}` ({ref_b})",
                param_label(key, opts.no_path),
                old.display(),
                new.display()
            );
        }
    }
    if opts.include_unchanged && !diff.unchanged.is_empty() {
        out.push_str("\n**Unchanged**\n\n");
        for (key, value) in &diff.unchanged {
            let _ = writeln!(
                out,
                "- `{}` = `{}`",
                param_label(key, opts.no_path),
                value.display()
            );
        }
    }
    out
}

fn render_param_ascii_rows(headers: &[&str], rows: &[Vec<String>], out: &mut String) {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(cell.len());
        }
    }
    for (idx, header) in headers.iter().enumerate() {
        let _ = write!(out, "{header:<width$}", width = widths[idx]);
        if idx + 1 < headers.len() {
            out.push_str("  ");
        }
    }
    out.push('\n');
    for (idx, width) in widths.iter().enumerate() {
        out.push_str(&"-".repeat(*width));
        if idx + 1 < widths.len() {
            out.push_str("  ");
        }
    }
    out.push('\n');
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            let _ = write!(out, "{cell:<width$}", width = widths[idx]);
            if idx + 1 < row.len() {
                out.push_str("  ");
            }
        }
        out.push('\n');
    }
}

fn param_label(key: &ParamKey, no_path: bool) -> String {
    if no_path {
        return key.param.clone();
    }
    format!("{}:{}", key.path.display(), key.param)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use clap::Parser;
    use std::process::Command;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn git_init(repo: &Path) {
        git(repo, &["init", "--initial-branch=main"]);
        git(repo, &["config", "user.email", "t@test.com"]);
        git(repo, &["config", "user.name", "Test"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            // Tests in other modules may have set `GIT_DIR` to a
            // shared test repo via `GitDirGuard`. Clearing both
            // env vars makes subprocess git commands honor only
            // the explicit `current_dir`.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn run_show_reads_params_at_head() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::write(repo.join("params.yaml"), b"lr: 0.1\nepochs: 3\n").unwrap();
        git(repo, &["add", "params.yaml"]);
        git(repo, &["commit", "-m", "init"]);

        let args = ShowArgs {
            git_ref: "HEAD".into(),
            paths: vec![],
            format: Format::Table,
            json: false,
        };
        run_show_in(&args, repo).unwrap();
    }

    #[test]
    fn run_diff_between_branches() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_init(repo);
        std::fs::write(repo.join("params.yaml"), b"lr: 0.1\n").unwrap();
        git(repo, &["add", "params.yaml"]);
        git(repo, &["commit", "-m", "a"]);
        git(repo, &["checkout", "-b", "b"]);
        std::fs::write(repo.join("params.yaml"), b"lr: 0.2\n").unwrap();
        git(repo, &["commit", "-am", "b"]);

        let args = DiffArgs {
            revisions: vec!["main".into(), "b".into()],
            paths: vec![],
            format: Format::Table,
            md: false,
            all: false,
            deps: false,
            no_path: false,
            json: false,
        };
        run_diff_in(&args, repo).unwrap();
    }

    #[test]
    fn diff_args_parse_dvc_targets_and_revisions() {
        let args = DiffArgs::try_parse_from([
            "diff",
            "--targets",
            "params.yaml",
            "conf/model.yaml",
            "--all",
            "--md",
            "--no-path",
            "--",
            "main",
            "candidate",
        ])
        .unwrap();

        assert_eq!(
            args.paths,
            vec![
                PathBuf::from("params.yaml"),
                PathBuf::from("conf/model.yaml")
            ]
        );
        assert!(args.all);
        assert!(args.md);
        assert!(args.no_path);
        assert_eq!(args.revisions, vec!["main", "candidate"]);
        assert_eq!(
            args.comparison_refs().unwrap(),
            ("main".to_owned(), "candidate".to_owned())
        );
    }

    #[test]
    fn diff_args_default_to_head_vs_workspace() {
        let args = DiffArgs::try_parse_from(["diff"]).unwrap();
        assert_eq!(
            args.comparison_refs().unwrap(),
            ("HEAD".to_owned(), WORKSPACE_REF.to_owned())
        );

        let args = DiffArgs::try_parse_from(["diff", "main"]).unwrap();
        assert_eq!(
            args.comparison_refs().unwrap(),
            ("main".to_owned(), WORKSPACE_REF.to_owned())
        );
    }

    #[test]
    fn default_targets_include_workflow_and_stage_param_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(
            repo.join("crab.yaml"),
            "params:\n  - conf/global.yaml\nstages:\n  train:\n    cmd: \"true\"\n    wdir: models\n    params:\n      - model.lr\n      - train.yaml:\n          - epochs\n      - all.json:\n",
        )
        .unwrap();

        let targets = default_targets(repo, &[], false).unwrap();
        let rendered = targets
            .iter()
            .map(|target| {
                (
                    target.path.clone(),
                    target
                        .keys
                        .as_ref()
                        .map(|keys| keys.iter().cloned().collect::<Vec<_>>()),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                (PathBuf::from("conf/global.yaml"), None),
                (PathBuf::from("models/all.json"), None),
                (
                    PathBuf::from("models/train.yaml"),
                    Some(vec!["epochs".to_owned()])
                ),
                (PathBuf::from("params.yaml"), None),
            ]
        );

        let deps = default_targets(repo, &[], true).unwrap();
        assert_eq!(
            deps.iter()
                .map(|target| target.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("conf/global.yaml"),
                PathBuf::from("models/all.json"),
                PathBuf::from("models/train.yaml"),
            ]
        );
    }

    #[test]
    fn params_diff_target_keys_expand_subtrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::fs::write(
            repo.join("params.yaml"),
            b"lr: 0.01\ntrain:\n  epochs: 10\n  layers: 3\n",
        )
        .unwrap();

        let targets = vec![ParamTarget {
            path: PathBuf::from("params.yaml"),
            keys: Some(BTreeSet::from(["train".to_owned()])),
        }];
        let map = read_param_map_at_ref(repo, WORKSPACE_REF, &targets).unwrap();

        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&ParamKey {
            path: PathBuf::from("params.yaml"),
            param: "train.epochs".to_owned(),
        }));
        assert!(map.contains_key(&ParamKey {
            path: PathBuf::from("params.yaml"),
            param: "train.layers".to_owned(),
        }));
    }

    #[test]
    fn params_diff_keeps_same_param_names_scoped_by_path() {
        let mut old = ParamMap::new();
        old.insert(
            ParamKey {
                path: PathBuf::from("params.yaml"),
                param: "lr".to_owned(),
            },
            Scalar::Float(0.1),
        );
        old.insert(
            ParamKey {
                path: PathBuf::from("conf/model.yaml"),
                param: "lr".to_owned(),
            },
            Scalar::Float(0.01),
        );

        let mut new = ParamMap::new();
        new.insert(
            ParamKey {
                path: PathBuf::from("params.yaml"),
                param: "lr".to_owned(),
            },
            Scalar::Float(0.2),
        );
        new.insert(
            ParamKey {
                path: PathBuf::from("conf/model.yaml"),
                param: "lr".to_owned(),
            },
            Scalar::Float(0.02),
        );

        let diff = diff_param_maps(&old, &new);
        assert_eq!(diff.changed.len(), 2);
        assert!(diff.changed.contains_key(&ParamKey {
            path: PathBuf::from("params.yaml"),
            param: "lr".to_owned(),
        }));
        assert!(diff.changed.contains_key(&ParamKey {
            path: PathBuf::from("conf/model.yaml"),
            param: "lr".to_owned(),
        }));
    }

    #[test]
    fn params_diff_table_hides_path_only_when_requested() {
        let mut old = ParamMap::new();
        old.insert(
            ParamKey {
                path: PathBuf::from("params.yaml"),
                param: "lr".to_owned(),
            },
            Scalar::Float(0.1),
        );
        let mut new = ParamMap::new();
        new.insert(
            ParamKey {
                path: PathBuf::from("params.yaml"),
                param: "lr".to_owned(),
            },
            Scalar::Float(0.2),
        );
        let diff = diff_param_maps(&old, &new);

        let with_path = render_param_table(
            &diff,
            "HEAD",
            "workspace",
            ParamRenderOptions {
                no_path: false,
                include_unchanged: false,
            },
        );
        assert!(with_path.contains("params.yaml"));

        let without_path = render_param_table(
            &diff,
            "HEAD",
            "workspace",
            ParamRenderOptions {
                no_path: true,
                include_unchanged: false,
            },
        );
        assert!(!without_path.contains("params.yaml"));
        assert!(without_path.contains("lr"));
    }

    #[test]
    fn scalar_json_roundtrip_shapes() {
        let cases = [
            (Scalar::Bool(true), "true"),
            (Scalar::Int(42), "42"),
            (Scalar::String("abc".into()), "\"abc\""),
        ];
        for (s, expected) in cases {
            let j: ScalarJson = (&s).into();
            let out = serde_json::to_string(&j).unwrap();
            assert_eq!(out, expected, "mapping for {s:?}");
        }
        // Float carries its own display.
        let f = ScalarJson::from(&Scalar::Float(0.25));
        assert_eq!(serde_json::to_string(&f).unwrap(), "0.25");
        // Null serializes as JSON null.
        let n = ScalarJson::from(&Scalar::Null);
        assert_eq!(serde_json::to_string(&n).unwrap(), "null");
    }
}
