//! `.gitattributes` filter cache for LFS/XET dispatch.
//!
//! Parses `.gitattributes` files (root + nested) once per filter session,
//! caches the compiled patterns, and provides [`FilterAttrCache::resolve_filter`]
//! to determine which handler (LFS or Crab/XET) should process a given file path.
//!
//! Resolution follows git's "last matching line wins" semantics for the
//! `filter` attribute. When both `filter=lfs` and `filter=crab` match the
//! same path, the filter from the later line wins.

use std::path::Path;
use std::time::SystemTime;

use bstr::{BStr, ByteSlice as _};

/// The filter handler to use for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    /// Use the LFS handler (SHA-256 + LFS pointer + LFS object store).
    Lfs,
    /// Use the Crab/XET handler (blake3 + CDC + xorb store).
    Crab,
}

/// A single compiled entry from a `.gitattributes` line.
#[derive(Debug, Clone)]
pub struct FilterEntry {
    /// The glob pattern from the `.gitattributes` line.
    pub pattern: String,
    /// Whether this pattern contained a `/` (full-path match) or not (basename match).
    pub has_path_separator: bool,
    /// The filter kind declared on this line, if any.
    pub filter: Option<FilterKind>,
    /// The line number in the original `.gitattributes` file (for debug logging).
    pub line_number: u32,
}

/// Cached `.gitattributes` filter rules for a repository.
///
/// Built once per filter session and shared across all files processed
/// in that session. Rebuilt when `.gitattributes` mtime changes.
pub struct FilterAttrCache {
    /// Compiled entries in `.gitattributes` file order.
    entries: Vec<FilterEntry>,
    compiled_patterns: Vec<Option<gix_glob::Pattern>>,
    /// Mtime of the root `.gitattributes` when last parsed.
    root_mtime: Option<SystemTime>,
}

impl FilterAttrCache {
    /// Build a new cache by parsing `.gitattributes` rooted at `repo_root`.
    ///
    /// Collects the root file and index-tracked nested `.gitattributes` files.
    /// Prefer [`collect_all_entries`] + [`Self::from_entries`] when the caller
    /// also needs raw entries, avoiding duplicate index scans and file reads.
    pub fn from_repo_root(repo_root: &Path) -> Self {
        let (entries, root_mtime) = collect_all_entries(repo_root);
        Self::from_entries(entries, root_mtime)
    }

    /// Build a cache from pre-collected entries.
    ///
    /// Use [`collect_all_entries`] to share the input with other consumers,
    /// such as LFS pattern extraction, without rereading attribute files.
    pub fn from_entries(entries: Vec<FilterEntry>, root_mtime: Option<SystemTime>) -> Self {
        let compiled_patterns = entries
            .iter()
            .map(|entry| gix_glob::Pattern::from_bytes(entry.pattern.as_bytes()))
            .collect();
        Self {
            entries,
            compiled_patterns,
            root_mtime,
        }
    }

    /// Return a reference to the cached entries (for external LFS-pattern
    /// extraction without a second parse).
    pub fn entries(&self) -> &[FilterEntry] {
        &self.entries
    }

    /// Returns `true` if the cache should be rebuilt because `.gitattributes`
    /// has been modified since this cache was created.
    pub fn is_stale(&self, repo_root: &Path) -> bool {
        let ga_path = repo_root.join(".gitattributes");
        let current_mtime = std::fs::metadata(&ga_path)
            .ok()
            .and_then(|m| m.modified().ok());
        self.root_mtime != current_mtime
    }

    /// Resolve which filter handler should process the given file path.
    ///
    /// Iterates through all entries in `.gitattributes` file order. For each
    /// entry whose pattern matches `pathname`, updates the winner. Returns
    /// the filter from the **last** matching line, or `None` if no filter
    /// attribute matches.
    ///
    /// Follows git wildmatch semantics:
    /// - Patterns without `/` match against the basename (any directory).
    /// - Patterns with `/` match against the full relative path.
    pub fn resolve_filter(&self, pathname: &str) -> Option<FilterKind> {
        let mut winner: Option<FilterKind> = None;

        for (entry, pattern) in self.entries.iter().zip(&self.compiled_patterns) {
            if pattern
                .as_ref()
                .is_some_and(|pattern| entry_matches(pattern, pathname))
            {
                winner = entry.filter;
            }
        }

        winner
    }
}

// Entry collection.

