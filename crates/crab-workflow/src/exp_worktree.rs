//! Experiment throwaway worktree management.
//!
//! Every `crab exp run` materializes the commit at `HEAD`
//! (captured at run start) into a fresh directory under
//! `.crab/workflow/exp/<exp_id>/`, overlays the user's `--set
//! key=value` parameter overrides onto the declared params files on
//! disk, and then hands the tmpdir to the DAG executor as an
//! isolated working tree. The main worktree is never mutated (R23).
//!
//! ## Why overlay overrides on disk rather than in memory?
//!
//! Stage hashing walks declared params files by path. If overrides
//! lived in a side channel the participating inputs would diverge
//! from what any `crab run` sees in the main worktree — cache
//! hits would become spurious and `--explain-miss` unhelpful. By
//! writing overrides onto disk the hasher needs no special case:
//! the params file *is* the source of truth, matching the main
//! worktree's invariant.
//!
//! ## Checkout strategy
//!
//! We shell out to `git read-tree <commit>` with a temporary index,
//! then `git checkout-index -a -f` into the tmpdir. This populates
//! the tmpdir from the exact committed tree without touching the
//! user's main index, runs the Crab clean/smudge filter on entries
//! that declare it, and leaves no registration in `.git/worktrees/`.
//! The tradeoff is that the tmpdir has no `.git` of its own — the
//! executor runs against a detached tree and never issues commits
//! back into it. That matches the experiment model: a run produces
//! cache entries and metadata blobs, not new commits on the tmpdir.
//!
//! ## Crash safety
//!
//! [`ExperimentWorktree::cleanup`] removes the tmpdir explicitly.
//! If the caller drops the handle without calling `cleanup` (panic,
//! early return), the [`Drop`] impl runs a best-effort removal.
//! When even that fails, the orphan sweep in
//! [`sweep_orphan_experiment_tmpdirs`] picks up any remaining
//! tmpdirs on the next invocation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{debug, info, warn};

use crate::experiment::ExperimentId;
use crate::materialize::write_atomic;
use crate::params;
use crate::yaml as yaml_mod;
use crate::{Result, WorkflowError as CrabError};

/// Relative path (from the repo root) to the parent directory that
/// holds per-experiment tmpdir worktrees. Exposed so the orphan
/// sweep and tests can reason about it without re-derivation.
pub const EXP_WORKTREE_PARENT_REL: &str = ".crab/workflow/exp";

/// Handle to a live experiment worktree.
///
/// Construction resolves `HEAD` to a concrete commit OID, populates
/// the tmpdir from that commit, and overlays the caller's
/// `param_overrides` onto the declared params files inside it.
/// Callers hand this tmpdir to the DAG executor as the
/// effective repo root for the duration of the run.
///
/// [`Drop`] performs a best-effort cleanup if the handle is lost;
/// well-ordered teardown should still call [`ExperimentWorktree::cleanup`]
/// so errors surface.
#[derive(Debug)]
pub struct ExperimentWorktree {
    /// Identifier this worktree was created for. Mirrors the
    /// last path component of [`Self::path`].
    pub exp_id: ExperimentId,

    /// Absolute path to the tmpdir:
    /// `{repo_root}/.crab/workflow/exp/<uuid>/`.
    pub path: PathBuf,

    /// Git commit OID captured at tmpdir creation. This is the
    /// `base_commit` the caller writes into
    /// [`crate::experiment::ExperimentMetadata::base_commit`].
    pub base_commit: String,

    /// True once [`Self::cleanup`] has removed the tmpdir so
    /// [`Drop`] becomes a no-op on explicit teardown paths.
    cleaned: bool,
}

impl ExperimentWorktree {
    /// Create a fresh experiment tmpdir under
    /// `{repo_root}/.crab/workflow/exp/<exp_id>/`, check HEAD out
    /// into it, and apply `param_overrides` to the declared params
    /// files.
    ///
    /// ## Errors
    ///
    /// - [`CrabError::ExperimentCollision`] — the target tmpdir
    ///   already exists. uuid7 collisions are astronomically rare,
    ///   so this almost certainly indicates a caller reusing an id
    ///   or a stale directory that the orphan sweep hasn't touched.
    /// - [`CrabError::Configuration`] — an override key didn't
    ///   match any structure in any declared params file, or
    ///   `crab.yaml` declares a params file the repo doesn't ship.
    /// - [`CrabError::Io`] — filesystem failures during mkdir,
    ///   checkout, or override write-back.
    /// - [`CrabError::Internal`] — git subprocess failures
    ///   (spawn, non-zero exit) or HEAD resolution failures.
    ///
    /// Failure after the tmpdir has been created cleans up the
    /// partial tmpdir before returning, so callers don't need a
    /// defensive `Drop` guard around the whole construction.
    pub fn create(
        repo_root: &Path,
        exp_id: ExperimentId,
        param_overrides: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let repo_root_abs = absolutize(repo_root)?;
        let git_dir = resolve_git_dir(&repo_root_abs)?;

        // Capture HEAD before any on-disk work. If HEAD moves after
        // this point the tmpdir still reflects the commit we pinned.
        let base_commit = resolve_head_commit(&git_dir, &repo_root_abs)?;

        Self::create_resolved(repo_root_abs, git_dir, exp_id, base_commit, param_overrides)
    }

    /// Create a fresh experiment tmpdir from a previously captured
    /// commit. Queued experiments use this path so `crab exp start`
    /// runs the tree that was queued, even if the user's workspace has
    /// moved on.
    pub fn create_at_commit(
        repo_root: &Path,
        exp_id: ExperimentId,
        base_commit: &str,
        param_overrides: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let repo_root_abs = absolutize(repo_root)?;
        let git_dir = resolve_git_dir(&repo_root_abs)?;
        Self::create_resolved(
            repo_root_abs,
            git_dir,
            exp_id,
            base_commit.to_owned(),
            param_overrides,
        )
    }

    fn create_resolved(
        repo_root_abs: PathBuf,
        git_dir: PathBuf,
        exp_id: ExperimentId,
        base_commit: String,
        param_overrides: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let parent_dir = repo_root_abs.join(EXP_WORKTREE_PARENT_REL);
        let tmpdir = parent_dir.join(exp_id.to_string());

        if tmpdir.exists() {
            return Err(CrabError::ExperimentCollision {
                id: exp_id.to_string(),
            });
        }

        // Parent may not exist on a repo that hasn't opted into the
        // workflow layer yet. `create_dir_all` is idempotent.
        fs::create_dir_all(&parent_dir).map_err(CrabError::Io)?;
        fs::create_dir(&tmpdir).map_err(CrabError::Io)?;

        // From here on we own the tmpdir — roll it back on any
        // subsequent error so a partial failure doesn't leak disk.
        let guard = CleanupOnDrop::new(&tmpdir);

        checkout_commit_into(&git_dir, &repo_root_abs, &tmpdir, &base_commit)?;
        copy_repo_config_into_worktree(&repo_root_abs, &tmpdir)?;

        let param_overrides = crate::hydra::compose_if_enabled(&tmpdir, param_overrides)?;
        apply_overrides(&tmpdir, &param_overrides)?;

        // Commit the cleanup guard: we've succeeded, the worktree
        // handle's Drop takes over responsibility.
        guard.disarm();

        info!(
            exp_id = %exp_id,
            base_commit = %base_commit,
            tmpdir = %tmpdir.display(),
            "experiment worktree created"
        );

        Ok(Self {
            exp_id,
            path: tmpdir,
            base_commit,
            cleaned: false,
        })
    }

    /// Remove the tmpdir. After this returns successfully, dropping
    /// the handle is a no-op.
    ///
    /// Errors from `remove_dir_all` are logged at `warn!` and
    /// returned; the orphan sweep ([`sweep_orphan_experiment_tmpdirs`])
    /// will catch anything that survived, so callers can safely
    /// ignore the error in cleanup contexts that are best-effort.
    pub fn cleanup(mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {
                self.cleaned = true;
                debug!(
                    exp_id = %self.exp_id,
                    tmpdir = %self.path.display(),
                    "experiment worktree cleaned up"
                );
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone — idempotent success path. The orphan
                // sweep or a previous cleanup attempt got there first.
                self.cleaned = true;
                Ok(())
            }
            Err(e) => {
                warn!(
                    exp_id = %self.exp_id,
                    tmpdir = %self.path.display(),
                    error = %e,
                    "experiment worktree cleanup failed; orphan sweep will retry"
                );
                Err(CrabError::Io(e))
            }
        }
    }
}

