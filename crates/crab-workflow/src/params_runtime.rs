//! Params and metrics: read, diff, render.
//!
//! Parsing and flattened scalar contracts live in `crab-workflow`; this Module
//! keeps the CLI/runtime Adapters that read params from the working tree or Git,
//! resolve stage parameter references, and render command output.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "gix-facade"))]
use std::process::Command;

use gix_hash::ObjectId;
use gix_object::{Find, FindExt};
use serde::Serialize;

use crate::params::{Scalar, ScalarMap, parse as parse_contract};
use crate::stage::ParamRef;
use crate::{Result, WorkflowError as CrabError};

const DEFAULT_PARAMS_FILE: &str = "params.yaml";

fn parse(bytes: &[u8], path: &Path) -> Result<ScalarMap> {
    parse_contract(bytes, path).map_err(runtime_params_error)
}

#[cfg(test)]
fn parse_yaml(text: &str) -> Result<ScalarMap> {
    crate::params::parse_yaml(text).map_err(runtime_params_error)
}

#[cfg(test)]
fn parse_json(text: &str) -> Result<ScalarMap> {
    crate::params::parse_json(text).map_err(runtime_params_error)
}

#[cfg(test)]
fn parse_toml(text: &str) -> Result<ScalarMap> {
    crate::params::parse_toml(text).map_err(runtime_params_error)
}

#[cfg(test)]
fn parse_python(text: &str) -> Result<ScalarMap> {
    crate::params::parse_python(text).map_err(runtime_params_error)
}

fn runtime_params_error(error: crate::WorkflowError) -> CrabError {
    match error {
        crate::WorkflowError::ParamsInvalid { key, origin } => {
            CrabError::Configuration { key, origin }
        }
        other => other,
    }
}

/// Resolve a stage's declared param refs against working-tree params files.
///
/// When the workflow does not declare top-level params files, stage refs
/// default to `params.yaml`, matching DVC's common `params: [model.lr]`
/// form. File-scoped refs read only their declared file. Returned keys
/// are stable lockfile keys and values are scalar display strings that
/// participate in stage hashing and lockfiles.
pub fn resolve_stage_param_values(
    repo_root: &Path,
    declared_files: &[PathBuf],
    refs: &[ParamRef],
    stage_name: &str,
) -> Result<BTreeMap<String, String>> {
    resolve_stage_param_values_with_wdir(repo_root, declared_files, refs, stage_name, None)
}

/// Resolve a stage's declared param refs with DVC-compatible `wdir` semantics.
///
/// Bare refs use workflow-level params files when explicitly declared; otherwise
/// they default to `params.yaml` under `wdir` when set. File-scoped stage refs
/// are relative to `wdir`.
pub fn resolve_stage_param_values_with_wdir(
    repo_root: &Path,
    declared_files: &[PathBuf],
    refs: &[ParamRef],
    stage_name: &str,
    wdir: Option<&Path>,
) -> Result<BTreeMap<String, String>> {
    if refs.is_empty() {
        return Ok(BTreeMap::new());
    }

    let effective_files = if declared_files.is_empty() {
        vec![stage_param_file_path(Path::new(DEFAULT_PARAMS_FILE), wdir)]
    } else {
        declared_files.to_vec()
    };
    let mut merged_values: Option<ScalarMap> = None;

    let mut resolved = BTreeMap::new();
    for param_ref in refs {
        match (param_ref.file(), param_ref.key()) {
            (None, Some(key)) => {
                if merged_values.is_none() {
                    merged_values = Some(read_working_tree_files(repo_root, &effective_files)?);
                }
                let Some(values) = merged_values.as_ref() else {
                    return Err(CrabError::Configuration {
                        key: format!("stage '{stage_name}' params"),
                        origin: "params files were not loaded".to_owned(),
                    });
                };
                let mut found = false;
                for (matched_key, value) in values
                    .iter()
                    .filter(|(candidate, _)| param_key_matches(key, candidate))
                {
                    resolved.insert(param_ref.lock_key_for(matched_key), value.display());
                    found = true;
                }
                if !found {
                    return Err(CrabError::Configuration {
                        key: format!("stage '{stage_name}' param '{key}'"),
                        origin: format!("not found in {}", format_path_list(&effective_files)),
                    });
                }
            }
            (Some(file), Some(key)) => {
                let file_path = stage_param_file_path(file, wdir);
                let values = read_working_tree_files(repo_root, std::slice::from_ref(&file_path))?;
                let mut found = false;
                for (matched_key, value) in values
                    .iter()
                    .filter(|(candidate, _)| param_key_matches(key, candidate))
                {
                    resolved.insert(
                        param_lock_key(param_ref, matched_key, wdir),
                        value.display(),
                    );
                    found = true;
                }
                if !found {
                    return Err(CrabError::Configuration {
                        key: format!("stage '{stage_name}' param '{}:{key}'", file_path.display()),
                        origin: format!("not found in {}", file_path.display()),
                    });
                }
            }
            (Some(file), None) => {
                let file_path = stage_param_file_path(file, wdir);
                let values = read_working_tree_files(repo_root, std::slice::from_ref(&file_path))?;
                for (key, value) in values {
                    resolved.insert(param_lock_key(param_ref, &key, wdir), value.display());
                }
            }
            (None, None) => {}
        }
    }
    Ok(resolved)
}

fn stage_param_file_path(file: &Path, wdir: Option<&Path>) -> PathBuf {
    if file.is_absolute() {
        return file.to_path_buf();
    }
    match wdir {
        Some(wdir) => wdir.join(file),
        None => file.to_path_buf(),
    }
}

fn param_lock_key(param_ref: &ParamRef, key: &str, wdir: Option<&Path>) -> String {
    match param_ref.file() {
        Some(file) => {
            let file = stage_param_file_path(file, wdir);
            format!("{}:{key}", file.display())
        }
        None => key.to_owned(),
    }
}

/// Returns whether `key` is the selected param or one of its dotted children.
#[must_use]
pub fn param_key_matches(selector: &str, key: &str) -> bool {
    key == selector
        || key
            .strip_prefix(selector)
            .is_some_and(|suffix| suffix.starts_with('.'))
}

