//! Assemble stage for `crab import`.
//!
//! Once ingest has drained the journal — every entry is either
//! `Staged { file_hash }`, `Failed`, or `Skipped` — assemble walks
//! the planner's [`CommitWindow`] list and emits one git commit per
//! window. For each window the worker applies the window's entries
//! to the working tree (pointer blob writes for normal entries,
//! `unlink` for delete-marker sentinels) and runs `git add -A &&
//! git commit --date=<window_end>`.
//!
//! The stage owns a handful of distinct concerns:
//!
//! 1. **Safety rails on `--into`.** Refuses a non-empty target
//!    that isn't a freshly-initialized empty git repo, surfacing
//!    [`CrabError::ImportTargetNotEmpty`]. `--force` bypasses.
//! 2. **`git init --initial-branch=<branch>`** when the directory
//!    is not already a git repo. Idempotent on an existing empty
//!    repo.
//! 3. **Filter driver registration** via
//!    [`crate::cmd::init::install_filter_driver`] after the import
//!    commits land so hydrate / dehydrate work on the newly imported
//!    repo without re-cleaning pointer blobs during assembly.
//! 4. **`.gitattributes` synthesis.** Bucket the [`Staged`] entries
//!    by extension; emit `filter=crab` for every extension whose
//!    files are committed as Crab pointer blobs. Append user-supplied
//!    `--track` globs verbatim and merge into any existing file
//!    without clobbering user lines. Committed once in the first
//!    commit.
//! 5. **Git identity precondition.** Verify `user.name` and
//!    `user.email` are configured before we start the window
//!    walk — failing here gives a clean message rather than the
//!    opaque `git commit` error.
//! 6. **Window walk.** For each window, write every entry's
//!    pointer blob (or unlink, for delete markers), `git add -A`,
//!    then `git commit --date=<window_end>` with the resolved
//!    author / message template. Capture each commit OID.
//! 7. **Remote registration.** `git remote add origin <target-url>`
//!    once at the end; if `origin` already exists and `--force`
//!    was passed, use `set-url` instead. Otherwise error with
//!    [`CrabError::ImportRemoteExists`].
//! 8. **Progress events.** Emit one [`AssembleEvent`] per commit
//!    via [`AssembleProgressSink`].
//!
//! No `std::sync::Mutex` is held across an `.await` — the progress
//! sink lives behind `tokio::sync::Mutex<P>` and guards are
//! dropped immediately after emission.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crab_types::time::from_epoch_millis;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::metrics::Metrics;
use crate::import::ingest::DELETE_MARKER_FILE_HASH;
use crate::import::journal::EntryState;
use crate::import::window::CommitWindow;
use crab_types::pointer::Pointer;

fn git_command(into: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(into)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
    command
}

/// Resolved inputs for [`run_assemble`].
///
/// The progress sink and cancellation token mirror ingest's
/// shape so the coordinator can reuse its plumbing.
pub struct AssembleInputs<P: AssembleProgressSink> {
    /// Local path of the git repo we are building.
    pub into: PathBuf,
    /// Initial branch name passed to `git init --initial-branch`.
    pub branch: String,
    /// `--force`: bypass non-empty-target and existing-origin
    /// safety rails.
    pub force: bool,
    /// `--resume`: target contents may come from a prior attempt.
    pub resume: bool,
    /// Canonical import target URL; written as `origin` after the
    /// final commit lands.
    pub target_url: String,
    /// Planned commit sequence. In flat mode this is a single
    /// window; in versioned mode there is one window per time
    /// bucket from the planner.
    pub windows: Vec<CommitWindow>,
    /// User-supplied `--track` globs, appended verbatim to
    /// `.gitattributes`.
    pub track: Vec<String>,
    /// Optional `--message` template. Falls back to the default
    /// per-window message when absent.
    pub message_template: Option<String>,
    /// Optional `--author-template`. Falls back to the configured
    /// git identity when absent.
    pub author_template: Option<String>,
    /// Progress sink shared with the coordinator.
    pub progress: Arc<Mutex<P>>,
    /// Optional lifetime metrics handle. Assemble increments
    /// `import_commits_total` per commit and `import_files_total`
    /// / `import_versions_total` once at the end of the walk.
    pub metrics: Option<Arc<Metrics>>,
    /// Cancellation token. Honored between windows.
    pub cancel: CancellationToken,
}

/// Counters the assemble stage folds into the final `ImportSummary`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssembleStats {
    /// Number of git commits created by the window walk.
    pub commits_created: u64,
    /// Distinct paths visible in the HEAD commit's tree.
    pub files_imported: u64,
    /// Total entry count across all windows (versions for
    /// versioned mode; equal to `files_imported` for flat).
    pub versions_imported: u64,
    /// OID of the first commit in the linear history, if any.
    pub first_commit_oid: Option<String>,
    /// OID of the HEAD commit after the walk, if any.
    pub head_commit_oid: Option<String>,
}

/// One `assemble.event` emitted after a commit lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembleEvent {
    /// Window start (epoch seconds, UTC).
    pub window_start: i64,
    /// Window end (epoch seconds, UTC) — also the commit's date.
    pub window_end: i64,
    /// Commit OID captured via `git rev-parse HEAD`.
    pub commit_oid: String,
    /// Number of paths added in this commit.
    pub files_added: u64,
    /// Number of paths modified in this commit.
    pub files_modified: u64,
    /// Number of paths deleted in this commit.
    pub files_deleted: u64,
}

/// Progress sink for the assemble stage.
pub trait AssembleProgressSink: Send {
    /// Deliver one `assemble.event`. Called once per successful
    /// commit from the window walk.
    fn assemble_event(&mut self, event: &AssembleEvent);
}

impl AssembleProgressSink for () {
    fn assemble_event(&mut self, _event: &AssembleEvent) {}
}