impl Drop for ExperimentWorktree {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        // Never panic from Drop. `remove_dir_all` on a missing path
        // returns NotFound; anything else gets logged and swallowed
        // so the orphan sweep handles it next run.
        if let Err(e) = fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                exp_id = %self.exp_id,
                tmpdir = %self.path.display(),
                error = %e,
                "experiment worktree drop cleanup failed"
            );
        }
    }
}

/// Remove experiment tmpdirs whose `exp_id` isn't in `active_exp_ids`.
///
/// Runs over `{repo_root}/.crab/workflow/exp/*/` and deletes any
/// directory whose name parses as a UUID not present in the caller-
/// supplied active set. Directories whose names don't parse as a
/// UUID are left alone — they may belong to future subsystems that
/// share the same parent. Returns the number of tmpdirs removed.
///
/// Mirrors [`crate::resume::sweep_orphan_sidecars`]'s
/// contract: absent parent directory is not an error, and per-entry
/// removal failures surface as [`CrabError::Io`].
pub fn sweep_orphan_experiment_tmpdirs(
    repo_root: &Path,
    active_exp_ids: &[ExperimentId],
) -> Result<usize> {
    let parent = repo_root.join(EXP_WORKTREE_PARENT_REL);
    let iter = match fs::read_dir(&parent) {
        Ok(i) => i,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CrabError::Io(e)),
    };

    // Build a set for O(1) membership checks. Set size is capped by
    // concurrent run count (small), so the allocation is cheap.
    let active: BTreeSet<ExperimentId> = active_exp_ids.iter().copied().collect();

    let mut removed = 0usize;
    for entry in iter {
        let entry = entry.map_err(CrabError::Io)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;
        if !file_type.is_dir() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Parse the directory name as an experiment id. Directories
        // that aren't experiment tmpdirs (bad UUID shape, non-v7
        // UUID) get skipped — deleting those would be overreach.
        let Ok(parsed) = name.parse::<ExperimentId>() else {
            continue;
        };

        if active.contains(&parsed) {
            continue;
        }

        match fs::remove_dir_all(&path) {
            Ok(()) => {
                removed += 1;
                info!(
                    exp_id = %parsed,
                    tmpdir = %path.display(),
                    "orphan experiment worktree removed"
                );
            }
            Err(e) => {
                warn!(
                    exp_id = %parsed,
                    tmpdir = %path.display(),
                    error = %e,
                    "orphan experiment worktree removal failed"
                );
                return Err(CrabError::Io(e));
            }
        }
    }

    Ok(removed)
}

// ─── helpers ──────────────────────────────────────────────────────────

/// Canonicalize `path` to an absolute form. Canonicalization follows
/// symlinks, which is desirable here: the tmpdir path we hand to
/// `git checkout-index --work-tree` must be the filesystem's own
/// name for the directory so the filter driver's path fixups agree.
fn absolutize(path: &Path) -> Result<PathBuf> {
    // `fs::canonicalize` requires the path to exist. For repos in
    // normal state that's true; fall back to a plain absolute form
    // if not (tests may hand us a just-created temp dir that hasn't
    // been canonicalized yet — `TempDir` paths are already canonical
    // on most platforms, so this is belt-and-suspenders).
    match fs::canonicalize(path) {
        Ok(p) => Ok(strip_verbatim_prefix(p)),
        Err(_) => {
            if path.is_absolute() {
                Ok(strip_verbatim_prefix(path.to_path_buf()))
            } else {
                let cwd = std::env::current_dir().map_err(CrabError::Io)?;
                Ok(strip_verbatim_prefix(cwd.join(path)))
            }
        }
    }
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = value.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// Resolve the `.git` directory that anchors `repo_root`.
///
/// Unlike [`crate::git::discover::discover_git_dir`], we do not
/// honor `GIT_DIR`: callers pass an explicit repo root and we want
/// deterministic resolution from that root. Mirrors the approach in
/// [`crate::params`].
fn resolve_git_dir(repo_root: &Path) -> Result<PathBuf> {
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

/// Resolve HEAD to a 40-char hex commit OID.
///
/// On `--features gix-facade`, uses `gix::Repository::rev_parse_single()`.
/// Default builds shell out to `git rev-parse --verify HEAD` scoped
/// to the explicit `git_dir`, matching the pattern used elsewhere
/// in the workflow layer (see [`crate::params`]).
fn resolve_head_commit(git_dir: &Path, work_tree: &Path) -> Result<String> {
    #[cfg(feature = "gix-facade")]
    {
        let _ = work_tree;
        let repo = gix::open(git_dir).map_err(|error| CrabError::Internal(error.to_string()))?;
        let id = repo.rev_parse_single("HEAD").map_err(|e| {
            CrabError::Internal(format!(
                "git rev-parse HEAD failed in {}: {e}",
                git_dir.display()
            ))
        })?;
        let sha = id.to_hex().to_string();
        // `gix` returns full hex for SHA-1 repos (crab's only
        // hash kind today). Keep the strict check for safety —
        // future SHA-256 adoption bumps to 64 chars and should
        // flip this check rather than silently producing a
        // mismatched SHA downstream.
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CrabError::Internal(format!(
                "rev_parse returned unexpected HEAD SHA '{sha}'"
            )));
        }
        Ok(sha)
    }

    #[cfg(not(feature = "gix-facade"))]
    {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(work_tree)
            .env("GIT_DIR", git_dir)
            .env_remove("GIT_WORK_TREE")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| CrabError::Internal(format!("failed to spawn git rev-parse: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CrabError::Internal(format!(
                "git rev-parse HEAD failed in {}: {}",
                work_tree.display(),
                stderr.trim()
            )));
        }

        let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(CrabError::Internal(format!(
                "git rev-parse returned unexpected HEAD SHA '{sha}'"
            )));
        }
        Ok(sha)
    }
}

/// Run `git read-tree <commit>` and `git checkout-index -a -f` into `tmpdir`.
///
/// `--work-tree=<tmpdir>` and `--git-dir=<repo>/.git` scope the
/// command precisely so the checkout lands in the tmpdir without
/// touching the main working tree. `-a` checks out all cached
/// entries at the given index; `-f` forces overwrite (the tmpdir is
/// empty, but keeps the command idempotent in case we ever need to
/// re-run it).
fn checkout_commit_into(
    git_dir: &Path,
    work_tree: &Path,
    tmpdir: &Path,
    base_commit: &str,
) -> Result<()> {
    // SHELLOUT: delegates to git's checkout-index machinery to
    // materialize a commit into a tmpdir for experiment workspaces.
    // gitoxide's worktree-state checkout supports this shape but
    // routes through an ODB adapter (see `requirements.md` Req 6 /
    // Task 7). This site is not in Task 8's scope — the spec's Keep
    // table for workflow worktree creation applies until the Task 7
    // rollout brings gitoxide's checkout online for this path.
    let index_parent = tmpdir.parent().ok_or_else(|| {
        CrabError::Internal(format!(
            "experiment tmpdir has no parent: {}",
            tmpdir.display()
        ))
    })?;
    let index_file = tempfile::NamedTempFile::new_in(index_parent).map_err(CrabError::Io)?;
    let index_path = index_file.path().to_path_buf();

    let read_tree = Command::new("git")
        .args([
            "--git-dir",
            &git_dir.to_string_lossy(),
            "read-tree",
            base_commit,
        ])
        .current_dir(work_tree)
        .env("GIT_INDEX_FILE", &index_path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git read-tree: {e}")))?;

    if !read_tree.status.success() {
        let stderr = String::from_utf8_lossy(&read_tree.stderr);
        return Err(CrabError::Internal(format!(
            "git read-tree failed for commit {base_commit}: {}",
            stderr.trim()
        )));
    }

    let status = Command::new("git")
        .args([
            "--git-dir",
            &git_dir.to_string_lossy(),
            "--work-tree",
            &tmpdir.to_string_lossy(),
            "checkout-index",
            "-a",
            "-f",
        ])
        .current_dir(work_tree)
        .env("GIT_INDEX_FILE", &index_path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to spawn git checkout-index: {e}")))?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(CrabError::Internal(format!(
            "git checkout-index failed for commit {base_commit} into {}: {}",
            tmpdir.display(),
            stderr.trim()
        )));
    }

    Ok(())
}

fn copy_repo_config_into_worktree(repo_root: &Path, tmpdir: &Path) -> Result<()> {
    let src = repo_root.join(".crab").join("config.toml");
    if !src.is_file() {
        return Ok(());
    }

    let dst = tmpdir.join(".crab").join("config.toml");
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    fs::copy(&src, &dst).map_err(CrabError::Io)?;
    Ok(())
}

