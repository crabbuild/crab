//! LFS track and untrack via `.gitattributes` management.
//!
//! Adds, removes, and lists LFS tracking patterns in `.gitattributes`.
//! Handles glob character escaping and ensures idempotent operations.
//! Detects conflicts with Crab/XET (`filter=crab`) entries.

use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// The LFS attributes suffix appended after the escaped pattern.
const LFS_ATTRS: &str = "filter=lfs diff=lfs merge=lfs -text";
const LFS_UNTRACK_ATTRS: &str = "!text !filter !merge !diff";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockableMode {
    Preserve,
    Enable,
    Disable,
}

#[derive(Debug, Clone, Copy)]
pub struct TrackOptions {
    pub force: bool,
    pub dry_run: bool,
    pub lockable: LockableMode,
}

impl TrackOptions {
    #[must_use]
    pub fn new(force: bool, dry_run: bool) -> Self {
        Self {
            force,
            dry_run,
            lockable: LockableMode::Preserve,
        }
    }
}

/// Outcome of a track operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackOutcome {
    /// Pattern added.
    Tracked,
    /// Pattern was already tracked by LFS.
    AlreadyTracked,
    /// Pattern was previously tracked by Crab/XET and has been switched.
    SwitchedFromCrab,
    /// Pattern has an existing entry with different (non-filter) attributes.
    Updated,
}

/// Outcome of a conflict check before tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictCheck {
    /// No conflict — safe to track.
    Clear,
    /// Pattern is already tracked by LFS.
    AlreadyLfs,
    /// Pattern is tracked by Crab/XET — conflict.
    CrabConflict,
}

/// A tracked pattern with its filter type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedPattern {
    pub pattern: String,
    pub filter: FilterType,
}

/// Filter type for a tracked pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    Lfs,
    Crab,
}

impl std::fmt::Display for FilterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterType::Lfs => write!(f, "lfs"),
            FilterType::Crab => write!(f, "crab"),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check for conflicts before tracking a pattern.
///
/// Returns [`ConflictCheck::CrabConflict`] when the same pattern is already
/// tracked with `filter=crab`.
pub fn check_conflict(pattern: &str, repo_root: &Path) -> ConflictCheck {
    check_conflict_for_escaped(&escape_attr_pattern(pattern), repo_root)
}

/// Check for conflicts before tracking a literal filename.
pub fn check_conflict_filename(filename: &str, repo_root: &Path) -> ConflictCheck {
    check_conflict_for_escaped(&escape_attr_filename(filename), repo_root)
}

fn check_conflict_for_escaped(escaped: &str, repo_root: &Path) -> ConflictCheck {
    let ga_path = repo_root.join(".gitattributes");
    let Ok(content) = std::fs::read_to_string(&ga_path) else {
        return ConflictCheck::Clear;
    };

    let has_lfs = is_lfs_pattern_present(&content, escaped);
    let has_crab = is_crab_pattern_present(&content, escaped);

    // Both filters present for same pattern — conflict.
    if has_lfs && has_crab {
        return ConflictCheck::CrabConflict;
    }

    // Already tracked by LFS only.
    if has_lfs {
        return ConflictCheck::AlreadyLfs;
    }

    // Tracked by Crab/XET only — conflict.
    if has_crab {
        return ConflictCheck::CrabConflict;
    }

    ConflictCheck::Clear
}

/// Add an LFS tracking pattern to `.gitattributes`.
///
/// Appends `<escaped_pattern> filter=lfs diff=lfs merge=lfs -text` to the
/// file. If the pattern is already tracked by LFS, the file is left unchanged.
/// Creates `.gitattributes` if it does not exist.
///
/// # Errors
///
/// Returns [`CrabError::Io`] if the file cannot be read or written.
pub fn track(pattern: &str, repo_root: &Path) -> Result<TrackOutcome> {
    track_with_opts(pattern, repo_root, false, false)
}