/// Collect all `.gitattributes` filter entries from root + nested files.
///
/// Returns the entries in git priority order (root first, nested last —
/// "last match wins") and the mtime of the root `.gitattributes`
/// (for staleness detection).
///
/// The root `.gitattributes` is read from the working tree so the just-run
/// `crab setup` timing works (the file may not be staged yet). Nested
/// `.gitattributes` are discovered via `git ls-files`, which reads git's
/// in-memory index instead of stat-walking the whole working tree. This
/// keeps filter-process startup O(tracked `.gitattributes` files) rather
/// than O(total files) — critical on repos with millions of files where
/// the previous `read_dir` walk could stall for seconds.
///
/// Any `git` failure silently falls back to root-only, matching the
/// best-effort behavior of the `std::fs` reads below.
///
/// Callers that need both a [`FilterAttrCache`] and raw LFS patterns
/// should call this once and feed the result to both consumers, avoiding
/// a redundant second parse. See [`FilterAttrCache::from_entries`].
pub fn collect_all_entries(repo_root: &Path) -> (Vec<FilterEntry>, Option<SystemTime>) {
    let root_ga = repo_root.join(".gitattributes");
    let root_mtime = std::fs::metadata(&root_ga)
        .ok()
        .and_then(|m| m.modified().ok());

    let mut entries = Vec::new();

    // Root .gitattributes.
    collect_entries(&root_ga, &mut entries);

    // Nested .gitattributes from the git index. Cheap (reads the index, not
    // the working tree) and scales with tracked files rather than total.
    for ga_rel in list_tracked_gitattributes(repo_root) {
        // Skip the root file — already parsed above. `git ls-files --full-name`
        // yields repo-root-relative paths, so the root entry is exactly
        // ".gitattributes".
        if ga_rel == ".gitattributes" {
            continue;
        }
        let prefix = parent_dir_as_prefix(&ga_rel);
        let ga_abs = repo_root.join(&ga_rel);
        collect_entries_with_prefix(&ga_abs, &prefix, &mut entries);
    }

    (entries, root_mtime)
}

/// List tracked `.gitattributes` files via `git ls-files`, repo-root-relative.
///
/// Returns an empty vec on any `git` failure (non-zero exit, spawn error,
/// non-UTF8 path) so callers degrade to root-only. Uses NUL-delimited
/// output (`-z`) for path safety.
fn list_tracked_gitattributes(repo_root: &Path) -> Vec<String> {
    // SHELLOUT: `git ls-files` is the established way to enumerate tracked
    // files without walking the working tree (see reset.rs / clone.rs). Runs
    // once at filter-process startup; the index is already in memory.
    let output = match std::process::Command::new("git")
        .args(["ls-files", "--full-name", "-z", "--"])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "git ls-files failed; falling back to root-only .gitattributes"
            );
            return Vec::new();
        }
    };

    if !output.status.success() {
        tracing::debug!(
            status = ?output.status.code(),
            "git ls-files exited non-zero; falling back to root-only .gitattributes"
        );
        return Vec::new();
    }

    output
        .stdout
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .filter(|entry| {
            // Basename is ".gitattributes". `ends_with` alone is wrong
            // (matches "foo.gitattributes"); require a preceding '/' or an
            // exact match for the root file.
            matches!(entry.rsplit(|b| *b == b'/').next(), Some(b".gitattributes"))
        })
        .filter_map(|entry| std::str::from_utf8(entry).ok().map(str::to_string))
        .collect()
}

/// Derive the directory prefix for a repo-root-relative `.gitattributes`
/// path. `subdir/foo/.gitattributes` → `"subdir/foo"`; `.gitattributes`
/// (root) → `""`.
fn parent_dir_as_prefix(ga_rel: &str) -> String {
    match ga_rel.rfind('/') {
        Some(idx) => ga_rel[..idx].to_owned(),
        None => String::new(),
    }
}

// Parsing.

/// Collect filter entries from a single `.gitattributes` file.
fn collect_entries(ga_path: &Path, entries: &mut Vec<FilterEntry>) {
    let Ok(content) = std::fs::read(ga_path) else {
        return;
    };
    collect_parsed_entries(&content, "", entries);
}

/// Collect filter entries from a nested `.gitattributes`, prefixing
/// patterns with the subdirectory path.
fn collect_entries_with_prefix(ga_path: &Path, prefix: &str, entries: &mut Vec<FilterEntry>) {
    let Ok(content) = std::fs::read(ga_path) else {
        return;
    };
    collect_parsed_entries(&content, prefix, entries);
}