/// Entry point: build a git repo from the planned commit windows.
///
/// Returns the aggregated [`AssembleStats`] when every window has
/// landed a commit. Cancellation between windows surfaces as
/// [`CrabError::Cancelled`] with whatever commits already
/// landed staying in place — resume semantics are a later
/// concern (Task 18).
pub async fn run_assemble<P>(inputs: AssembleInputs<P>) -> Result<AssembleStats>
where
    P: AssembleProgressSink + 'static,
{
    let AssembleInputs {
        into,
        branch,
        force,
        resume,
        target_url,
        windows,
        track,
        message_template,
        author_template,
        progress,
        metrics,
        cancel,
    } = inputs;

    check_cancelled(&cancel)?;

    info!(
        into = %into.display(),
        branch = %branch,
        windows = windows.len(),
        force,
        "assemble: starting"
    );

    // Resume may re-enter after assemble created `.git`, pointer
    // files, or `origin`; the verified journal is the safety rail.
    if !resume {
        ensure_target_dir(&into, force)?;
    }
    ensure_git_repo(&into, &branch)?;
    ensure_internal_state_ignored(&into)?;

    let gitattributes_body = synthesize_gitattributes(&windows, &track, &into)?;

    if resume && let Some(stats) = try_reuse_resume_head(&into, &windows, &track)? {
        crate::cmd::init::install_filter_driver(&into)?;
        if let Some(m) = metrics.as_deref() {
            m.add_import_files_total(stats.files_imported);
            m.add_import_versions_total(stats.versions_imported);
        }
        register_origin(&into, &target_url, force)?;
        info!(
            files = stats.files_imported,
            versions = stats.versions_imported,
            head = ?stats.head_commit_oid,
            "assemble: reused existing resume HEAD"
        );
        return Ok(stats);
    }

    ensure_git_identity(&into)?;

    let mut stats = AssembleStats::default();

    for (idx, window) in windows.iter().enumerate() {
        check_cancelled(&cancel)?;

        let is_first = idx == 0;

        // Previous tree sits behind HEAD (None for the very first
        // commit). We need the prior paths both for the add/modify
        // split and to decide what to remove when the window
        // contains delete markers.
        let prior_paths = if is_first {
            Vec::new()
        } else {
            list_tree_paths(&into, "HEAD")?
        };

        let (files_added, files_modified, files_deleted) =
            apply_window_to_worktree(&into, window, &prior_paths)?;

        if is_first {
            write_gitattributes_if_needed(&into, &gitattributes_body)?;
        }

        git_add_all(&into)?;
        git_untrack_internal_state(&into)?;

        let commit_oid = commit_window(
            &into,
            window,
            files_added,
            files_modified,
            files_deleted,
            message_template.as_deref(),
            author_template.as_deref(),
        )?;

        stats.commits_created += 1;
        stats.versions_imported += u64::try_from(window.entries.len()).unwrap_or(u64::MAX);
        if stats.first_commit_oid.is_none() {
            stats.first_commit_oid = Some(commit_oid.clone());
        }
        stats.head_commit_oid = Some(commit_oid.clone());

        if let Some(m) = metrics.as_deref() {
            m.inc_import_commits_total();
        }

        let event = AssembleEvent {
            window_start: window.window_start,
            window_end: window.window_end,
            commit_oid,
            files_added,
            files_modified,
            files_deleted,
        };
        {
            let mut sink = progress.lock().await;
            sink.assemble_event(&event);
        }

        info!(
            window_start = window.window_start,
            window_end = window.window_end,
            commit_oid = %event.commit_oid,
            files_added,
            files_modified,
            files_deleted,
            "assemble: commit landed"
        );
    }

    // Count imported source paths, not generated support files
    // such as .gitattributes that make the repo usable after clone.
    if stats.head_commit_oid.is_some() {
        stats.files_imported =
            u64::try_from(final_imported_paths(&windows).len()).unwrap_or(u64::MAX);
    }

    // Import writes Crab pointer blobs directly. Register the filter
    // after those commits land so future user edits hydrate/dehydrate
    // normally without routing import's own `git add` through clean.
    crate::cmd::init::install_filter_driver(&into)?;

    if let Some(m) = metrics.as_deref() {
        m.add_import_files_total(stats.files_imported);
        m.add_import_versions_total(stats.versions_imported);
    }

    register_origin(&into, &target_url, force)?;

    info!(
        commits = stats.commits_created,
        files = stats.files_imported,
        versions = stats.versions_imported,
        head = ?stats.head_commit_oid,
        "assemble: complete"
    );

    Ok(stats)
}

/// Safety rail: refuse non-empty targets that aren't an empty
/// freshly-initialized git repo, unless `--force` is set.
///
/// Accepts the directory when:
///
/// 1. It does not exist yet.
/// 2. It exists and is empty.
/// 3. It exists and contains only a `.git` subdirectory that
///    itself has no commits (a just-inited repo with an empty
///    working tree).
/// 4. `--force` was passed.
fn ensure_target_dir(into: &Path, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }

    if !into.exists() {
        return Ok(());
    }

    let read_dir = std::fs::read_dir(into)?;
    let mut has_git = false;
    let mut has_other = false;
    for entry in read_dir {
        let entry = entry?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(".git") {
            has_git = true;
        } else if name == std::ffi::OsStr::new(".crab") {
            // `.crab/` is the coordinator's own bookkeeping
            // (journal + staging). It's always created by the
            // pipeline itself, never by the user, so it must not
            // count as "other content" that would trip the non-
            // empty-target safety rail.
        } else {
            has_other = true;
        }
    }

    if has_other {
        return Err(CrabError::ImportTargetNotEmpty {
            path: into.display().to_string(),
        });
    }

    if !has_git {
        // Empty directory — accept.
        return Ok(());
    }

    // Only `.git` exists. Confirm it's empty-repo shape: no
    // commits on HEAD yet.
    let output = git_command(into)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        // HEAD resolves → this repo has commits.
        return Err(CrabError::ImportTargetNotEmpty {
            path: into.display().to_string(),
        });
    }

    Ok(())
}

/// Ensure `<into>` is a git repo on `branch`. If `.git` does not
/// exist we run `git init --initial-branch=<branch>`. Creates the
/// parent directory when needed.
fn ensure_git_repo(into: &Path, branch: &str) -> Result<()> {
    if !into.exists() {
        std::fs::create_dir_all(into)?;
    }
    if into.join(".git").exists() {
        return Ok(());
    }

    let output = git_command(into)
        .args(["init", "--initial-branch"])
        .arg(branch)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git init".into(),
            origin: format!("git init --initial-branch={branch} failed: {stderr}"),
        });
    }
    debug!(into = %into.display(), branch, "assemble: git init complete");
    Ok(())
}

fn ensure_internal_state_ignored(into: &Path) -> Result<()> {
    let exclude = git_path(into, "info/exclude")?;
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == "/.crab/") {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("/.crab/\n");
    std::fs::write(exclude, updated)?;
    Ok(())
}