/// Track with options for force override and dry-run.
pub fn track_with_opts(
    pattern: &str,
    repo_root: &Path,
    force: bool,
    dry_run: bool,
) -> Result<TrackOutcome> {
    track_with_options(pattern, repo_root, TrackOptions::new(force, dry_run))
}

/// Track with full options.
pub fn track_with_options(
    pattern: &str,
    repo_root: &Path,
    options: TrackOptions,
) -> Result<TrackOutcome> {
    track_escaped_with_options(&escape_attr_pattern(pattern), repo_root, options)
}

/// Track a literal filename, escaping glob metacharacters for `.gitattributes`.
pub fn track_filename_with_opts(
    filename: &str,
    repo_root: &Path,
    force: bool,
    dry_run: bool,
) -> Result<TrackOutcome> {
    track_filename_with_options(filename, repo_root, TrackOptions::new(force, dry_run))
}

/// Track a literal filename with full options.
pub fn track_filename_with_options(
    filename: &str,
    repo_root: &Path,
    options: TrackOptions,
) -> Result<TrackOutcome> {
    track_escaped_with_options(&escape_attr_filename(filename), repo_root, options)
}

fn track_escaped_with_options(
    escaped: &str,
    repo_root: &Path,
    options: TrackOptions,
) -> Result<TrackOutcome> {
    let ga_path = repo_root.join(".gitattributes");
    let new_line = lfs_attrs_line(escaped, options.lockable);

    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let has_lfs = is_lfs_pattern_present(&content, escaped);
    let has_crab = is_crab_pattern_present(&content, escaped);
    let desired_lockable = desired_lockable(options.lockable);

    // Both filters present: remove crab entry, keep LFS entry (resolve conflict).
    if has_lfs && has_crab {
        if !options.force {
            return Ok(TrackOutcome::AlreadyTracked); // conflict detected by caller
        }
        let new_content = replace_matching_entries(&content, escaped, &new_line);
        if !options.dry_run {
            std::fs::write(&ga_path, sort_lines(&new_content))?;
        }
        return Ok(TrackOutcome::SwitchedFromCrab);
    }

    // Already tracked by LFS only.
    if has_lfs {
        if desired_lockable
            .is_none_or(|desired| is_lfs_entry_lockable(&content, escaped) == Some(desired))
        {
            return Ok(TrackOutcome::AlreadyTracked);
        }

        let new_content = replace_matching_entries(&content, escaped, &new_line);
        if !options.dry_run {
            std::fs::write(&ga_path, sort_lines(&new_content))?;
        }
        return Ok(TrackOutcome::Updated);
    }

    // Tracked by Crab/XET only — replace with LFS.
    if has_crab {
        if !options.force {
            return Ok(TrackOutcome::AlreadyTracked); // conflict detected by caller
        }
        let new_content = replace_crab_with_lfs(&content, escaped, &new_line);
        if !options.dry_run {
            std::fs::write(&ga_path, sort_lines(&new_content))?;
        }
        return Ok(TrackOutcome::SwitchedFromCrab);
    }

    // Check for existing non-filter entry with same pattern.
    let outcome = if has_non_filter_entry_for(&content, escaped) {
        let new_content = replace_non_filter_entry(&content, escaped, &new_line);
        if !options.dry_run {
            std::fs::write(&ga_path, sort_lines(&new_content))?;
        }
        TrackOutcome::Updated
    } else {
        let mut output = content;
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&new_line);
        output.push('\n');
        if !options.dry_run {
            std::fs::write(&ga_path, sort_lines(&output))?;
        }
        TrackOutcome::Tracked
    };

    Ok(outcome)
}

/// Remove an LFS tracking pattern from `.gitattributes`.
///
/// Removes lines where the first token (after unescaping) matches `pattern`
/// and the line contains `filter=lfs`. Other lines are preserved.
/// Deletes `.gitattributes` if it becomes empty.
///
/// # Errors
///
/// Returns [`CrabError::Io`] if the file cannot be read or written.
pub fn untrack(pattern: &str, repo_root: &Path) -> Result<()> {
    let ga_path = repo_root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let escaped = escape_attr_pattern(pattern);
    let mut has_remaining = false;
    let mut output = String::with_capacity(content.len());

    for line in content.lines() {
        if should_remove_line(line, &escaped) {
            continue;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            has_remaining = true;
        }
        output.push_str(line);
        output.push('\n');
    }

    if has_remaining {
        std::fs::write(&ga_path, output)?;
    } else {
        // Delete empty .gitattributes.
        let _ = std::fs::remove_file(&ga_path);
    }
    Ok(())
}

