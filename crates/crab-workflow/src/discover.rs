//! Workflow discovery — locate `crab.yaml` file(s) under a repo root.
//!
//! Two modes, selected by the user (R2):
//!
//! - [`DiscoverMode::Root`]: only the repo-root `crab.yaml` participates.
//!   Nested yaml files anywhere under the tree are a hard error — we
//!   refuse to guess which one the user meant.
//! - [`DiscoverMode::Recursive`]: every `crab.yaml` under the root
//!   participates. The root yaml (if present) comes first; others are
//!   sorted by path for deterministic iteration.
//!
//! When [`merge`] stitches multiple recursive yamls into a single
//! [`Workflow`], nested stage names are prefixed with the containing
//! directory joined by dots per R17. A stage named `clean` in
//! `data/crab.yaml` becomes `data.clean`; in
//! `pipelines/eval/crab.yaml` it becomes `pipelines.eval.clean`.
//! `Dep::StageOut` references inside a nested file are rewritten to
//! the prefixed form so the DAG resolves within the merged set.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::stage::{Dep, Out, StageName};
use crate::{Defaults, Workflow, yaml};
use crate::{Result, WorkflowError as CrabError};

/// Directories that never contain user-authored stage yaml and would
/// explode the walk if traversed (node_modules especially). Compared
/// by exact file-name match; hidden dirs under `.` are additionally
/// filtered by the leading dot.
const DEFAULT_IGNORE_DIRS: &[&str] = &[".git", ".crab", "node_modules", "target"];

/// The file name we look for at every visited directory.
const WORKFLOW_FILE_NAME: &str = "crab.yaml";

/// Suffix used for per-workflow yaml files. A file named
/// `train.workflow.yaml` declares a workflow whose stages are prefixed
/// with `train.` when merged with other workflow files. The root
/// [`WORKFLOW_FILE_NAME`] (`crab.yaml`) remains the place for shared
/// defaults, params, and stages that don't belong to a named workflow.
const WORKFLOW_YAML_SUFFIX: &str = ".workflow.yaml";

/// Classify a yaml file name as a workflow input.
fn is_workflow_yaml(name: &str) -> bool {
    name == WORKFLOW_FILE_NAME || name.ends_with(WORKFLOW_YAML_SUFFIX)
}

/// Discovery mode: `Root` or `Recursive`. Mirrors
/// [`crate::core::config::WorkflowDiscover`] so the caller can thread
/// it from CLI flags or config without a second enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverMode {
    /// Only consider the repo-root `crab.yaml`. Nested yaml files
    /// under the root are rejected with
    /// [`CrabError::WorkflowDiscoveryAmbiguous`].
    Root,
    /// Walk the tree under the repo root and return every
    /// `crab.yaml` discovered. The root-level yaml sorts first so
    /// callers that merge by order get stable output.
    Recursive,
}

/// Locate `crab.yaml` files under `repo_root`.
///
/// In `Root` mode the return value is either the single root yaml
/// (if present) or an empty vec. Nested yamls found anywhere below
/// the root (ignoring the usual build / VCS directories) cause an
/// error so users cannot silently half-configure the workflow layer.
///
/// In `Recursive` mode every yaml under the tree is returned in
/// sorted order, with the root yaml always first when present.
pub fn discover(repo_root: &Path, mode: DiscoverMode) -> Result<Vec<PathBuf>> {
    let root_yaml = repo_root.join(WORKFLOW_FILE_NAME);
    let has_root = root_yaml.is_file();

    match mode {
        DiscoverMode::Root => {
            // Nested yaml is a hard error in Root mode. We still
            // report the root yaml as a candidate when both are
            // present so the user sees the full picture.
            let nested = find_all_yamls(repo_root)?;
            let nested_only: Vec<PathBuf> =
                nested.into_iter().filter(|p| p != &root_yaml).collect();
            if !nested_only.is_empty() {
                let mut candidates = Vec::with_capacity(nested_only.len() + 1);
                if has_root {
                    candidates.push(root_yaml.clone());
                }
                candidates.extend(nested_only);
                return Err(CrabError::WorkflowDiscoveryAmbiguous { candidates });
            }
            Ok(if has_root {
                vec![root_yaml]
            } else {
                Vec::new()
            })
        }
        DiscoverMode::Recursive => {
            let mut all = find_all_yamls(repo_root)?;
            // Always surface the repo-root yaml first so merge
            // iteration order is deterministic and the root
            // workflow's declarations take precedence.
            if let Some(pos) = all.iter().position(|p| p == &root_yaml) {
                let root = all.remove(pos);
                all.insert(0, root);
            }
            Ok(all)
        }
    }
}