/// Read and merge params files from the working tree.
///
/// Later files override earlier files on key conflicts, matching the
/// template context merge order used by `crab.yaml` parsing.
pub fn read_working_tree_files(repo_root: &Path, paths: &[PathBuf]) -> Result<ScalarMap> {
    let mut merged = ScalarMap::new();
    for path in paths {
        let full_path = repo_root.join(path);
        let bytes = std::fs::read(&full_path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CrabError::StageDepMissing {
                    stage: "params".into(),
                    path: path.clone(),
                }
            } else {
                CrabError::Io(e)
            }
        })?;
        let map = parse(&bytes, path)?;
        for (key, value) in map {
            merged.insert(key, value);
        }
    }
    Ok(merged)
}

fn format_path_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Read declared param files at a git ref and merge them into a
/// single [`ScalarMap`].
///
/// * `repo_root` is the working-tree root (the directory that
///   contains `.git`).
/// * `ref_name` is anything `git rev-parse` resolves — a branch, a
///   tag, a full SHA, or `HEAD`.
/// * `paths` are repo-relative paths to param files. An empty slice
///   defaults to `["params.yaml"]` if that path exists in the tree.
///
/// When `ref_name == "HEAD"` and a declared path is not tracked at
/// HEAD, we read from the working-tree file instead. This mirrors
/// the common case where a fresh params file hasn't been committed
/// yet — `crab params show` should still return it.
pub fn read_at_ref(repo_root: &Path, ref_name: &str, paths: &[PathBuf]) -> Result<ScalarMap> {
    if ref_name == "workspace" {
        let effective_paths = if paths.is_empty() {
            let candidate = PathBuf::from(DEFAULT_PARAMS_FILE);
            if repo_root.join(&candidate).is_file() {
                vec![candidate]
            } else {
                return Ok(ScalarMap::new());
            }
        } else {
            paths.to_vec()
        };
        return read_working_tree_files(repo_root, &effective_paths);
    }

    let git_dir = find_git_dir(repo_root)?;

    let effective_paths: Vec<PathBuf> = if paths.is_empty() {
        // Default: `params.yaml` at the root if any version exists
        // — either committed to the ref or present in the working
        // tree.
        let candidate = PathBuf::from("params.yaml");
        if path_exists_at_ref(&git_dir, ref_name, &candidate)?
            || repo_root.join(&candidate).is_file()
        {
            vec![candidate]
        } else {
            return Ok(ScalarMap::new());
        }
    } else {
        paths.to_vec()
    };

    let mut merged = ScalarMap::new();
    for path in &effective_paths {
        let bytes = read_blob_at_ref(&git_dir, ref_name, path)?;
        let bytes = match bytes {
            Some(b) => b,
            None => {
                // Fallback: HEAD + untracked working-tree file.
                if ref_name == "HEAD" {
                    let working = repo_root.join(path);
                    if working.is_file() {
                        std::fs::read(&working).map_err(CrabError::Io)?
                    } else {
                        return Err(CrabError::StageDepMissing {
                            stage: "params".into(),
                            path: path.clone(),
                        });
                    }
                } else {
                    return Err(CrabError::StageDepMissing {
                        stage: "params".into(),
                        path: path.clone(),
                    });
                }
            }
        };
        let map = parse(&bytes, path)?;
        for (k, v) in map {
            merged.insert(k, v);
        }
    }
    Ok(merged)
}

/// Resolve the `.git` directory for `repo_root` by walking upwards
/// via `gix-discover`. Unlike [`crate::git::discover::discover_git_dir`],
/// this does not consult the `GIT_DIR` env var: callers pass an
/// explicit `repo_root` and we honor that, so env-var-driven
/// redirection (used by other tests in this crate) doesn't leak in.
pub fn find_git_dir(repo_root: &Path) -> Result<PathBuf> {
    match gix_discover::upwards(repo_root) {
        Ok((repo_path, _trust)) => {
            let (git_dir, _wt) = repo_path.into_repository_and_work_tree_directories();
            Ok(git_dir)
        }
        Err(e) => Err(CrabError::Internal(format!(
            "failed to discover .git directory under {}: {e}",
            repo_root.display()
        ))),
    }
}

/// Resolve a refspec to a commit OID. Returns `None` if the ref
/// does not exist (caller decides whether that's fatal).
///
/// On `--features gix-facade`, uses `gix::Repository::rev_parse_single()`.
/// Default builds shell out to `git rev-parse --verify`, scoping
/// the process to `git_dir` so tests that set `GIT_DIR` elsewhere
/// don't redirect us to the wrong repo.
fn rev_parse(git_dir: &Path, refspec: &str) -> Result<Option<String>> {
    #[cfg(feature = "gix-facade")]
    {
        let repo = gix::open(git_dir).map_err(|error| CrabError::Internal(error.to_string()))?;
        let sha = repo
            .rev_parse_single(refspec)
            .ok()
            .map(|id| id.to_hex().to_string());
        if let Some(ref s) = sha
            && s.len() != 40
        {
            return Err(CrabError::Internal(format!(
                "rev_parse returned unexpected output for '{refspec}': {s}"
            )));
        }
        Ok(sha)
    }

    #[cfg(not(feature = "gix-facade"))]
    {
        let work_dir = git_dir.parent().unwrap_or(Path::new("."));
        let output = Command::new("git")
            .args(["rev-parse", "--verify", refspec])
            .current_dir(work_dir)
            .env("GIT_DIR", git_dir)
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;
        if !output.status.success() {
            return Ok(None);
        }
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if sha.len() != 40 {
            return Err(CrabError::Internal(format!(
                "git rev-parse returned unexpected output for '{refspec}': {sha}"
            )));
        }
        Ok(Some(sha))
    }
}

