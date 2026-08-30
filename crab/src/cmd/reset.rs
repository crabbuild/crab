//! `crab reset <patterns>` — unstage files from both git's index and
//! the crab staging area.
//!
//! Mirrors `git reset HEAD -- <paths>` semantics: removes files from
//! git's staging (index) and cleans up the corresponding crab chunk
//! data from the local staging area. The working tree is never modified.
//!
//! Also provides a post-`git reset`/`git rm`/`git checkout` hook via
//! `crab reset --sync` that scans for files no longer in git's index
//! and removes their staging data.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::core::error::{self, CrabError, Result};
use crate::core::pattern::{PatternFilter, build_filter};
use crab_staging::StagingArea;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;

/// Arguments for the `crab reset` command.
pub struct ResetArgs {
    /// Glob patterns to unstage (e.g. `*.safetensors`, `models/`).
    pub patterns: Vec<String>,
    /// Dry run: show what would be unstaged without modifying anything.
    pub dry_run: bool,
    /// Sync mode: scan for files removed from git's index and clean
    /// their staging data. Use after `git reset`, `git rm`, etc.
    pub sync: bool,
}

/// Summary of a completed reset operation.
struct ResetSummary {
    files_unstaged: u64,
    staging_cleaned: u64,
}

/// Run the `crab reset` command.
///
/// In default mode, runs `git reset HEAD -- <paths>` for matching files
/// and removes their chunk data from the crab staging area.
///
/// In `--sync` mode, scans the working tree for crab-tracked files
/// whose pointers are no longer in git's index and cleans their staging
/// data.
pub async fn run_reset(args: &ResetArgs, cancel: &CancellationToken) -> Result<()> {
    let git_dir = crate::git::discover::discover_git_dir()?;
    let repo_root = git_dir
        .parent()
        .ok_or_else(|| CrabError::Internal("git dir has no parent".into()))?
        .to_path_buf();

    let tracked = TrackedClassifier::open(&repo_root)?;
    if tracked.is_empty() {
        println!("No crab-tracked patterns in .gitattributes.");
        return Ok(());
    }

    if args.sync {
        return run_sync_mode(&repo_root, &tracked, args, cancel).await;
    }

    if args.patterns.is_empty() {
        println!("Usage: crab reset <patterns...>");
        println!("       crab reset --sync");
        return Ok(());
    }

    let filter = build_filter(&args.patterns, &[])?;

    // Find crab-tracked files that match the user's patterns.
    let candidates = collect_staged_candidates(&repo_root, &tracked, &filter, cancel)?;

    if candidates.is_empty() {
        println!("No matching staged files found.");
        return Ok(());
    }

    if args.dry_run {
        println!("Would unstage {} file(s):", candidates.len());
        for (path, _) in &candidates {
            let rel = path.strip_prefix(&repo_root).unwrap_or(path);
            println!("  {}", rel.display());
        }
        return Ok(());
    }

    // Open the staging area.
    let staging_root = repo_root.join(".crab").join("staging");
    let staging = Arc::new(StagingArea::open(staging_root).await?);

    let mut summary = ResetSummary {
        files_unstaged: 0,
        staging_cleaned: 0,
    };

    // Unstage from git's index first.
    let paths: Vec<&Path> = candidates.iter().map(|(p, _)| p.as_path()).collect();
    run_git_reset(&paths, &repo_root)?;

    // Clean crab staging data for each file.
    for (abs_path, pointer) in &candidates {
        error::check_cancelled(cancel)?;

        let rel = abs_path.strip_prefix(&repo_root).unwrap_or(abs_path);
        let file_hash = MerkleHash::from(pointer.file_hash);

        match staging.release_published_path(rel, &file_hash) {
            Ok(true) => {
                debug!(path = %rel.display(), "cleaned staging data");
                summary.staging_cleaned += 1;
            }
            Ok(false) => {
                debug!(path = %rel.display(), "no staging data found");
            }
            Err(e) => {
                warn!(path = %rel.display(), error = %e, "failed to clean staging data");
            }
        }

        summary.files_unstaged += 1;
    }

    // Close the staging area.
    match Arc::try_unwrap(staging) {
        Ok(s) => s.close().await?,
        Err(_) => {
            warn!("staging area still referenced, skipping explicit close");
        }
    }

    println!(
        "Reset {} file(s), cleaned {} from staging.",
        summary.files_unstaged, summary.staging_cleaned,
    );

    Ok(())
}

