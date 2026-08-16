//! `crab track {glob}` / `crab untrack {glob}` — manage `.gitattributes`
//! entries for crab filter processing.
//!
//! Adds or removes lines of the form:
//! ```text
//! {glob} filter=crab diff=crab merge=crab -text
//! ```
//!
//! When invoked without arguments, lists currently tracked patterns.

use std::path::Path;

use serde::Serialize;

use crate::core::error::Result;
use crate::core::output::{OutputMode, emit_json};

/// Suffix appended to each tracked glob in `.gitattributes`.
const ATTRS_SUFFIX: &str = "filter=crab diff=crab merge=crab -text";

/// Build the full `.gitattributes` line for a given glob pattern.
fn attrs_line(glob: &str) -> String {
    format!("{glob} {ATTRS_SUFFIX}")
}

/// Payload emitted by `crab track --json` (list mode).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TrackPayload {
    pub patterns: Vec<TrackPattern>,
}

/// A single tracked pattern entry.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TrackPattern {
    pub glob: String,
    pub source: String,
}

/// List tracked crab patterns from `.gitattributes` in the cwd.
pub fn run_track_list(mode: OutputMode) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_track_list_in(&cwd, mode)
}

/// List tracked crab patterns from `.gitattributes` in `root`.
pub fn run_track_list_in(root: &Path, mode: OutputMode) -> Result<()> {
    let patterns = collect_tracked_patterns(root)?;

    if mode == OutputMode::Json {
        let payload = TrackPayload { patterns };
        emit_json("track", "1.0", payload);
        return Ok(());
    }

    // Text mode: print each pattern like git-lfs does.
    for pat in &patterns {
        println!("    {} ({ATTRS_SUFFIX})", pat.glob);
    }
    Ok(())
}

/// Parse `.gitattributes` and return crab-tracked patterns.
pub fn collect_tracked_patterns(root: &Path) -> Result<Vec<TrackPattern>> {
    let path = root.join(".gitattributes");
    let content = read_or_empty(&path)?;
    let source = ".gitattributes".to_owned();

    let patterns = content
        .lines()
        .filter_map(|line| {
            line.strip_suffix(ATTRS_SUFFIX)
                .map(str::trim_end)
                .filter(|g| !g.is_empty())
                .map(|g| TrackPattern {
                    glob: g.to_owned(),
                    source: source.clone(),
                })
        })
        .collect();

    Ok(patterns)
}

/// Add a glob pattern to `.gitattributes` for crab filter processing.
///
/// If the pattern is already present, this is a no-op. The file is created
/// if it does not exist.
///
/// Operates on `.gitattributes` in the current working directory.
///
/// # Errors
///
/// Returns [`CrabError::Io`] on filesystem failures.
pub fn run_track(glob: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_track_in(glob, &cwd)
}

/// Remove a glob pattern from `.gitattributes`.
///
/// Removes all lines that match the exact crab attributes line for the
/// given glob. If the pattern is not present, this is a no-op.
///
/// Operates on `.gitattributes` in the current working directory.
///
/// # Errors
///
/// Returns [`CrabError::Io`] on filesystem failures.
pub fn run_untrack(glob: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_untrack_in(glob, &cwd)
}

/// Track implementation that accepts an explicit root directory.
pub fn run_track_in(glob: &str, root: &Path) -> Result<()> {
    let path = root.join(".gitattributes");
    let line = attrs_line(glob);

    // Serialize read-modify-write cycles so concurrent track operations
    // cannot overwrite each other's attribute changes.
    let _lock = acquire_gitattributes_lock(root)?;

    let existing = read_or_empty(&path)?;

    // Already tracked — nothing to do.
    if existing.lines().any(|l| l == line) {
        tracing::info!(glob = %glob, "pattern already tracked");
        return Ok(());
    }

    let mut content = existing;
    // Ensure we start on a fresh line.
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');

    write_atomic(&path, content.as_bytes())?;
    tracing::info!(glob = %glob, "tracking pattern");
    Ok(())
}

/// Untrack implementation that accepts an explicit root directory.
pub fn run_untrack_in(glob: &str, root: &Path) -> Result<()> {
    let path = root.join(".gitattributes");
    let line = attrs_line(glob);

    // See run_track_in for the lock rationale.
    let _lock = acquire_gitattributes_lock(root)?;

    let existing = read_or_empty(&path)?;
    if existing.is_empty() {
        tracing::info!(glob = %glob, "no .gitattributes file, nothing to untrack");
        return Ok(());
    }

    let filtered: Vec<&str> = existing.lines().filter(|l| *l != line).collect();

    // Nothing changed — pattern wasn't present.
    if filtered.len() == existing.lines().count() {
        tracing::info!(glob = %glob, "pattern not found in .gitattributes");
        return Ok(());
    }

    let mut content = filtered.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }

    write_atomic(&path, content.as_bytes())?;
    tracing::info!(glob = %glob, "untracked pattern");
    Ok(())
}