/// Add a Git LFS untrack override for a pattern without removing older rules.
///
/// `git lfs migrate export` keeps historical tracking rules and appends a
/// later override so normal Git attributes precedence disables LFS for new
/// content while preserving the history of prior tracking decisions.
pub fn append_untrack_override(pattern: &str, repo_root: &Path) -> Result<()> {
    let ga_path = repo_root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };

    let escaped = escape_attr_pattern(pattern);
    let new_line = untrack_attrs_line(&escaped);
    if content.lines().any(|line| line.trim() == new_line) {
        return Ok(());
    }

    let mut output = content;
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&new_line);
    output.push('\n');
    std::fs::write(&ga_path, sort_lines(&output))?;

    Ok(())
}

/// Return all tracked patterns from `.gitattributes` with their filter types.
///
/// Includes both LFS and Crab/XET tracked patterns.
/// Returns an empty vec if the file does not exist.
pub fn list_all(repo_root: &Path) -> Result<Vec<TrackedPattern>> {
    let ga_path = repo_root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut patterns = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let filter = if trimmed.contains("filter=lfs") {
            Some(FilterType::Lfs)
        } else if trimmed.contains("filter=crab") {
            Some(FilterType::Crab)
        } else {
            None
        };

        if let (Some(ft), Some(raw)) = (filter, trimmed.split_whitespace().next()) {
            patterns.push(TrackedPattern {
                pattern: unescape_attr_pattern(raw),
                filter: ft,
            });
        }
    }

    Ok(patterns)
}

/// Return LFS-tracked patterns only (backward compatible).
pub fn list(repo_root: &Path) -> Result<Vec<String>> {
    let all = list_all(repo_root)?;
    Ok(all
        .into_iter()
        .filter(|p| p.filter == FilterType::Lfs)
        .map(|p| p.pattern)
        .collect())
}

/// Return Git-indexed paths that match a `git lfs track` pattern.
pub fn matching_index_paths(
    pattern: &str,
    repo_root: &Path,
    filename: bool,
) -> Result<Vec<String>> {
    let paths = git_index_paths(repo_root)?;
    Ok(paths
        .into_iter()
        .filter(|path| track_pattern_matches(pattern, path, filename))
        .collect())
}

/// Touch matching Git-indexed paths and return the paths touched.
pub fn mark_matches_stat_dirty_paths(
    pattern: &str,
    repo_root: &Path,
    filename: bool,
) -> Result<Vec<String>> {
    let paths = matching_index_paths(pattern, repo_root, filename)?;
    let mut touched = Vec::new();

    for path in paths {
        let full_path = repo_root.join(&path);
        if !full_path.is_file() {
            continue;
        }
        filetime::set_file_mtime(&full_path, filetime::FileTime::now())?;
        touched.push(path);
    }

    Ok(touched)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn git_index_paths(repo_root: &Path) -> Result<Vec<String>> {
    let output = git_command(repo_root)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "lfs track".to_owned(),
            origin: format!("failed to run git ls-files: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "lfs track".to_owned(),
            origin: format!("git ls-files failed: {stderr}"),
        });
    }

    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect())
}

/// Run Git against an explicit repository root, ignoring ambient repository
/// overrides that could redirect an index scan to another checkout.
fn git_command(repo_root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repo_root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_QUARANTINE_PATH")
        .env_remove("GIT_NAMESPACE");
    command
}