/// Sync mode: scan for crab-tracked files whose staging data is
/// orphaned (file no longer in git's index or working tree).
async fn run_sync_mode(
    repo_root: &Path,
    tracked: &TrackedClassifier,
    args: &ResetArgs,
    cancel: &CancellationToken,
) -> Result<()> {
    let filter = if args.patterns.is_empty() {
        build_filter(&["**/*".to_owned()], &[])?
    } else {
        build_filter(&args.patterns, &[])?
    };

    // Find pointer files in the working tree (these are files that were
    // checked out in lazy mode or left as pointers).
    let pointers = collect_pointer_files(repo_root, tracked, &filter, cancel)?;

    if pointers.is_empty() {
        println!("No orphaned staging data found.");
        return Ok(());
    }

    // Check which of these pointers are NOT in git's index.
    let orphaned: Vec<(PathBuf, Pointer)> = pointers
        .into_iter()
        .filter(|(path, _)| !is_in_git_index(path, repo_root))
        .collect();

    if orphaned.is_empty() {
        println!("No orphaned staging data found.");
        return Ok(());
    }

    if args.dry_run {
        println!("Would clean staging data for {} file(s):", orphaned.len());
        for (path, _) in &orphaned {
            let rel = path.strip_prefix(repo_root).unwrap_or(path);
            println!("  {}", rel.display());
        }
        return Ok(());
    }

    let staging_root = repo_root.join(".crab").join("staging");
    let staging = Arc::new(StagingArea::open(staging_root).await?);

    let mut cleaned = 0u64;
    for (abs_path, pointer) in &orphaned {
        error::check_cancelled(cancel)?;

        let rel = abs_path.strip_prefix(repo_root).unwrap_or(abs_path);
        let file_hash = MerkleHash::from(pointer.file_hash);

        match staging.release_published_path(rel, &file_hash) {
            Ok(true) => {
                debug!(path = %rel.display(), "cleaned orphaned staging data");
                cleaned += 1;
            }
            Ok(false) => {
                debug!(path = %rel.display(), "no staging data to clean");
            }
            Err(e) => {
                warn!(path = %rel.display(), error = %e, "failed to clean staging data");
            }
        }
    }

    match Arc::try_unwrap(staging) {
        Ok(s) => s.close().await?,
        Err(_) => {
            warn!("staging area still referenced, skipping explicit close");
        }
    }

    println!("Sync complete: cleaned {cleaned} orphaned file(s) from staging.");

    Ok(())
}