/// Apply `--set key=value` overrides to declared params files in
/// `tmpdir`.
///
/// Reads `crab.yaml` from the tmpdir (no-op if absent — experiments
/// without a workflow YAML have nowhere to apply overrides, so the
/// operation silently succeeds when there are no overrides, and
/// errors when there are), reads each declared params file, sets
/// the dotted-key value in the parsed document, and writes it back
/// via [`write_atomic`].
///
/// Every override must resolve to some declared params file; an
/// unmatched override is a user error and surfaces as
/// [`CrabError::Configuration`] so silent mismatches can't make
/// stage hashing inconsistent.
fn apply_overrides(tmpdir: &Path, overrides: &BTreeMap<String, String>) -> Result<()> {
    apply_declared_param_overrides(tmpdir, overrides, "experiment", "--set")
}

pub(crate) fn apply_declared_param_overrides(
    tmpdir: &Path,
    overrides: &BTreeMap<String, String>,
    origin: &str,
    flag_name: &str,
) -> Result<()> {
    if overrides.is_empty() {
        return Ok(());
    }

    let declared_params = declared_params_for_overrides(tmpdir)?;
    if declared_params.is_empty() {
        // Overrides were requested but there's nowhere to apply them.
        // Name the first offending key so the diagnostic is
        // actionable rather than vague.
        //
        // `overrides` is known non-empty (we returned early above)
        // but structuring the error path around that invariant via
        // an explicit match keeps the code expect-free.
        let first_key = match overrides.keys().next() {
            Some(k) => k.as_str(),
            None => "<none>",
        };
        return Err(CrabError::Configuration {
            key: format!(
                "{flag_name} key not found in any declared params file: {first_key} \
                 (crab.yaml declares no params files in the tmpdir)"
            ),
            origin: origin.to_owned(),
        });
    }

    // Track which keys have been absorbed so we can flag unused
    // overrides at the end. Removing from a local clone keeps the
    // caller's map intact.
    let mut remaining: BTreeMap<String, String> = overrides.clone();

    for rel_params_path in &declared_params {
        let params_path = tmpdir.join(rel_params_path);
        if !params_path.is_file() {
            // Declared in crab.yaml but absent in the checkout —
            // skip rather than fail: the stage runner will catch
            // this as a missing dep with a more informative error.
            continue;
        }

        let format = detect_params_format(&params_path)?;
        let original = fs::read_to_string(&params_path).map_err(CrabError::Io)?;
        let (document_overrides, mut document_keys) =
            overrides_for_params_file(rel_params_path, &remaining, origin, flag_name)?;

        let (updated_text, consumed_document_keys) =
            apply_overrides_to_document(&original, format, &document_overrides)?;
        let mut consumed = Vec::new();
        for document_key in consumed_document_keys {
            if let Some(original_key) = document_keys.remove(&document_key) {
                consumed.push(original_key);
            }
        }
        for k in &consumed {
            remaining.remove(k);
        }

        if !consumed.is_empty() {
            // Preserve existing mode bits on the file so the
            // override doesn't suddenly make a 0o600 secrets file
            // world-readable.
            let mode = file_mode(&params_path)?;
            let run_id = uuid::Uuid::now_v7();
            write_atomic(&params_path, updated_text.as_bytes(), run_id, mode)?;
            debug!(
                params_path = %params_path.display(),
                applied = consumed.len(),
                "experiment overrides written"
            );
        }
    }

    if !remaining.is_empty() {
        let unmatched = remaining.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(CrabError::Configuration {
            key: format!("{flag_name} key(s) not found in any declared params file: {unmatched}"),
            origin: origin.to_owned(),
        });
    }

    Ok(())
}

fn declared_params_for_overrides(tmpdir: &Path) -> Result<Vec<PathBuf>> {
    let yaml_path = tmpdir.join("crab.yaml");
    let mut declared_params: Vec<PathBuf> = if yaml_path.is_file() {
        let text = fs::read_to_string(&yaml_path).map_err(CrabError::Io)?;
        let wf = yaml_mod::parse_at(&yaml_path, &text)?;
        wf.params
    } else {
        Vec::new()
    };

    if declared_params.is_empty() && tmpdir.join("params.yaml").is_file() {
        declared_params.push(PathBuf::from("params.yaml"));
    }

    Ok(declared_params)
}

fn overrides_for_params_file(
    rel_params_path: &Path,
    remaining: &BTreeMap<String, String>,
    origin: &str,
    flag_name: &str,
) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let mut document_overrides = BTreeMap::new();
    let mut document_keys = BTreeMap::new();

    for (override_key, value) in remaining {
        if let Some((file, key)) = split_file_scoped_override(override_key) {
            if Path::new(file) != rel_params_path {
                continue;
            }
            insert_document_override(
                rel_params_path,
                &mut document_overrides,
                &mut document_keys,
                key,
                override_key,
                value,
                origin,
                flag_name,
            )?;
        } else {
            insert_document_override(
                rel_params_path,
                &mut document_overrides,
                &mut document_keys,
                override_key,
                override_key,
                value,
                origin,
                flag_name,
            )?;
        }
    }

    Ok((document_overrides, document_keys))
}

fn split_file_scoped_override(key: &str) -> Option<(&str, &str)> {
    let (file, param) = key.split_once(':')?;
    if file.is_empty() || param.is_empty() {
        return None;
    }
    Some((file, param))
}

fn insert_document_override(
    rel_params_path: &Path,
    document_overrides: &mut BTreeMap<String, String>,
    document_keys: &mut BTreeMap<String, String>,
    document_key: &str,
    original_key: &str,
    value: &str,
    origin: &str,
    flag_name: &str,
) -> Result<()> {
    let target_key = override_target_key(document_key);
    if document_overrides
        .keys()
        .any(|existing| override_target_key(existing) == target_key)
    {
        return Err(CrabError::Configuration {
            key: format!(
                "multiple {flag_name} keys target {}:{target_key}",
                rel_params_path.display()
            ),
            origin: origin.to_owned(),
        });
    }
    document_overrides.insert(document_key.to_owned(), value.to_owned());
    document_keys.insert(document_key.to_owned(), original_key.to_owned());
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideOp {
    SetExisting,
    Add,
    AddOrSet,
    Remove,
}

fn parse_override_op(key: &str) -> (OverrideOp, &str) {
    if let Some(rest) = key.strip_prefix("++") {
        (OverrideOp::AddOrSet, rest)
    } else if let Some(rest) = key.strip_prefix('+') {
        (OverrideOp::Add, rest)
    } else if let Some(rest) = key.strip_prefix('~') {
        (OverrideOp::Remove, rest)
    } else {
        (OverrideOp::SetExisting, key)
    }
}

fn override_target_key(key: &str) -> String {
    parse_override_op(key).1.to_owned()
}

/// Returns whether an override operation may target a missing value.
#[must_use]
pub fn override_allows_missing_value(key: &str) -> bool {
    let (_, document_key) = split_file_scoped_override(key).unwrap_or(("", key));
    matches!(parse_override_op(document_key).0, OverrideOp::Remove)
}

/// Parse the file extension to decide which document format to
/// emit. Matches [`crate::params::parse`]'s extension
/// dispatch so overrides land in a file the stage hasher will read
/// back identically.
fn detect_params_format(path: &Path) -> Result<ParamsFormat> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("yaml" | "yml") => Ok(ParamsFormat::Yaml),
        Some("json") => Ok(ParamsFormat::Json),
        Some("toml") => Ok(ParamsFormat::Toml),
        Some("py") => Ok(ParamsFormat::Python),
        other => Err(CrabError::Configuration {
            key: format!(
                "{}: unsupported params extension {:?} (expected .yaml, .yml, .json, .toml, or .py)",
                path.display(),
                other.unwrap_or("<none>")
            ),
            origin: "experiment".into(),
        }),
    }
}

/// Which on-disk format a params file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamsFormat {
    Yaml,
    Json,
    Toml,
    Python,
}

/// Apply the subset of `overrides` that match dotted keys in `text`,
/// re-emit the document, and return `(new_text, consumed_keys)`.
///
/// The caller passes the full override map; keys that don't match
/// the document are ignored (they may match a *different* declared
/// params file). Emitting the new text uses the same serializer as
/// the corresponding parser, so round-tripping preserves ordering
/// and comments conservatively (serde_yaml preserves insertion order
/// for mappings; TOML and JSON order keys by their types' iteration
/// rules).
fn apply_overrides_to_document(
    text: &str,
    format: ParamsFormat,
    overrides: &BTreeMap<String, String>,
) -> Result<(String, Vec<String>)> {
    match format {
        ParamsFormat::Yaml => apply_yaml(text, overrides),
        ParamsFormat::Json => apply_json(text, overrides),
        ParamsFormat::Toml => apply_toml(text, overrides),
        ParamsFormat::Python => Ok(apply_python(text, overrides)),
    }
}