/// Merge multiple parsed workflows into one.
///
/// The first element of `yamls` is treated as the root workflow
/// (its stage names are kept unprefixed, its `params` / `metrics` /
/// `plots` and `defaults` are preserved). Every subsequent element
/// is nested: its stage names are prefixed with the dot-joined
/// relative directory from the repo root, and its declared `deps`,
/// `outs`, `metrics`, and `plots` paths are rewritten to be
/// repo-relative by prepending the same relative directory. Declared
/// params, metrics and plots from nested yamls are merged into the
/// root's lists.
///
/// Returns [`CrabError::WorkflowDiscoveryAmbiguous`] with the
/// colliding file paths if two nested yamls produce the same
/// effective stage name.
pub fn merge(repo_root: &Path, yamls: &[(PathBuf, Workflow)]) -> Result<Workflow> {
    let (merged, _provenance) = merge_with_provenance(repo_root, yamls)?;
    Ok(merged)
}

/// Like [`merge`], but also returns per-stage provenance — the map
/// from a merged stage name to the workflow YAML path that declared
/// it. Needed by the split-lockfile I/O layer so stages are written
/// to the correct per-file lockfile.
///
/// Cheap to build: the merge already tracks origins internally for
/// collision detection, so we just expose the same map the old
/// implementation discarded.
pub fn merge_with_provenance(
    repo_root: &Path,
    yamls: &[(PathBuf, Workflow)],
) -> Result<(Workflow, BTreeMap<StageName, PathBuf>)> {
    if yamls.is_empty() {
        return Ok((
            Workflow {
                params: Vec::new(),
                metrics: Vec::new(),
                plots: Vec::new(),
                plot_configs: Vec::new(),
                defaults: Defaults::default(),
                stages: BTreeMap::new(),
                workflow_membership: BTreeMap::new(),
            },
            BTreeMap::new(),
        ));
    }

    // Classify yaml paths: the repo-root `crab.yaml` (if present)
    // is the "root" workflow whose stages stay unprefixed. Every
    // other file — nested `crab.yaml` or any `*.workflow.yaml` at
    // any depth — is treated as "nested" and gets its stages
    // prefixed. The classification is a function of the path, not
    // of its position in the input vec, so callers can pass yamls
    // in any order.
    let root_marker = repo_root.join(WORKFLOW_FILE_NAME);
    let (root_slot, nested_slots): (Vec<_>, Vec<_>) =
        yamls.iter().partition(|(p, _)| p == &root_marker);

    // Seed from the root workflow if present; otherwise start from
    // an empty workflow and only carry nested-file contents. Other
    // file-scope fields (params / metrics / plots / defaults) come
    // from the root entirely — nested defaults would be ambiguous
    // (which `env` policy wins for a stage?) and are intentionally
    // ignored.
    let mut merged = if let Some((_, root_wf)) = root_slot.first() {
        Workflow {
            params: root_wf.params.clone(),
            metrics: root_wf.metrics.clone(),
            plots: root_wf.plots.clone(),
            plot_configs: root_wf.plot_configs.clone(),
            defaults: root_wf.defaults.clone(),
            stages: BTreeMap::new(),
            workflow_membership: BTreeMap::new(),
        }
    } else {
        Workflow {
            params: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            plot_configs: Vec::new(),
            defaults: Defaults::default(),
            stages: BTreeMap::new(),
            workflow_membership: BTreeMap::new(),
        }
    };

    // Track which file each merged stage came from so a collision
    // message names both candidates.
    let mut origins: BTreeMap<StageName, PathBuf> = BTreeMap::new();

    // Insert root-level stages first; they keep their original names.
    if let Some((root_path, root_wf)) = root_slot.first() {
        for (name, stage) in &root_wf.stages {
            merged.stages.insert(name.clone(), stage.clone());
            origins.insert(name.clone(), (*root_path).clone());
        }
    }

    // Insert nested-yaml stages, each prefixed by the effective
    // prefix derived from the yaml path. Guard against prefix
    // collisions (two yamls mapping to the same prefix) so stages
    // from distinct files can never silently mix.
    let mut prefix_sources: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (yaml_path, wf) in &nested_slots {
        let rel_dir = yaml_relative_dir(repo_root, yaml_path);
        let prefix_components = prefix_components_for(&rel_dir, yaml_path);
        if prefix_components.is_empty() {
            // A bare `crab.yaml` that isn't the repo-root one
            // (e.g., a stray file somewhere) would land here. We
            // already have a root — this is an ambiguous setup.
            return Err(CrabError::WorkflowDiscoveryAmbiguous {
                candidates: vec![
                    root_slot
                        .first()
                        .map(|(p, _)| (*p).clone())
                        .unwrap_or_else(|| repo_root.to_path_buf()),
                    (*yaml_path).clone(),
                ],
            });
        }
        let dotted_prefix = prefix_components.join(".");
        // Validate the prefix produces valid stage-name segments.
        validate_prefix_components(&prefix_components, yaml_path)?;

        if let Some(other) = prefix_sources.get(&dotted_prefix) {
            return Err(CrabError::WorkflowDiscoveryAmbiguous {
                candidates: vec![other.clone(), (*yaml_path).clone()],
            });
        }
        prefix_sources.insert(dotted_prefix.clone(), (*yaml_path).clone());

        // Build a map from original → prefixed names so we can
        // rewrite Dep::StageOut references within the same nested
        // yaml consistently. Cross-file StageOut deps are not
        // supported yet — they would require declaring which file
        // a stage lives in, and no syntax for that exists today.
        let mut rename: BTreeMap<StageName, StageName> = BTreeMap::new();
        for name in wf.stages.keys() {
            rename.insert(name.clone(), StageName::from_joined(&dotted_prefix, name)?);
        }

        for (orig_name, stage) in &wf.stages {
            let Some(new_name) = rename.get(orig_name).cloned() else {
                // Rename map is populated above in lock-step with
                // `wf.stages`; reaching this arm means the keys
                // drifted between the two loops, which can only
                // happen if the workflow was mutated underneath us.
                return Err(CrabError::Configuration {
                    key: format!("stage '{orig_name}'"),
                    origin: "discover: rename map out of sync with workflow stages".to_owned(),
                });
            };
            if let Some(prev_file) = origins.get(&new_name) {
                return Err(CrabError::WorkflowDiscoveryAmbiguous {
                    candidates: vec![prev_file.clone(), (*yaml_path).clone()],
                });
            }

            let mut rewritten = stage.clone();
            rewritten.name = new_name.clone();
            // Rewrite path-based deps and outs so they're repo-
            // relative: every nested yaml declares paths relative
            // to its own directory.
            rewritten.deps = stage
                .deps
                .iter()
                .map(|d| rewrite_dep(d, &rel_dir, &rename))
                .collect();
            rewritten.outs = stage
                .outs
                .iter()
                .map(|o| rewrite_out(o, &rel_dir))
                .collect();
            rewritten.metrics = stage
                .metrics
                .iter()
                .map(|p| PathBuf::from(join_rel(&rel_dir, p)))
                .collect();
            rewritten.plots = stage
                .plots
                .iter()
                .map(|p| PathBuf::from(join_rel(&rel_dir, p)))
                .collect();

            origins.insert(new_name.clone(), (*yaml_path).clone());
            merged.stages.insert(new_name, rewritten);
        }

        // Merge file-scope params / metrics / plots verbatim. The
        // user's nested yaml lists these relative to the nested
        // directory, so we prepend that prefix for uniformity with
        // the rewritten stage paths.
        for p in &wf.params {
            merged.params.push(PathBuf::from(join_rel(&rel_dir, p)));
        }
        for p in &wf.metrics {
            merged.metrics.push(PathBuf::from(join_rel(&rel_dir, p)));
        }
        for p in &wf.plots {
            merged.plots.push(PathBuf::from(join_rel(&rel_dir, p)));
        }
    }

    Ok((merged, origins))
}