/// Collect crab-tracked files that are staged in git's index and have
/// a parseable pointer in the git object database.
fn collect_staged_candidates(
    repo_root: &Path,
    tracked: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, Pointer)>> {
    // Use `git ls-files --cached` to list files in the index, then
    // filter to crab-tracked patterns and read their staged blob
    // to extract the pointer.
    let output = std::process::Command::new("git")
        .args(["ls-files", "--cached", "-z"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git ls-files: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Internal(format!(
            "git ls-files failed: {stderr}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut candidates = Vec::new();

    for entry in stdout.split('\0') {
        if entry.is_empty() {
            continue;
        }
        error::check_cancelled(cancel)?;

        let rel_path = Path::new(entry);

        if !tracked.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();
        if !filter.matches(&rel_str) {
            continue;
        }

        // Read the staged blob from git's index to get the pointer.
        let blob = read_staged_blob(repo_root, entry);
        let pointer = match blob {
            Ok(bytes) => match Pointer::parse(&bytes) {
                Ok(p) => p,
                Err(_) => continue, // Not a pointer in the index, skip.
            },
            Err(_) => continue,
        };

        candidates.push((repo_root.join(rel_path), pointer));
    }

    Ok(candidates)
}

/// Collect pointer files from the working tree.
fn collect_pointer_files(
    repo_root: &Path,
    tracked: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, Pointer)>> {
    let mut results = Vec::new();
    walk_pointer_files(repo_root, repo_root, tracked, filter, cancel, &mut results)?;
    Ok(results)
}

fn walk_pointer_files(
    root: &Path,
    dir: &Path,
    tracked: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
    out: &mut Vec<(PathBuf, Pointer)>,
) -> Result<()> {
    error::check_cancelled(cancel)?;

    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            walk_pointer_files(root, &path, tracked, filter, cancel, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        if !tracked.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();
        if !filter.matches(&rel_str) {
            continue;
        }

        if !crate::engine::pointer::is_working_tree_pointer(&path).unwrap_or(false) {
            continue;
        }

        // Parse the pointer.
        let Ok(content) = std::fs::read(&path) else {
            continue;
        };
        let Ok(pointer) = Pointer::parse(&content) else {
            continue;
        };

        out.push((path, pointer));
    }

    Ok(())
}

/// Read the staged blob for a file from git's index.
fn read_staged_blob(repo_root: &Path, path: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(["show", &format!(":{path}")])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Internal(format!("failed to run git show: {e}")))?;

    if !output.status.success() {
        return Err(CrabError::Internal(format!("git show :{path} failed")));
    }

    Ok(output.stdout)
}

/// Check whether a file is in git's index.
fn is_in_git_index(abs_path: &Path, repo_root: &Path) -> bool {
    let Ok(rel) = abs_path.strip_prefix(repo_root) else {
        return false;
    };

    let output = std::process::Command::new("git")
        .args(["ls-files", "--cached", "--error-unmatch", "--"])
        .arg(rel)
        .current_dir(repo_root)
        .output();

    matches!(output, Ok(o) if o.status.success())
}

/// Run `git reset HEAD -- <paths>` to unstage files from git's index.
fn run_git_reset(paths: &[&Path], repo_root: &Path) -> Result<()> {
    const BATCH_SIZE: usize = 100;

    for batch in paths.chunks(BATCH_SIZE) {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["reset", "HEAD", "--"]);
        for path in batch {
            cmd.arg(path);
        }
        cmd.current_dir(repo_root);

        let output = cmd
            .output()
            .map_err(|e| CrabError::Internal(format!("failed to run git reset: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // git reset returns non-zero for files not in the index — warn
            // but don't fail the whole operation.
            warn!(stderr = %stderr, "git reset reported warnings");
        }
    }

    Ok(())
}

/// Per-site classifier for `.gitattributes filter=crab` lookup.
///
/// Under `gix-pathmatch`, wraps the consolidated
/// [`core::attrs::TrackedClassifier`] (backed by `gix_attributes::Search`).
/// Otherwise falls back to the legacy suffix-matching helper driven by
/// patterns parsed out of the root `.gitattributes` line-by-line.
#[cfg(feature = "gix-pathmatch")]
struct TrackedClassifier(crate::core::attrs::TrackedClassifier);

#[cfg(not(feature = "gix-pathmatch"))]
struct TrackedClassifier {
    patterns: Vec<String>,
}

impl TrackedClassifier {
    fn open(root: &Path) -> Result<Self> {
        #[cfg(feature = "gix-pathmatch")]
        {
            Ok(TrackedClassifier(
                crate::core::attrs::TrackedClassifier::open(root, "crab")?,
            ))
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            Ok(TrackedClassifier {
                patterns: parse_gitattributes_globs_legacy(root)?,
            })
        }
    }

    fn is_empty(&self) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            false
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.patterns.is_empty()
        }
    }

    fn is_tracked(&self, rel_path: &Path) -> bool {
        let rel_str = rel_path.to_string_lossy();
        #[cfg(feature = "gix-pathmatch")]
        {
            self.0.is_tracked(&rel_str)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            let _ = rel_str;
            matches_any_tracked_legacy(rel_path, &self.patterns)
        }
    }
}

/// Legacy fallback for builds without `gix-pathmatch`. The consolidated
/// matcher lives in `core::attrs`.
#[cfg(not(feature = "gix-pathmatch"))]
fn parse_gitattributes_globs_legacy(root: &Path) -> Result<Vec<String>> {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let globs = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    Ok(globs)
}

/// Legacy suffix-matching helper retained for builds without `gix-pathmatch`.
#[cfg(not(feature = "gix-pathmatch"))]
fn matches_any_tracked_legacy(rel_path: &Path, patterns: &[String]) -> bool {
    let path_str = rel_path.to_string_lossy();

    for pattern in patterns {
        if pattern == "*" || pattern == "**" || pattern == "**/*" {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix('*')
            && path_str.ends_with(suffix)
        {
            return true;
        }
        if *pattern == *path_str {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_any_tracked_extension() {
        let patterns = vec!["*.dmg".to_owned()];
        assert!(matches_any_tracked_legacy(Path::new("test.dmg"), &patterns));
        assert!(!matches_any_tracked_legacy(
            Path::new("test.txt"),
            &patterns
        ));
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_any_tracked_wildcard() {
        let patterns = vec!["**/*".to_owned()];
        assert!(matches_any_tracked_legacy(
            Path::new("anything.xyz"),
            &patterns
        ));
    }

    #[test]
    fn classifier_detects_tracked_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n\
             *.txt text\n",
        )
        .unwrap();

        let classifier = TrackedClassifier::open(dir.path()).unwrap();
        assert!(classifier.is_tracked(Path::new("model.bin")));
        assert!(!classifier.is_tracked(Path::new("readme.md")));
    }

    #[test]
    fn classifier_is_empty_when_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        let classifier = TrackedClassifier::open(dir.path()).unwrap();
        #[cfg(not(feature = "gix-pathmatch"))]
        assert!(classifier.is_empty());
        // In gix-pathmatch mode the reader always advertises non-empty;
        // the walk simply produces zero matches against no rules.
        #[cfg(feature = "gix-pathmatch")]
        assert!(!classifier.is_tracked(Path::new("model.bin")));
    }
}