fn apply_yaml(text: &str, overrides: &BTreeMap<String, String>) -> Result<(String, Vec<String>)> {
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| CrabError::Configuration {
            key: format!("yaml parse error during override: {e}"),
            origin: "experiment".into(),
        })?;

    let mut consumed = Vec::new();
    for (dotted, raw) in overrides {
        if apply_yaml_override(&mut value, dotted, raw) {
            consumed.push(dotted.clone());
        }
    }

    let serialized = serde_yaml::to_string(&value).map_err(|e| CrabError::Configuration {
        key: format!("yaml serialize error after override: {e}"),
        origin: "experiment".into(),
    })?;
    Ok((serialized, consumed))
}

fn apply_json(text: &str, overrides: &BTreeMap<String, String>) -> Result<(String, Vec<String>)> {
    let mut value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| CrabError::Configuration {
            key: format!("json parse error during override: {e}"),
            origin: "experiment".into(),
        })?;

    let mut consumed = Vec::new();
    for (dotted, raw) in overrides {
        if apply_json_override(&mut value, dotted, raw) {
            consumed.push(dotted.clone());
        }
    }

    // `to_string_pretty` keeps the file human-diffable.
    let serialized =
        serde_json::to_string_pretty(&value).map_err(|e| CrabError::Configuration {
            key: format!("json serialize error after override: {e}"),
            origin: "experiment".into(),
        })?;
    // Preserve the conventional trailing newline that most JSON
    // formatters emit; diffs stay one-liner-tidy.
    let mut serialized = serialized;
    if !serialized.ends_with('\n') {
        serialized.push('\n');
    }
    Ok((serialized, consumed))
}

fn apply_toml(text: &str, overrides: &BTreeMap<String, String>) -> Result<(String, Vec<String>)> {
    let mut value: toml::Value = toml::from_str(text).map_err(|e| CrabError::Configuration {
        key: format!("toml parse error during override: {e}"),
        origin: "experiment".into(),
    })?;

    let mut consumed = Vec::new();
    for (dotted, raw) in overrides {
        if apply_toml_override(&mut value, dotted, raw) {
            consumed.push(dotted.clone());
        }
    }

    let serialized = toml::to_string(&value).map_err(|e| CrabError::Configuration {
        key: format!("toml serialize error after override: {e}"),
        origin: "experiment".into(),
    })?;
    Ok((serialized, consumed))
}

fn apply_python(text: &str, overrides: &BTreeMap<String, String>) -> (String, Vec<String>) {
    let assignments = collect_python_assignments(text);
    let mut chunks: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    if chunks.is_empty() && !text.is_empty() {
        chunks.push(text.to_owned());
    }

    let mut assignment_by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut values = Vec::with_capacity(assignments.len());
    for (idx, assignment) in assignments.iter().enumerate() {
        assignment_by_name
            .entry(assignment.name.clone())
            .or_default()
            .push(idx);
        values.push(Some(assignment.value.clone()));
    }

    let mut touched = BTreeSet::new();
    let mut appends: Vec<(String, params::PythonLiteral)> = Vec::new();
    let mut consumed = Vec::new();

    for (dotted, raw) in overrides {
        if apply_python_override(
            dotted,
            raw,
            &assignment_by_name,
            &mut values,
            &mut touched,
            &mut appends,
        ) {
            consumed.push(dotted.clone());
        }
    }

    for idx in touched {
        let assignment = &assignments[idx];
        match values.get(idx).and_then(Option::as_ref) {
            Some(value) => {
                chunks[assignment.start_line] = format!(
                    "{} = {}{}",
                    assignment.lhs.trim_end(),
                    serialize_python_literal(value),
                    assignment.line_ending
                );
                for chunk in chunks
                    .iter_mut()
                    .take(assignment.end_line)
                    .skip(assignment.start_line + 1)
                {
                    chunk.clear();
                }
            }
            None => {
                for chunk in chunks
                    .iter_mut()
                    .take(assignment.end_line)
                    .skip(assignment.start_line)
                {
                    chunk.clear();
                }
            }
        }
    }

    if !appends.is_empty() {
        if let Some(last) = chunks.last_mut()
            && !last.ends_with('\n')
        {
            last.push('\n');
        }
        for (name, value) in appends {
            chunks.push(format!("{name} = {}\n", serialize_python_literal(&value)));
        }
    }

    (chunks.concat(), consumed)
}

#[derive(Debug, Clone)]
struct PythonAssignment {
    name: String,
    lhs: String,
    start_line: usize,
    end_line: usize,
    line_ending: String,
    value: params::PythonLiteral,
}

fn collect_python_assignments(text: &str) -> Vec<PythonAssignment> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut assignments = Vec::new();
    let mut idx = 0;

    while idx < lines.len() {
        let (body, ending) = split_line_ending(lines[idx]);
        if body.len() != body.trim_start().len() {
            idx += 1;
            continue;
        }

        if let Some(class_name) = parse_python_class_name(body) {
            idx = collect_python_class_assignments(&lines, idx, class_name, &mut assignments);
            continue;
        }

        let Some((name, lhs, rhs)) = split_python_assignment_line(body) else {
            idx += 1;
            continue;
        };

        if let Some((value, end_line)) =
            parse_python_literal_from_lines(&lines, idx + 1, rhs, lines.len())
        {
            assignments.push(PythonAssignment {
                name: name.to_owned(),
                lhs: lhs.to_owned(),
                start_line: idx,
                end_line,
                line_ending: if ending.is_empty() {
                    "\n".to_owned()
                } else {
                    ending.to_owned()
                },
                value,
            });
            idx = end_line;
        } else {
            idx += 1;
        }
    }

    assignments
}

fn collect_python_class_assignments(
    lines: &[&str],
    class_line: usize,
    class_name: &str,
    assignments: &mut Vec<PythonAssignment>,
) -> usize {
    let class_indent = leading_python_indent(lines[class_line]);
    let end = python_block_end(lines, class_line + 1, class_indent);
    let body_indent = first_python_significant_indent(lines, class_line + 1, end);
    let mut idx = class_line + 1;

    while idx < end {
        if is_python_blank_or_comment(lines[idx])
            || Some(leading_python_indent(lines[idx])) != body_indent
        {
            idx += 1;
            continue;
        }

        let (body, ending) = split_line_ending(lines[idx]);
        let trimmed = body.trim_start();
        let indent = &body[..body.len() - trimmed.len()];
        if is_python_init_def(trimmed) {
            let def_indent = leading_python_indent(lines[idx]);
            let def_end = python_block_end(lines, idx + 1, def_indent);
            collect_python_init_assignments(lines, idx + 1, def_end, class_name, assignments);
            idx = def_end;
            continue;
        }

        let Some((name, lhs, rhs)) = split_python_assignment_line(trimmed) else {
            idx += 1;
            continue;
        };
        if let Some((value, end_line)) = parse_python_literal_from_lines(lines, idx + 1, rhs, end) {
            assignments.push(PythonAssignment {
                name: format!("{class_name}.{name}"),
                lhs: format!("{indent}{lhs}"),
                start_line: idx,
                end_line,
                line_ending: if ending.is_empty() {
                    "\n".to_owned()
                } else {
                    ending.to_owned()
                },
                value,
            });
            idx = end_line;
        } else {
            idx += 1;
        }
    }

    end
}

fn collect_python_init_assignments(
    lines: &[&str],
    start: usize,
    end: usize,
    class_name: &str,
    assignments: &mut Vec<PythonAssignment>,
) {
    let mut idx = start;
    while idx < end {
        if is_python_blank_or_comment(lines[idx]) {
            idx += 1;
            continue;
        }

        let (body, ending) = split_line_ending(lines[idx]);
        let trimmed = body.trim_start();
        let indent = &body[..body.len() - trimmed.len()];
        let Some((name, lhs, rhs)) = split_python_self_assignment_line(trimmed) else {
            idx += 1;
            continue;
        };
        if let Some((value, end_line)) = parse_python_literal_from_lines(lines, idx + 1, rhs, end) {
            assignments.push(PythonAssignment {
                name: format!("{class_name}.{name}"),
                lhs: format!("{indent}{lhs}"),
                start_line: idx,
                end_line,
                line_ending: if ending.is_empty() {
                    "\n".to_owned()
                } else {
                    ending.to_owned()
                },
                value,
            });
            idx = end_line;
        } else {
            idx += 1;
        }
    }
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(body) = line.strip_suffix("\r\n") {
        (body, "\r\n")
    } else if let Some(body) = line.strip_suffix('\n') {
        (body, "\n")
    } else {
        (line, "")
    }
}