/// Parse every discovered yaml, then merge. Convenience wrapper so
/// callers don't have to round-trip through two helpers.
pub fn parse_all(repo_root: &Path, yaml_paths: &[PathBuf]) -> Result<Workflow> {
    let (merged, _) = parse_all_with_provenance(repo_root, yaml_paths)?;
    Ok(merged)
}

/// Parse + merge variant that also returns stage provenance for the
/// split-lockfile layer. Reads each yaml once — the extra
/// bookkeeping is effectively free on top of the existing parse.
pub fn parse_all_with_provenance(
    repo_root: &Path,
    yaml_paths: &[PathBuf],
) -> Result<(Workflow, BTreeMap<StageName, PathBuf>)> {
    let mut parsed: Vec<(PathBuf, Workflow)> = Vec::with_capacity(yaml_paths.len());
    for path in yaml_paths {
        let text = std::fs::read_to_string(path).map_err(CrabError::Io)?;
        let wf = yaml::parse_at(path, &text)?;
        parsed.push((path.clone(), wf));
    }
    merge_with_provenance(repo_root, &parsed)
}

// ─── Internals ─────────────────────────────────────────────────────────

/// Walk `root` and collect every `crab.yaml` file, skipping the
/// usual build / VCS dirs. Order is depth-first but the result is
/// sorted so callers get deterministic output.
fn find_all_yamls(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        // Nothing to discover — the root yaml lookup above will
        // handle the "no repo at all" case cleanly.
        return Ok(out);
    }
    walk_collect(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Permission denied inside a repo we can otherwise read is
        // usually an unreadable hidden dir (`.Trash`, macOS
        // `.Spotlight-V100`, etc.). Skip rather than error out so a
        // single unreadable subtree doesn't block `crab run`.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(CrabError::Io(e)),
    };

    for entry in entries {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if file_type.is_dir() {
            if is_ignored_dir(&name_str) {
                continue;
            }
            walk_collect(&path, out)?;
        } else if file_type.is_file() && is_workflow_yaml(&name_str) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_ignored_dir(name: &str) -> bool {
    DEFAULT_IGNORE_DIRS.contains(&name)
}

/// Compute the stage-name prefix components for a yaml file.
///
/// Prefix rules:
///
/// - Repo-root `crab.yaml` → no prefix (handled by the caller).
/// - Nested `crab.yaml` at `a/b/crab.yaml` → `["a", "b"]`.
/// - `train.workflow.yaml` at the repo root → `["train"]`.
/// - `pipelines/eval.workflow.yaml` → `["pipelines", "eval"]`.
///
/// Returns an empty vec when the yaml is a bare `crab.yaml` at the
/// repo root (the caller classifies that file separately, so
/// reaching this helper with an empty result means something is
/// wrong with the inputs).
fn prefix_components_for(rel_dir: &str, yaml_path: &Path) -> Vec<String> {
    let mut components: Vec<String> = if rel_dir.is_empty() {
        Vec::new()
    } else {
        rel_dir.split('/').map(|s| s.to_owned()).collect()
    };

    // Pull the workflow name out of the filename when it's a
    // `*.workflow.yaml` file. The root `crab.yaml` has no
    // workflow-name suffix and contributes nothing here.
    if let Some(name) = yaml_path.file_name().and_then(|n| n.to_str())
        && let Some(stem) = name.strip_suffix(WORKFLOW_YAML_SUFFIX)
        && !stem.is_empty()
    {
        components.push(stem.to_owned());
    }

    components
}

/// Reject prefix components that would produce invalid stage names.
/// The check runs per-component rather than on the joined form so
/// the error message points at the specific offending segment.
fn validate_prefix_components(components: &[String], yaml_path: &Path) -> Result<()> {
    for seg in components {
        // The stage-name grammar disallows dots; a segment with a
        // literal dot would collide with the dotted-prefix separator
        // and produce surprising stage names. Same treatment as
        // `path_to_dot_prefix` applies to directory-derived prefixes.
        if seg.is_empty() || seg.contains('.') || seg.contains(' ') {
            return Err(CrabError::Configuration {
                key: format!(
                    "workflow prefix segment {seg:?} from {} is invalid",
                    yaml_path.display()
                ),
                origin: "discover".to_owned(),
            });
        }
    }
    Ok(())
}

/// Directory containing `yaml_path`, relative to `repo_root`.
/// Returns an empty string when the yaml sits in the repo root.
fn yaml_relative_dir(repo_root: &Path, yaml_path: &Path) -> String {
    let Some(parent) = yaml_path.parent() else {
        return String::new();
    };
    let rel = parent.strip_prefix(repo_root).unwrap_or(parent);
    // Preserve POSIX-style slashes in the internal representation;
    // the hasher and lockfile normalize paths independently.
    let s = rel.to_string_lossy();
    if s.is_empty() || s == "." {
        String::new()
    } else {
        s.replace('\\', "/")
    }
}

/// Convert a relative directory like `data/clean` into the dotted
/// prefix form `data.clean`, validating each segment as a stage-name
/// token per R17.
///
/// Preserved for its test coverage of the grammar rules enforced by
/// [`validate_prefix_components`] — the production call site moved
/// to `prefix_components_for` + validation once `*.workflow.yaml`
/// discovery landed.
#[cfg(test)]
fn path_to_dot_prefix(rel_dir: &str) -> Result<String> {
    let segments: Vec<&str> = rel_dir.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Ok(String::new());
    }
    // Validate each segment through StageName::parse so a directory
    // named "bad name" or "123" surfaces a clear error before any
    // downstream component treats the prefix as trusted.
    for seg in &segments {
        StageName::parse(seg).map_err(|error| match error {
            crate::WorkflowError::StageNameInvalid { name, reason } => {
                CrabError::WorkflowStageNameInvalid { name, reason }
            }
            other => other,
        })?;
    }
    Ok(segments.join("."))
}