fn collect_parsed_entries(content: &[u8], prefix: &str, entries: &mut Vec<FilterEntry>) {
    for parsed in gix_attributes::parse(content).filter_map(Result::ok) {
        let (gix_attributes::parse::Kind::Pattern(pattern), assignments, line_number) = parsed
        else {
            continue;
        };
        let Some(filter) = filter_assignment(assignments) else {
            continue;
        };
        let raw_pattern = pattern.to_string();
        let pattern = if prefix.is_empty() {
            raw_pattern
        } else {
            format!("{prefix}/{}", raw_pattern.trim_start_matches('/'))
        };
        entries.push(FilterEntry {
            has_path_separator: pattern.contains('/'),
            pattern,
            filter,
            line_number: u32::try_from(line_number).unwrap_or(u32::MAX),
        });
    }
}

fn filter_assignment<'a>(
    assignments: impl Iterator<
        Item = Result<gix_attributes::AssignmentRef<'a>, gix_attributes::name::Error>,
    >,
) -> Option<Option<FilterKind>> {
    let mut filter = None;
    for assignment in assignments.filter_map(Result::ok) {
        if assignment.name.as_str() != "filter" {
            continue;
        }
        filter = Some(
            match assignment
                .state
                .as_bstr()
                .map(<BStr as AsRef<[u8]>>::as_ref)
            {
                Some(b"lfs") => Some(FilterKind::Lfs),
                Some(b"crab") => Some(FilterKind::Crab),
                Some(_) | None => None,
            },
        );
    }
    filter
}

// Pattern matching.