/// Read a file to a string, returning an empty string if it doesn't exist.
fn read_or_empty(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Write `data` to `path` atomically via a temp file + rename.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Guard that holds an advisory flock for the duration of a track/untrack
/// operation. The persistent lock inode lives below Crab's ignored state
/// directory so it never appears as an untracked worktree file.
struct GitattributesLock {
    _file: std::fs::File,
}

/// Acquire an exclusive advisory lock around `.gitattributes` modification.
///
/// The lock serializes concurrent `crab track`/`crab untrack`
/// invocations so their read-modify-write cycles don't clobber each
/// other. Released via RAII when the returned guard is dropped.
fn acquire_gitattributes_lock(root: &Path) -> Result<GitattributesLock> {
    use fs4::fs_std::FileExt as LockFileExt;

    let lock_dir = root.join(".crab/locks");
    std::fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join("gitattributes.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;

    file.lock_exclusive()?;

    Ok(GitattributesLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::output::OutputMode;

    #[test]
    fn track_creates_gitattributes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        run_track_in("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert_eq!(content, "*.bin filter=crab diff=crab merge=crab -text\n",);
        assert!(!dir.path().join(".gitattributes.lock").exists());
        assert!(dir.path().join(".crab/locks/gitattributes.lock").exists());
    }

    #[test]
    fn track_appends_to_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let ga = dir.path().join(".gitattributes");
        std::fs::write(&ga, "*.txt text\n").unwrap();

        run_track_in("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(&ga).unwrap();
        assert!(content.contains("*.txt text\n"));
        assert!(content.contains("*.bin filter=crab diff=crab merge=crab -text\n"));
    }

    #[test]
    fn track_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        run_track_in("*.bin", dir.path()).unwrap();
        run_track_in("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        let count = content.lines().filter(|l| l.contains("*.bin")).count();
        assert_eq!(count, 1, "pattern should appear exactly once");
    }

    #[test]
    fn untrack_removes_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        run_track_in("*.bin", dir.path()).unwrap();
        run_track_in("*.dat", dir.path()).unwrap();

        run_untrack_in("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(!content.contains("*.bin"));
        assert!(content.contains("*.dat filter=crab diff=crab merge=crab -text\n"));
    }

    #[test]
    fn untrack_is_noop_when_pattern_absent() {
        let dir = tempfile::tempdir().unwrap();
        run_track_in("*.bin", dir.path()).unwrap();

        // Untrack a pattern that was never tracked.
        run_untrack_in("*.dat", dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.bin"));
    }

    #[test]
    fn untrack_is_noop_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        // No .gitattributes exists — should not error.
        run_untrack_in("*.bin", dir.path()).unwrap();
    }

    #[test]
    fn untrack_preserves_non_crab_lines() {
        let dir = tempfile::tempdir().unwrap();
        let ga = dir.path().join(".gitattributes");
        std::fs::write(
            &ga,
            "*.txt text\n*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        run_untrack_in("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(&ga).unwrap();
        assert_eq!(content, "*.txt text\n");
    }

    #[test]
    fn attrs_line_format() {
        assert_eq!(
            attrs_line("*.bin"),
            "*.bin filter=crab diff=crab merge=crab -text",
        );
    }

    #[test]
    fn list_returns_tracked_patterns() {
        let dir = tempfile::tempdir().unwrap();
        run_track_in("*.bin", dir.path()).unwrap();
        run_track_in("*.dat", dir.path()).unwrap();

        let patterns = collect_tracked_patterns(dir.path()).unwrap();
        assert_eq!(patterns.len(), 2);
        assert_eq!(patterns[0].glob, "*.bin");
        assert_eq!(patterns[0].source, ".gitattributes");
        assert_eq!(patterns[1].glob, "*.dat");
    }

    #[test]
    fn list_ignores_non_crab_lines() {
        let dir = tempfile::tempdir().unwrap();
        let ga = dir.path().join(".gitattributes");
        std::fs::write(
            &ga,
            "*.txt text\n*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let patterns = collect_tracked_patterns(dir.path()).unwrap();
        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].glob, "*.bin");
    }

    #[test]
    fn list_empty_when_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        let patterns = collect_tracked_patterns(dir.path()).unwrap();
        assert!(patterns.is_empty());
    }

    #[test]
    fn list_text_mode_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        run_track_in("*.bin", dir.path()).unwrap();
        run_track_list_in(dir.path(), OutputMode::Text).unwrap();
    }
}