fn split_python_assignment_line(line: &str) -> Option<(&str, &str, &str)> {
    let name_end = python_ident_end(line, 0)?;
    let name = &line[..name_end];
    if is_python_keyword(name) {
        return None;
    }

    let mut pos = skip_python_inline_ws(line, name_end);
    if line[pos..].starts_with(':') {
        pos += 1;
        let eq = line[pos..].find('=')?;
        pos += eq;
    }

    if !line[pos..].starts_with('=') || line[pos..].starts_with("==") {
        return None;
    }

    Some((name, &line[..pos], &line[pos + 1..]))
}

fn split_python_self_assignment_line(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix("self.")?;
    let name_end = python_ident_end(rest, 0)?;
    let name = &rest[..name_end];

    let mut pos = skip_python_inline_ws(rest, name_end);
    if rest[pos..].starts_with(':') {
        pos += 1;
        let eq = rest[pos..].find('=')?;
        pos += eq;
    }

    if !rest[pos..].starts_with('=') || rest[pos..].starts_with("==") {
        return None;
    }

    Some((name, &line[..("self.".len() + pos)], &rest[pos + 1..]))
}

fn parse_python_class_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("class ")?;
    let name_end = python_ident_end(rest, 0)?;
    let name = &rest[..name_end];
    let tail = rest[name_end..].trim_start();
    if tail.starts_with(':') || tail.starts_with('(') {
        Some(name)
    } else {
        None
    }
}

fn is_python_init_def(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("def") else {
        return false;
    };
    rest.trim_start().starts_with("__init__(")
}

fn leading_python_indent(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn is_python_blank_or_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

fn python_block_end(lines: &[&str], start: usize, parent_indent: usize) -> usize {
    let mut idx = start;
    while idx < lines.len() {
        if is_python_blank_or_comment(lines[idx]) {
            idx += 1;
            continue;
        }
        if leading_python_indent(lines[idx]) <= parent_indent {
            break;
        }
        idx += 1;
    }
    idx
}

fn first_python_significant_indent(lines: &[&str], start: usize, end: usize) -> Option<usize> {
    lines
        .iter()
        .take(end)
        .skip(start)
        .find(|line| !is_python_blank_or_comment(line))
        .map(|line| leading_python_indent(line))
}

fn parse_python_literal_from_lines(
    lines: &[&str],
    mut next: usize,
    rhs: &str,
    end: usize,
) -> Option<(params::PythonLiteral, usize)> {
    let mut expr = rhs.to_owned();
    loop {
        match params::parse_python_literal(&expr) {
            Ok(value) => return Some((value, next)),
            Err(params::PythonParseError::Incomplete) if next < end => {
                let (next_body, _) = split_line_ending(lines[next]);
                expr.push('\n');
                expr.push_str(next_body);
                next += 1;
            }
            Err(params::PythonParseError::Incomplete | params::PythonParseError::Unsupported) => {
                return None;
            }
        }
    }
}

fn python_ident_end(text: &str, start: usize) -> Option<usize> {
    let mut chars = text[start..].char_indices();
    let (_, first) = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = start + first.len_utf8();
    for (offset, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = start + offset + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn skip_python_inline_ws(text: &str, mut pos: usize) -> usize {
    while let Some(ch) = text[pos..].chars().next() {
        if ch != ' ' && ch != '\t' {
            break;
        }
        pos += ch.len_utf8();
    }
    pos
}

fn is_python_keyword(name: &str) -> bool {
    matches!(
        name,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

fn apply_python_override(
    dotted: &str,
    raw: &str,
    assignment_by_name: &BTreeMap<String, Vec<usize>>,
    values: &mut [Option<params::PythonLiteral>],
    touched: &mut BTreeSet<usize>,
    appends: &mut Vec<(String, params::PythonLiteral)>,
) -> bool {
    let (op, key) = parse_override_op(dotted);
    let new_value = python_literal_from_raw(raw);
    if let Some(indices) = assignment_by_name.get(key) {
        return apply_python_top_level_override(op, indices, new_value, values, touched);
    }

    let components: Vec<&str> = key.split('.').filter(|seg| !seg.is_empty()).collect();
    let Some((top, rest)) = components.split_first() else {
        return false;
    };

    if let Some(indices) = assignment_by_name.get(*top) {
        if rest.is_empty() {
            return apply_python_top_level_override(op, indices, new_value, values, touched);
        }
        let Some(idx) = indices.last().copied() else {
            return false;
        };
        let Some(Some(value)) = values.get_mut(idx) else {
            return false;
        };
        if apply_python_nested_override(value, rest, op, new_value) {
            touched.insert(idx);
            return true;
        }
        return false;
    }

    if has_python_class_assignments(top, assignment_by_name) {
        return false;
    }

    match op {
        OverrideOp::Add | OverrideOp::AddOrSet => {
            let value = if rest.is_empty() {
                new_value
            } else {
                python_literal_for_nested_path(rest, new_value)
            };
            appends.push(((*top).to_owned(), value));
            true
        }
        OverrideOp::SetExisting | OverrideOp::Remove => false,
    }
}

fn has_python_class_assignments(
    name: &str,
    assignment_by_name: &BTreeMap<String, Vec<usize>>,
) -> bool {
    let prefix = format!("{name}.");
    assignment_by_name
        .keys()
        .any(|key| key.starts_with(&prefix))
}

fn apply_python_top_level_override(
    op: OverrideOp,
    indices: &[usize],
    new_value: params::PythonLiteral,
    values: &mut [Option<params::PythonLiteral>],
    touched: &mut BTreeSet<usize>,
) -> bool {
    match op {
        OverrideOp::SetExisting | OverrideOp::AddOrSet => {
            let Some(idx) = indices.last().copied() else {
                return false;
            };
            values[idx] = Some(new_value);
            touched.insert(idx);
            true
        }
        OverrideOp::Add => false,
        OverrideOp::Remove => {
            if indices.is_empty() {
                return false;
            }
            for idx in indices {
                values[*idx] = None;
                touched.insert(*idx);
            }
            true
        }
    }
}

fn apply_python_nested_override(
    value: &mut params::PythonLiteral,
    path: &[&str],
    op: OverrideOp,
    new_value: params::PythonLiteral,
) -> bool {
    let Some((head, tail)) = path.split_first() else {
        return false;
    };

    if tail.is_empty() {
        return apply_python_nested_leaf(value, head, op, new_value);
    }

    match value {
        params::PythonLiteral::Mapping(map) => {
            if let Some(child) = map.get_mut(*head) {
                return apply_python_nested_override(child, tail, op, new_value);
            }
            if matches!(op, OverrideOp::Add | OverrideOp::AddOrSet) {
                let child = map
                    .entry((*head).to_owned())
                    .or_insert_with(|| params::PythonLiteral::Mapping(BTreeMap::new()));
                return apply_python_nested_override(child, tail, op, new_value);
            }
            false
        }
        params::PythonLiteral::Sequence(items) => {
            let Ok(index) = head.parse::<usize>() else {
                return false;
            };
            let Some(child) = items.get_mut(index) else {
                return false;
            };
            apply_python_nested_override(child, tail, op, new_value)
        }
        params::PythonLiteral::Scalar(_) => false,
    }
}

fn apply_python_nested_leaf(
    value: &mut params::PythonLiteral,
    key: &str,
    op: OverrideOp,
    new_value: params::PythonLiteral,
) -> bool {
    match value {
        params::PythonLiteral::Mapping(map) => match op {
            OverrideOp::SetExisting => {
                let Some(slot) = map.get_mut(key) else {
                    return false;
                };
                *slot = new_value;
                true
            }
            OverrideOp::Add => {
                if map.contains_key(key) {
                    return false;
                }
                map.insert(key.to_owned(), new_value);
                true
            }
            OverrideOp::AddOrSet => {
                map.insert(key.to_owned(), new_value);
                true
            }
            OverrideOp::Remove => map.remove(key).is_some(),
        },
        params::PythonLiteral::Sequence(items) => {
            let Ok(index) = key.parse::<usize>() else {
                return false;
            };
            match op {
                OverrideOp::SetExisting | OverrideOp::AddOrSet => {
                    let Some(slot) = items.get_mut(index) else {
                        return false;
                    };
                    *slot = new_value;
                    true
                }
                OverrideOp::Add | OverrideOp::Remove => false,
            }
        }
        params::PythonLiteral::Scalar(_) => false,
    }
}

fn python_literal_for_nested_path(
    path: &[&str],
    leaf: params::PythonLiteral,
) -> params::PythonLiteral {
    let Some((head, tail)) = path.split_first() else {
        return leaf;
    };
    let mut map = BTreeMap::new();
    map.insert(
        (*head).to_owned(),
        python_literal_for_nested_path(tail, leaf),
    );
    params::PythonLiteral::Mapping(map)
}

fn python_literal_from_raw(raw: &str) -> params::PythonLiteral {
    params::PythonLiteral::Scalar(python_scalar_from_raw(raw))
}

fn python_scalar_from_raw(raw: &str) -> params::Scalar {
    match raw {
        "true" | "True" => return params::Scalar::Bool(true),
        "false" | "False" => return params::Scalar::Bool(false),
        "null" | "None" => return params::Scalar::Null,
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return params::Scalar::Int(i);
    }
    if let Ok(f) = raw.parse::<f64>()
        && f.is_finite()
    {
        return params::Scalar::Float(f);
    }
    params::Scalar::String(raw.to_owned())
}

fn serialize_python_literal(value: &params::PythonLiteral) -> String {
    match value {
        params::PythonLiteral::Scalar(scalar) => serialize_python_scalar(scalar),
        params::PythonLiteral::Sequence(items) => {
            let values = items
                .iter()
                .map(serialize_python_literal)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{values}]")
        }
        params::PythonLiteral::Mapping(map) => {
            let values = map
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        quote_python_string(key),
                        serialize_python_literal(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{values}}}")
        }
    }
}

fn serialize_python_scalar(scalar: &params::Scalar) -> String {
    match scalar {
        params::Scalar::Bool(true) => "True".to_owned(),
        params::Scalar::Bool(false) => "False".to_owned(),
        params::Scalar::Int(i) => i.to_string(),
        params::Scalar::Float(f) => {
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{f:.1}")
            } else {
                format!("{f}")
            }
        }
        params::Scalar::String(s) => quote_python_string(s),
        params::Scalar::Null => "None".to_owned(),
    }
}

fn quote_python_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn apply_yaml_override(root: &mut serde_yaml::Value, dotted: &str, raw_value: &str) -> bool {
    let (op, key) = parse_override_op(dotted);
    if key.is_empty() {
        return false;
    }
    match op {
        OverrideOp::SetExisting => set_yaml_scalar(root, key, raw_value),
        OverrideOp::Add => insert_yaml_scalar(root, key, raw_value, false),
        OverrideOp::AddOrSet => insert_yaml_scalar(root, key, raw_value, true),
        OverrideOp::Remove => remove_yaml_value(root, key),
    }
}

fn set_yaml_scalar(root: &mut serde_yaml::Value, dotted: &str, raw_value: &str) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    // Navigate intermediate components.
    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            serde_yaml::Value::Mapping(m) => {
                let key = serde_yaml::Value::String((*seg).to_owned());
                match m.get_mut(&key) {
                    Some(v) => v,
                    None => return false,
                }
            }
            _ => return false,
        };
    }

    // Leaf: only write when the key already exists.
    let key = serde_yaml::Value::String((*last).to_owned());
    match cursor {
        serde_yaml::Value::Mapping(m) => {
            if !m.contains_key(&key) {
                return false;
            }
            m.insert(key, yaml_scalar_from_raw(raw_value));
            true
        }
        _ => false,
    }
}