/// Check whether an entry's pattern matches a file path.
///
/// Follows git wildmatch semantics:
/// - Patterns without `/` match the basename (by prepending `**/` before matching).
/// - Patterns with `/` match the full path.
fn entry_matches(pattern: &gix_glob::Pattern, pathname: &str) -> bool {
    let path = pathname.as_bytes().as_bstr();
    pattern.matches_repo_relative_path(
        path,
        path.rfind_byte(b'/').map(|position| position + 1),
        Some(false),
        gix_glob::pattern::Case::Sensitive,
        gix_glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn cache_from_str(content: &str) -> FilterAttrCache {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), content).unwrap();
        FilterAttrCache::from_repo_root(dir.path())
    }

    #[test]
    fn resolve_single_lfs_pattern() {
        let cache = cache_from_str("*.bin filter=lfs diff=lfs merge=lfs -text\n");
        assert_eq!(cache.resolve_filter("model.bin"), Some(FilterKind::Lfs));
        assert_eq!(cache.resolve_filter("model.txt"), None);
    }

    #[test]
    fn resolve_single_crab_pattern() {
        let cache = cache_from_str("*.safetensors filter=crab diff=crab merge=crab -text\n");
        assert_eq!(
            cache.resolve_filter("model.safetensors"),
            Some(FilterKind::Crab)
        );
        assert_eq!(cache.resolve_filter("model.bin"), None);
    }

    #[test]
    fn last_match_wins_same_pattern() {
        let cache = cache_from_str(
            "*.bin filter=lfs diff=lfs merge=lfs -text\n\
             *.bin filter=crab diff=crab merge=crab -text\n",
        );
        // Second line wins.
        assert_eq!(cache.resolve_filter("model.bin"), Some(FilterKind::Crab));
    }

    #[test]
    fn last_match_wins_reversed() {
        let cache = cache_from_str(
            "*.bin filter=crab diff=crab merge=crab -text\n\
             *.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        // Second line wins.
        assert_eq!(cache.resolve_filter("model.bin"), Some(FilterKind::Lfs));
    }

    #[test]
    fn overlapping_patterns_last_wins() {
        let cache = cache_from_str(
            "*.bin filter=crab diff=crab merge=crab -text\n\
             models/*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        // Both match models/data.bin, last match (line 2) wins.
        assert_eq!(
            cache.resolve_filter("models/data.bin"),
            Some(FilterKind::Lfs)
        );
        // Only line 1 matches other.bin.
        assert_eq!(cache.resolve_filter("other.bin"), Some(FilterKind::Crab));
    }

    #[test]
    fn exact_file_path_matches() {
        let cache = cache_from_str("models/special.bin filter=lfs diff=lfs merge=lfs -text\n");
        assert_eq!(
            cache.resolve_filter("models/special.bin"),
            Some(FilterKind::Lfs)
        );
        assert_eq!(cache.resolve_filter("models/other.bin"), None);
        assert_eq!(cache.resolve_filter("special.bin"), None);
    }

    #[test]
    fn quoted_literal_filename_matches_git_attribute_syntax() {
        let dir = temp_git_repo();
        let nested = dir.path().join("team");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(
            nested.join(".gitattributes"),
            r#""/large gateway \\[v1\\] #\\*\\?.bin" filter=lfs diff=lfs merge=lfs -text
"#,
        )
        .unwrap();
        let status = std::process::Command::new("git")
            .args(["add", "team/.gitattributes"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let cache = FilterAttrCache::from_repo_root(dir.path());

        assert_eq!(
            cache.resolve_filter("team/large gateway [v1] #*?.bin"),
            Some(FilterKind::Lfs)
        );
        assert_eq!(
            cache.resolve_filter("team/large gateway av1] #xx.bin"),
            None
        );
        assert_eq!(
            cache.resolve_filter("team/deeper/large gateway [v1] #*?.bin"),
            None
        );
    }

    #[test]
    fn unset_filter_clears_an_earlier_match() {
        let cache = cache_from_str("*.bin filter=lfs\nspecial.bin -filter\n");

        assert_eq!(cache.resolve_filter("ordinary.bin"), Some(FilterKind::Lfs));
        assert_eq!(cache.resolve_filter("special.bin"), None);
    }

    #[test]
    fn basename_pattern_matches_any_directory() {
        // Pattern without `/` should match basename in any directory.
        let cache = cache_from_str("model.bin filter=lfs diff=lfs merge=lfs -text\n");
        assert_eq!(cache.resolve_filter("model.bin"), Some(FilterKind::Lfs));
        assert_eq!(
            cache.resolve_filter("subdir/model.bin"),
            Some(FilterKind::Lfs)
        );
        assert_eq!(
            cache.resolve_filter("deep/nested/model.bin"),
            Some(FilterKind::Lfs)
        );
        assert_eq!(cache.resolve_filter("other.bin"), None);
    }

    #[test]
    fn no_filter_match_returns_none() {
        let cache = cache_from_str("*.txt text\n# just a comment\n");
        assert_eq!(cache.resolve_filter("file.txt"), None);
        assert_eq!(cache.resolve_filter("file.bin"), None);
    }

    #[test]
    fn empty_cache_returns_none() {
        let cache = cache_from_str("");
        assert_eq!(cache.resolve_filter("anything.bin"), None);
    }

    #[test]
    fn cache_staleness_detected() {
        let dir = tempfile::tempdir().unwrap();
        let ga_path = dir.path().join(".gitattributes");
        std::fs::write(&ga_path, "*.bin filter=lfs\n").unwrap();

        let cache = FilterAttrCache::from_repo_root(dir.path());
        assert!(
            !cache.is_stale(dir.path()),
            "fresh cache should not be stale"
        );

        // Touch the file.
        std::fs::write(&ga_path, "*.bin filter=crab\n").unwrap();
        assert!(
            cache.is_stale(dir.path()),
            "cache should be stale after file change"
        );
    }

    /// Initialize a temp dir as a git repo so `git ls-files` works in
    /// `collect_all_entries`. Mirrors the harness in init.rs tests.
    fn temp_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        dir
    }

    /// `collect_all_entries` must find nested `.gitattributes` via the git
    /// index and prefix their patterns with the subdirectory path, without
    /// walking the working tree. This is the perf-critical path on large
    /// repos — regression would reintroduce the recursive `read_dir` walk.
    #[test]
    fn collect_finds_nested_gitattributes_via_index() {
        let dir = temp_git_repo();
        let root = dir.path();

        // Root .gitattributes (working-tree read; needs no staging).
        std::fs::write(root.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();

        // Nested .gitattributes — must be staged for `git ls-files` to see it.
        let sub = root.join("models");
        std::fs::create_dir_all(&sub).unwrap();
        let nested_ga = sub.join(".gitattributes");
        std::fs::write(&nested_ga, "*.bin filter=crab\n").unwrap();
        let status = std::process::Command::new("git")
            .args(["add", "models/.gitattributes"])
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git add failed");

        let cache = FilterAttrCache::from_repo_root(root);

        // Root pattern wins for files outside models/.
        assert_eq!(
            cache.resolve_filter("data.bin"),
            Some(FilterKind::Lfs),
            "root .gitattributes should apply to top-level files"
        );
        // Nested pattern (last match wins) overrides for files in models/.
        assert_eq!(
            cache.resolve_filter("models/weights.bin"),
            Some(FilterKind::Crab),
            "nested models/.gitattributes should override root for models/ files"
        );
    }

    /// When the repo isn't a git repo (no index), `collect_all_entries`
    /// must still read the root `.gitattributes` from the working tree and
    /// silently skip nested discovery rather than failing.
    #[test]
    fn collect_falls_back_to_root_when_not_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), "*.bin filter=lfs\n").unwrap();

        let cache = FilterAttrCache::from_repo_root(dir.path());
        assert_eq!(cache.resolve_filter("model.bin"), Some(FilterKind::Lfs));
    }
}