fn git_path(into: &Path, path: &str) -> Result<PathBuf> {
    let output = git_command(into)
        .args(["rev-parse", "--git-path", path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git rev-parse --git-path".into(),
            origin: format!("git rev-parse --git-path {path} failed: {stderr}"),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let git_path = PathBuf::from(raw);
    if git_path.is_absolute() {
        Ok(git_path)
    } else {
        Ok(into.join(git_path))
    }
}

/// Verify git has a usable identity before we start committing.
///
/// `git config --get` returns exit 1 when the key is unset; we
/// translate the empty value to
/// [`CrabError::ImportMissingGitIdentity`].
fn ensure_git_identity(into: &Path) -> Result<()> {
    for key in &["user.name", "user.email"] {
        let output = git_command(into)
            .args(["config", "--get", key])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || value.is_empty() {
            return Err(CrabError::ImportMissingGitIdentity);
        }
        debug!(key, value, "assemble: git identity present");
    }
    Ok(())
}

/// Build the additional `.gitattributes` lines that this import
/// should contribute.
///
/// Heuristic:
///
/// 1. For every `Staged`, non-delete-marker entry across every
///    window, bucket by file-extension (lowercase, no leading dot).
/// 2. For each extension with at least one committed pointer blob,
///    emit `*.<ext> filter=crab diff=crab merge=crab -text`.
/// 3. Append every user-supplied `--track` glob verbatim.
/// 4. Drop lines already present in `<into>/.gitattributes` so we
///    never clobber a user-maintained file.
///
/// Returns an empty `String` when we have no new lines to write;
/// callers treat that as "no .gitattributes commit needed".
fn synthesize_gitattributes(
    windows: &[CommitWindow],
    track: &[String],
    into: &Path,
) -> Result<String> {
    let new_lines = required_gitattributes_lines(windows, track);

    if new_lines.is_empty() {
        return Ok(String::new());
    }

    // Filter out anything already present in an existing
    // .gitattributes so we never overwrite user lines.
    let existing_path = into.join(".gitattributes");
    let existing = if existing_path.exists() {
        std::fs::read_to_string(&existing_path)?
    } else {
        String::new()
    };
    let existing_lines: std::collections::HashSet<String> =
        existing.lines().map(str::to_owned).collect();

    let fresh: Vec<&String> = new_lines
        .iter()
        .filter(|line| !existing_lines.contains(*line))
        .collect();

    if fresh.is_empty() {
        return Ok(String::new());
    }

    let mut out = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        out.push('\n');
    }
    for line in fresh {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn required_gitattributes_lines(windows: &[CommitWindow], track: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;

    let mut auto_exts: BTreeSet<String> = BTreeSet::new();

    for window in windows {
        for entry in &window.entries {
            // Only Staged, non-delete-marker entries need filter
            // coverage. Failed / skipped entries are not part of
            // the committed tree, and delete markers remove paths.
            if entry.is_delete_marker {
                continue;
            }
            match &entry.state {
                EntryState::Staged { file_hash } if *file_hash != DELETE_MARKER_FILE_HASH => {}
                _ => continue,
            }
            let Some(ext) = extension_of(&entry.relative_path) else {
                continue;
            };
            auto_exts.insert(ext);
        }
    }

    let mut lines: Vec<String> = auto_exts
        .into_iter()
        .map(|ext| format!("*.{ext} filter=crab diff=crab merge=crab -text"))
        .collect();

    for glob in track {
        // Append verbatim. The user already decided the full line
        // shape for `--track`; we don't second-guess it beyond
        // deduping against existing lines.
        let trimmed = glob.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines.push(trimmed.to_owned());
    }

    lines
}

/// Lowercase file extension (no leading dot) or `None` if the
/// basename does not contain an extension. Matches the convention
/// used elsewhere in the codebase for extension-keyed lookups.
fn extension_of(relative_path: &str) -> Option<String> {
    let name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Append `body` to `<into>/.gitattributes` (creating the file if
/// needed). A blank body is a no-op so callers don't have to
/// branch on it.
fn write_gitattributes_if_needed(into: &Path, body: &str) -> Result<()> {
    if body.is_empty() {
        return Ok(());
    }
    let path = into.join(".gitattributes");
    let mut contents = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(body);
    std::fs::write(&path, contents)?;
    Ok(())
}

fn final_imported_paths(windows: &[CommitWindow]) -> std::collections::BTreeSet<String> {
    let mut paths = std::collections::BTreeSet::new();

    for window in windows {
        for entry in &window.entries {
            let state_file_hash = match &entry.state {
                EntryState::Staged { file_hash } => Some(*file_hash),
                _ => None,
            };
            let is_delete = entry.is_delete_marker
                || state_file_hash.is_some_and(|h| h == DELETE_MARKER_FILE_HASH);
            if is_delete {
                paths.remove(&entry.relative_path);
                continue;
            }
            if state_file_hash.is_some() {
                paths.insert(entry.relative_path.clone());
            }
        }
    }

    paths
}

fn final_pointer_entries(
    windows: &[CommitWindow],
) -> std::collections::BTreeMap<String, ([u8; 32], u64)> {
    let mut entries = std::collections::BTreeMap::new();

    for window in windows {
        for entry in &window.entries {
            let Some(file_hash) = (match &entry.state {
                EntryState::Staged { file_hash } => Some(*file_hash),
                _ => None,
            }) else {
                continue;
            };
            let is_delete = entry.is_delete_marker || file_hash == DELETE_MARKER_FILE_HASH;
            if is_delete {
                entries.remove(&entry.relative_path);
            } else {
                entries.insert(entry.relative_path.clone(), (file_hash, entry.size));
            }
        }
    }

    entries
}

fn try_reuse_resume_head(
    into: &Path,
    windows: &[CommitWindow],
    track: &[String],
) -> Result<Option<AssembleStats>> {
    let Some(head) = rev_parse_optional(into, "HEAD")? else {
        return Ok(None);
    };

    let pointer_entries = final_pointer_entries(windows);
    let required_attrs = required_gitattributes_lines(windows, track);
    let mut expected_paths = final_imported_paths(windows);
    if !required_attrs.is_empty() {
        expected_paths.insert(".gitattributes".to_owned());
    }

    let actual_paths: std::collections::BTreeSet<String> =
        list_tree_paths(into, "HEAD")?.into_iter().collect();
    if actual_paths != expected_paths {
        return Ok(None);
    }

    for (relative_path, (expected_hash, expected_size)) in pointer_entries {
        let Some(bytes) = show_head_blob(into, &relative_path)? else {
            return Ok(None);
        };
        let Ok(pointer) = Pointer::parse(&bytes) else {
            return Ok(None);
        };
        if pointer.file_hash != expected_hash || pointer.size != expected_size {
            return Ok(None);
        }
    }

    if !required_attrs.is_empty() {
        let Some(bytes) = show_head_blob(into, ".gitattributes")? else {
            return Ok(None);
        };
        let Ok(attrs) = std::str::from_utf8(&bytes) else {
            return Ok(None);
        };
        let existing: std::collections::HashSet<&str> = attrs.lines().collect();
        if required_attrs
            .iter()
            .any(|line| !existing.contains(line.as_str()))
        {
            return Ok(None);
        }
    }

    let versions_imported = windows
        .iter()
        .map(|window| u64::try_from(window.entries.len()).unwrap_or(u64::MAX))
        .fold(0u64, u64::saturating_add);

    Ok(Some(AssembleStats {
        commits_created: 0,
        files_imported: u64::try_from(final_imported_paths(windows).len()).unwrap_or(u64::MAX),
        versions_imported,
        first_commit_oid: first_commit_oid(into)?,
        head_commit_oid: Some(head),
    }))
}

/// Apply one window's worth of entries to the working tree.
///
/// For each `Staged` entry: write the entry's pointer blob at
/// `<into>/<relative_path>`, creating parent directories as needed.
/// For delete-marker entries (identified by the sentinel file
/// hash or the explicit `is_delete_marker` flag): unlink
/// `<into>/<relative_path>` if it exists.
///
/// Returns `(added, modified, deleted)` counters based on what was
/// already present in `prior_paths`. The counts are advisory —
/// they feed the commit message and the `assemble.event`.
fn apply_window_to_worktree(
    into: &Path,
    window: &CommitWindow,
    prior_paths: &[String],
) -> Result<(u64, u64, u64)> {
    use std::collections::HashSet;

    let prior: HashSet<&str> = prior_paths.iter().map(String::as_str).collect();

    let mut added: u64 = 0;
    let mut modified: u64 = 0;
    let mut deleted: u64 = 0;

    for entry in &window.entries {
        let rel = &entry.relative_path;
        let target = into.join(rel);

        // Reject pathological relatives up-front: the enumerate
        // stage already rejects git-invalid keys, but defense in
        // depth matters here — we're about to call
        // `create_dir_all` on arbitrary input.
        if !crate::import::is_importable_relative_path(rel) {
            return Err(CrabError::Internal(format!(
                "assemble: relative path {rel:?} escapes the working tree"
            )));
        }

        let state_file_hash = match &entry.state {
            EntryState::Staged { file_hash } => Some(*file_hash),
            _ => None,
        };

        let is_delete =
            entry.is_delete_marker || state_file_hash.is_some_and(|h| h == DELETE_MARKER_FILE_HASH);

        if is_delete {
            if target.exists() {
                std::fs::remove_file(&target)?;
                deleted += 1;
                debug!(path = %rel, "assemble: removed delete-marker entry");
            }
            continue;
        }

        let Some(file_hash) = state_file_hash else {
            // Failed / skipped / in-progress entries should never
            // reach the assemble stage; if one sneaks in, skip
            // rather than blow up the whole commit. `debug!` so a
            // per-file loop doesn't spam `warn!`; the coordinator
            // already reports failure counts at the stage
            // boundary.
            debug!(
                path = %rel,
                state = ?entry.state,
                "assemble: skipping non-staged entry"
            );
            continue;
        };

        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let pointer = Pointer {
            file_hash,
            size: entry.size,
            shard_hint: None,
        };
        std::fs::write(&target, pointer.serialize())?;
        debug!(path = %rel, size = entry.size, "assemble: wrote pointer blob");

        if prior.contains(rel.as_str()) {
            modified += 1;
        } else {
            added += 1;
        }
    }

    Ok((added, modified, deleted))
}

/// `git add -A` — stage every working-tree change produced by
/// [`apply_window_to_worktree`].
fn git_add_all(into: &Path) -> Result<()> {
    let output = git_command(into)
        .args(["add", "-A"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git add".into(),
            origin: format!("git add -A failed: {stderr}"),
        });
    }
    Ok(())
}

fn git_untrack_internal_state(into: &Path) -> Result<()> {
    let output = git_command(into)
        .args(["rm", "--cached", "-r", "--ignore-unmatch", ".crab"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git rm --cached".into(),
            origin: format!("git rm --cached -r --ignore-unmatch .crab failed: {stderr}"),
        });
    }
    Ok(())
}

/// Commit the currently-staged tree with deterministic author
/// and committer dates. Returns the new commit OID.
fn commit_window(
    into: &Path,
    window: &CommitWindow,
    files_added: u64,
    files_modified: u64,
    files_deleted: u64,
    message_template: Option<&str>,
    author_template: Option<&str>,
) -> Result<String> {
    let date = epoch_to_rfc3339(window.window_end);

    let message = resolve_message(
        message_template,
        &date,
        files_added,
        files_modified,
        files_deleted,
    );

    let mut cmd = git_command(into);
    cmd.arg("commit")
        .arg("--allow-empty")
        .arg(format!("--date={date}"))
        .arg("-m")
        .arg(&message)
        .env("GIT_COMMITTER_DATE", &date)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(author) = author_template {
        cmd.arg(format!("--author={author}"));
    }

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git commit".into(),
            origin: format!("git commit failed for window end {date}: {stderr}"),
        });
    }

    // Capture the fresh OID. `git commit --porcelain` would work
    // but is less portable than `git rev-parse HEAD` across git
    // versions that we might see in CI.
    let rev = git_command(into)
        .args(["rev-parse", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !rev.status.success() {
        let stderr = String::from_utf8_lossy(&rev.stderr);
        return Err(CrabError::Internal(format!(
            "git rev-parse HEAD failed after commit: {stderr}"
        )));
    }
    let oid = String::from_utf8_lossy(&rev.stdout).trim().to_owned();
    if oid.is_empty() {
        return Err(CrabError::Internal(
            "git rev-parse HEAD returned empty output".into(),
        ));
    }
    Ok(oid)
}

/// Default-or-overridden commit message.
fn resolve_message(
    template: Option<&str>,
    window_end_rfc3339: &str,
    added: u64,
    modified: u64,
    deleted: u64,
) -> String {
    if let Some(t) = template {
        return t.to_owned();
    }
    format!(
        "Import bucket state at {window_end_rfc3339}\n\n{added} files added, {modified} modified, {deleted} deleted"
    )
}

/// Convert an epoch-seconds timestamp to an RFC 3339 string
/// without sub-second precision. git accepts either form; we use
/// the whole-second variant to keep commit headers clean.
fn epoch_to_rfc3339(epoch_secs: i64) -> String {
    // `from_epoch_millis` is happy to produce a `.000Z` suffix for
    // seconds. Strip the `.000` so the output matches the commit
    // header git would emit for a user's own `--date` input.
    let total_ms = epoch_secs
        .saturating_mul(1000)
        .try_into()
        .unwrap_or(u64::MAX);
    let with_millis = from_epoch_millis(total_ms);
    // "YYYY-MM-DDTHH:MM:SS.mmmZ" → "YYYY-MM-DDTHH:MM:SSZ"
    if let Some(stripped) = with_millis.strip_suffix(".000Z") {
        format!("{stripped}Z")
    } else {
        with_millis
    }
}

/// List the paths present in a tree (default: HEAD). Used to
/// decide added vs modified counts for the current window's
/// entries.
fn list_tree_paths(into: &Path, treeish: &str) -> Result<Vec<String>> {
    let output = git_command(into)
        .args(["ls-tree", "-r", "--name-only", treeish])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Empty repo (no HEAD yet) is a legitimate case — treat
        // as an empty tree.
        if stderr.contains("Not a valid object") || stderr.contains("unknown revision") {
            return Ok(Vec::new());
        }
        return Err(CrabError::Internal(format!(
            "git ls-tree {treeish} failed: {stderr}"
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().map(str::to_owned).collect())
}

fn rev_parse_optional(into: &Path, rev: &str) -> Result<Option<String>> {
    let output = git_command(into)
        .args(["rev-parse", "--verify", rev])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if oid.is_empty() {
        Ok(None)
    } else {
        Ok(Some(oid))
    }
}

fn show_head_blob(into: &Path, relative_path: &str) -> Result<Option<Vec<u8>>> {
    let spec = format!("HEAD:{relative_path}");
    let output = git_command(into)
        .args(["show", &spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if output.status.success() {
        return Ok(Some(output.stdout));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("exists on disk, but not in")
        || stderr.contains("Path")
        || stderr.contains("does not exist")
        || stderr.contains("not found")
    {
        return Ok(None);
    }
    Err(CrabError::Internal(format!(
        "git show {spec} failed: {stderr}"
    )))
}

fn first_commit_oid(into: &Path) -> Result<Option<String>> {
    let output = git_command(into)
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::to_owned))
}

/// Register the target URL as `origin`. If the remote already
/// exists and `--force` is set, switch the URL via `set-url`;
/// otherwise error.
fn register_origin(into: &Path, target_url: &str, force: bool) -> Result<()> {
    let existing = git_command(into)
        .args(["remote", "get-url", "origin"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if existing.status.success() {
        let existing_url = String::from_utf8_lossy(&existing.stdout).trim().to_owned();
        if existing_url == target_url {
            return Ok(());
        }
        if !force {
            return Err(CrabError::ImportRemoteExists {
                existing_url,
                new_url: target_url.to_owned(),
            });
        }
        let output = git_command(into)
            .args(["remote", "set-url", "origin", target_url])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CrabError::Configuration {
                key: "git remote set-url".into(),
                origin: format!("git remote set-url origin {target_url} failed: {stderr}"),
            });
        }
        debug!(old = %existing_url, new = %target_url, "assemble: overwrote origin URL");
        return Ok(());
    }

    let output = git_command(into)
        .args(["remote", "add", "origin", target_url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git remote add".into(),
            origin: format!("git remote add origin {target_url} failed: {stderr}"),
        });
    }
    Ok(())
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

    use std::sync::Arc;

    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::import::journal::{EntryState, ImportEntry};
    use crab_types::pointer::Pointer;

    /// Acquire the process-wide `GIT_DIR` mutex and clear any stray
    /// `GIT_DIR` env var for the scope of the guard. Every test in
    /// this module that runs real `git` commands must hold it — a
    /// concurrent test elsewhere may have `GIT_DIR` pointed at a
    /// different repo and we'd silently redirect our git calls there.
    fn git_env_guard() -> GitEnvGuard {
        GitEnvGuard::new()
    }

    struct GitEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
        prev_config_global: Option<std::ffi::OsString>,
        prev_config_nosystem: Option<std::ffi::OsString>,
        _global_config: tempfile::NamedTempFile,
    }

    impl GitEnvGuard {
        fn new() -> Self {
            let lock = crate::test::git_repo::GIT_DIR_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            let prev_config_global = std::env::var_os("GIT_CONFIG_GLOBAL");
            let prev_config_nosystem = std::env::var_os("GIT_CONFIG_NOSYSTEM");
            let global_config = tempfile::NamedTempFile::new().unwrap();
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            unsafe { std::env::remove_var("GIT_DIR") };
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", global_config.path()) };
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            unsafe { std::env::set_var("GIT_CONFIG_NOSYSTEM", "1") };
            Self {
                _lock: lock,
                prev,
                prev_config_global,
                prev_config_nosystem,
                _global_config: global_config,
            }
        }
    }

    impl Drop for GitEnvGuard {
        fn drop(&mut self) {
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            match &self.prev {
                Some(v) => unsafe { std::env::set_var("GIT_DIR", v) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
            }
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            match &self.prev_config_global {
                Some(v) => unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", v) },
                None => unsafe { std::env::remove_var("GIT_CONFIG_GLOBAL") },
            }
            // SAFETY: access is serialized by GIT_DIR_MUTEX.
            match &self.prev_config_nosystem {
                Some(v) => unsafe { std::env::set_var("GIT_CONFIG_NOSYSTEM", v) },
                None => unsafe { std::env::remove_var("GIT_CONFIG_NOSYSTEM") },
            }
        }
    }

    /// Progress sink that records every event for assertions.
    #[derive(Default)]
    struct RecordingSink {
        events: Vec<AssembleEvent>,
    }

    impl AssembleProgressSink for RecordingSink {
        fn assemble_event(&mut self, event: &AssembleEvent) {
            self.events.push(event.clone());
        }
    }

    fn sample_hash(seed: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        h
    }

    fn staged(path: &str, hash: [u8; 32], size: u64, last_modified: i64) -> ImportEntry {
        ImportEntry {
            relative_path: path.into(),
            version_id: String::new(),
            size,
            etag: None,
            last_modified,
            is_delete_marker: false,
            state: EntryState::Staged { file_hash: hash },
        }
    }

    fn delete_marker(path: &str, version_id: &str, last_modified: i64) -> ImportEntry {
        ImportEntry {
            relative_path: path.into(),
            version_id: version_id.into(),
            size: 0,
            etag: None,
            last_modified,
            is_delete_marker: true,
            state: EntryState::Staged {
                file_hash: DELETE_MARKER_FILE_HASH,
            },
        }
    }

    /// Configure git user.name / user.email on the given repo so
    /// `git commit` succeeds without touching the host's global
    /// identity. Used by every integration test that lands a real
    /// commit.
    fn configure_test_identity(into: &Path) {
        for (key, val) in [("user.name", "Crab Test"), ("user.email", "test@crab.dev")] {
            let status = Command::new("git")
                .args(["config", "--local", key, val])
                .current_dir(into)
                .status()
                .expect("git config --local must run");
            assert!(status.success(), "git config --local {key} failed");
        }
    }

    /// Running `git log --format=%H -- <path>` against the repo
    /// and returning the commit OIDs that touched `path`, in
    /// newest-first order.
    fn git_log_oids_for_path(into: &Path, path: &str) -> Vec<String> {
        let output = Command::new("git")
            .args(["log", "--format=%H", "--", path])
            .current_dir(into)
            .output()
            .expect("git log must run");
        assert!(output.status.success(), "git log failed");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    // ── extension_of ─────────────────────────────────────────────

    #[test]
    fn extension_of_strips_lowercases_and_tolerates_missing() {
        assert_eq!(
            extension_of("models/a.SafeTensors").as_deref(),
            Some("safetensors")
        );
        assert_eq!(extension_of("no-extension").as_deref(), None);
        assert_eq!(extension_of("dot.").as_deref(), None);
        assert_eq!(extension_of("nested/dir/file.bin").as_deref(), Some("bin"));
    }

    // ── synthesize_gitattributes ─────────────────────────────────

    #[test]
    fn synthesize_gitattributes_emits_all_pointer_extensions() {
        let windows = vec![CommitWindow {
            window_start: 0,
            window_end: 0,
            entries: vec![
                staged("a.bin", sample_hash(1), 2 * 1024 * 1024, 0),
                staged("b.bin", sample_hash(2), 2 * 1024 * 1024, 0),
                staged("m.safetensors", sample_hash(3), 5 * 1024 * 1024, 0),
                staged("c.txt", sample_hash(4), 100, 0),
                staged("d.txt", sample_hash(5), 100, 0),
                staged("e.txt", sample_hash(6), 100, 0),
            ],
        }];
        let tmp = TempDir::new().unwrap();
        let body = synthesize_gitattributes(&windows, &[], tmp.path()).unwrap();
        assert!(
            body.contains("*.bin filter=crab"),
            "body should track *.bin, got:\n{body}"
        );
        assert!(
            body.contains("*.txt filter=crab"),
            "body should track *.txt because import committed txt pointers, got:\n{body}"
        );
        assert!(
            body.contains("*.safetensors filter=crab"),
            "body should track *.safetensors, got:\n{body}"
        );
    }

    #[test]
    fn synthesize_gitattributes_appends_user_track_globs_verbatim() {
        let windows = vec![CommitWindow {
            window_start: 0,
            window_end: 0,
            entries: vec![],
        }];
        let tmp = TempDir::new().unwrap();
        let body = synthesize_gitattributes(
            &windows,
            &["models/*.ckpt filter=crab -text".to_owned()],
            tmp.path(),
        )
        .unwrap();
        assert!(body.contains("models/*.ckpt filter=crab -text"));
    }

    #[test]
    fn synthesize_gitattributes_preserves_existing_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let windows = vec![CommitWindow {
            window_start: 0,
            window_end: 0,
            entries: vec![
                staged("a.bin", sample_hash(1), 2 * 1024 * 1024, 0),
                staged("b.bin", sample_hash(2), 2 * 1024 * 1024, 0),
            ],
        }];
        let body = synthesize_gitattributes(&windows, &[], tmp.path()).unwrap();
        assert!(
            body.is_empty(),
            "existing line must suppress duplicate emission; got:\n{body}"
        );
    }

    // ── ensure_target_dir ────────────────────────────────────────

    #[test]
    fn ensure_target_dir_accepts_missing_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("not-yet-there");
        ensure_target_dir(&path, false).unwrap();
    }

    #[test]
    fn ensure_target_dir_accepts_empty_dir() {
        let tmp = TempDir::new().unwrap();
        ensure_target_dir(tmp.path(), false).unwrap();
    }

    #[test]
    fn ensure_target_dir_rejects_non_empty_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"content").unwrap();
        let err = ensure_target_dir(tmp.path(), false).unwrap_err();
        assert!(matches!(err, CrabError::ImportTargetNotEmpty { .. }));
    }

    #[test]
    fn ensure_target_dir_force_bypasses_rejection() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"content").unwrap();
        ensure_target_dir(tmp.path(), true).unwrap();
    }

    #[test]
    fn ensure_target_dir_accepts_empty_git_repo() {
        let tmp = TempDir::new().unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(tmp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        ensure_target_dir(tmp.path(), false).unwrap();
    }

    // ── ensure_git_identity ──────────────────────────────────────

    #[test]
    fn ensure_git_identity_errors_without_config() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(tmp.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let err = ensure_git_identity(tmp.path()).unwrap_err();
        assert!(matches!(err, CrabError::ImportMissingGitIdentity));
    }

    // ── Task 12.8: flat single-commit integration test ───────────

    #[tokio::test]
    async fn flat_run_assemble_produces_one_commit_with_expected_pointers() {
        let _git_env = git_env_guard();
        // Seed a single-window journal plan with three entries.
        // After run_assemble:
        //   - exactly one commit lands on `main`,
        //   - each expected path exists as a valid pointer,
        //   - the commit date equals the window's end timestamp.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        // 2024-06-15T12:00:00Z
        let ts: i64 = 1_718_452_800;
        let window = CommitWindow {
            window_start: ts,
            window_end: ts,
            entries: vec![
                staged("data/a.bin", sample_hash(1), 4096, ts),
                staged("models/m.safetensors", sample_hash(2), 8192, ts),
                staged("readme.txt", sample_hash(3), 64, ts),
            ],
        };

        // Pre-init so we can configure a local identity scoped to
        // the test (the module itself would git-init on demand,
        // but we need the identity before run_assemble's
        // identity check runs — configure_test_identity assumes
        // `.git/` exists).
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let progress = Arc::new(Mutex::new(RecordingSink::default()));
        let cancel = CancellationToken::new();

        let inputs = AssembleInputs {
            into: into.clone(),
            branch: "main".into(),
            force: false,
            resume: false,
            target_url: "crab://bucket/repo".into(),
            windows: vec![window.clone()],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::clone(&progress),
            metrics: None,
            cancel,
        };

        let stats = run_assemble(inputs).await.expect("assemble must succeed");

        assert_eq!(stats.commits_created, 1);
        assert_eq!(stats.versions_imported, 3);
        assert_eq!(stats.files_imported, 3);
        let head = stats.head_commit_oid.clone().expect("head OID present");
        assert_eq!(stats.first_commit_oid.as_ref(), Some(&head));

        // One event emitted.
        let sink = progress.lock().await;
        assert_eq!(sink.events.len(), 1);
        assert_eq!(sink.events[0].commit_oid, head);
        assert_eq!(sink.events[0].files_added, 3);
        assert_eq!(sink.events[0].files_modified, 0);
        assert_eq!(sink.events[0].files_deleted, 0);
        drop(sink);

        // Each pointer file exists and parses.
        for (rel, size, hash) in [
            ("data/a.bin", 4096u64, sample_hash(1)),
            ("models/m.safetensors", 8192, sample_hash(2)),
            ("readme.txt", 64, sample_hash(3)),
        ] {
            let bytes = std::fs::read(into.join(rel)).unwrap();
            let ptr = Pointer::parse(&bytes).expect("pointer must parse");
            assert_eq!(ptr.file_hash, hash, "hash mismatch for {rel}");
            assert_eq!(ptr.size, size, "size mismatch for {rel}");
            assert_eq!(ptr.shard_hint, None);
        }

        let attrs = std::fs::read_to_string(into.join(".gitattributes")).unwrap();
        assert!(attrs.contains("*.bin filter=crab"));
        assert!(attrs.contains("*.safetensors filter=crab"));
        assert!(attrs.contains("*.txt filter=crab"));

        // Commit author date == window end in RFC3339.
        let output = Command::new("git")
            .args(["log", "-1", "--format=%aI"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(output.status.success(), "git log failed");
        let author_date = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let expected = epoch_to_rfc3339(ts);
        assert!(
            author_date.starts_with(&expected[..expected.len() - 1])
                || author_date == format!("{expected}"),
            "expected date starts with {expected}, got {author_date}"
        );

        // Exactly one commit on main.
        let rev_count = Command::new("git")
            .args(["rev-list", "--count", "main"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(rev_count.status.success());
        let count = String::from_utf8_lossy(&rev_count.stdout).trim().to_owned();
        assert_eq!(count, "1", "expected exactly 1 commit");

        // Origin registered.
        let url = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(url.status.success());
        assert_eq!(
            String::from_utf8_lossy(&url.stdout).trim(),
            "crab://bucket/repo"
        );
    }

    #[tokio::test]
    async fn resume_assemble_accepts_existing_worktree_and_origin() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let ts: i64 = 1_718_452_800;
        let window = CommitWindow {
            window_start: ts,
            window_end: ts,
            entries: vec![staged("large.bin", sample_hash(7), 20_000_000_000, ts)],
        };

        std::fs::write(into.join("large.bin"), b"left by interrupted assemble").unwrap();
        let remote = Command::new("git")
            .args(["remote", "add", "origin", "crab://bucket/repo"])
            .current_dir(&into)
            .status()
            .unwrap();
        assert!(remote.success());

        let resume_inputs = AssembleInputs {
            into: into.clone(),
            branch: "main".into(),
            force: false,
            resume: true,
            target_url: "crab://bucket/repo".into(),
            windows: vec![window.clone()],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::new(Mutex::new(RecordingSink::default())),
            metrics: None,
            cancel: CancellationToken::new(),
        };
        let first_stats = run_assemble(resume_inputs)
            .await
            .expect("resume assemble must accept existing worktree");
        assert_eq!(first_stats.commits_created, 1);

        let url = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(url.status.success());
        assert_eq!(
            String::from_utf8_lossy(&url.stdout).trim(),
            "crab://bucket/repo"
        );

        let rev_count = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(rev_count.status.success());
        assert_eq!(String::from_utf8_lossy(&rev_count.stdout).trim(), "1");

        let second_inputs = AssembleInputs {
            into: into.clone(),
            branch: "main".into(),
            force: false,
            resume: true,
            target_url: "crab://bucket/repo".into(),
            windows: vec![window],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::new(Mutex::new(RecordingSink::default())),
            metrics: None,
            cancel: CancellationToken::new(),
        };
        let second_stats = run_assemble(second_inputs)
            .await
            .expect("resume assemble should reuse matching HEAD");
        assert_eq!(second_stats.commits_created, 0);

        let rev_count = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(rev_count.status.success());
        assert_eq!(String::from_utf8_lossy(&rev_count.stdout).trim(), "1");
    }

    #[tokio::test]
    async fn run_assemble_rejects_unsafe_import_path() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let ts: i64 = 1_718_452_800;
        let window = CommitWindow {
            window_start: ts,
            window_end: ts,
            entries: vec![staged("data/../escape.bin", sample_hash(8), 4096, ts)],
        };

        let inputs = AssembleInputs {
            into,
            branch: "main".into(),
            force: false,
            resume: false,
            target_url: "crab://bucket/repo".into(),
            windows: vec![window],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::new(Mutex::new(RecordingSink::default())),
            metrics: None,
            cancel: CancellationToken::new(),
        };
        let err = run_assemble(inputs)
            .await
            .expect_err("unsafe import path must be rejected");
        assert!(matches!(err, CrabError::Internal(_)));
    }

    #[tokio::test]
    async fn run_assemble_excludes_import_runtime_state_from_commit() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        std::fs::create_dir_all(into.join(".crab/staging")).unwrap();
        std::fs::write(into.join(".crab/import-journal.db"), b"journal").unwrap();
        std::fs::write(into.join(".crab/staging/index.db"), b"staging").unwrap();

        let ts = 1_718_452_800;
        let window = CommitWindow {
            window_start: ts,
            window_end: ts,
            entries: vec![staged("large.bin", sample_hash(1), 4096, ts)],
        };

        let inputs = AssembleInputs {
            into: into.clone(),
            branch: "main".into(),
            force: false,
            resume: false,
            target_url: "crab://bucket/repo".into(),
            windows: vec![window],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::new(Mutex::new(RecordingSink::default())),
            metrics: None,
            cancel: CancellationToken::new(),
        };

        let stats = run_assemble(inputs).await.expect("assemble must succeed");
        assert_eq!(stats.files_imported, 1);

        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(output.status.success());
        let paths = String::from_utf8_lossy(&output.stdout);
        assert!(paths.lines().any(|line| line == "large.bin"));
        assert!(
            !paths.lines().any(|line| line.starts_with(".crab/")),
            "import runtime state must not be committed, got:\n{paths}"
        );
    }

    #[tokio::test]
    async fn run_assemble_commits_attributes_without_counting_them_as_imported_files() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let ts = 1_718_452_800;
        let window = CommitWindow {
            window_start: ts,
            window_end: ts,
            entries: vec![staged("large.bin", sample_hash(1), 20 * 1024 * 1024, ts)],
        };

        let inputs = AssembleInputs {
            into: into.clone(),
            branch: "main".into(),
            force: false,
            resume: false,
            target_url: "crab://bucket/repo".into(),
            windows: vec![window],
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::new(Mutex::new(RecordingSink::default())),
            metrics: None,
            cancel: CancellationToken::new(),
        };

        let stats = run_assemble(inputs).await.expect("assemble must succeed");
        assert_eq!(stats.files_imported, 1);

        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(output.status.success());
        let paths = String::from_utf8_lossy(&output.stdout);
        assert!(paths.lines().any(|line| line == ".gitattributes"));
        assert!(paths.lines().any(|line| line == "large.bin"));
    }

    // ── Task 12.9: versioned multi-commit integration test ──────

    #[tokio::test]
    async fn versioned_run_assemble_reconstructs_per_window_state() {
        let _git_env = git_env_guard();
        // Three windows touching the same path:
        //   window 1 @ t=100 → add file_a
        //   window 2 @ t=200 → modify file_a
        //   window 3 @ t=300 → delete file_a
        // After the walk:
        //   * 3 commits on main,
        //   * each commit's tree reflects the expected state,
        //   * `git log -- file_a` shows 3 entries.
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();

        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        configure_test_identity(&into);

        let windows = vec![
            CommitWindow {
                window_start: 100,
                window_end: 100,
                entries: vec![staged("file_a", sample_hash(10), 1024, 100)],
            },
            CommitWindow {
                window_start: 200,
                window_end: 200,
                entries: vec![staged("file_a", sample_hash(20), 2048, 200)],
            },
            CommitWindow {
                window_start: 300,
                window_end: 300,
                entries: vec![delete_marker("file_a", "v3", 300)],
            },
        ];

        let progress = Arc::new(Mutex::new(RecordingSink::default()));
        let cancel = CancellationToken::new();

        let inputs = AssembleInputs {
            into: into.clone(),
            branch: "main".into(),
            force: false,
            resume: false,
            target_url: "crab://bucket/versioned".into(),
            windows,
            track: Vec::new(),
            message_template: None,
            author_template: None,
            progress: Arc::clone(&progress),
            metrics: None,
            cancel,
        };

        let stats = run_assemble(inputs).await.expect("assemble must succeed");

        assert_eq!(stats.commits_created, 3);
        assert_eq!(stats.versions_imported, 3);
        assert_eq!(stats.files_imported, 0, "file_a is deleted in HEAD");

        let events = progress.lock().await.events.clone();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].files_added, 1);
        assert_eq!(events[0].files_modified, 0);
        assert_eq!(events[0].files_deleted, 0);
        assert_eq!(events[1].files_added, 0);
        assert_eq!(events[1].files_modified, 1);
        assert_eq!(events[1].files_deleted, 0);
        assert_eq!(events[2].files_added, 0);
        assert_eq!(events[2].files_modified, 0);
        assert_eq!(events[2].files_deleted, 1);

        // `git log -- file_a` shows 3 entries.
        let oids = git_log_oids_for_path(&into, "file_a");
        assert_eq!(
            oids.len(),
            3,
            "git log -- file_a must show all three touches, got: {oids:?}"
        );

        // Per-commit tree inspection.
        //   Commit 1: file_a exists with sample_hash(10) / size 1024.
        //   Commit 2: file_a exists with sample_hash(20) / size 2048.
        //   Commit 3: file_a absent.
        let commits: Vec<String> = oids.iter().rev().cloned().collect();

        // Commit 1 pointer.
        let blob = Command::new("git")
            .args(["show"])
            .arg(format!("{}:file_a", commits[0]))
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(blob.status.success(), "git show commit1:file_a failed");
        let ptr1 = Pointer::parse(&blob.stdout).expect("pointer must parse");
        assert_eq!(ptr1.file_hash, sample_hash(10));
        assert_eq!(ptr1.size, 1024);

        // Commit 2 pointer (the modification).
        let blob = Command::new("git")
            .args(["show"])
            .arg(format!("{}:file_a", commits[1]))
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(blob.status.success(), "git show commit2:file_a failed");
        let ptr2 = Pointer::parse(&blob.stdout).expect("pointer must parse");
        assert_eq!(ptr2.file_hash, sample_hash(20));
        assert_eq!(ptr2.size, 2048);

        // Commit 3 tree must not contain file_a.
        let tree = Command::new("git")
            .args(["ls-tree", "-r", "--name-only"])
            .arg(&commits[2])
            .current_dir(&into)
            .output()
            .unwrap();
        assert!(tree.status.success());
        let paths = String::from_utf8_lossy(&tree.stdout);
        assert!(
            !paths.lines().any(|l| l == "file_a"),
            "commit 3 tree must not contain file_a, got:\n{paths}"
        );

        // Commit dates are deterministic (match window_end).
        for (commit, ts) in commits.iter().zip([100, 200, 300]) {
            let output = Command::new("git")
                .args(["log", "-1", "--format=%aI"])
                .arg(commit)
                .current_dir(&into)
                .output()
                .unwrap();
            assert!(output.status.success());
            let date = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let expected = epoch_to_rfc3339(ts);
            // git's %aI omits the fractional second; allow either
            // the literal RFC3339 or a zero-offset equivalent.
            assert!(
                date.starts_with(&expected[..expected.len() - 1]),
                "expected commit date for ts={ts} to start with {expected}, got {date}"
            );
        }
    }

    // ── Remote existing-origin safety rail ───────────────────────

    #[tokio::test]
    async fn register_origin_errors_when_origin_exists_without_force() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        // Pre-configure origin.
        let status = Command::new("git")
            .args(["remote", "add", "origin", "https://old.example/repo"])
            .current_dir(&into)
            .status()
            .unwrap();
        assert!(status.success());

        let err = register_origin(&into, "crab://bucket/repo", false).unwrap_err();
        match err {
            CrabError::ImportRemoteExists {
                existing_url,
                new_url,
            } => {
                assert_eq!(existing_url, "https://old.example/repo");
                assert_eq!(new_url, "crab://bucket/repo");
            }
            other => panic!("expected ImportRemoteExists, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_origin_overwrites_with_force() {
        let _git_env = git_env_guard();
        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        std::fs::create_dir_all(&into).unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&into)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .args(["remote", "add", "origin", "https://old.example/repo"])
            .current_dir(&into)
            .status()
            .unwrap();
        assert!(status.success());

        register_origin(&into, "crab://bucket/repo", true).unwrap();
        let url = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&into)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&url.stdout).trim(),
            "crab://bucket/repo"
        );
    }
}