fn insert_yaml_scalar(
    root: &mut serde_yaml::Value,
    dotted: &str,
    raw_value: &str,
    overwrite: bool,
) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            serde_yaml::Value::Mapping(m) => {
                let key = serde_yaml::Value::String((*seg).to_owned());
                m.entry(key)
                    .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
            }
            _ => return false,
        };
    }

    let key = serde_yaml::Value::String((*last).to_owned());
    match cursor {
        serde_yaml::Value::Mapping(m) => {
            if !overwrite && m.contains_key(&key) {
                return false;
            }
            m.insert(key, yaml_scalar_from_raw(raw_value));
            true
        }
        _ => false,
    }
}

fn remove_yaml_value(root: &mut serde_yaml::Value, dotted: &str) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            serde_yaml::Value::Mapping(m) => {
                let key = serde_yaml::Value::String((*seg).to_owned());
                match m.get_mut(&key) {
                    Some(v) => v,
                    None => return false,
                }
            }
            _ => return false,
        };
    }

    match cursor {
        serde_yaml::Value::Mapping(m) => {
            let key = serde_yaml::Value::String((*last).to_owned());
            m.remove(&key).is_some()
        }
        _ => false,
    }
}

/// Interpret a CLI string as a YAML scalar. Booleans and numbers
/// coerce; everything else lands as a string. This matches the
/// DVC convention of "dwim on the scalar type" — users writing
/// `--set model.lr=0.005` expect a float in the file, not the
/// literal string `"0.005"`.
fn yaml_scalar_from_raw(raw: &str) -> serde_yaml::Value {
    if let Some(b) = parse_bool(raw) {
        return serde_yaml::Value::Bool(b);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return serde_yaml::Value::Number(i.into());
    }
    if let Ok(f) = raw.parse::<f64>()
        && f.is_finite()
    {
        return serde_yaml::Value::Number(serde_yaml::Number::from(f));
    }
    serde_yaml::Value::String(raw.to_owned())
}

fn apply_json_override(root: &mut serde_json::Value, dotted: &str, raw_value: &str) -> bool {
    let (op, key) = parse_override_op(dotted);
    if key.is_empty() {
        return false;
    }
    match op {
        OverrideOp::SetExisting => set_json_scalar(root, key, raw_value),
        OverrideOp::Add => insert_json_scalar(root, key, raw_value, false),
        OverrideOp::AddOrSet => insert_json_scalar(root, key, raw_value, true),
        OverrideOp::Remove => remove_json_value(root, key),
    }
}

fn set_json_scalar(root: &mut serde_json::Value, dotted: &str, raw_value: &str) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            serde_json::Value::Object(m) => match m.get_mut(*seg) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }

    match cursor {
        serde_json::Value::Object(m) => {
            if !m.contains_key(*last) {
                return false;
            }
            m.insert((*last).to_owned(), json_scalar_from_raw(raw_value));
            true
        }
        _ => false,
    }
}

fn insert_json_scalar(
    root: &mut serde_json::Value,
    dotted: &str,
    raw_value: &str,
    overwrite: bool,
) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            serde_json::Value::Object(m) => m
                .entry((*seg).to_owned())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new())),
            _ => return false,
        };
    }

    match cursor {
        serde_json::Value::Object(m) => {
            if !overwrite && m.contains_key(*last) {
                return false;
            }
            m.insert((*last).to_owned(), json_scalar_from_raw(raw_value));
            true
        }
        _ => false,
    }
}

fn remove_json_value(root: &mut serde_json::Value, dotted: &str) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            serde_json::Value::Object(m) => match m.get_mut(*seg) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }

    match cursor {
        serde_json::Value::Object(m) => m.remove(*last).is_some(),
        _ => false,
    }
}

fn json_scalar_from_raw(raw: &str) -> serde_json::Value {
    if let Some(b) = parse_bool(raw) {
        return serde_json::Value::Bool(b);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return serde_json::Value::Number(i.into());
    }
    if let Ok(f) = raw.parse::<f64>()
        && f.is_finite()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return serde_json::Value::Number(n);
    }
    serde_json::Value::String(raw.to_owned())
}

fn apply_toml_override(root: &mut toml::Value, dotted: &str, raw_value: &str) -> bool {
    let (op, key) = parse_override_op(dotted);
    if key.is_empty() {
        return false;
    }
    match op {
        OverrideOp::SetExisting => set_toml_scalar(root, key, raw_value),
        OverrideOp::Add => insert_toml_scalar(root, key, raw_value, false),
        OverrideOp::AddOrSet => insert_toml_scalar(root, key, raw_value, true),
        OverrideOp::Remove => remove_toml_value(root, key),
    }
}

fn set_toml_scalar(root: &mut toml::Value, dotted: &str, raw_value: &str) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            toml::Value::Table(t) => match t.get_mut(*seg) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }

    match cursor {
        toml::Value::Table(t) => {
            if !t.contains_key(*last) {
                return false;
            }
            t.insert((*last).to_owned(), toml_scalar_from_raw(raw_value));
            true
        }
        _ => false,
    }
}