/// Return the blob bytes for `path` at `ref_name`, or `None` if the
/// path is not tracked at that ref.
pub fn read_blob_at_ref(git_dir: &Path, ref_name: &str, path: &Path) -> Result<Option<Vec<u8>>> {
    let Some(commit_hex) = rev_parse(git_dir, ref_name)? else {
        return Err(CrabError::NotFound {
            path: ref_name.to_owned(),
        });
    };

    let objects_dir = git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("git objects directory not found: {}", objects_dir.display()),
        )));
    }
    let odb = gix_odb::at(&objects_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open git ODB at {}: {e}",
            objects_dir.display()
        ))
    })?;

    let commit_oid = ObjectId::from_hex(commit_hex.as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid commit OID {commit_hex}: {e}")))?;

    // Resolve commit → root tree.
    let tree_id = {
        let mut buf = Vec::new();
        let mut iter = odb
            .find_commit_iter(&commit_oid, &mut buf)
            .map_err(|e| CrabError::Internal(format!("failed to read commit {commit_hex}: {e}")))?;
        iter.tree_id().map_err(|e| {
            CrabError::Internal(format!(
                "failed to parse tree from commit {commit_hex}: {e}"
            ))
        })?
    };

    // Walk the path components one tree at a time.
    let mut current_tree = tree_id;
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => n.to_str(),
            _ => None,
        })
        .collect();
    if components.is_empty() {
        return Err(CrabError::Configuration {
            key: format!("empty path for ref read: {}", path.display()),
            origin: "params".into(),
        });
    }

    for (i, component) in components.iter().enumerate() {
        let last = i == components.len() - 1;
        let mut buf = Vec::new();
        let tree_iter = match odb.find_tree_iter(&current_tree, &mut buf) {
            Ok(t) => t,
            Err(e) => {
                return Err(CrabError::Internal(format!(
                    "failed to read tree {current_tree}: {e}"
                )));
            }
        };
        let mut matched: Option<(ObjectId, gix_object::tree::EntryKind)> = None;
        for entry_result in tree_iter {
            let entry = entry_result
                .map_err(|e| CrabError::Internal(format!("corrupt tree {current_tree}: {e}")))?;
            let name_bytes: &[u8] = entry.filename.as_ref();
            if name_bytes == component.as_bytes() {
                matched = Some((entry.oid.to_owned(), entry.mode.kind()));
                break;
            }
        }

        let Some((oid, kind)) = matched else {
            return Ok(None);
        };

        if last {
            if !matches!(
                kind,
                gix_object::tree::EntryKind::Blob | gix_object::tree::EntryKind::BlobExecutable
            ) {
                return Err(CrabError::Configuration {
                    key: format!("{} at ref {ref_name} is not a regular file", path.display()),
                    origin: "params".into(),
                });
            }
            let mut blob_buf = Vec::new();
            let data = odb
                .try_find(&oid, &mut blob_buf)
                .map_err(|e| CrabError::Internal(format!("failed to read blob {oid}: {e}")))?
                .ok_or_else(|| CrabError::Internal(format!("blob {oid} missing from ODB")))?;
            if data.kind != gix_object::Kind::Blob {
                return Err(CrabError::Internal(format!("oid {oid} is not a blob")));
            }
            return Ok(Some(data.data.to_vec()));
        }

        if !matches!(kind, gix_object::tree::EntryKind::Tree) {
            return Ok(None);
        }
        current_tree = oid;
    }

    Ok(None)
}

fn path_exists_at_ref(git_dir: &Path, ref_name: &str, path: &Path) -> Result<bool> {
    Ok(read_blob_at_ref(git_dir, ref_name, path)?.is_some())
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

/// Structured diff between two [`ScalarMap`]s.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScalarDiff {
    /// Keys present only in `b`.
    pub added: ScalarMap,
    /// Keys present only in `a`.
    pub removed: ScalarMap,
    /// Keys present in both with different values: `(old, new)`.
    pub changed: BTreeMap<String, (Scalar, Scalar)>,
    /// Keys present in both with identical values.
    pub unchanged: ScalarMap,
}