fn track_pattern_matches(pattern: &str, path: &str, filename: bool) -> bool {
    if filename {
        return path == pattern;
    }

    #[cfg(feature = "gix-pathmatch")]
    {
        if let Ok(filter) = crate::core::pathmatch::build_filter(&[pattern.to_owned()], &[]) {
            return filter.matches(path);
        }
    }

    legacy_track_pattern_matches(pattern, path)
}

fn legacy_track_pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        path.ends_with(suffix)
    } else {
        path == pattern || path.ends_with(&format!("/{pattern}"))
    }
}

fn is_lfs_pattern_present(content: &str, escaped_pattern: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains("filter=lfs") {
            continue;
        }
        if trimmed
            .split_whitespace()
            .next()
            .is_some_and(|first| first == escaped_pattern)
        {
            return true;
        }
    }
    false
}

fn is_lfs_entry_lockable(content: &str, escaped_pattern: &str) -> Option<bool> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains("filter=lfs") {
            continue;
        }
        if trimmed
            .split_whitespace()
            .next()
            .is_some_and(|first| first == escaped_pattern)
        {
            return Some(trimmed.split_whitespace().any(|part| part == "lockable"));
        }
    }
    None
}

fn is_crab_pattern_present(content: &str, escaped_pattern: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains("filter=crab") {
            continue;
        }
        if trimmed
            .split_whitespace()
            .next()
            .is_some_and(|first| first == escaped_pattern)
        {
            return true;
        }
    }
    false
}

fn has_non_filter_entry_for(content: &str, escaped_pattern: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.contains("filter=") {
            continue;
        }
        if trimmed
            .split_whitespace()
            .next()
            .is_some_and(|first| first == escaped_pattern)
        {
            return true;
        }
    }
    false
}