fn insert_toml_scalar(
    root: &mut toml::Value,
    dotted: &str,
    raw_value: &str,
    overwrite: bool,
) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            toml::Value::Table(t) => t
                .entry((*seg).to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new())),
            _ => return false,
        };
    }

    match cursor {
        toml::Value::Table(t) => {
            if !overwrite && t.contains_key(*last) {
                return false;
            }
            t.insert((*last).to_owned(), toml_scalar_from_raw(raw_value));
            true
        }
        _ => false,
    }
}

fn remove_toml_value(root: &mut toml::Value, dotted: &str) -> bool {
    let components: Vec<&str> = dotted.split('.').collect();
    let Some((last, intermediates)) = components.split_last() else {
        return false;
    };

    let mut cursor = root;
    for seg in intermediates {
        cursor = match cursor {
            toml::Value::Table(t) => match t.get_mut(*seg) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }

    match cursor {
        toml::Value::Table(t) => t.remove(*last).is_some(),
        _ => false,
    }
}

fn toml_scalar_from_raw(raw: &str) -> toml::Value {
    if let Some(b) = parse_bool(raw) {
        return toml::Value::Boolean(b);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    if let Ok(f) = raw.parse::<f64>()
        && f.is_finite()
    {
        return toml::Value::Float(f);
    }
    toml::Value::String(raw.to_owned())
}

/// Shared boolean coercion for CLI scalar values. Accepts the
/// YAML-1.2 / common JSON-ish forms; rejects ambiguous YAML-1.1
/// keywords like `yes`/`no`/`on`/`off` so users who type those get
/// them as strings (which is what they'd see in the params file
/// under the canonical yaml serializer, too).
fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Read the unix mode bits for `path`. Falls back to `0o644` on
/// non-unix targets where mode isn't meaningful for params files.
fn file_mode(path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path).map_err(CrabError::Io)?;
        Ok(meta.mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(0o644)
    }
}

/// Small RAII guard that removes `path` when dropped unless
/// [`Self::disarm`] has been called. Used during `create` so a
/// failure after the tmpdir has been mkdir'd leaves no litter.
struct CleanupOnDrop {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl CleanupOnDrop {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: std::cell::Cell::new(true),
        }
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        if self.armed.get()
            && let Err(e) = fs::remove_dir_all(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                tmpdir = %self.path.display(),
                error = %e,
                "experiment worktree partial cleanup failed"
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
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use tempfile::TempDir;

    /// Build a throwaway git repo at `root` with a single commit
    /// containing `crab.yaml`, `params.yaml`, and a canary file.
    /// Returns the canary contents for later equality checks.
    fn init_repo(root: &Path) -> String {
        run_git(root, &["init", "--initial-branch=main"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "commit.gpgsign", "false"]);

        let crab_yaml = r#"
params:
  - params.yaml
stages:
  dummy:
    cmd: "true"
"#;
        fs::write(root.join("crab.yaml"), crab_yaml).unwrap();

        let params_yaml = r#"
model:
  lr: 0.001
  epochs: 10
data:
  window: 30
"#;
        fs::write(root.join("params.yaml"), params_yaml).unwrap();

        let canary = "canary contents".to_owned();
        fs::write(root.join("canary.txt"), &canary).unwrap();

        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);
        canary
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn create_fails_when_tmpdir_already_exists() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let exp_id = ExperimentId::new_v7();
        let tmpdir = root.join(EXP_WORKTREE_PARENT_REL).join(exp_id.to_string());
        fs::create_dir_all(&tmpdir).unwrap();

        let err = ExperimentWorktree::create(root, exp_id, &BTreeMap::new()).unwrap_err();
        assert!(
            matches!(err, CrabError::ExperimentCollision { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn create_populates_tmpdir_from_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let expected_canary = init_repo(root);

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &BTreeMap::new()).unwrap();

        let canary_in_tmp = fs::read_to_string(worktree.path.join("canary.txt")).unwrap();
        assert_eq!(
            canary_in_tmp, expected_canary,
            "canary byte mismatch: tmpdir must mirror HEAD"
        );

        // `base_commit` must look like a full SHA-1.
        assert_eq!(worktree.base_commit.len(), 40);
        assert!(worktree.base_commit.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn create_ignores_user_index_drift_after_head_capture() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        fs::write(root.join("params.yaml"), "model:\n  lr: 0.9\n").unwrap();
        run_git(root, &["add", "params.yaml"]);

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &BTreeMap::new()).unwrap();
        let params_text = fs::read_to_string(worktree.path.join("params.yaml")).unwrap();

        assert!(
            params_text.contains("lr: 0.001"),
            "experiment should materialize committed HEAD, got:\n{params_text}"
        );
    }

    #[test]
    fn create_with_overrides_mutates_params_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let mut overrides = BTreeMap::new();
        overrides.insert("model.lr".to_owned(), "0.005".to_owned());

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &overrides).unwrap();

        // Read back through the same parser the stage hasher uses.
        let bytes = fs::read(worktree.path.join("params.yaml")).unwrap();
        let parsed = crate::params::parse(&bytes, Path::new("params.yaml")).unwrap();
        let lr = parsed
            .get("model.lr")
            .expect("model.lr present after override");
        assert_eq!(
            lr.as_f64(),
            Some(0.005),
            "override did not land: parsed value = {lr:?}"
        );

        // Untouched keys stay put.
        let window = parsed.get("data.window").expect("data.window present");
        assert_eq!(window.as_f64(), Some(30.0));
    }

    #[test]
    fn create_with_hydra_style_overrides_adds_updates_and_removes_params() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let mut overrides = BTreeMap::new();
        overrides.insert("+model.dropout".to_owned(), "0.2".to_owned());
        overrides.insert("++model.lr".to_owned(), "0.004".to_owned());
        overrides.insert("~data.window".to_owned(), String::new());

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &overrides).unwrap();

        let bytes = fs::read(worktree.path.join("params.yaml")).unwrap();
        let parsed = crate::params::parse(&bytes, Path::new("params.yaml")).unwrap();
        assert_eq!(
            parsed.get("model.lr").and_then(|scalar| scalar.as_f64()),
            Some(0.004)
        );
        assert_eq!(
            parsed
                .get("model.dropout")
                .and_then(|scalar| scalar.as_f64()),
            Some(0.2)
        );
        assert!(!parsed.contains_key("data.window"));
    }

    #[test]
    fn create_with_file_scoped_overrides_mutates_named_params_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        fs::write(
            root.join("crab.yaml"),
            "params:\n  - params.yaml\n  - custom.yaml\nstages:\n  dummy:\n    cmd: \"true\"\n",
        )
        .unwrap();
        fs::write(root.join("custom.yaml"), "model:\n  lr: 0.002\n").unwrap();
        run_git(root, &["add", "crab.yaml", "custom.yaml"]);
        run_git(root, &["commit", "-m", "add custom params"]);

        let mut overrides = BTreeMap::new();
        overrides.insert("custom.yaml:model.lr".to_owned(), "0.009".to_owned());

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &overrides).unwrap();

        let params_bytes = fs::read(worktree.path.join("params.yaml")).unwrap();
        let params = crate::params::parse(&params_bytes, Path::new("params.yaml")).unwrap();
        assert_eq!(
            params.get("model.lr").and_then(|scalar| scalar.as_f64()),
            Some(0.001)
        );

        let custom_bytes = fs::read(worktree.path.join("custom.yaml")).unwrap();
        let custom = crate::params::parse(&custom_bytes, Path::new("custom.yaml")).unwrap();
        assert_eq!(
            custom.get("model.lr").and_then(|scalar| scalar.as_f64()),
            Some(0.009)
        );
    }

    #[test]
    fn override_ops_apply_to_json_and_toml_documents() {
        let overrides = BTreeMap::from([
            ("+model.dropout".to_owned(), "0.2".to_owned()),
            ("++model.lr".to_owned(), "0.004".to_owned()),
            ("~data.window".to_owned(), String::new()),
        ]);

        let (json_text, json_consumed) =
            apply_json(r#"{"model":{"lr":0.001},"data":{"window":30}}"#, &overrides).unwrap();
        assert_eq!(json_consumed.len(), 3);
        let json = crate::params::parse(json_text.as_bytes(), Path::new("params.json")).unwrap();
        assert_eq!(
            json.get("model.lr").and_then(|scalar| scalar.as_f64()),
            Some(0.004)
        );
        assert_eq!(
            json.get("model.dropout").and_then(|scalar| scalar.as_f64()),
            Some(0.2)
        );
        assert!(!json.contains_key("data.window"));

        let (toml_text, toml_consumed) =
            apply_toml("model.lr = 0.001\ndata.window = 30\n", &overrides).unwrap();
        assert_eq!(toml_consumed.len(), 3);
        let toml = crate::params::parse(toml_text.as_bytes(), Path::new("params.toml")).unwrap();
        assert_eq!(
            toml.get("model.lr").and_then(|scalar| scalar.as_f64()),
            Some(0.004)
        );
        assert_eq!(
            toml.get("model.dropout").and_then(|scalar| scalar.as_f64()),
            Some(0.2)
        );
        assert!(!toml.contains_key("data.window"));
    }

    #[test]
    fn override_ops_apply_to_python_documents() {
        let overrides = BTreeMap::from([
            ("gamma".to_owned(), "4".to_owned()),
            ("++model.dropout".to_owned(), "0.2".to_owned()),
            ("model.layers.1".to_owned(), "4".to_owned()),
            ("TrainConfig.EPOCHS".to_owned(), "80".to_owned()),
            ("TrainConfig.layers".to_owned(), "12".to_owned()),
            ("~DupConfig.layers".to_owned(), String::new()),
            ("+name".to_owned(), "resnet".to_owned()),
            ("~enabled".to_owned(), String::new()),
        ]);

        let (python_text, consumed) = apply_python(
            "gamma = 3\nenabled = True\nmodel = {\n    \"layers\": [2, 3],\n}\nclass TrainConfig:\n    EPOCHS = 70\n\n    def __init__(self):\n        self.layers = 5\n        self.layers = 9\n\nclass DupConfig:\n    def __init__(self):\n        self.layers = 1\n        self.layers = 2\n",
            &overrides,
        );
        assert_eq!(consumed.len(), 8);

        let parsed = crate::params::parse(python_text.as_bytes(), Path::new("params.py")).unwrap();
        assert_eq!(
            parsed.get("gamma").and_then(|scalar| scalar.as_f64()),
            Some(4.0)
        );
        assert_eq!(
            parsed
                .get("model.dropout")
                .and_then(|scalar| scalar.as_f64()),
            Some(0.2)
        );
        assert_eq!(
            parsed
                .get("model.layers.1")
                .and_then(|scalar| scalar.as_f64()),
            Some(4.0)
        );
        assert_eq!(
            parsed.get("name"),
            Some(&crate::params::Scalar::String("resnet".to_owned()))
        );
        assert_eq!(
            parsed
                .get("TrainConfig.EPOCHS")
                .and_then(|scalar| scalar.as_f64()),
            Some(80.0)
        );
        assert_eq!(
            parsed
                .get("TrainConfig.layers")
                .and_then(|scalar| scalar.as_f64()),
            Some(12.0)
        );
        assert!(!parsed.contains_key("enabled"));
        assert!(!parsed.contains_key("DupConfig.layers"));
    }

    #[test]
    fn create_with_file_scoped_overrides_mutates_python_params_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        fs::write(
            root.join("crab.yaml"),
            "params:\n  - params.py\n  - params.yaml\nstages:\n  dummy:\n    cmd: \"true\"\n",
        )
        .unwrap();
        fs::write(
            root.join("params.py"),
            "gamma = 3\nenabled = True\nmodel = {\"lr\": 0.002}\nclass TrainConfig:\n    EPOCHS = 70\n    def __init__(self):\n        self.layers = 5\n        self.layers = 9\n",
        )
        .unwrap();
        run_git(root, &["add", "crab.yaml", "params.py"]);
        run_git(root, &["commit", "-m", "add python params"]);

        let mut overrides = BTreeMap::new();
        overrides.insert("params.py:gamma".to_owned(), "4".to_owned());
        overrides.insert("params.py:++model.dropout".to_owned(), "0.2".to_owned());
        overrides.insert("params.py:TrainConfig.EPOCHS".to_owned(), "80".to_owned());
        overrides.insert("params.py:TrainConfig.layers".to_owned(), "12".to_owned());
        overrides.insert("params.py:~enabled".to_owned(), String::new());

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &overrides).unwrap();

        let py_bytes = fs::read(worktree.path.join("params.py")).unwrap();
        let py_params = crate::params::parse(&py_bytes, Path::new("params.py")).unwrap();
        assert_eq!(
            py_params.get("gamma").and_then(|scalar| scalar.as_f64()),
            Some(4.0)
        );
        assert_eq!(
            py_params
                .get("model.dropout")
                .and_then(|scalar| scalar.as_f64()),
            Some(0.2)
        );
        assert_eq!(
            py_params
                .get("TrainConfig.EPOCHS")
                .and_then(|scalar| scalar.as_f64()),
            Some(80.0)
        );
        assert_eq!(
            py_params
                .get("TrainConfig.layers")
                .and_then(|scalar| scalar.as_f64()),
            Some(12.0)
        );
        assert!(!py_params.contains_key("enabled"));
    }

    #[test]
    fn create_rejects_unknown_override_key() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let mut overrides = BTreeMap::new();
        overrides.insert("nonexistent.key".to_owned(), "1".to_owned());

        let exp_id = ExperimentId::new_v7();
        let err = ExperimentWorktree::create(root, exp_id, &overrides).unwrap_err();
        match err {
            CrabError::Configuration { key, origin } => {
                assert!(key.contains("nonexistent.key"), "message: {key}");
                assert_eq!(origin, "experiment");
            }
            other => panic!("wrong variant: {other}"),
        }

        // The failed create should have cleaned up the partial tmpdir.
        let tmpdir = root.join(EXP_WORKTREE_PARENT_REL).join(exp_id.to_string());
        assert!(
            !tmpdir.exists(),
            "partial tmpdir should have been cleaned up after failure"
        );
    }

    #[test]
    fn cleanup_removes_tmpdir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let exp_id = ExperimentId::new_v7();
        let worktree = ExperimentWorktree::create(root, exp_id, &BTreeMap::new()).unwrap();
        let tmpdir = worktree.path.clone();
        assert!(tmpdir.exists());

        worktree.cleanup().unwrap();
        assert!(!tmpdir.exists(), "cleanup must remove tmpdir");
    }

    #[test]
    fn drop_cleans_up_on_unwind() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        init_repo(root);

        let exp_id = ExperimentId::new_v7();
        let tmpdir_path = root.join(EXP_WORKTREE_PARENT_REL).join(exp_id.to_string());

        // Force a panic while holding the worktree. After the unwind
        // the Drop impl must have removed the tmpdir.
        let root_buf = root.to_path_buf();
        let result = catch_unwind(AssertUnwindSafe(move || {
            let _worktree =
                ExperimentWorktree::create(&root_buf, exp_id, &BTreeMap::new()).unwrap();
            panic!("simulated failure while worktree is live");
        }));
        assert!(result.is_err(), "closure was expected to panic");
        assert!(!tmpdir_path.exists(), "Drop must remove tmpdir on unwind");
    }

    #[test]
    fn sweep_removes_inactive_tmpdirs_and_keeps_active() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let parent = root.join(EXP_WORKTREE_PARENT_REL);
        fs::create_dir_all(&parent).unwrap();

        let active = ExperimentId::new_v7();
        let orphan = ExperimentId::new_v7();

        let active_path = parent.join(active.to_string());
        let orphan_path = parent.join(orphan.to_string());
        fs::create_dir_all(&active_path).unwrap();
        fs::create_dir_all(&orphan_path).unwrap();

        let removed = sweep_orphan_experiment_tmpdirs(root, &[active]).unwrap();
        assert_eq!(removed, 1);
        assert!(active_path.exists(), "active tmpdir must survive");
        assert!(!orphan_path.exists(), "orphan tmpdir must be removed");
    }

    #[test]
    fn sweep_handles_missing_parent_dir() {
        let tmp = TempDir::new().unwrap();
        // Parent dir doesn't exist — sweep returns 0 without error.
        let removed = sweep_orphan_experiment_tmpdirs(tmp.path(), &[]).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn sweep_skips_non_uuid_directory_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let parent = root.join(EXP_WORKTREE_PARENT_REL);
        fs::create_dir_all(&parent).unwrap();

        let weird = parent.join("not-a-uuid");
        fs::create_dir_all(&weird).unwrap();

        let removed = sweep_orphan_experiment_tmpdirs(root, &[]).unwrap();
        assert_eq!(removed, 0, "non-uuid directories must not be removed");
        assert!(weird.exists());
    }
}