/// Rewrite a [`Dep`] so its paths are repo-relative rather than
/// relative to the nested yaml's directory. `rename` maps original
/// stage names in the nested file to their prefixed form for
/// `Dep::StageOut`.
fn rewrite_dep(dep: &Dep, rel_dir: &str, rename: &BTreeMap<StageName, StageName>) -> Dep {
    match dep {
        Dep::Path(p) => Dep::Path(PathBuf::from(join_rel(rel_dir, p))),
        Dep::StageOut { stage, out } => {
            // Only rewrite when the referenced stage lives in the
            // same nested file. Anything else (unknown name, or a
            // root-level stage referenced from a nested file) is
            // left as-is; the DAG builder will reject an unknown
            // reference with its own clear error.
            let new_stage = rename.get(stage).cloned().unwrap_or_else(|| stage.clone());
            Dep::StageOut {
                stage: new_stage,
                out: PathBuf::from(join_rel(rel_dir, out)),
            }
        }
        // Remote deps carry URLs / refs, never filesystem paths;
        // leave them alone.
        Dep::CrabRef { .. } | Dep::GitRef { .. } | Dep::Url { .. } | Dep::OciImage { .. } => {
            dep.clone()
        }
    }
}

fn rewrite_out(out: &Out, rel_dir: &str) -> Out {
    Out {
        path: PathBuf::from(join_rel(rel_dir, &out.path)),
        kind: out.kind,
        cache: out.cache,
        push: out.push,
        remote: out.remote.clone(),
        persist: out.persist,
        max_bytes: out.max_bytes,
    }
}