fn replace_crab_with_lfs(content: &str, escaped_pattern: &str, new_line: &str) -> String {
    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("filter=crab")
            && trimmed
                .split_whitespace()
                .next()
                .is_some_and(|p| p == escaped_pattern)
        {
            output.push_str(new_line);
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    output
}

fn replace_matching_entries(content: &str, escaped_pattern: &str, new_line: &str) -> String {
    let mut output = String::with_capacity(content.len());
    let mut wrote_lfs = false;

    for line in content.lines() {
        let trimmed = line.trim();
        let matches_pattern = !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && trimmed
                .split_whitespace()
                .next()
                .is_some_and(|p| p == escaped_pattern);

        if matches_pattern && trimmed.contains("filter=crab") {
            continue;
        }

        if matches_pattern && trimmed.contains("filter=lfs") {
            if !wrote_lfs {
                output.push_str(new_line);
                output.push('\n');
                wrote_lfs = true;
            }
            continue;
        }

        output.push_str(line);
        output.push('\n');
    }

    if !wrote_lfs {
        output.push_str(new_line);
        output.push('\n');
    }

    output
}

fn replace_non_filter_entry(content: &str, escaped_pattern: &str, new_line: &str) -> String {
    let mut output = String::with_capacity(content.len());
    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && !trimmed.contains("filter=")
            && trimmed
                .split_whitespace()
                .next()
                .is_some_and(|p| p == escaped_pattern)
        {
            output.push_str(new_line);
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    output
}

fn lfs_attrs_line(escaped: &str, lockable: LockableMode) -> String {
    match lockable {
        LockableMode::Enable => format!("{escaped} {LFS_ATTRS} lockable"),
        LockableMode::Preserve | LockableMode::Disable => format!("{escaped} {LFS_ATTRS}"),
    }
}

fn untrack_attrs_line(escaped: &str) -> String {
    format!("{escaped} {LFS_UNTRACK_ATTRS}")
}

fn desired_lockable(lockable: LockableMode) -> Option<bool> {
    match lockable {
        LockableMode::Preserve => None,
        LockableMode::Enable => Some(true),
        LockableMode::Disable => Some(false),
    }
}

fn sort_key(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        ""
    } else {
        trimmed.split_whitespace().next().unwrap_or("")
    }
}

fn sort_lines(content: &str) -> String {
    let mut lines: Vec<&str> = content.lines().collect();
    lines.sort_by(|a, b| {
        let key_a = sort_key(a);
        let key_b = sort_key(b);
        key_a.cmp(key_b)
    });
    let mut output = String::with_capacity(content.len());
    for line in lines {
        output.push_str(line);
        output.push('\n');
    }
    output
}

fn should_remove_line(line: &str, escaped_pattern: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    if !trimmed.contains("filter=lfs") {
        return false;
    }
    trimmed
        .split_whitespace()
        .next()
        .is_some_and(|first| first == escaped_pattern)
}

fn escape_attr_pattern(pattern: &str) -> String {
    let mut escaped = pattern.replace('\\', "\\\\");
    escaped = escaped.replace(' ', "[[:space:]]");
    escaped = escaped.replace('#', "\\#");
    escaped
}

fn escape_attr_filename(filename: &str) -> String {
    let mut escaped = String::with_capacity(filename.len());
    for c in filename.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            ' ' => escaped.push_str("[[:space:]]"),
            '#' => escaped.push_str("\\#"),
            '*' | '?' | '[' | ']' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

fn unescape_attr_pattern(escaped: &str) -> String {
    let mut s = escaped.replace("\\#", "#");
    s = s.replace("[[:space:]]", " ");
    s = s.replace("\\*", "*");
    s = s.replace("\\?", "?");
    s = s.replace("\\[", "[");
    s = s.replace("\\]", "]");
    s = s.replace("\\\\", "\\");
    s
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn track_creates_gitattributes_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = track("*.bin", dir.path()).unwrap();
        assert_eq!(outcome, TrackOutcome::Tracked);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.bin filter=lfs diff=lfs merge=lfs -text"));
    }

    #[test]
    fn track_filename_escapes_glob_characters() {
        let dir = tempfile::tempdir().unwrap();
        let outcome =
            track_filename_with_opts("project [1]?.psd", dir.path(), false, false).unwrap();
        assert_eq!(outcome, TrackOutcome::Tracked);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("project[[:space:]]\\[1\\]\\?.psd filter=lfs"));
    }

    #[test]
    fn track_lockable_adds_lockable_attribute() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = track_with_options(
            "*.psd",
            dir.path(),
            TrackOptions {
                force: false,
                dry_run: false,
                lockable: LockableMode::Enable,
            },
        )
        .unwrap();

        assert_eq!(outcome, TrackOutcome::Tracked);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.psd filter=lfs diff=lfs merge=lfs -text lockable"));
    }

    #[test]
    fn track_lockable_updates_existing_lfs_entry() {
        let dir = tempfile::tempdir().unwrap();
        track("*.psd", dir.path()).unwrap();

        let outcome = track_with_options(
            "*.psd",
            dir.path(),
            TrackOptions {
                force: false,
                dry_run: false,
                lockable: LockableMode::Enable,
            },
        )
        .unwrap();

        assert_eq!(outcome, TrackOutcome::Updated);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.psd filter=lfs diff=lfs merge=lfs -text lockable"));
    }

    #[test]
    fn track_not_lockable_removes_lockable_attribute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.psd filter=lfs diff=lfs merge=lfs -text lockable\n",
        )
        .unwrap();

        let outcome = track_with_options(
            "*.psd",
            dir.path(),
            TrackOptions {
                force: false,
                dry_run: false,
                lockable: LockableMode::Disable,
            },
        )
        .unwrap();

        assert_eq!(outcome, TrackOutcome::Updated);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.psd filter=lfs diff=lfs merge=lfs -text"));
        assert!(!content.contains("lockable"));
    }

    #[test]
    fn append_untrack_override_preserves_lfs_rule() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        append_untrack_override("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.bin filter=lfs diff=lfs merge=lfs -text\n"));
        assert!(content.contains("*.bin !text !filter !merge !diff\n"));
    }

    #[test]
    fn append_untrack_override_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();

        append_untrack_override("*.bin", dir.path()).unwrap();
        append_untrack_override("*.bin", dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert_eq!(
            content
                .lines()
                .filter(|line| line.trim() == "*.bin !text !filter !merge !diff")
                .count(),
            1
        );
    }

    #[test]
    fn track_preserve_leaves_lockable_entry_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.psd filter=lfs diff=lfs merge=lfs -text lockable\n",
        )
        .unwrap();

        let outcome = track_with_opts("*.psd", dir.path(), false, false).unwrap();

        assert_eq!(outcome, TrackOutcome::AlreadyTracked);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.psd filter=lfs diff=lfs merge=lfs -text lockable"));
    }

    #[test]
    fn track_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        track("*.bin", dir.path()).unwrap();
        let outcome = track("*.bin", dir.path()).unwrap();
        assert_eq!(outcome, TrackOutcome::AlreadyTracked);
    }

    #[test]
    fn check_conflict_detects_crab() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text