impl ScalarDiff {
    /// `true` iff `added`, `removed`, and `changed` are all empty.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Compute the diff between two [`ScalarMap`]s.
pub fn diff(a: &ScalarMap, b: &ScalarMap) -> ScalarDiff {
    let mut added = ScalarMap::new();
    let mut removed = ScalarMap::new();
    let mut changed = BTreeMap::new();
    let mut unchanged = ScalarMap::new();

    for (k, va) in a {
        match b.get(k) {
            Some(vb) if va == vb => {
                unchanged.insert(k.clone(), va.clone());
            }
            Some(vb) => {
                changed.insert(k.clone(), (va.clone(), vb.clone()));
            }
            None => {
                removed.insert(k.clone(), va.clone());
            }
        }
    }
    for (k, vb) in b {
        if !a.contains_key(k) {
            added.insert(k.clone(), vb.clone());
        }
    }
    ScalarDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

/// Configuration for [`render_table`] / [`render_markdown`] /
/// [`render_pr_comment`]. `metrics_mode` enables numeric delta +
/// percent annotations on the `changed` section.
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// When `true`, changed rows display `abs Δ` and `% Δ` columns
    /// for numeric scalars. Non-numeric changed rows leave the
    /// delta cells empty.
    pub metrics_mode: bool,
    /// When `true`, unchanged keys are included in rendered output.
    pub include_unchanged: bool,
}

/// Render a [`ScalarDiff`] as an ASCII table. Three sections:
/// `added`, `removed`, `changed`. An empty section emits a single
/// `(none)` row so renderers never go silent on the user.
pub fn render_table(diff: &ScalarDiff, opts: RenderOptions) -> String {
    let mut out = String::new();

    out.push_str("=== Added ===\n");
    if diff.added.is_empty() {
        out.push_str("(none)\n");
    } else {
        let rows: Vec<[String; 2]> = diff
            .added
            .iter()
            .map(|(k, v)| [k.clone(), v.display()])
            .collect();
        render_ascii_rows(&["key", "value"], &rows, &mut out);
    }
    out.push('\n');

    out.push_str("=== Removed ===\n");
    if diff.removed.is_empty() {
        out.push_str("(none)\n");
    } else {
        let rows: Vec<[String; 2]> = diff
            .removed
            .iter()
            .map(|(k, v)| [k.clone(), v.display()])
            .collect();
        render_ascii_rows(&["key", "value"], &rows, &mut out);
    }
    out.push('\n');

    out.push_str("=== Changed ===\n");
    if diff.changed.is_empty() {
        out.push_str("(none)\n");
    } else if opts.metrics_mode {
        let rows: Vec<[String; 5]> = diff
            .changed
            .iter()
            .map(|(k, (old, new))| {
                let (abs_delta, pct_delta) = numeric_delta(old, new);
                [
                    k.clone(),
                    old.display(),
                    new.display(),
                    abs_delta,
                    pct_delta,
                ]
            })
            .collect();
        render_ascii_rows(&["key", "old", "new", "abs Δ", "% Δ"], &rows, &mut out);
    } else {
        let rows: Vec<[String; 3]> = diff
            .changed
            .iter()
            .map(|(k, (old, new))| [k.clone(), old.display(), new.display()])
            .collect();
        render_ascii_rows(&["key", "old", "new"], &rows, &mut out);
    }
    if opts.include_unchanged {
        out.push('\n');
        out.push_str("=== Unchanged ===\n");
        if diff.unchanged.is_empty() {
            out.push_str("(none)\n");
        } else {
            let rows: Vec<[String; 2]> = diff
                .unchanged
                .iter()
                .map(|(k, v)| [k.clone(), v.display()])
                .collect();
            render_ascii_rows(&["key", "value"], &rows, &mut out);
        }
    }
    if opts.include_unchanged && !diff.unchanged.is_empty() {
        out.push_str("**Unchanged**\n\n");
        for (k, v) in &diff.unchanged {
            let _ = writeln!(out, "- `{k}` = `{}`", v.display());
        }
        out.push('\n');
    }
    out
}

/// Render a [`ScalarDiff`] as JSON. The output shape matches the
/// envelope `data` payload — a sibling of, not a replacement for,
/// [`Envelope`](crate::core::output::Envelope).
pub fn render_json(diff: &ScalarDiff) -> String {
    serde_json::to_string_pretty(diff).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// Render a [`ScalarDiff`] as GitHub-flavored markdown tables.
pub fn render_markdown(diff: &ScalarDiff, opts: RenderOptions) -> String {
    let mut out = String::new();

    out.push_str("### Added\n\n");
    if diff.added.is_empty() {
        out.push_str("_none_\n\n");
    } else {
        out.push_str("| key | value |\n| --- | --- |\n");
        for (k, v) in &diff.added {
            let _ = writeln!(out, "| `{k}` | `{}` |", v.display());
        }
        out.push('\n');
    }

    out.push_str("### Removed\n\n");
    if diff.removed.is_empty() {
        out.push_str("_none_\n\n");
    } else {
        out.push_str("| key | value |\n| --- | --- |\n");
        for (k, v) in &diff.removed {
            let _ = writeln!(out, "| `{k}` | `{}` |", v.display());
        }
        out.push('\n');
    }

    out.push_str("### Changed\n\n");
    if diff.changed.is_empty() {
        out.push_str("_none_\n");
    } else if opts.metrics_mode {
        out.push_str("| key | old | new | Δ | % |\n| --- | --- | --- | --- | --- |\n");
        for (k, (old, new)) in &diff.changed {
            let (abs_delta, pct_delta) = numeric_delta(old, new);
            let _ = writeln!(
                out,
                "| `{k}` | `{}` | `{}` | {abs_delta} | {pct_delta} |",
                old.display(),
                new.display(),
            );
        }
    } else {
        out.push_str("| key | old | new |\n| --- | --- | --- |\n");
        for (k, (old, new)) in &diff.changed {
            let _ = writeln!(out, "| `{k}` | `{}` | `{}` |", old.display(), new.display(),);
        }
    }
    if opts.include_unchanged {
        out.push_str("\n### Unchanged\n\n");
        if diff.unchanged.is_empty() {
            out.push_str("_none_\n");
        } else {
            out.push_str("| key | value |\n| --- | --- |\n");
            for (k, v) in &diff.unchanged {
                let _ = writeln!(out, "| `{k}` | `{}` |", v.display());
            }
        }
    }
    out
}

/// Render a [`ScalarDiff`] as a PR-comment-ready markdown snippet.
///
/// Gains and regressions are visually distinguished with emoji and
/// directional arrows. When `metrics_mode` is on, `higher_is_better`
/// controls whether a positive delta is a ✓ (gain) or ✗ (regression).
/// For params-mode renders, changes are simply flagged `•` — callers
/// have no signal on what "better" means for a threshold or flag.
pub fn render_pr_comment(diff: &ScalarDiff, opts: RenderOptions, higher_is_better: bool) -> String {
    let mut out = String::new();

    if diff.is_empty() {
        out.push_str("_no changes_\n");
        return out;
    }

    if !diff.added.is_empty() {
        out.push_str("**Added**\n\n");
        for (k, v) in &diff.added {
            let _ = writeln!(out, "- ➕ `{k}` = `{}`", v.display());
        }
        out.push('\n');
    }

    if !diff.removed.is_empty() {
        out.push_str("**Removed**\n\n");
        for (k, v) in &diff.removed {
            let _ = writeln!(out, "- ➖ `{k}` (was `{}`)", v.display());
        }
        out.push('\n');
    }

    if !diff.changed.is_empty() {
        out.push_str("**Changed**\n\n");
        for (k, (old, new)) in &diff.changed {
            let marker = if opts.metrics_mode {
                match (old.as_f64(), new.as_f64()) {
                    (Some(a), Some(b)) => {
                        let improved = (b > a) == higher_is_better;
                        if (a - b).abs() < f64::EPSILON {
                            "•"
                        } else if improved {
                            "✓"
                        } else {
                            "✗"
                        }
                    }
                    _ => "•",
                }
            } else {
                "•"
            };
            if opts.metrics_mode {
                let (abs_delta, pct_delta) = numeric_delta(old, new);
                let _ = writeln!(
                    out,
                    "- {marker} `{k}`: `{}` → `{}` ({abs_delta} / {pct_delta})",
                    old.display(),
                    new.display(),
                );
            } else {
                let _ = writeln!(
                    out,
                    "- {marker} `{k}`: `{}` → `{}`",
                    old.display(),
                    new.display(),
                );
            }
        }
    }
    out
}

/// Compute `(abs_delta, pct_delta)` strings for a numeric change.
/// Returns `("", "")` for non-numeric scalars so renderers can
/// leave those columns blank.
fn numeric_delta(old: &Scalar, new: &Scalar) -> (String, String) {
    match (old.as_f64(), new.as_f64()) {
        (Some(a), Some(b)) => {
            let abs = b - a;
            let sign = if abs >= 0.0 { "+" } else { "" };
            let abs_str = format!("{sign}{}", format_delta_float(abs));
            let pct_str = if a.abs() < f64::EPSILON {
                // Avoid divide-by-zero while still conveying the
                // direction of the change.
                if abs.abs() < f64::EPSILON {
                    "+0.00%".to_owned()
                } else if abs > 0.0 {
                    "+∞%".to_owned()
                } else {
                    "-∞%".to_owned()
                }
            } else {
                let pct = (abs / a) * 100.0;
                let sign = if pct >= 0.0 { "+" } else { "" };
                format!("{sign}{pct:.2}%")
            };
            (abs_str, pct_str)
        }
        _ => (String::new(), String::new()),
    }
}

fn format_delta_float(f: f64) -> String {
    // Use a reasonable precision for metrics (4 decimal places) but
    // drop trailing zeros so small integer deltas render cleanly.
    let s = format!("{f:.4}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Render `rows` as a fixed-width ASCII table into `out`. The
/// columns are padded to the widest entry in each column.
fn render_ascii_rows<const N: usize>(headers: &[&str; N], rows: &[[String; N]], out: &mut String) {
    let mut widths = [0usize; N];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = widths[i].max(h.len());
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    // Header row.
    for (i, h) in headers.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let pad = widths[i] - h.len();
        out.push_str(h);
        out.push_str(&" ".repeat(pad));
    }
    out.push('\n');

    // Separator.
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        out.push_str(&"-".repeat(*w));
    }
    out.push('\n');

    // Data rows.
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let pad = widths[i] - cell.len();
            out.push_str(cell);
            out.push_str(&" ".repeat(pad));
        }
        out.push('\n');
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

    // --- Parsers ---

    #[test]
    fn parse_yaml_flattens_nested_maps() {
        let text = "model:\n  lr: 0.01\n  epochs: 5\n";
        let map = parse_yaml(text).unwrap();
        assert_eq!(map.get("model.lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("model.epochs"), Some(&Scalar::Int(5)));
    }

    #[test]
    fn parse_yaml_flattens_arrays_to_indexed_keys() {
        let text = "widths: [64, 128, 256]\n";
        let map = parse_yaml(text).unwrap();
        assert_eq!(map.get("widths.0"), Some(&Scalar::Int(64)));
        assert_eq!(map.get("widths.1"), Some(&Scalar::Int(128)));
        assert_eq!(map.get("widths.2"), Some(&Scalar::Int(256)));
    }

    #[test]
    fn parse_json_flattens_nested_objects() {
        let text = r#"{"model": {"lr": 0.01, "epochs": 5}, "name": "resnet"}"#;
        let map = parse_json(text).unwrap();
        assert_eq!(map.get("model.lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("model.epochs"), Some(&Scalar::Int(5)));
        assert_eq!(map.get("name"), Some(&Scalar::String("resnet".into())));
    }

    #[test]
    fn parse_toml_flattens_tables() {
        let text = "[model]\nlr = 0.01\nepochs = 5\n";
        let map = parse_toml(text).unwrap();
        assert_eq!(map.get("model.lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("model.epochs"), Some(&Scalar::Int(5)));
    }

    #[test]
    fn parse_python_flattens_literal_assignments() {
        let text = r#"
import os

lr = 0.01
enabled = True
name = 'resnet'
missing = None
widths = [64, 128]
model = {
    "layers": (2, 3),
    "dropout": 0.2,
}
optim = dict(kind="adam", weight_decay=0.001)
dynamic = os.getenv("DYNAMIC")

class TrainConfig:
    EPOCHS = 70

    def __init__(self):
        self.layers = 5
        self.layers = 9
        self.sum = 1 + 2
        local = 3

class TestConfig:
    TEST_DIR = 'path'
    METRICS = ['metric']
"#;
        let map = parse_python(text).unwrap();
        assert_eq!(map.get("lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("enabled"), Some(&Scalar::Bool(true)));
        assert_eq!(map.get("name"), Some(&Scalar::String("resnet".into())));
        assert_eq!(map.get("missing"), Some(&Scalar::Null));
        assert_eq!(map.get("widths.0"), Some(&Scalar::Int(64)));
        assert_eq!(map.get("widths.1"), Some(&Scalar::Int(128)));
        assert_eq!(map.get("model.layers.0"), Some(&Scalar::Int(2)));
        assert_eq!(map.get("model.layers.1"), Some(&Scalar::Int(3)));
        assert_eq!(map.get("model.dropout"), Some(&Scalar::Float(0.2)));
        assert_eq!(map.get("optim.kind"), Some(&Scalar::String("adam".into())));
        assert_eq!(map.get("optim.weight_decay"), Some(&Scalar::Float(0.001)));
        assert_eq!(map.get("TrainConfig.EPOCHS"), Some(&Scalar::Int(70)));
        assert_eq!(map.get("TrainConfig.layers"), Some(&Scalar::Int(9)));
        assert_eq!(
            map.get("TestConfig.TEST_DIR"),
            Some(&Scalar::String("path".into()))
        );
        assert_eq!(
            map.get("TestConfig.METRICS.0"),
            Some(&Scalar::String("metric".into()))
        );
        assert!(!map.contains_key("dynamic"));
        assert!(!map.contains_key("TrainConfig.sum"));
        assert!(!map.contains_key("TrainConfig.local"));
    }

    #[test]
    fn parse_rejects_nan_and_infinity() {
        // JSON doesn't have NaN literals; YAML does: `.nan`, `.inf`.
        let yaml = "x: .nan\n";
        let err = parse_yaml(yaml).unwrap_err();
        assert!(
            matches!(err, CrabError::Configuration { .. }),
            "expected Configuration, got {err:?}"
        );

        let yaml = "x: .inf\n";
        let err = parse_yaml(yaml).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn parse_dispatches_by_extension() {
        let yaml_path = PathBuf::from("foo.yaml");
        let json_path = PathBuf::from("foo.json");
        let toml_path = PathBuf::from("foo.toml");
        let yml_path = PathBuf::from("foo.yml");
        let py_path = PathBuf::from("foo.py");
        let bad_path = PathBuf::from("foo.txt");

        assert!(parse(b"a: 1", &yaml_path).is_ok());
        assert!(parse(b"{\"a\":1}", &json_path).is_ok());
        assert!(parse(b"a = 1", &toml_path).is_ok());
        assert!(parse(b"a: 1", &yml_path).is_ok());
        assert!(parse(b"a = 1", &py_path).is_ok());

        let err = parse(b"", &bad_path).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn parse_rejects_non_utf8_bytes() {
        let path = PathBuf::from("foo.yaml");
        // Lone continuation byte — invalid UTF-8 start.
        let err = parse(&[0xff], &path).unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn resolve_stage_param_values_handles_file_scoped_and_whole_file_refs() {
        use crate::stage::ParamRef;

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("params.yaml"), b"model:\n  lr: 0.01\n").unwrap();
        std::fs::write(
            tmp.path().join("custom.yaml"),
            b"epochs: 5\nmodel:\n  dropout: 0.2\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("all.json"),
            br#"{"alpha": 1, "beta": true}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("params.py"), b"gamma = 3\n").unwrap();

        let refs = vec![
            ParamRef::parse("model.lr").unwrap(),
            ParamRef::parse_in_file(PathBuf::from("custom.yaml"), "epochs").unwrap(),
            ParamRef::parse_in_file(PathBuf::from("custom.yaml"), "model.dropout").unwrap(),
            ParamRef::all_in_file(PathBuf::from("all.json")).unwrap(),
            ParamRef::parse_in_file(PathBuf::from("params.py"), "gamma").unwrap(),
        ];

        let values = resolve_stage_param_values(tmp.path(), &[], &refs, "train").unwrap();
        assert_eq!(values.get("model.lr").map(String::as_str), Some("0.01"));
        assert_eq!(
            values.get("custom.yaml:epochs").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            values.get("custom.yaml:model.dropout").map(String::as_str),
            Some("0.2")
        );
        assert_eq!(values.get("all.json:alpha").map(String::as_str), Some("1"));
        assert_eq!(
            values.get("all.json:beta").map(String::as_str),
            Some("true")
        );
        assert_eq!(values.get("params.py:gamma").map(String::as_str), Some("3"));
    }

    #[test]
    fn resolve_stage_param_values_expands_subtree_refs() {
        use crate::stage::ParamRef;

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("params.yaml"),
            b"lr: 0.01\ntrain:\n  epochs: 10\n  layers: 3\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("custom.yaml"),
            b"model:\n  dropout: 0.2\n  width: 128\n",
        )
        .unwrap();

        let refs = vec![
            ParamRef::parse("train").unwrap(),
            ParamRef::parse_in_file(PathBuf::from("custom.yaml"), "model").unwrap(),
        ];

        let values = resolve_stage_param_values(tmp.path(), &[], &refs, "train").unwrap();
        assert_eq!(values.len(), 4);
        assert_eq!(values.get("train.epochs").map(String::as_str), Some("10"));
        assert_eq!(values.get("train.layers").map(String::as_str), Some("3"));
        assert_eq!(
            values.get("custom.yaml:model.dropout").map(String::as_str),
            Some("0.2")
        );
        assert_eq!(
            values.get("custom.yaml:model.width").map(String::as_str),
            Some("128")
        );
    }

    #[test]
    fn resolve_stage_param_values_with_wdir_reads_stage_local_files() {
        use crate::stage::ParamRef;

        let tmp = tempfile::TempDir::new().unwrap();
        let training = tmp.path().join("training");
        std::fs::create_dir_all(&training).unwrap();
        std::fs::write(tmp.path().join("params.yaml"), b"model:\n  lr: 9.99\n").unwrap();
        std::fs::write(training.join("params.yaml"), b"model:\n  lr: 0.01\n").unwrap();
        std::fs::write(training.join("custom.yaml"), b"epochs: 5\n").unwrap();

        let refs = vec![
            ParamRef::parse("model.lr").unwrap(),
            ParamRef::parse_in_file(PathBuf::from("custom.yaml"), "epochs").unwrap(),
        ];
        let values = resolve_stage_param_values_with_wdir(
            tmp.path(),
            &[],
            &refs,
            "train",
            Some(Path::new("training")),
        )
        .unwrap();

        assert_eq!(values.get("model.lr").map(String::as_str), Some("0.01"));
        assert_eq!(
            values
                .get("training/custom.yaml:epochs")
                .map(String::as_str),
            Some("5")
        );
    }

    #[test]
    fn parse_yaml_rejects_non_map_root() {
        let err = parse_yaml("[1, 2, 3]\n").unwrap_err();
        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    // --- Diff ---

    #[test]
    fn diff_classifies_added_removed_changed_unchanged() {
        let mut a = ScalarMap::new();
        a.insert("keep".into(), Scalar::Int(1));
        a.insert("change".into(), Scalar::Int(1));
        a.insert("remove".into(), Scalar::String("old".into()));

        let mut b = ScalarMap::new();
        b.insert("keep".into(), Scalar::Int(1));
        b.insert("change".into(), Scalar::Int(2));
        b.insert("add".into(), Scalar::Bool(true));

        let d = diff(&a, &b);
        assert_eq!(d.unchanged.get("keep"), Some(&Scalar::Int(1)));
        assert!(d.unchanged.contains_key("keep"));
        assert_eq!(
            d.changed.get("change"),
            Some(&(Scalar::Int(1), Scalar::Int(2)))
        );
        assert!(d.removed.contains_key("remove"));
        assert!(d.added.contains_key("add"));
    }

    #[test]
    fn diff_empty_maps_produces_empty_diff() {
        let a = ScalarMap::new();
        let b = ScalarMap::new();
        let d = diff(&a, &b);
        assert!(d.is_empty());
    }

    // --- Renderers ---

    #[test]
    fn render_table_shows_three_sections() {
        let mut a = ScalarMap::new();
        a.insert("x".into(), Scalar::Int(1));
        let mut b = ScalarMap::new();
        b.insert("y".into(), Scalar::Int(2));
        let d = diff(&a, &b);
        let table = render_table(&d, RenderOptions::default());
        assert!(table.contains("=== Added ==="));
        assert!(table.contains("=== Removed ==="));
        assert!(table.contains("=== Changed ==="));
    }

    #[test]
    fn render_pr_comment_uses_emoji_for_gains_and_regressions() {
        let mut a = ScalarMap::new();
        a.insert("accuracy".into(), Scalar::Float(0.80));
        let mut b = ScalarMap::new();
        b.insert("accuracy".into(), Scalar::Float(0.85));
        let d = diff(&a, &b);
        let out = render_pr_comment(
            &d,
            RenderOptions {
                metrics_mode: true,
                ..RenderOptions::default()
            },
            true, // higher is better
        );
        assert!(out.contains('✓'), "should mark gain: {out}");
        assert!(out.contains("0.05"), "should show abs delta: {out}");
        // Accuracy went up 0.05/0.80 = 6.25%.
        assert!(out.contains("6.25%"), "should show percent delta: {out}");
    }

    #[test]
    fn render_pr_comment_flips_polarity_when_lower_is_better() {
        // Loss going up (regression) with higher_is_better=false.
        let mut a = ScalarMap::new();
        a.insert("loss".into(), Scalar::Float(0.10));
        let mut b = ScalarMap::new();
        b.insert("loss".into(), Scalar::Float(0.20));
        let d = diff(&a, &b);
        let out = render_pr_comment(
            &d,
            RenderOptions {
                metrics_mode: true,
                ..RenderOptions::default()
            },
            false,
        );
        assert!(out.contains('✗'), "should mark regression: {out}");
    }

    #[test]
    fn numeric_delta_handles_zero_baseline() {
        let (abs, pct) = numeric_delta(&Scalar::Float(0.0), &Scalar::Float(1.0));
        assert_eq!(abs, "+1");
        assert_eq!(pct, "+∞%");
    }

    // --- read_at_ref: integration with a real git repo ---

    /// Drive `git` from $PATH to set up a repo, commit a YAML file,
    /// and verify `read_at_ref` reproduces the flattened map. We
    /// rely on an external `git` for setup because constructing a
    /// commit from scratch via gitoxide is fiddly for a unit test —
    /// the production code path only *reads* via gitoxide.
    #[test]
    fn read_at_ref_reads_committed_params_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        if !git_available() {
            eprintln!("skipping: git not on PATH");
            return;
        }
        git_init(repo);
        std::fs::write(
            repo.join("params.yaml"),
            b"model:\n  lr: 0.01\n  epochs: 5\n",
        )
        .unwrap();
        git(repo, &["add", "params.yaml"]);
        git(repo, &["commit", "-m", "init"]);

        let map = read_at_ref(repo, "HEAD", &[PathBuf::from("params.yaml")]).unwrap();
        assert_eq!(map.get("model.lr"), Some(&Scalar::Float(0.01)));
        assert_eq!(map.get("model.epochs"), Some(&Scalar::Int(5)));
    }

    #[test]
    fn read_at_ref_default_paths_picks_up_params_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        if !git_available() {
            return;
        }
        git_init(repo);
        std::fs::write(repo.join("params.yaml"), b"lr: 0.1\n").unwrap();
        git(repo, &["add", "params.yaml"]);
        git(repo, &["commit", "-m", "init"]);

        let map = read_at_ref(repo, "HEAD", &[]).unwrap();
        assert_eq!(map.get("lr"), Some(&Scalar::Float(0.1)));
    }

    #[test]
    fn read_at_ref_falls_back_to_working_tree_for_head_when_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        if !git_available() {
            return;
        }
        git_init(repo);
        // Commit nothing; params.yaml only in the working tree.
        std::fs::write(repo.join("README"), b"hi\n").unwrap();
        git(repo, &["add", "README"]);
        git(repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("params.yaml"), b"lr: 0.5\n").unwrap();
        let map = read_at_ref(repo, "HEAD", &[PathBuf::from("params.yaml")]).unwrap();
        assert_eq!(map.get("lr"), Some(&Scalar::Float(0.5)));
    }

    #[test]
    fn read_at_ref_workspace_reads_working_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        if !git_available() {
            return;
        }
        git_init(repo);
        std::fs::write(repo.join("params.yaml"), b"lr: 0.1\n").unwrap();
        git(repo, &["add", "params.yaml"]);
        git(repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("params.yaml"), b"lr: 0.5\n").unwrap();
        let map = read_at_ref(repo, "workspace", &[PathBuf::from("params.yaml")]).unwrap();
        assert_eq!(map.get("lr"), Some(&Scalar::Float(0.5)));
    }

    #[test]
    fn read_at_ref_missing_path_at_non_head_ref_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        if !git_available() {
            return;
        }
        git_init(repo);
        std::fs::write(repo.join("README"), b"hi\n").unwrap();
        git(repo, &["add", "README"]);
        git(repo, &["commit", "-m", "init"]);

        let err = read_at_ref(repo, "main", &[PathBuf::from("params.yaml")]).unwrap_err();
        assert!(
            matches!(err, CrabError::StageDepMissing { .. }),
            "expected StageDepMissing, got {err:?}"
        );
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
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
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            // Clear any inherited GIT_DIR / GIT_WORK_TREE so parallel
            // tests that set those env vars can't redirect our child
            // processes to the wrong repo.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
        assert!(status.success(), "git {args:?} failed");
    }

    // --- insta snapshots for renderers ---

    fn sample_diff() -> ScalarDiff {
        let mut a = ScalarMap::new();
        a.insert("model.lr".into(), Scalar::Float(0.01));
        a.insert("model.epochs".into(), Scalar::Int(5));
        a.insert("dataset".into(), Scalar::String("train".into()));
        a.insert("dropout".into(), Scalar::Float(0.5));

        let mut b = ScalarMap::new();
        b.insert("model.lr".into(), Scalar::Float(0.02));
        b.insert("model.epochs".into(), Scalar::Int(5));
        b.insert("dataset".into(), Scalar::String("train".into()));
        b.insert("batch_size".into(), Scalar::Int(64));

        diff(&a, &b)
    }

    fn sample_metrics_diff() -> ScalarDiff {
        let mut a = ScalarMap::new();
        a.insert("accuracy".into(), Scalar::Float(0.80));
        a.insert("loss".into(), Scalar::Float(0.50));
        a.insert("f1".into(), Scalar::Float(0.75));

        let mut b = ScalarMap::new();
        b.insert("accuracy".into(), Scalar::Float(0.85));
        b.insert("loss".into(), Scalar::Float(0.40));
        b.insert("f1".into(), Scalar::Float(0.75));

        diff(&a, &b)
    }

    #[test]
    fn snapshot_render_table_params() {
        let out = render_table(&sample_diff(), RenderOptions::default());
        insta::assert_snapshot!("render_table_params", out);
    }

    #[test]
    fn snapshot_render_table_metrics() {
        let out = render_table(
            &sample_metrics_diff(),
            RenderOptions {
                metrics_mode: true,
                ..RenderOptions::default()
            },
        );
        insta::assert_snapshot!("render_table_metrics", out);
    }

    #[test]
    fn snapshot_render_json_params() {
        let out = render_json(&sample_diff());
        insta::assert_snapshot!("render_json_params", out);
    }

    #[test]
    fn snapshot_render_markdown_params() {
        let out = render_markdown(&sample_diff(), RenderOptions::default());
        insta::assert_snapshot!("render_markdown_params", out);
    }

    #[test]
    fn snapshot_render_markdown_metrics() {
        let out = render_markdown(
            &sample_metrics_diff(),
            RenderOptions {
                metrics_mode: true,
                ..RenderOptions::default()
            },
        );
        insta::assert_snapshot!("render_markdown_metrics", out);
    }

    #[test]
    fn snapshot_render_pr_comment_metrics_higher_is_better() {
        let out = render_pr_comment(
            &sample_metrics_diff(),
            RenderOptions {
                metrics_mode: true,
                ..RenderOptions::default()
            },
            true,
        );
        insta::assert_snapshot!("render_pr_comment_metrics_higher", out);
    }

    // --- Round-trip property tests ---

    proptest::proptest! {
        #[test]
        fn yaml_round_trip_preserves_map(m in arb_scalar_map()) {
            let text = to_yaml(&m);
            let parsed = parse_yaml(&text).expect("parse");
            assert_eq!(parsed, m, "yaml round trip");
        }

        #[test]
        fn json_round_trip_preserves_map(m in arb_scalar_map()) {
            let text = to_json(&m);
            let parsed = parse_json(&text).expect("parse");
            assert_eq!(parsed, m, "json round trip");
        }

        #[test]
        fn toml_round_trip_preserves_map(m in arb_scalar_map()) {
            let text = to_toml(&m);
            let parsed = parse_toml(&text).expect("parse");
            assert_eq!(parsed, m, "toml round trip");
        }
    }

    /// Strategy: generate random flat `ScalarMap` values with
    /// finite floats and reasonable keys. The round-trip test
    /// serializes each value via its native format library
    /// (`serde_yaml`, `serde_json`, `toml`) so the resulting text
    /// is guaranteed to parse back to the same value.
    ///
    /// The float generator snaps values to 4 decimal places in a
    /// bounded range. This avoids hitting `ryu`'s shortest-string
    /// output that occasionally round-trips through serde_json to a
    /// neighboring f64 — a well-known precision artifact of the
    /// format, not a parser bug. Stage params and metrics files in
    /// the real world never carry more precision than this anyway.
    fn arb_scalar_map() -> impl proptest::strategy::Strategy<Value = ScalarMap> {
        use proptest::prelude::*;
        let key = "[a-z][a-z0-9_]{0,7}";
        let scalar = prop_oneof![
            any::<bool>().prop_map(Scalar::Bool),
            any::<i32>().prop_map(|i| Scalar::Int(i64::from(i))),
            (-10_000i32..10_000i32).prop_map(|n| {
                // Quantize to 4 decimal places so the value has a
                // short exact decimal representation.
                Scalar::Float(f64::from(n) / 10_000.0)
            }),
            "[a-z0-9 _-]{0,16}".prop_map(Scalar::String),
        ];
        proptest::collection::btree_map(key, scalar, 0..8)
    }

    /// Render a flat map as YAML via `serde_yaml`. The top-level
    /// shape is a `Mapping` whose values are native YAML scalars,
    /// so the serializer picks the shortest round-trip form for
    /// floats automatically.
    fn to_yaml(m: &ScalarMap) -> String {
        use serde_yaml::{Mapping, Number, Value};
        let mut mapping = Mapping::new();
        for (k, v) in m {
            mapping.insert(Value::String(k.clone()), scalar_to_yaml(v));
        }
        if mapping.is_empty() {
            // serde_yaml emits `{}` for empty maps which re-parses
            // into a mapping as expected.
            return "{}\n".to_owned();
        }
        let _ = Number::from(0); // force the import resolver to keep Number available
        serde_yaml::to_string(&Value::Mapping(mapping)).expect("yaml serialize")
    }

    fn scalar_to_yaml(s: &Scalar) -> serde_yaml::Value {
        use serde_yaml::Value;
        match s {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(*b),
            Scalar::Int(i) => Value::Number((*i).into()),
            Scalar::Float(f) => Value::Number(serde_yaml::Number::from(*f)),
            Scalar::String(s) => Value::String(s.clone()),
        }
    }

    fn to_json(m: &ScalarMap) -> String {
        let mut root = serde_json::Map::new();
        for (k, v) in m {
            root.insert(k.clone(), scalar_to_json(v));
        }
        serde_json::to_string(&serde_json::Value::Object(root)).expect("json serialize")
    }

    fn scalar_to_json(s: &Scalar) -> serde_json::Value {
        use serde_json::Value;
        match s {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(*b),
            Scalar::Int(i) => Value::Number((*i).into()),
            Scalar::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .expect("finite float"),
            Scalar::String(s) => Value::String(s.clone()),
        }
    }

    fn to_toml(m: &ScalarMap) -> String {
        // `toml::Value::Table` accepts only string keys and there's
        // no native null; the generator filters nulls out above so
        // this is safe.
        let mut table = toml::value::Table::new();
        for (k, v) in m {
            table.insert(k.clone(), scalar_to_toml(v));
        }
        toml::to_string(&toml::Value::Table(table)).expect("toml serialize")
    }

    fn scalar_to_toml(s: &Scalar) -> toml::Value {
        match s {
            Scalar::Null => {
                // Unreachable under `arb_scalar_map`. Explicit panic
                // message so a future change to the generator surfaces
                // the TOML gap loudly.
                panic!("toml has no null scalar; restrict generator")
            }
            Scalar::Bool(b) => toml::Value::Boolean(*b),
            Scalar::Int(i) => toml::Value::Integer(*i),
            Scalar::Float(f) => toml::Value::Float(*f),
            Scalar::String(s) => toml::Value::String(s.clone()),
        }
    }
}