/// Prepend a POSIX-style relative directory to a path, flattening
/// to forward slashes. Empty `rel_dir` returns the path unchanged.
fn join_rel(rel_dir: &str, p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if rel_dir.is_empty() {
        s
    } else {
        format!("{rel_dir}/{s}")
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
    use std::fs;
    use tempfile::TempDir;

    fn write_yaml(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    const MINI_YAML: &str = "stages:\n  clean:\n    cmd: \"true\"\n";

    #[test]
    fn root_mode_returns_only_root_yaml_when_no_nested() {
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "crab.yaml", MINI_YAML);

        let found = discover(tmp.path(), DiscoverMode::Root).unwrap();
        assert_eq!(found, vec![tmp.path().join("crab.yaml")]);
    }

    #[test]
    fn root_mode_returns_empty_when_no_yaml_anywhere() {
        let tmp = TempDir::new().unwrap();
        let found = discover(tmp.path(), DiscoverMode::Root).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn root_mode_rejects_nested_yaml() {
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), "data/crab.yaml", MINI_YAML);

        let err = discover(tmp.path(), DiscoverMode::Root).unwrap_err();
        match err {
            CrabError::WorkflowDiscoveryAmbiguous { candidates } => {
                // Root yaml must appear in the candidate list so the
                // user sees both halves of the ambiguity.
                assert!(
                    candidates
                        .iter()
                        .any(|p| p == &tmp.path().join("crab.yaml")),
                    "root yaml missing from candidates: {candidates:?}"
                );
                assert!(
                    candidates
                        .iter()
                        .any(|p| p == &tmp.path().join("data/crab.yaml")),
                    "nested yaml missing from candidates: {candidates:?}"
                );
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn root_mode_rejects_nested_yaml_even_without_root_yaml() {
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "data/crab.yaml", MINI_YAML);

        let err = discover(tmp.path(), DiscoverMode::Root).unwrap_err();
        assert!(matches!(err, CrabError::WorkflowDiscoveryAmbiguous { .. }));
    }

    #[test]
    fn recursive_mode_returns_root_first_then_sorted() {
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), "b/crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), "a/crab.yaml", MINI_YAML);

        let found = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        assert_eq!(found[0], tmp.path().join("crab.yaml"));
        // Tail is sorted lexicographically — `a/` before `b/`.
        assert_eq!(found[1], tmp.path().join("a/crab.yaml"));
        assert_eq!(found[2], tmp.path().join("b/crab.yaml"));
    }

    #[test]
    fn recursive_skips_ignored_directories() {
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "crab.yaml", MINI_YAML);
        // These should all be skipped.
        write_yaml(tmp.path(), ".git/crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), ".crab/crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), "node_modules/crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), "target/crab.yaml", MINI_YAML);

        let found = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        assert_eq!(found, vec![tmp.path().join("crab.yaml")]);
    }

    #[test]
    fn root_mode_ignores_yamls_inside_dot_git_and_dot_crab() {
        // Nested yaml inside an ignored directory must not trip the
        // ambiguity check — otherwise a fresh `.crab/` scratch dir
        // would block every Root-mode run.
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "crab.yaml", MINI_YAML);
        write_yaml(tmp.path(), ".git/modules/foo/crab.yaml", MINI_YAML);

        let found = discover(tmp.path(), DiscoverMode::Root).unwrap();
        assert_eq!(found, vec![tmp.path().join("crab.yaml")]);
    }

    #[test]
    fn merge_prefixes_nested_stage_names_with_dotted_directory() {
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "crab.yaml",
            "stages:\n  root_stage:\n    cmd: \"true\"\n",
        );
        write_yaml(
            tmp.path(),
            "data/crab.yaml",
            "stages:\n  clean:\n    cmd: \"true\"\n    deps:\n      - raw.csv\n    outs:\n      - path: clean.parquet\n        remote: cold-storage\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let merged = parse_all(tmp.path(), &paths).unwrap();

        // Root stage keeps its name.
        assert!(
            merged
                .stages
                .contains_key(&StageName::parse("root_stage").unwrap())
        );
        // Nested stage gets the `data.` prefix and its dep/out paths
        // are rewritten to be repo-relative.
        let nested = merged
            .stages
            .get(&StageName::parse_effective("data.clean").unwrap())
            .expect("nested stage prefixed with directory");
        match &nested.deps[0] {
            Dep::Path(p) => assert_eq!(p, &PathBuf::from("data/raw.csv")),
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(nested.outs[0].path, PathBuf::from("data/clean.parquet"));
        assert_eq!(nested.outs[0].remote.as_deref(), Some("cold-storage"));
    }

    #[test]
    fn merge_rewrites_stage_out_deps_within_same_nested_file() {
        let tmp = TempDir::new().unwrap();
        write_yaml(tmp.path(), "crab.yaml", "stages: {}\n");
        // Two nested stages where `b` depends on `a`'s out.
        write_yaml(
            tmp.path(),
            "data/crab.yaml",
            "stages:\n  a:\n    cmd: \"true\"\n    outs:\n      - a.bin\n  b:\n    cmd: \"true\"\n    deps:\n      - stage_out:\n          stage: a\n          out: a.bin\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let merged = parse_all(tmp.path(), &paths).unwrap();
        let b = merged
            .stages
            .get(&StageName::parse_effective("data.b").unwrap())
            .expect("data.b exists");
        match &b.deps[0] {
            Dep::StageOut { stage, out } => {
                assert_eq!(stage.as_str(), "data.a");
                assert_eq!(out, &PathBuf::from("data/a.bin"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn merge_rewrites_nested_stage_metrics_and_plots() {
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "pipelines/eval/crab.yaml",
            "stages:\n  score:\n    cmd: python score.py\n    metrics: [scores.json]\n    plots: [plots/roc.csv]\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let merged = parse_all(tmp.path(), &paths).unwrap();
        let stage = merged
            .stages
            .get(&StageName::parse_effective("pipelines.eval.score").unwrap())
            .expect("nested score stage exists");

        assert_eq!(
            stage.metrics,
            vec![PathBuf::from("pipelines/eval/scores.json")]
        );
        assert_eq!(
            stage.plots,
            vec![PathBuf::from("pipelines/eval/plots/roc.csv")]
        );
    }

    #[test]
    fn merge_rejects_duplicate_effective_names() {
        // Two nested files under `data/` shouldn't collide, but a
        // user who writes the same effective name across files
        // (here: root `data.clean` and nested `data/` stage `clean`)
        // gets a clean ambiguity error.
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "crab.yaml",
            // Root-level stage that happens to be named with a dot
            // is impossible via the base-name parser, so we
            // construct the collision by pointing two nested files
            // at the same prefix.
            "stages: {}\n",
        );
        write_yaml(
            tmp.path(),
            "data/crab.yaml",
            "stages:\n  clean:\n    cmd: \"true\"\n",
        );
        // Same directory, same stage name — in practice only one
        // crab.yaml can live in a dir, but we synthesize the
        // collision in memory to exercise the check.
        let nested_path = tmp.path().join("data/crab.yaml");
        let wf = yaml::parse(&std::fs::read_to_string(&nested_path).unwrap()).unwrap();

        let yamls = vec![
            (
                tmp.path().join("crab.yaml"),
                Workflow {
                    params: Vec::new(),
                    metrics: Vec::new(),
                    plots: Vec::new(),
                    plot_configs: Vec::new(),
                    defaults: Defaults::default(),
                    stages: BTreeMap::new(),
                    workflow_membership: BTreeMap::new(),
                },
            ),
            (nested_path.clone(), wf.clone()),
            (nested_path, wf),
        ];
        let err = merge(tmp.path(), &yamls).unwrap_err();
        assert!(matches!(err, CrabError::WorkflowDiscoveryAmbiguous { .. }));
    }

    #[test]
    fn path_to_dot_prefix_joins_segments_with_dot() {
        assert_eq!(path_to_dot_prefix("data").unwrap(), "data");
        assert_eq!(
            path_to_dot_prefix("pipelines/clean").unwrap(),
            "pipelines.clean"
        );
        assert_eq!(path_to_dot_prefix("").unwrap(), "");
    }

    #[test]
    fn path_to_dot_prefix_rejects_invalid_segments() {
        // Directory names that don't satisfy the stage-name grammar
        // surface as WorkflowStageNameInvalid — better than silently
        // producing a name the rest of the system will choke on.
        let err = path_to_dot_prefix("123bad").unwrap_err();
        assert!(matches!(err, CrabError::WorkflowStageNameInvalid { .. }));
    }

    #[test]
    fn recursive_mode_discovers_named_workflow_yaml_files() {
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "train.workflow.yaml",
            "stages:\n  preprocess:\n    cmd: \"true\"\n",
        );
        write_yaml(
            tmp.path(),
            "eval.workflow.yaml",
            "stages:\n  evaluate:\n    cmd: \"true\"\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"train.workflow.yaml".to_owned()));
        assert!(names.contains(&"eval.workflow.yaml".to_owned()));
    }

    #[test]
    fn merge_prefixes_named_workflow_yaml_stages_by_filename_stem() {
        // `train.workflow.yaml` at the repo root should have its
        // stages prefixed with `train.` so they don't collide with
        // other files.
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "train.workflow.yaml",
            "stages:\n  preprocess:\n    cmd: \"true\"\n",
        );
        write_yaml(
            tmp.path(),
            "eval.workflow.yaml",
            "stages:\n  evaluate:\n    cmd: \"true\"\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let merged = parse_all(tmp.path(), &paths).unwrap();

        assert!(
            merged
                .stages
                .contains_key(&StageName::parse_effective("train.preprocess").unwrap())
        );
        assert!(
            merged
                .stages
                .contains_key(&StageName::parse_effective("eval.evaluate").unwrap())
        );
    }

    #[test]
    fn merge_mixes_root_crab_yaml_with_named_workflow_yaml() {
        // A root `crab.yaml` carries shared defaults + root stages;
        // per-workflow files carry their own stages under a prefix.
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "crab.yaml",
            "stages:\n  setup:\n    cmd: \"true\"\n",
        );
        write_yaml(
            tmp.path(),
            "train.workflow.yaml",
            "stages:\n  preprocess:\n    cmd: \"true\"\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let merged = parse_all(tmp.path(), &paths).unwrap();

        assert!(
            merged
                .stages
                .contains_key(&StageName::parse("setup").unwrap())
        );
        assert!(
            merged
                .stages
                .contains_key(&StageName::parse_effective("train.preprocess").unwrap())
        );
    }

    #[test]
    fn merge_detects_prefix_collision_between_dir_and_filename() {
        // `train/crab.yaml` and `train.workflow.yaml` both want the
        // `train.` prefix. We refuse rather than silently merging.
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "train/crab.yaml",
            "stages:\n  preprocess:\n    cmd: \"true\"\n",
        );
        write_yaml(
            tmp.path(),
            "train.workflow.yaml",
            "stages:\n  augment:\n    cmd: \"true\"\n",
        );

        let paths = discover(tmp.path(), DiscoverMode::Recursive).unwrap();
        let err = parse_all(tmp.path(), &paths).unwrap_err();
        assert!(matches!(err, CrabError::WorkflowDiscoveryAmbiguous { .. }));
    }

    #[test]
    fn root_mode_rejects_named_workflow_yaml_files() {
        // In Root mode, any `*.workflow.yaml` is treated as "nested"
        // and refused — the user must opt into `discover = "recursive"`.
        let tmp = TempDir::new().unwrap();
        write_yaml(
            tmp.path(),
            "train.workflow.yaml",
            "stages:\n  preprocess:\n    cmd: \"true\"\n",
        );
        let err = discover(tmp.path(), DiscoverMode::Root).unwrap_err();
        assert!(matches!(err, CrabError::WorkflowDiscoveryAmbiguous { .. }));
    }
}