",
        )
        .unwrap();
        assert_eq!(
            check_conflict("*.bin", dir.path()),
            ConflictCheck::CrabConflict
        );
    }

    #[test]
    fn check_conflict_detects_lfs() {
        let dir = tempfile::tempdir().unwrap();
        track("*.bin", dir.path()).unwrap();
        assert_eq!(
            check_conflict("*.bin", dir.path()),
            ConflictCheck::AlreadyLfs
        );
    }

    #[test]
    fn check_conflict_detects_mixed_lfs_and_crab() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n\
             *.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        assert_eq!(
            check_conflict("*.bin", dir.path()),
            ConflictCheck::CrabConflict
        );
    }

    #[test]
    fn force_overrides_crab() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text
",
        )
        .unwrap();
        let outcome = track_with_opts("*.bin", dir.path(), true, false).unwrap();
        assert_eq!(outcome, TrackOutcome::SwitchedFromCrab);
        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("filter=lfs"));
        assert!(!content.contains("filter=crab"));
    }

    #[test]
    fn force_removes_crab_entry_when_lfs_entry_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n\
             *.bin filter=crab diff=crab merge=crab -text\n\
             *.txt text\n",
        )
        .unwrap();

        let outcome = track_with_opts("*.bin", dir.path(), true, false).unwrap();
        assert_eq!(outcome, TrackOutcome::SwitchedFromCrab);

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).unwrap();
        assert!(content.contains("*.bin filter=lfs"));
        assert!(!content.contains("*.bin filter=crab"));
        assert!(content.contains("*.txt text"));
    }

    #[test]
    fn list_all_shows_filter_types() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text
*.safetensors filter=crab diff=crab merge=crab -text
",
        )
        .unwrap();
        let all = list_all(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].filter, FilterType::Lfs);
        assert_eq!(all[1].filter, FilterType::Crab);
    }

    #[test]
    fn list_only_returns_lfs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text
*.safetensors filter=crab diff=crab merge=crab -text
",
        )
        .unwrap();
        let lfs = list(dir.path()).unwrap();
        assert_eq!(lfs, vec!["*.bin"]);
    }

    #[test]
    fn no_modify_attrs_marks_indexed_matches_without_attrs_file() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(dir.path())
            .status()
            .unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"payload").unwrap();
        std::process::Command::new("git")
            .args(["add", "data.bin"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        let old_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old_time)).unwrap();

        let touched = mark_matches_stat_dirty_paths("*.bin", dir.path(), false).unwrap();

        assert_eq!(touched, vec!["data.bin"]);
        assert!(!dir.path().join(".gitattributes").exists());
        assert!(std::fs::metadata(&path).unwrap().modified().unwrap() > old_time);
    }

    #[test]
    fn filename_match_is_literal_for_index_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(dir.path())
            .status()
            .unwrap();
        std::fs::write(dir.path().join("project [1].psd"), b"payload").unwrap();
        std::fs::write(dir.path().join("project X.psd"), b"payload").unwrap();
        std::process::Command::new("git")
            .args(["add", "project [1].psd", "project X.psd"])
            .current_dir(dir.path())
            .status()
            .unwrap();

        let matches = matching_index_paths("project [1].psd", dir.path(), true).unwrap();

        assert_eq!(matches, vec!["project [1].psd"]);
    }
}
